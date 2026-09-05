//! Wrapper renderer for tool activity rows.
//!
//! One row covers the P1 "tool requested" line plus the paired result body.
//! A row belonging to a `Role::Tool` turn keeps the P1 muted result-card
//! styling. A collapsed group header (three or more consecutive activities)
//! renders in place of the run until the user expands it; while expanded,
//! the group's first member row carries the affordance that re-folds it.

use gpui::{
    App, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, Window, div,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    text::TextViewStyle,
    v_flex,
};
use rust_i18n::t;

use crate::chat::projection::{RowId, RowKind};
use crate::chat::transcript::{Part, PartSource, Role};
use crate::ui::markdown::MarkdownBody;

use super::{MaterializeContext, RowAction, RowChange, RowRenderContext, RowRenderer};

pub(crate) struct ToolActivityRenderer {
    name: String,
    result: Option<String>,
    body: Option<MarkdownBody>,
    is_group_header: bool,
    materialized: bool,
}

impl ToolActivityRenderer {
    pub(crate) fn new() -> Self {
        Self {
            name: String::new(),
            result: None,
            body: None,
            is_group_header: false,
            materialized: false,
        }
    }

    pub(crate) fn new_group() -> Self {
        let mut renderer = Self::new();
        renderer.is_group_header = true;
        renderer
    }

    fn seed_from(&mut self, ctx: &MaterializeContext) {
        match ctx.part {
            Some(part) => {
                self.name = tool_call_name(part).unwrap_or_default();
                self.result = ctx.paired_result.map(|result| result.content.clone());
            }
            None => {
                self.name.clear();
                self.result = None;
            }
        }
    }
}

fn tool_call_name(part: &Part) -> Option<String> {
    match &part.source {
        PartSource::ToolCall { name, .. } => Some(name.clone()),
        _ => None,
    }
}

impl RowRenderer for ToolActivityRenderer {
    fn kind(&self) -> RowKind {
        if self.is_group_header {
            RowKind::ToolActivityGroup
        } else {
            RowKind::ToolActivity
        }
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.seed_from(ctx);
        if let Some(result) = ctx.paired_result
            && !result.content.is_empty()
        {
            self.body = Some(MarkdownBody::new_with_presentation(
                &result.content,
                ctx.owner_id,
                ctx.presentation,
                cx,
            ));
        } else {
            self.body = None;
        }
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
            RowChange::Replace => {
                let previous_result = self.result.clone();
                self.seed_from(ctx);
                let next = self.result.clone().unwrap_or_default();
                if previous_result.as_deref() != Some(next.as_str()) {
                    if next.is_empty() {
                        self.body = None;
                    } else {
                        // Rebuild: paired results are terminal content, and
                        // the row may also have changed which result it shows.
                        self.body = Some(MarkdownBody::new_with_presentation(
                            &next,
                            ctx.owner_id,
                            ctx.presentation,
                            cx,
                        ));
                    }
                }
                self.materialized = true;
            }
            RowChange::Append { .. } | RowChange::Finished => {}
        }
    }

    fn render(
        &self,
        ctx: &RowRenderContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        // A waiting turn hides its rows behind the shimmer (P1 parity).
        if ctx.waiting {
            return div().into_any_element();
        }
        if self.is_group_header {
            return self.render_group_header(ctx, cx);
        }
        let muted_foreground = cx.theme().muted_foreground;
        let name = self.name.clone();
        let requested = (!name.is_empty()).then(|| {
            div()
                .text_color(muted_foreground)
                .child(t!("chat.tool_requested", name = name).to_string())
        });
        let content = self
            .body
            .as_ref()
            .map(|body| body.text_view(TextViewStyle::default()));

        if ctx.role == Role::Tool {
            // P1 muted result card for standalone tool turns.
            let (radius_lg, muted) = {
                let theme = cx.theme();
                (theme.radius_lg, theme.muted)
            };
            let card = crate::appearance::contrast::pane_block(muted, cx);
            let card_text = crate::appearance::contrast::text_on(muted_foreground, card, cx);
            return h_flex()
                .w_full()
                .justify_start()
                .child(
                    div()
                        .min_w_0()
                        .max_w(gpui::px(560.))
                        .rounded(radius_lg)
                        .bg(card)
                        .text_color(card_text)
                        .px_3()
                        .py_1p5()
                        .children(requested)
                        .children(content),
                )
                .into_any_element();
        }

        v_flex()
            .w_full()
            // The first member of an expanded group carries the affordance
            // that re-folds it: the collapsed header's toggle is gone while
            // the group is expanded, so this is the only way back.
            .children(
                ctx.group
                    .filter(|_| ctx.group_leader)
                    .map(|group| self.render_group_collapse(group, ctx, cx)),
            )
            .children(requested)
            .children(content)
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

impl ToolActivityRenderer {
    fn render_group_collapse(
        &self,
        group: RowId,
        ctx: &RowRenderContext,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let dispatch = ctx.dispatch.clone();
        let row_id = ctx.row_id;
        let collapse_selector = format!("{}-collapse", row_id.debug_name());
        div()
            .text_color(cx.theme().muted_foreground)
            .debug_selector(move || collapse_selector)
            .child(
                Button::new(("tool-group-collapse", row_id.part.as_u64()))
                    .ghost()
                    .xsmall()
                    .compact()
                    .label(t!("chat.tool_group_collapse").to_string())
                    .tooltip(t!("chat.tool_group_collapse").to_string())
                    .on_click(move |_, window, cx| {
                        dispatch.send(RowAction::ToggleDisclosure { row_id: group }, window, cx);
                    }),
            )
            .into_any_element()
    }

    fn render_group_header(&self, ctx: &RowRenderContext, cx: &mut App) -> gpui::AnyElement {
        let muted_foreground = cx.theme().muted_foreground;
        let count = ctx.group_count.max(1);
        let label = t!("chat.tool_group", count = count).to_string();
        let dispatch = ctx.dispatch.clone();
        let row_id = ctx.row_id;
        let expand_selector = format!("{}-expand", row_id.debug_name());
        v_flex()
            .w_full()
            .child(
                div()
                    .text_color(muted_foreground)
                    .debug_selector(move || expand_selector)
                    .child(
                        Button::new(("tool-group-toggle", row_id.part.as_u64()))
                            .ghost()
                            .xsmall()
                            .compact()
                            .label(label)
                            .tooltip(t!("chat.tool_group_expand").to_string())
                            .on_click(move |_, window, cx| {
                                dispatch.send(RowAction::ToggleDisclosure { row_id }, window, cx);
                            }),
                    ),
            )
            .into_any_element()
    }
}
