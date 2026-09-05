//! Wrapper renderer for a user message bubble.
//!
//! The visual composition matches the P1 turn renderer: a right-aligned
//! rounded bubble with the secondary surface and its contrast-derived text.
//! Markdown vs plain presentation follows the same
//! `user_message_markdown` preference; the choice happens at materialize
//! time, and the view re-materializes user rows when the preference flips.

use gpui::{
    App, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{ActiveTheme as _, h_flex, text::TextView, text::TextViewStyle};

use crate::chat::projection::RowKind;
use crate::ui::markdown::MarkdownBody;

use super::{MaterializeContext, RowChange, RowRenderContext, RowRenderer};

pub(crate) struct UserBubbleRenderer {
    text: String,
    part_id: u64,
    body: Option<MarkdownBody>,
    materialized: bool,
}

impl UserBubbleRenderer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            part_id: 0,
            body: None,
            materialized: false,
        }
    }
}

impl RowRenderer for UserBubbleRenderer {
    fn kind(&self) -> RowKind {
        RowKind::UserBubble
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.text = ctx.prose_text().unwrap_or_default().to_string();
        self.part_id = ctx.row_id.part.as_u64();
        self.body = if ctx.user_message_markdown && !self.text.is_empty() {
            Some(MarkdownBody::new_with_presentation(
                &self.text,
                ctx.owner_id,
                ctx.presentation,
                cx,
            ))
        } else {
            None
        };
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
                let text = ctx.prose_text().unwrap_or_default().to_string();
                if let Some(body) = self.body.as_mut() {
                    body.set_text(&text, cx);
                }
                self.text = text;
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
        let (radius_lg, secondary, secondary_foreground) = {
            let theme = cx.theme();
            (theme.radius_lg, theme.secondary, theme.secondary_foreground)
        };
        let bubble = crate::appearance::contrast::pane_block(secondary, cx);
        let bubble_text = crate::appearance::contrast::text_on(secondary_foreground, bubble, cx);
        let part_id = self.part_id;
        let text: SharedString = self.text.clone().into();
        let bubble_selector = format!("{}-bubble", ctx.row_id.debug_name());
        h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .debug_selector(move || bubble_selector)
                    .min_w_0()
                    .max_w(px(560.))
                    .rounded(radius_lg)
                    .bg(bubble)
                    .text_color(bubble_text)
                    .px_3()
                    .py_1p5()
                    .when_some(self.body.as_ref(), |this, body| {
                        this.child(body.text_view(TextViewStyle::default()))
                    })
                    .when(self.body.is_none(), |this| {
                        this.child(
                            TextView::plain(("user-message-plain", part_id), text)
                                .selectable(true)
                                .style(TextViewStyle::default()),
                        )
                    }),
            )
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
