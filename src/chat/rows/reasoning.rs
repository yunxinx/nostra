//! Wrapper renderer for one reasoning ("chain of thought") card.
//!
//! This is a direct wrap of the P0 [`ReasoningTrace`] card: the toggle
//! button, the seven-line height budget, Natural/Virtualized scroll
//! migration, and the `MarkdownBody::finish` contract are all unchanged.
//! The renderer owns the trace; disclosure is reported back to the
//! projection at release and restored at re-materialization, and wheel
//! input on the card is forwarded to the view through
//! [`RowAction::ReplayNestedScroll`] so transcript-level easing, the
//! inactive-window frame contract, and the nested scroll boundary stay in
//! one place.

use std::rc::Rc;

use gpui::{AnyElement, App, ClickEvent, IntoElement, ScrollWheelEvent, Window, div};

use crate::chat::ReasoningTrace;
use crate::chat::projection::{DisclosureState, RowKind};
use crate::chat::reasoning_card;
use crate::chat::transcript::PartSource;
use crate::ui::markdown::MarkdownPresentation;

use super::{MaterializeContext, RowAction, RowChange, RowRenderContext, RowRenderer};

type ToggleHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type CopyValue = Rc<dyn Fn(&mut Window, &mut App) -> gpui::SharedString>;
type ScrollHandler = Rc<dyn Fn(&ScrollWheelEvent, &mut Window, &mut App)>;

pub(crate) struct ReasoningRenderer {
    display: String,
    finished: bool,
    content_index: usize,
    trace: Option<ReasoningTrace>,
    materialized: bool,
    owner_id: u64,
    presentation: Option<MarkdownPresentation>,
}

impl ReasoningRenderer {
    pub(crate) fn new() -> Self {
        Self {
            display: String::new(),
            finished: false,
            content_index: 0,
            trace: None,
            materialized: false,
            owner_id: 0,
            presentation: None,
        }
    }

    fn build_trace(&mut self, presentation: &MarkdownPresentation, cx: &mut App) {
        self.trace = if self.display.is_empty() {
            None
        } else if self.finished {
            Some(ReasoningTrace::completed_with_presentation(
                self.display.clone(),
                self.owner_id,
                presentation,
                cx,
            ))
        } else {
            let mut trace = ReasoningTrace::new_with_presentation(self.owner_id, presentation, cx);
            trace.push(&self.display, cx);
            Some(trace)
        };
    }
}

impl ReasoningRenderer {
    #[cfg(test)]
    pub(crate) fn trace_for_test(&self) -> Option<&ReasoningTrace> {
        self.trace.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn trace_for_test_mut(&mut self) -> Option<&mut ReasoningTrace> {
        self.trace.as_mut()
    }
}

impl RowRenderer for ReasoningRenderer {
    fn kind(&self) -> RowKind {
        RowKind::Reasoning
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.owner_id = ctx.owner_id;
        self.presentation = Some(ctx.presentation.clone());
        if let Some(part) = ctx.part {
            self.finished = part.finished;
            self.content_index = part.content_index;
            if let PartSource::Reasoning { reasoning, .. } = &part.source {
                // A live insert seeds empty: stream batches publish Insert
                // then Append after the model already carries the delta, and
                // the following Append replays the accumulated source (P1
                // empty-seed rule). A late materialization (cold restore
                // mid-stream, first layout) re-reads the accumulated content
                // so no prefix is lost.
                self.display = if part.finished || !ctx.append_replays_part {
                    reasoning.display.clone()
                } else {
                    String::new()
                };
            }
        }
        if let Some(presentation) = self.presentation.clone() {
            self.build_trace(&presentation, cx);
        }
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.trace = None;
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App) {
        match change {
            RowChange::Append { delta } => {
                self.display.push_str(delta);
                if let Some(trace) = self.trace.as_mut() {
                    trace.push(delta, cx);
                } else if !self.display.is_empty() {
                    // First content on a row that materialized empty.
                    self.owner_id = ctx.owner_id;
                    self.presentation = Some(ctx.presentation.clone());
                    if let Some(presentation) = self.presentation.clone() {
                        self.build_trace(&presentation, cx);
                    }
                }
            }
            RowChange::Finished => {
                self.finished = true;
                if let Some(trace) = self.trace.as_mut() {
                    trace.finish(cx);
                }
            }
            RowChange::Replace => {
                // Reuse semantics from the P1 mirror: keep the trace entity
                // when the block survived reconciliation so disclosure,
                // timing, and the markdown identity persist.
                let next = ctx
                    .reasoning_display()
                    .map(str::to_string)
                    .unwrap_or_default();
                let now_finished = ctx.part.is_some_and(|part| part.finished);
                self.owner_id = ctx.owner_id;
                self.presentation = Some(ctx.presentation.clone());
                let had_trace = self.trace.take();
                match (had_trace, next.is_empty()) {
                    (Some(mut trace), false) => {
                        if trace.source_len() != next.len() {
                            trace.set_source(&next, cx);
                        }
                        if now_finished {
                            trace.finish(cx);
                        }
                        self.trace = Some(trace);
                    }
                    _ => {
                        self.display = next;
                        self.finished = now_finished;
                        if let Some(presentation) = self.presentation.clone() {
                            self.build_trace(&presentation, cx);
                        }
                    }
                }
                self.display = ctx
                    .reasoning_display()
                    .map(str::to_string)
                    .unwrap_or_default();
                self.finished = now_finished || self.finished;
            }
        }
    }

