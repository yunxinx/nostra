//! "General" settings page: interface language and diagnostics.

use gpui::{AnyElement, App, IntoElement as _, SharedString};
use gpui_component::{Sizable as _, switch::Switch};
use rust_i18n::t;

use super::ui;
use crate::preferences::{Language, PreferenceHandle, Preferences};
use crate::{i18n, logging, preferences};

pub(super) fn render(
    cx: &App,
    preference_handle: &PreferenceHandle,
    preferences: &Preferences,
) -> AnyElement {
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
                    preferences.language.key().into(),
                    |value, cx| i18n::change(Language::from_key(&value), cx),
                ),
                preferences.hide_settings_info_buttons,
                cx,
            ),
            detailed_logging_row(cx, preference_handle, preferences),
            restore_last_chat_row(cx, preference_handle, preferences),
            restore_last_workspace_row(cx, preference_handle, preferences),
        ],
        cx,
    )
}

fn detailed_logging_row(
    cx: &App,
    preference_handle: &PreferenceHandle,
    preferences: &Preferences,
) -> AnyElement {
    let preference_handle_for_click = preference_handle.clone();
    ui::row(
        "detailed-logging",
        t!("settings.detailed_logging").to_string(),
        Some(t!("settings.detailed_logging_desc").to_string()),
        Switch::new("detailed-logging-switch")
            .small()
            .checked(preferences.detailed_logging)
            .on_click(move |enabled, _, cx| {
                logging::set_detailed(*enabled);
                preferences::update_with(cx, &preference_handle_for_click, |prefs| {
                    prefs.detailed_logging = *enabled
                });
                cx.refresh_windows();
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn restore_last_chat_row(
    cx: &App,
    preference_handle: &PreferenceHandle,
    preferences: &Preferences,
) -> AnyElement {
    let preference_handle_for_click = preference_handle.clone();
    ui::row(
        "restore-last-chat",
        t!("settings.restore_last_chat").to_string(),
        Some(t!("settings.restore_last_chat_desc").to_string()),
        Switch::new("restore-last-chat-switch")
            .small()
            .checked(preferences.restore_last_chat_on_start)
            .on_click(move |enabled, _, cx| {
                preferences::update_with(cx, &preference_handle_for_click, |prefs| {
                    prefs.restore_last_chat_on_start = *enabled
                });
                cx.refresh_windows();
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn restore_last_workspace_row(
    cx: &App,
    preference_handle: &PreferenceHandle,
    preferences: &Preferences,
) -> AnyElement {
    let preference_handle_for_click = preference_handle.clone();
    ui::row(
        "restore-last-workspace",
        t!("settings.restore_last_workspace").to_string(),
        Some(t!("settings.restore_last_workspace_desc").to_string()),
        Switch::new("restore-last-workspace-switch")
            .small()
            .checked(preferences.restore_last_workspace_on_start)
            .on_click(move |enabled, _, cx| {
                preferences::update_with(cx, &preference_handle_for_click, |prefs| {
                    prefs.restore_last_workspace_on_start = *enabled
                });
                cx.refresh_windows();
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
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
