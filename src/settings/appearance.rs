//! "Appearance" settings page: theme, font, effects, and settings hints.

use gpui::{AnyElement, App, IntoElement as _, SharedString};
#[cfg(target_os = "macos")]
use gpui::{Entity, ParentElement as _, Styled as _, px};
use gpui_component::{Sizable as _, switch::Switch};
#[cfg(target_os = "macos")]
use gpui_component::{
    h_flex,
    slider::{Slider, SliderState},
};
use rust_i18n::t;

use super::ui;
#[cfg(target_os = "macos")]
use crate::appearance::glass;
use crate::appearance::{fonts, theme};
use crate::preferences::{ComposerFont, PreferenceHandle, Preferences, ThemeMode};
use crate::ui::markdown;

/// Dropdown identifier for "follow system" (the absence of a mode override).
const MODE_SYSTEM: &str = "system";
const MODE_LIGHT: &str = "light";
const MODE_DARK: &str = "dark";

pub(super) fn render(
    cx: &App,
    preference_handle: &PreferenceHandle,
    preferences: &Preferences,
    #[cfg(target_os = "macos")] glass_opacity: &Entity<SliderState>,
) -> AnyElement {
    let mut rows = vec![
        theme_mode_row(
            preferences.theme_mode,
            preferences.hide_settings_info_buttons,
            preference_handle,
            cx,
        ),
        theme_row(
            false,
            preferences.hide_settings_info_buttons,
            preference_handle,
            cx,
        ),
        theme_row(
            true,
            preferences.hide_settings_info_buttons,
            preference_handle,
            cx,
        ),
        composer_font_row(
            preferences.hide_settings_info_buttons,
            preference_handle,
            cx,
        ),
        user_message_markdown_row(preferences, preference_handle, cx),
        smooth_chat_scrolling_row(preferences, preference_handle, cx),
        code_wrap_row(preferences, preference_handle, cx),
        code_line_numbers_row(preferences, preference_handle, cx),
    ];
    #[cfg(target_os = "macos")]
    {
        rows.push(glass_effect_row(preferences, preference_handle, cx));
        rows.push(glass_opacity_row(glass_opacity, preferences, cx));
    }
    rows.push(info_buttons_row(preferences, preference_handle, cx));

    ui::section(rows, cx)
}

