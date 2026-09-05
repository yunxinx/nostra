//! Wrapper renderer for the turn action bar.
//!
//! One row per turn that ended its stream with copyable prose and no error —
//! any role (the P1 visual gate), with `Role::User` only flipping the
//! alignment. Visibility follows the *turn-level* hover state
//! (`ctx.turn_hovered`) instead of an element hover group, because a turn
//! now spans several list rows that cannot share one group. The trigger
//! stays mounted and `invisible()` at rest — the same contract as the P1
//! hover-reveal copy, so the pointer can travel to it without it unmounting.

use gpui::{
    App, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{clipboard::Clipboard, h_flex};
use rust_i18n::t;

use crate::chat::projection::RowKind;
use crate::chat::transcript::{Role, Transcript};

use super::{MaterializeContext, RowChange, RowRenderContext, RowRenderer};

pub(crate) struct TurnActionsRenderer {
    turn_id: Option<crate::chat::transcript::TurnId>,
    materialized: bool,
}

impl TurnActionsRenderer {
    pub(crate) fn new() -> Self {
        Self {
            turn_id: None,
            materialized: false,
        }
    }
}

impl RowRenderer for TurnActionsRenderer {
    fn kind(&self) -> RowKind {
        RowKind::TurnActions
    }

    fn materialize(&mut self, ctx: &MaterializeContext, _cx: &mut App) {
        self.turn_id = Some(ctx.row_id.turn);
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, _change: &RowChange, ctx: &MaterializeContext, _cx: &mut App) {
        self.turn_id = Some(ctx.row_id.turn);
        self.materialized = true;
    }

    fn render(
        &self,
        ctx: &RowRenderContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> gpui::AnyElement {
        let Some(turn_id) = self.turn_id else {
            return div().into_any_element();
        };
        let justify = match ctx.role {
            Role::User => h_flex().justify_end(),
            _ => h_flex(),
        };
        let dispatch = ctx.dispatch.clone();
        let row_id = ctx.row_id;
        let ui_id = turn_id.as_u64();
        let copy_selector = format!("{}-copy", row_id.debug_name());
        justify
            .w_full()
            .mt_1()
            .child(
                div()
                    .flex_none()
                    .debug_selector(move || copy_selector)
                    .invisible()
                    .when(ctx.turn_hovered, |this| this.visible())
                    .child(
                        Clipboard::new(ElementId::NamedInteger("turn-message-copy".into(), ui_id))
                            .value_fn(move |_, cx| dispatch.clipboard_value(row_id, cx))
                            .tooltip(t!("chat.copy_message").to_string()),
                    ),
            )
            .into_any_element()
    }

    fn copy_source(&self, transcript: &Transcript) -> Option<SharedString> {
        let turn_id = self.turn_id?;
        transcript.copyable_text(turn_id)
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
