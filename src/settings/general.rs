//! "General" settings page: interface language and diagnostics.

use gpui::{AnyElement, App, IntoElement as _, SharedString};
use gpui_component::{Sizable as _, switch::Switch};
use rust_i18n::t;

use super::ui;
use crate::preferences::Language;
use crate::{i18n, logging, preferences};

pub(super) fn render(cx: &App) -> AnyElement {
    let language_options: Vec<(SharedString, SharedString)> = Language::all()
        .iter()
        .map(|lang| (lang.key().into(), lang.label().into()))
        .collect();

    ui::section(
        vec![
            ui::row(
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
            ),
            detailed_logging_row(cx),
            restore_last_chat_row(cx),
            restore_last_workspace_row(cx),
        ],
        cx,
    )
}

fn detailed_logging_row(cx: &App) -> AnyElement {
    ui::row(
        "detailed-logging",
        t!("settings.detailed_logging").to_string(),
        Some(t!("settings.detailed_logging_desc").to_string()),
        Switch::new("detailed-logging-switch")
            .small()
            .checked(preferences::get(cx).detailed_logging)
            .on_click(|enabled, _, cx| {
                logging::set_detailed(*enabled);
                preferences::update(cx, |prefs| prefs.detailed_logging = *enabled);
                cx.refresh_windows();
            })
            .into_any_element(),
        cx,
    )
}

fn restore_last_chat_row(cx: &App) -> AnyElement {
    ui::row(
        "restore-last-chat",
        t!("settings.restore_last_chat").to_string(),
        Some(t!("settings.restore_last_chat_desc").to_string()),
        Switch::new("restore-last-chat-switch")
            .small()
            .checked(preferences::get(cx).restore_last_chat_on_start)
            .on_click(|enabled, _, cx| {
                preferences::update(cx, |prefs| prefs.restore_last_chat_on_start = *enabled);
                cx.refresh_windows();
            })
            .into_any_element(),
        cx,
    )
}

fn restore_last_workspace_row(cx: &App) -> AnyElement {
    ui::row(
        "restore-last-workspace",
        t!("settings.restore_last_workspace").to_string(),
        Some(t!("settings.restore_last_workspace_desc").to_string()),
        Switch::new("restore-last-workspace-switch")
            .small()
            .checked(preferences::get(cx).restore_last_workspace_on_start)
            .on_click(|enabled, _, cx| {
                preferences::update(cx, |prefs| {
                    prefs.restore_last_workspace_on_start = *enabled;
                });
                cx.refresh_windows();
            })
            .into_any_element(),
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_labels_resolve_in_every_locale() {
        for locale in ["en", "zh-CN"] {
            for key in [
                "settings.detailed_logging",
                "settings.detailed_logging_desc",
                "settings.restore_last_chat",
                "settings.restore_last_chat_desc",
                "settings.restore_last_workspace",
                "settings.restore_last_workspace_desc",
            ] {
                let resolved = t!(key, locale = locale).to_string();
                assert!(!resolved.contains(key), "{key} unresolved for {locale}");
            }
        }
    }
}
