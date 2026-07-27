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
use crate::glass;
use crate::preferences::{ComposerFont, ThemeMode};
use crate::{fonts, theme};

/// Dropdown identifier for "follow system" (the absence of a mode override).
const MODE_SYSTEM: &str = "system";
const MODE_LIGHT: &str = "light";
const MODE_DARK: &str = "dark";

pub(super) fn render(
    cx: &App,
    #[cfg(target_os = "macos")] glass_opacity: &Entity<SliderState>,
) -> AnyElement {
    let mut rows = vec![
        theme_mode_row(cx),
        theme_row(false, cx),
        theme_row(true, cx),
        composer_font_row(cx),
    ];
    #[cfg(target_os = "macos")]
    {
        rows.push(glass_effect_row(cx));
        rows.push(glass_opacity_row(glass_opacity, cx));
    }
    rows.push(info_buttons_row(cx));

    ui::section(rows, cx)
}

fn info_buttons_row(cx: &App) -> AnyElement {
    ui::row(
        "hide-info-buttons",
        t!("settings.hide_info_buttons").to_string(),
        Some(t!("settings.hide_info_buttons_desc").to_string()),
        Switch::new("hide-info-buttons-switch")
            .small()
            .checked(ui::info_buttons_hidden(cx))
            .on_click(|checked, _, cx| ui::set_info_buttons_hidden(*checked, cx))
            .into_any_element(),
        cx,
    )
}

#[cfg(target_os = "macos")]
fn glass_opacity_row(slider: &Entity<SliderState>, cx: &App) -> AnyElement {
    let percent = slider.read(cx).value().start().round() as i32;
    ui::row(
        "glass-opacity",
        t!("settings.glass_opacity").to_string(),
        Some(t!("settings.glass_opacity_desc").to_string()),
        h_flex()
            .w(px(220.))
            .gap_3()
            .items_center()
            .child(Slider::new(slider).flex_1().disabled(!glass::enabled(cx)))
            .child(format!("{percent}%"))
            .into_any_element(),
        cx,
    )
}

#[cfg(target_os = "macos")]
fn glass_effect_row(cx: &App) -> AnyElement {
    ui::row(
        "glass-effect",
        t!("settings.glass_effect").to_string(),
        Some(t!("settings.glass_effect_desc").to_string()),
        Switch::new("glass-effect-switch")
            .small()
            .checked(glass::enabled(cx))
            .on_click(|checked, window, cx| glass::set_enabled(*checked, window, cx))
            .into_any_element(),
        cx,
    )
}

fn theme_mode_row(cx: &App) -> AnyElement {
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
            mode_key(theme::mode_preference(cx)).into(),
            false,
            |value, cx| theme::set_mode(mode_from_key(&value), cx),
        ),
        cx,
    )
}

/// Theme picker for one slot (light or dark).  Options are the registered
/// themes matching that appearance; picking one only becomes visible when
/// that slot is the active one.
fn theme_row(dark: bool, cx: &App) -> AnyElement {
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
            true,
            |value, cx| theme::select_theme(&value, cx),
        ),
        cx,
    )
}

fn composer_font_row(cx: &App) -> AnyElement {
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
            false,
            |value, cx| fonts::set(ComposerFont::from_key(&value), cx),
        ),
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
