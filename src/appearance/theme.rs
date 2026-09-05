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

    crate::chat::projection::note_theme_changed();
    apply_mode(prefs.theme_mode, cx);
}

/// Change the theme mode preference (`None` = follow system), persist it,
/// and apply it to all windows.
pub fn set_mode(
    mode: Option<preferences::ThemeMode>,
    preference_handle: &preferences::PreferenceHandle,
    cx: &mut App,
) {
    preferences::update_with(cx, preference_handle, |p| p.theme_mode = mode);
    apply_mode(mode, cx);
}

/// Apply a system appearance change when the user has chosen to follow the
/// system. Explicit light/dark preferences deliberately ignore the event.
pub fn sync_system_appearance(
    mode: Option<preferences::ThemeMode>,
    window: &mut Window,
    cx: &mut App,
) {
    if mode.is_some() {
        return;
    }

    Theme::sync_system_appearance(Some(window), cx);
    cx.refresh_windows();
}

/// Select a registered theme by name.  The theme lands in the slot matching
/// its own light/dark mode; the visible palette only changes when that slot
/// is the active one.  The choice is persisted.
pub fn select_theme(name: &str, preference_handle: &preferences::PreferenceHandle, cx: &mut App) {
    let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() else {
        crate::logging::warn(
            "appearance.theme",
            format_args!("ignoring unknown registered-theme name: {name}"),
        );
        return;
    };

    let dark_slot = config.mode.is_dark();
    let saved: String = config.name.to_string();
    apply_theme_config(config, cx);
    preferences::update_with(cx, preference_handle, |p| {
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

/// Install a theme into its matching slot without changing user preferences.
///
/// Keeping this separate from [`select_theme`] lets tests exercise every
/// bundled palette without touching the process-wide preferences file.
fn apply_theme_config(config: Rc<ThemeConfig>, cx: &mut App) {
    crate::chat::projection::note_theme_changed();
    let theme = Theme::global_mut(cx);
    if config.mode.is_dark() {
        theme.dark_theme = config;
    } else {
        theme.light_theme = config;
    }
}

#[cfg(test)]
pub(crate) fn select_theme_for_test(name: &str, cx: &mut App) {
    let config = ThemeRegistry::global(cx)
        .themes()
        .get(name)
        .unwrap_or_else(|| panic!("registered theme {name:?}"))
        .clone();
    apply_theme_config(config, cx);

    let current = cx.theme().mode;
    Theme::change(current, None, cx);
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
    crate::chat::projection::note_theme_changed();
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
            crate::logging::error(
                "appearance.theme",
                format_args!("missing embedded theme file: {path}"),
            );
            continue;
        };
        match std::str::from_utf8(&bytes) {
            Ok(content) => {
                if let Err(e) = registry.load_themes_from_str(content) {
                    crate::logging::error(
                        "appearance.theme",
                        format_args!("failed to load themes from {path}: {e:?}"),
                    );
                }
            }
            Err(e) => crate::logging::error(
                "appearance.theme",
                format_args!("embedded theme {path} is not valid UTF-8: {e:?}"),
            ),
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

    /// Every bundled theme must define a `highlight` block.
    ///
    /// `Theme::apply_config` only assigns `highlight_theme` when the config
    /// carries one — a theme without it silently keeps whatever the *previous*
    /// theme installed, defaulting to `HighlightTheme::default_light()`. The
    /// symptom is quiet and confusing: dark UI with light syntax colors, and
    /// switching themes appears to do nothing to code blocks.
    #[test]
    fn every_bundled_theme_defines_highlight_styles() {
        for path in THEME_FILES {
            let bytes = assets::embedded(path).unwrap_or_else(|| panic!("missing embed: {path}"));
            let set: ThemeSet =
                serde_json::from_str(std::str::from_utf8(&bytes).expect("UTF-8")).expect("parses");
            for theme in &set.themes {
                let highlight = theme.highlight.as_ref().unwrap_or_else(|| {
                    panic!(
                        "{path}: theme {:?} has no `highlight` block, so selecting it would \
                         leave code blocks in the previously active theme's palette",
                        theme.name
                    )
                });
                // JSON error bodies lean on these three: keys, values, and the
                // punctuation between them.
                assert!(
                    highlight.syntax.property.is_some(),
                    "{path}: theme {:?} defines no syntax.property (JSON keys)",
                    theme.name
                );
                assert!(
                    highlight.syntax.string.is_some(),
                    "{path}: theme {:?} defines no syntax.string",
                    theme.name
                );
                assert!(
                    highlight.syntax.number.is_some(),
                    "{path}: theme {:?} defines no syntax.number",
                    theme.name
                );
            }
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

    /// Switching between the app's light and dark slots must actually move
    /// `highlight_theme`, which is what code blocks colour themselves from.
    /// Before Nostra Dark carried a `highlight` block this stayed pinned to the
    /// default light palette no matter what the user selected.
    #[gpui::test]
    fn switching_mode_changes_the_active_highlight_theme(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            preferences::init_global(Preferences::default(), cx);

            let prefs = Preferences {
                theme_mode: Some(preferences::ThemeMode::Dark),
                ..Preferences::default()
            };
            init(&prefs, cx);

            let dark = cx.theme().highlight_theme.clone();
            assert_eq!(
                dark.name, DEFAULT_DARK,
                "the dark slot's own highlight theme is active"
            );

            // `Theme::change` rather than `set_mode`, which would persist to the
            // user's real configuration directory.
            Theme::change(ThemeMode::Light, None, cx);
            let light = cx.theme().highlight_theme.clone();
            assert_ne!(
                light.name, dark.name,
                "switching to the light slot must install a different highlight theme"
            );
            assert_ne!(
                light.style.syntax.string, dark.style.syntax.string,
                "string colour must differ between the light and dark palettes"
            );

            Theme::change(ThemeMode::Dark, None, cx);
            assert_eq!(
                cx.theme().highlight_theme.name,
                dark.name,
                "switching back restores the dark highlight theme"
            );
        });
    }
}
