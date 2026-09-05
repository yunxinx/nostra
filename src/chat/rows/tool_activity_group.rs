//! Collapsed step-stack header for a run of consecutive tool activities
//! (PRD R3, design contract 2).
//!
//! Three or more consecutive activities collapse into one group row
//! (`RowKind::ToolActivityGroup`). The header is a full-width clickable row —
//! "Ran N steps · Latest: <name>" — and toggling it makes the projection
//! split back into the individual activity rows (`ProjectionDiff::Splice`),
//! each of which keeps its own fold state via the projection's
//! `DisclosureState` / member side map. While the group is expanded the
//! header row is gone, so the first member row carries the affordance that
//! re-folds it (see [`ToolActivityRenderer::render_group_collapse`]).
//!
//! The renderer itself is stateless: everything it shows — member count,
//! latest member name — arrives through the [`RowRenderContext`], and the
//! open/closed decision lives in the projection (`group_expanded`).

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, Window,
    div,
};
use gpui_component::{
    ActiveTheme as _, Icon, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
};
use rust_i18n::t;

use crate::appearance::contrast;
use crate::chat::projection::{DisclosureState, RowKind};
use crate::chat::rows::{DisclosureTarget, RowAction};

use super::{MaterializeContext, RowChange, RowRenderContext, RowRenderer};

pub(crate) struct ToolActivityGroupRenderer {
    materialized: bool,
}

impl ToolActivityGroupRenderer {
    pub(crate) fn new() -> Self {
        Self {
            materialized: false,
        }
    }
}

impl RowRenderer for ToolActivityGroupRenderer {
    fn kind(&self) -> RowKind {
        RowKind::ToolActivityGroup
    }

    fn materialize(&mut self, _ctx: &MaterializeContext, _cx: &mut App) {
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, _change: &RowChange, _ctx: &MaterializeContext, _cx: &mut App) {
        self.materialized = true;
    }

    fn render(&self, ctx: &RowRenderContext, _window: &mut Window, cx: &mut App) -> AnyElement {
        // A waiting turn hides its rows behind the shimmer (P1 parity).
        if ctx.waiting {
            return div().into_any_element();
        }
        let theme = cx.theme();
        let header_bg = contrast::pane_block(theme.muted, cx);
        let header_text = contrast::text_on(theme.foreground, header_bg, cx);
        let muted_text = contrast::text_on(theme.muted_foreground, header_bg, cx);
        let radius = theme.radius;
        let row_id = ctx.row_id;
        let count = ctx.group_count.max(1);
        let latest: &str = ctx.group_latest.as_deref().unwrap_or_default();
        let label = t!("chat.tool.steps", count = count, name = latest).to_string();

        let dispatch = ctx.dispatch.clone();
        let expand_selector = format!("{}-expand", row_id.debug_name());

        // `Button` has no debug selector of its own, so the clickable header
        // is wrapped in the selector div tests click through.
        div()
            .debug_selector(move || expand_selector)
            .child(
                Button::new(("tool-group-toggle", row_id.part.as_u64()))
                    .ghost()
                    .w_full()
                    .rounded(radius)
                    .bg(header_bg)
                    .accessibility_label(label.clone())
                    .on_click(move |_, window, cx| {
                        dispatch.send(
                            RowAction::ToggleDisclosure {
                                row_id,
                                target: DisclosureTarget::Group,
                            },
                            window,
                            cx,
                        );
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::default()
                                    .path("icons/tool.svg")
                                    .text_color(muted_text),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_shrink(1.)
                                    .text_sm()
                                    .font_medium()
                                    .text_color(header_text)
                                    .text_ellipsis()
                                    .child(label),
                            )
                            .child(
                                Icon::default()
                                    .path("icons/chevron-right.svg")
                                    .text_color(muted_text),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn disclosure(&self) -> DisclosureState {
        DisclosureState::default()
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
