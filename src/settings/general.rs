//! "General" settings page: interface language.

use gpui::{AnyElement, App, SharedString};
use rust_i18n::t;

use super::ui;
use crate::i18n;
use crate::preferences::Language;

pub(super) fn render(cx: &App) -> AnyElement {
    let language_options: Vec<(SharedString, SharedString)> = Language::all()
        .iter()
        .map(|lang| (lang.key().into(), lang.label().into()))
        .collect();

    ui::section(
        vec![ui::row(
            "language",
            t!("settings.language").to_string(),
            Some(t!("settings.language_desc").to_string()),
            ui::dropdown(
                "language-dd",
                language_options,
                i18n::current(cx).key().into(),
                |value, cx| i18n::change(Language::from_key(&value), cx),
            ),
            cx,
        )],
        cx,
    )
}