fn user_message_markdown_row(
    preferences: &Preferences,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    ui::row(
        "user-message-markdown",
        t!("settings.user_message_markdown").to_string(),
        Some(t!("settings.user_message_markdown_desc").to_string()),
        Switch::new("user-message-markdown-switch")
            .small()
            .checked(preferences.user_message_markdown)
            .on_click(move |checked, _, cx| {
                markdown::set_user_message_markdown(*checked, &preference_handle, cx);
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn smooth_chat_scrolling_row(
    preferences: &Preferences,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    ui::row(
        "smooth-chat-scrolling",
        t!("settings.smooth_chat_scrolling").to_string(),
        Some(t!("settings.smooth_chat_scrolling_desc").to_string()),
        Switch::new("smooth-chat-scrolling-switch")
            .small()
            .checked(preferences.smooth_chat_scrolling)
            .on_click(move |checked, _, cx| {
                crate::chat::set_smooth_scrolling(*checked, &preference_handle, cx)
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn code_wrap_row(
    preferences: &Preferences,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    ui::row(
        "code-wrap",
        t!("settings.code_wrap").to_string(),
        Some(t!("settings.code_wrap_desc").to_string()),
        Switch::new("code-wrap-switch")
            .small()
            .checked(preferences.code_block_wrap)
            .on_click(move |checked, _, cx| {
                markdown::set_global_wrap(*checked, &preference_handle, cx)
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn code_line_numbers_row(
    preferences: &Preferences,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    ui::row(
        "code-line-numbers",
        t!("settings.code_line_numbers").to_string(),
        Some(t!("settings.code_line_numbers_desc").to_string()),
        Switch::new("code-line-numbers-switch")
            .small()
            .checked(preferences.code_block_line_numbers)
            .on_click(move |checked, _, cx| {
                markdown::set_line_numbers(*checked, &preference_handle, cx)
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn info_buttons_row(
    preferences: &Preferences,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    ui::row(
        "hide-info-buttons",
        t!("settings.hide_info_buttons").to_string(),
        Some(t!("settings.hide_info_buttons_desc").to_string()),
        Switch::new("hide-info-buttons-switch")
            .small()
            .checked(preferences.hide_settings_info_buttons)
            .on_click(move |checked, _, cx| {
                ui::set_info_buttons_hidden(*checked, &preference_handle, cx)
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

#[cfg(target_os = "macos")]
fn glass_opacity_row(
    slider: &Entity<SliderState>,
    preferences: &Preferences,
    cx: &App,
) -> AnyElement {
    let percent = slider.read(cx).value().start().round() as i32;
    ui::row(
        "glass-opacity",
        t!("settings.glass_opacity").to_string(),
        Some(t!("settings.glass_opacity_desc").to_string()),
        h_flex()
            .w(px(220.))
            .gap_3()
            .items_center()
            .child(
                Slider::new(slider)
                    .flex_1()
                    .disabled(!preferences.glass_effect),
            )
            .child(format!("{percent}%"))
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

#[cfg(target_os = "macos")]
fn glass_effect_row(
    preferences: &Preferences,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    ui::row(
        "glass-effect",
        t!("settings.glass_effect").to_string(),
        Some(t!("settings.glass_effect_desc").to_string()),
        Switch::new("glass-effect-switch")
            .small()
            .checked(preferences.glass_effect)
            .on_click(move |checked, window, cx| {
                glass::set_enabled(*checked, &preference_handle, window, cx)
            })
            .into_any_element(),
        preferences.hide_settings_info_buttons,
        cx,
    )
}

fn theme_mode_row(
    mode: Option<ThemeMode>,
    hide_info: bool,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    let options: Vec<(SharedString, SharedString)> = vec![
        (
            MODE_SYSTEM.into(),
            t!("settings.mode.system").to_string().into(),
        ),
        (
            MODE_LIGHT.into(),
            t!("settings.mode.light").to_string().into(),
        ),
        (
            MODE_DARK.into(),
            t!("settings.mode.dark").to_string().into(),
        ),
    ];

    ui::row(
        "theme-mode",
        t!("settings.theme_mode").to_string(),
        Some(t!("settings.theme_mode_desc").to_string()),
        ui::dropdown(
            "theme-mode-dd",
            options,
            mode_key(mode).into(),
            move |value, cx| theme::set_mode(mode_from_key(&value), &preference_handle, cx),
        ),
        hide_info,
        cx,
    )
}

/// Theme picker for one slot (light or dark).  Options are the registered
/// themes matching that appearance; picking one only becomes visible when
/// that slot is the active one.
fn theme_row(
    dark: bool,
    hide_info: bool,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    let options: Vec<(SharedString, SharedString)> = theme::theme_names(dark, cx)
        .into_iter()
        .map(|name| (name.clone(), name))
        .collect();

    let (id, dd_id, title, description) = if dark {
        (
            "dark-theme",
            "dark-theme-dd",
            t!("settings.dark_theme").to_string(),
            t!("settings.dark_theme_desc").to_string(),
        )
    } else {
        (
            "light-theme",
            "light-theme-dd",
            t!("settings.light_theme").to_string(),
            t!("settings.light_theme_desc").to_string(),
        )
    };

    ui::row(
        id,
        title,
        Some(description),
        ui::dropdown(
            dd_id,
            options,
            theme::slot_theme_name(dark, cx),
            move |value, cx| theme::select_theme(&value, &preference_handle, cx),
        ),
        hide_info,
        cx,
    )
}

fn composer_font_row(
    hide_info: bool,
    preference_handle: &PreferenceHandle,
    cx: &App,
) -> AnyElement {
    let preference_handle = preference_handle.clone();
    let options: Vec<(SharedString, SharedString)> = ComposerFont::all()
        .iter()
        .map(|font| (font.key().into(), font.label().into()))
        .collect();

    ui::row(
        "composer-font",
        t!("settings.composer_font").to_string(),
        Some(t!("settings.composer_font_desc").to_string()),
        ui::dropdown(
            "composer-font-dd",
            options,
            fonts::active(cx).key().into(),
            move |value, cx| fonts::set(ComposerFont::from_key(&value), &preference_handle, cx),
        ),
        hide_info,
        cx,
    )
}

fn mode_key(mode: Option<ThemeMode>) -> &'static str {
    match mode {
        None => MODE_SYSTEM,
        Some(m) if m.is_dark() => MODE_DARK,
        Some(_) => MODE_LIGHT,
    }
}

fn mode_from_key(key: &str) -> Option<ThemeMode> {
    match key {
        MODE_LIGHT => Some(ThemeMode::Light),
        MODE_DARK => Some(ThemeMode::Dark),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rendering_labels_resolve_in_every_locale() {
        for locale in ["en", "zh-CN"] {
            for key in [
                "settings.user_message_markdown",
                "settings.user_message_markdown_desc",
                "settings.smooth_chat_scrolling",
                "settings.smooth_chat_scrolling_desc",
                "settings.code_wrap",
                "settings.code_wrap_desc",
                "settings.code_line_numbers",
                "settings.code_line_numbers_desc",
            ] {
                let resolved = t!(key, locale = locale).to_string();
                assert!(!resolved.contains(key), "{key} unresolved for {locale}");
            }
        }
    }
}
