//! Theme management on top of gpui-component's `ThemeRegistry`.
//!
//! Bundled theme JSONs (including the app's own "Nostra Dark", the exact
//! GitHub-soft-dark palette this app shipped with originally) are registered
//! at startup, then the saved light/dark theme slots and the saved mode are
//! restored.  All theme mutations in the app go through this module — views
//! never touch `Theme::global_mut` directly.

use std::rc::Rc;

use gpui::{App, Window};
use gpui_component::{ActiveTheme as _, Theme, ThemeConfig, ThemeMode, ThemeRegistry};

use crate::assets;
use crate::preferences::{self, Preferences};

/// Embedded theme files to register.  Paths are relative to the `assets/`
/// embed root.  `nostra.json` must stay in this list — it provides the
/// default dark theme.
const THEME_FILES: &[&str] = &[
    "themes/nostra.json",
    "themes/ayu.json",
    "themes/catppuccin.json",
    "themes/everforest.json",
    "themes/gruvbox.json",
    "themes/solarized.json",
    "themes/tokyonight.json",
];

/// Theme applied to the light slot when nothing valid is saved.
pub const DEFAULT_LIGHT: &str = "Default Light";
/// Theme applied to the dark slot when nothing valid is saved.
pub const DEFAULT_DARK: &str = "Nostra Dark";

/// Register bundled themes and restore the saved theme selection + mode.
/// Must run after `gpui_component::init` (which creates the registry) and
/// before the first window renders.
pub fn init(prefs: &Preferences, cx: &mut App) {
    register_embedded_themes(cx);

    let light = resolve(prefs.light_theme.as_deref(), DEFAULT_LIGHT, false, cx);
    let dark = resolve(prefs.dark_theme.as_deref(), DEFAULT_DARK, true, cx);
    {
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
    }

    apply_mode(prefs.theme_mode, cx);
}

/// Change the theme mode preference (`None` = follow system), persist it,
/// and apply it to all windows.
pub fn set_mode(mode: Option<preferences::ThemeMode>, cx: &mut App) {
    preferences::update(cx, |p| p.theme_mode = mode);
    apply_mode(mode, cx);
}

/// The persisted mode preference (`None` = follow system).
pub fn mode_preference(cx: &App) -> Option<preferences::ThemeMode> {
    preferences::get(cx).theme_mode
}

/// Apply a system appearance change when the user has chosen to follow the
/// system. Explicit light/dark preferences deliberately ignore the event.
pub fn sync_system_appearance(window: &mut Window, cx: &mut App) {
    if mode_preference(cx).is_some() {
        return;
    }

    Theme::sync_system_appearance(Some(window), cx);
    cx.refresh_windows();
}

/// Select a registered theme by name.  The theme lands in the slot matching
/// its own light/dark mode; the visible palette only changes when that slot
/// is the active one.  The choice is persisted.
pub fn select_theme(name: &str, cx: &mut App) {
    let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() else {
        eprintln!("ignoring unknown theme: {name}");
        return;
    };

    let dark_slot = config.mode.is_dark();
    let saved: String = config.name.to_string();
    {
        let theme = Theme::global_mut(cx);
        if dark_slot {
            theme.dark_theme = config;
        } else {
            theme.light_theme = config;
        }
    }
    preferences::update(cx, |p| {
        let slot = if dark_slot {
            &mut p.dark_theme
        } else {
            &mut p.light_theme
        };
        *slot = Some(saved);
    });

    // Re-apply the effective mode so an active-slot change becomes visible.
    let current = cx.theme().mode;
    Theme::change(current, None, cx);
    cx.refresh_windows();
}

/// Names of all registered themes matching the given appearance, sorted —
/// data for the settings dropdowns.
pub fn theme_names(dark: bool, cx: &App) -> Vec<gpui::SharedString> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .iter()
        .filter(|t| t.mode.is_dark() == dark)
        .map(|t| t.name.clone())
        .collect()
}

/// The theme name currently occupying the light or dark slot.
pub fn slot_theme_name(dark: bool, cx: &App) -> gpui::SharedString {
    if dark {
        cx.theme().dark_theme.name.clone()
    } else {
        cx.theme().light_theme.name.clone()
    }
}

/// Apply a mode preference without persisting it (used at startup and by
/// [`set_mode`]).
fn apply_mode(mode: Option<preferences::ThemeMode>, cx: &mut App) {
    match mode {
        None => Theme::sync_system_appearance(None, cx),
        Some(m) => Theme::change(to_ui_mode(m), None, cx),
    }
    cx.refresh_windows();
}

fn to_ui_mode(mode: preferences::ThemeMode) -> ThemeMode {
    if mode.is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

fn register_embedded_themes(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for path in THEME_FILES {
        let Some(bytes) = assets::embedded(path) else {
            eprintln!("missing embedded theme file: {path}");
            continue;
        };
        match std::str::from_utf8(&bytes) {
            Ok(content) => {
                if let Err(e) = registry.load_themes_from_str(content) {
                    eprintln!("failed to load themes from {path}: {e:?}");
                }
            }
            Err(e) => eprintln!("embedded theme {path} is not valid UTF-8: {e:?}"),
        }
    }
}

/// Look up a saved theme name for one slot, falling back to the app default
/// and finally to gpui-component's built-in default.  A theme only qualifies
/// for a slot when its own mode matches the slot's appearance.
fn resolve(saved: Option<&str>, app_default: &str, dark: bool, cx: &App) -> Rc<ThemeConfig> {
    let registry = ThemeRegistry::global(cx);
    let lookup = |name: &str| {
        registry
            .themes()
            .get(name)
            .filter(|c| c.mode.is_dark() == dark)
            .cloned()
    };

    saved
        .and_then(lookup)
        .or_else(|| lookup(app_default))
        .unwrap_or_else(|| {
            if dark {
                registry.default_dark_theme().clone()
            } else {
                registry.default_light_theme().clone()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::ThemeSet;

    /// Every bundled theme file must exist in the embed and parse as a
    /// `ThemeSet` — a broken file would otherwise be skipped silently at
    /// runtime and the theme list would quietly shrink.
    #[test]
    fn bundled_theme_files_parse() {
        for path in THEME_FILES {
            let bytes = assets::embedded(path).unwrap_or_else(|| panic!("missing embed: {path}"));
            let content = std::str::from_utf8(&bytes).expect("theme file must be UTF-8");
            let set: ThemeSet = serde_json::from_str(content)
                .unwrap_or_else(|e| panic!("theme file {path} failed to parse: {e:?}"));
            assert!(!set.themes.is_empty(), "{path} contains no themes");
        }
    }

    /// The app's default dark theme must exist with the exact name the
    /// resolver falls back to, and actually be a dark theme.
    #[test]
    fn nostra_dark_present_and_dark() {
        let bytes = assets::embedded("themes/nostra.json").expect("nostra.json embedded");
        let set: ThemeSet =
            serde_json::from_str(std::str::from_utf8(&bytes).unwrap()).expect("parses");
        let dark = set
            .themes
            .iter()
            .find(|t| t.name.as_ref() == DEFAULT_DARK)
            .expect("Nostra Dark theme present");
        assert!(dark.mode.is_dark());
    }
}
