//! Wrapper renderer for assistant prose rows.
//!
//! The wait placeholder row (an assistant turn with no other rows) carries
//! `part = PartId::NONE` and collapses to nothing here; the *view* renders
//! the P1 `ShimmerText` for a waiting turn. Real prose rows stream through
//! a retained [`MarkdownBody`], exactly like the P1 mirror did.

use gpui::{
    App, IntoElement, ParentElement as _, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::text::TextViewStyle;

use crate::chat::projection::RowKind;
use crate::chat::transcript::PartSource;
use crate::ui::markdown::MarkdownBody;

use super::{MaterializeContext, RowChange, RowRenderContext, RowRenderer};

pub(crate) struct ProseRenderer {
    text: String,
    finished: bool,
    body: Option<MarkdownBody>,
    materialized: bool,
}

impl ProseRenderer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            finished: false,
            body: None,
            materialized: false,
        }
    }

    fn rebuild_body(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        // A streaming body exists from the moment its part is inserted (even
        // empty), matching the P1 mirror: the first delta lands in the same
        // markdown state and tests can observe its owner immediately.
        self.body = if self.finished {
            if self.text.is_empty() {
                None
            } else {
                Some(MarkdownBody::new_with_presentation(
                    &self.text,
                    ctx.owner_id,
                    ctx.presentation,
                    cx,
                ))
            }
        } else {
            Some(MarkdownBody::new_streaming_with_presentation(
                &self.text,
                ctx.owner_id,
                ctx.presentation,
                cx,
            ))
        };
    }
}

impl ProseRenderer {
    #[cfg(test)]
    pub(crate) fn body_owner_for_test(&self) -> Option<u64> {
        self.body.as_ref().map(MarkdownBody::owner_id)
    }

    #[cfg(test)]
    pub(crate) fn text_for_test(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(crate) fn body_for_test(&self) -> Option<&MarkdownBody> {
        self.body.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn body_for_test_mut(&mut self) -> Option<&mut MarkdownBody> {
        self.body.as_mut()
    }
}

impl RowRenderer for ProseRenderer {
    fn kind(&self) -> RowKind {
        RowKind::AssistantProse
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        match ctx.part {
            Some(part) => {
                if let PartSource::Prose { text, .. } = &part.source {
                    // A live insert seeds empty: the transcript part already
                    // carries the triggering delta and the following Append
                    // replays it (P1 empty-seed rule). A late materialization
                    // (cold restore mid-stream, first layout) re-reads the
                    // accumulated content so no prefix is lost.
                    self.text = if part.finished || !ctx.append_replays_part {
                        text.clone()
                    } else {
                        String::new()
                    };
                }
                self.finished = part.finished;
            }
            None => {
                self.text.clear();
                self.finished = true;
            }
        }
        self.rebuild_body(ctx, cx);
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.body = None;
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App) {
        match change {
            RowChange::Append { delta } => {
                self.text.push_str(delta);
                if let Some(body) = self.body.as_mut() {
                    body.push_str(delta, cx);
                } else if !self.text.is_empty() {
                    // First content on a row that started empty: stream the
                    // authoritative text so far.
                    self.rebuild_body(ctx, cx);
                }
            }
            RowChange::Finished => {
                self.finished = true;
                if let Some(body) = self.body.as_mut() {
                    body.finish(cx);
                }
            }
            RowChange::Replace => {
                // Authoritative reconciliation keeps the markdown entity so
                // selection and keyed caches survive.
                let next = ctx.prose_text().unwrap_or_default().to_string();
                let now_finished = ctx.part.is_some_and(|part| part.finished);
                if let Some(body) = self.body.as_mut() {
                    body.set_text(&next, cx);
                    if now_finished && !self.finished {
                        body.finish(cx);
                    }
                } else if !next.is_empty() {
                    self.text = next.clone();
                    self.finished = now_finished;
                    self.rebuild_body(ctx, cx);
                }
                self.text = next;
                self.finished = now_finished;
            }
        }
    }

    fn render(
        &self,
        ctx: &RowRenderContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> gpui::AnyElement {
        // A waiting turn renders the shimmer instead of its rows; the view
        // builds the shimmer for the turn's first row.
        if ctx.waiting {
            return div().into_any_element();
        }
        if self.text.is_empty() {
            return div().w_full().into_any_element();
        }
        div()
            .w_full()
            .when_some(self.body.as_ref(), |this, body| {
                this.child(body.text_view(TextViewStyle::default()))
            })
            .into_any_element()
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
