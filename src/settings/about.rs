//! "About" settings page: app identity, version, license.

use gpui::{AnyElement, App, IntoElement as _, ParentElement as _, Styled as _, div};
use gpui_component::{ActiveTheme as _, StyledExt as _, label::Label, v_flex};
use rust_i18n::t;

pub(super) fn render(cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .py_12()
        .child(
            div()
                .text_lg()
                .font_semibold()
                .text_color(cx.theme().foreground)
                .child("Nostra"),
        )
        .child(
            Label::new(format!(
                "{} {}",
                t!("settings.about.version"),
                env!("CARGO_PKG_VERSION")
            ))
            .text_sm()
            .text_color(cx.theme().muted_foreground),
        )
        .child(
            Label::new(t!("settings.about.description").to_string())
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            Label::new(t!("settings.about.license").to_string())
                .text_xs()
                .text_color(cx.theme().muted_foreground),
        )
        .into_any_element()
}