    fn render(&self, ctx: &RowRenderContext, window: &mut Window, cx: &mut App) -> AnyElement {
        let Some(trace) = self.trace.as_ref() else {
            return div().into_any_element();
        };
        let row_id = ctx.row_id;

        let dispatch_toggle = ctx.dispatch.clone();
        let on_toggle: ToggleHandler = Rc::new(move |_, window, cx| {
            dispatch_toggle.send(RowAction::ToggleDisclosure { row_id }, window, cx);
        });

        let dispatch_copy = ctx.dispatch.clone();
        let copy_value: CopyValue = Rc::new(move |_, cx| dispatch_copy.clipboard_value(row_id, cx));

        // The painted-frame native anchor is captured at render time, the
        // same way the P1 listener did, so the eased replay never skips.
        let native_scroll_anchor = trace.current_scroll_offset();
        let dispatch_scroll = ctx.dispatch.clone();
        let on_scroll: ScrollHandler = Rc::new(move |event, window, cx| {
            dispatch_scroll.send(
                RowAction::ReplayNestedScroll {
                    row_id,
                    anchor: native_scroll_anchor,
                    dy: event.delta.pixel_delta(window.line_height()).y,
                    precise: event.delta.precise(),
                },
                window,
                cx,
            );
            cx.stop_propagation();
        });
        reasoning_card::render(
            trace,
            &self.display,
            self.finished,
            reasoning_card::ReasoningCardId {
                ui_id: row_id.part.as_u64(),
                content_index: self.content_index,
            },
            reasoning_card::ReasoningCardActions {
                on_toggle,
                copy_value,
                on_scroll,
            },
            window,
            cx,
        )
    }

    fn copy_source(
        &self,
        _transcript: &crate::chat::transcript::Transcript,
    ) -> Option<gpui::SharedString> {
        Some(self.display.clone().into())
    }

    fn disclosure(&self) -> DisclosureState {
        match self.trace.as_ref().map(ReasoningTrace::disclosed) {
            Some(true) => DisclosureState::Expanded,
            _ => DisclosureState::Collapsed,
        }
    }

    fn sync_disclosure(&mut self, disclosure: DisclosureState) {
        if let Some(trace) = self.trace.as_mut() {
            trace.set_disclosed(disclosure == DisclosureState::Expanded);
        }
    }

    fn toggle_disclosure(&mut self, cx: &mut App) -> Option<f32> {
        self.trace.as_mut()?.toggle_with_cx(cx)
    }

    fn nested_scroll_trace(&mut self) -> Option<&mut ReasoningTrace> {
        self.trace.as_mut()
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
