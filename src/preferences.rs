//! Persistent user preferences (sidebar, theme, language, window geometry).
//!
//! Prefs are written to a platform-specific config directory and read back on
//! startup.  Any deserialize failure falls back to `Preferences::default`, so
//! users never see a startup error just because the on-disk format has
//! drifted.  At runtime the current values live in the [`Prefs`] app-global;
//! mutations go through [`update`], which persists synchronously and atomically
//! so settings survive even if the app never quits cleanly.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

use gpui::{App, Global, Window};
use serde::{Deserialize, Serialize};

/// Directory name inside the platform config root.
const APP_DIRNAME: &str = "nostra";
const FILE_NAME: &str = "preferences.json";

/// Snapshot of user preferences that survives across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    /// Sidebar width in the expanded state.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// Whether the sidebar is collapsed.
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Explicit theme mode override.  `None` means "follow system".
    #[serde(default, deserialize_with = "theme_mode_or_default")]
    pub theme_mode: Option<ThemeMode>,
    /// Which bundled font the composer input uses.  Tolerant of values from
    /// older preference files (unknown names fall back to the default font
    /// instead of invalidating the whole file).
    #[serde(default, deserialize_with = "composer_font_or_default")]
    pub composer_font: ComposerFont,
    /// UI language.  Unknown values fall back to the default (zh-CN).
    #[serde(default, deserialize_with = "language_or_default")]
    pub language: Language,
    /// Theme name applied while in light mode.  `None` or an unregistered
    /// name falls back to the built-in default at startup.
    #[serde(default)]
    pub light_theme: Option<String>,
    /// Theme name applied while in dark mode.  Same fallback rules.
    #[serde(default)]
    pub dark_theme: Option<String>,
    /// Last known main-window geometry (restore bounds).  `None` on first
    /// run; invalid values are clamped or discarded at restore time.
    #[serde(default)]
    pub window: Option<WindowGeometry>,
    /// Last known settings-window geometry. `None` until that window has
    /// been opened; invalid values are discarded at restore time.
    #[serde(default)]
    pub settings_window: Option<WindowGeometry>,
}

/// Deserialize a [`ComposerFont`] via its derived impl, mapping unknown
/// values (e.g. fonts that have since been removed) to the default instead
/// of failing the whole preferences file.
fn composer_font_or_default<'de, D>(d: D) -> Result<ComposerFont, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(ComposerFont::deserialize(value).unwrap_or_default())
}

/// Same tolerance for [`Language`]: an unrecognized language tag reverts to
/// the default instead of invalidating the whole preferences file.
fn language_or_default<'de, D>(d: D) -> Result<Language, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(Language::deserialize(value).unwrap_or_default())
}

/// Unknown theme modes degrade to "follow system" without invalidating the
/// rest of the preferences file.
fn theme_mode_or_default<'de, D>(d: D) -> Result<Option<ThemeMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value.and_then(|value| ThemeMode::deserialize(value).ok()))
}

fn default_sidebar_width() -> f32 {
    272.0
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            sidebar_collapsed: false,
            theme_mode: None,
            composer_font: ComposerFont::default(),
            language: Language::default(),
            light_theme: None,
            dark_theme: None,
            window: None,
            settings_window: None,
        }
    }
}

/// UI languages the app can render in.  The serialized form doubles as the
/// stable identifier used by the settings dropdown.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    /// 简体中文 (default).
    #[default]
    ZhCn,
    /// English.
    En,
}

impl Language {
    /// BCP 47 tag understood by rust-i18n's locale lookup; must match the
    /// locale keys used in `locales/nostra.yml` and gpui-component's `ui.yml`.
    pub fn locale(self) -> &'static str {
        match self {
            Language::ZhCn => "zh-CN",
            Language::En => "en",
        }
    }

    /// Native-script label shown in the language dropdown.  Deliberately not
    /// translated: each language names itself.
    pub fn label(self) -> &'static str {
        match self {
            Language::ZhCn => "简体中文",
            Language::En => "English",
        }
    }

    /// Stable identifier for dropdown values (the serde kebab-case form).
    pub fn key(self) -> &'static str {
        match self {
            Language::ZhCn => "zh-cn",
            Language::En => "en",
        }
    }

    /// Inverse of [`Language::key`]; unknown keys fall back to the default.
    pub fn from_key(key: &str) -> Self {
        match key {
            "en" => Language::En,
            _ => Language::default(),
        }
    }

    pub fn all() -> [Language; 2] {
        [Language::ZhCn, Language::En]
    }
}

/// Main-window restore bounds in global screen coordinates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowGeometry {
    /// Capture normal restore bounds (not transient maximized/fullscreen size).
    pub fn from_window(window: &Window) -> Self {
        let bounds = window.window_bounds().get_bounds();
        Self {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        }
    }
}

/// Fonts the composer can render with.  The default bundles Latin + CJK +
/// fullwidth punctuation in one file, which keeps the input's soft-wrap
/// estimates exact on every platform (see chat.rs).  The JetBrains option
/// bundles only Latin and lets CJK fall back to the platform font (PingFang
/// on macOS) — same drift-safety mechanism, since the primary font carries
/// no fullwidth glyphs at all.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComposerFont {
    /// Maple Mono CN — rounded Latin + 圆体 CJK, fully self-contained.
    #[default]
    MapleMonoCn,
    /// JetBrains Mono for Latin, system font for CJK.
    JetBrainsMono,
}

impl ComposerFont {
    /// The family name recorded in the bundled TTF's name table; must match
    /// exactly for `font_family` to resolve to the embedded font.
    pub fn family(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "Maple Mono CN",
            ComposerFont::JetBrainsMono => "JetBrains Mono",
        }
    }

    /// Human-readable label for menus.  Font names are proper nouns, so the
    /// label is not routed through i18n.
    pub fn label(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "Maple Mono 圆体",
            ComposerFont::JetBrainsMono => "JetBrains Mono + 系统中文",
        }
    }

    /// Stable identifier for dropdown values (the serde kebab-case form,
    /// which splits on every capital: `jet-brains-mono`).
    pub fn key(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "maple-mono-cn",
            ComposerFont::JetBrainsMono => "jet-brains-mono",
        }
    }

    /// Inverse of [`ComposerFont::key`]; unknown keys fall back to default.
    pub fn from_key(key: &str) -> Self {
        match key {
            "jet-brains-mono" => ComposerFont::JetBrainsMono,
            _ => ComposerFont::default(),
        }
    }

    pub fn all() -> [ComposerFont; 2] {
        [ComposerFont::MapleMonoCn, ComposerFont::JetBrainsMono]
    }
}

/// Serializable theme mode.  We keep this decoupled from
/// `gpui_component::ThemeMode` so preferences can be read without the UI
/// crate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, ThemeMode::Dark)
    }
}

/// App-global holding the live preferences: the single source of truth
/// between startup and quit.  UI state that only matters at exit (sidebar
/// geometry, window bounds) is written back by `ChatApp` on quit; settings
/// changes go through [`update`] and persist immediately.
pub struct Prefs(pub Preferences);

impl Global for Prefs {}

/// Seed the [`Prefs`] global from the loaded preferences.  Must run during
/// app init, before any UI reads settings.
pub fn init_global(prefs: Preferences, cx: &mut App) {
    cx.set_global(Prefs(prefs));
}

/// The live preferences.
pub fn get(cx: &App) -> &Preferences {
    &cx.global::<Prefs>().0
}

/// Mutate the live preferences and persist the result.  The write happens
/// synchronously on purpose: the file is a few hundred bytes, and spawning
/// each save onto the background pool would let two rapid changes race on
/// the same path (fs::write is not atomic — last-spawned is not guaranteed
/// last-written).  Save errors are logged and otherwise ignored — a failed
/// write never breaks the running app.
pub fn update(cx: &mut App, f: impl FnOnce(&mut Preferences)) {
    let prefs = &mut cx.global_mut::<Prefs>().0;
    f(prefs);
    let snapshot = prefs.clone();
    if let Err(e) = save(&snapshot) {
        eprintln!("failed to save preferences: {e:?}");
    }
}

/// Fold exit-time state into the live preferences and return the merged
/// snapshot.  Unlike [`update`] this does not spawn a save — quit hooks run
/// the flush themselves so gpui can await it before the process exits.
pub fn snapshot_with(cx: &mut App, f: impl FnOnce(&mut Preferences)) -> Preferences {
    let prefs = &mut cx.global_mut::<Prefs>().0;
    f(prefs);
    prefs.clone()
}

/// Full path where preferences are stored.  `None` on platforms where no
/// standard config directory can be resolved from the environment.
pub fn path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(APP_DIRNAME).join(FILE_NAME))
}

/// Load preferences, or return defaults if the file is missing / corrupt.
pub fn load() -> Preferences {
    let Some(p) = path() else {
        return Preferences::default();
    };
    let Ok(contents) = std::fs::read_to_string(&p) else {
        return Preferences::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Persist preferences to disk.  Errors are returned so the caller can log
/// them; nothing about a save failure prevents the app from working.
pub fn save(prefs: &Preferences) -> anyhow::Result<()> {
    let Some(p) = path() else {
        anyhow::bail!("no config directory available on this platform");
    };
    save_to_path(&p, prefs)
}

/// Persist preferences by atomically replacing the target with a fully
/// written temporary file from the same directory.
fn save_to_path(path: &Path, prefs: &Preferences) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(prefs)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&json)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    // Persist the directory entry as well as the file contents on Unix.
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;

    Ok(())
}

/// Base config directory for the current platform.
fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Preference files written before the settings-window feature carry
    /// none of the new fields; they must load cleanly with defaults.
    #[test]
    fn legacy_file_gets_defaults_for_new_fields() {
        let legacy = r#"{
            "sidebar_width": 300.0,
            "sidebar_collapsed": true,
            "theme_mode": "dark",
            "composer_font": "jet-brains-mono"
        }"#;
        let prefs: Preferences = serde_json::from_str(legacy).expect("legacy file must parse");
        assert_eq!(prefs.sidebar_width, 300.0);
        assert!(prefs.sidebar_collapsed);
        assert_eq!(prefs.theme_mode, Some(ThemeMode::Dark));
        assert_eq!(prefs.composer_font, ComposerFont::JetBrainsMono);
        assert_eq!(prefs.language, Language::ZhCn);
        assert_eq!(prefs.light_theme, None);
        assert_eq!(prefs.dark_theme, None);
        assert_eq!(prefs.window, None);
        assert_eq!(prefs.settings_window, None);
    }

    /// Unknown enum tags (from newer or older builds) degrade to defaults
    /// instead of poisoning the whole file.
    #[test]
    fn unknown_language_and_font_fall_back_to_default() {
        let json = r#"{
            "language": "klingon",
            "composer_font": "comic-sans"
        }"#;
        let prefs: Preferences = serde_json::from_str(json).expect("must parse");
        assert_eq!(prefs.language, Language::ZhCn);
        assert_eq!(prefs.composer_font, ComposerFont::MapleMonoCn);
    }

    #[test]
    fn unknown_theme_mode_falls_back_without_discarding_other_preferences() {
        let json = r#"{
            "sidebar_width": 336.0,
            "sidebar_collapsed": true,
            "theme_mode": "sepia",
            "language": "en"
        }"#;

        let prefs: Preferences = serde_json::from_str(json).expect("preferences must parse");

        assert_eq!(prefs.sidebar_width, 336.0);
        assert!(prefs.sidebar_collapsed);
        assert_eq!(prefs.theme_mode, None);
        assert_eq!(prefs.language, Language::En);
    }

    #[test]
    fn atomic_save_replaces_an_existing_preferences_file() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "truncated old contents").expect("seed old file");
        let prefs = Preferences {
            sidebar_width: 336.0,
            language: Language::En,
            ..Preferences::default()
        };

        save_to_path(&path, &prefs).expect("save preferences atomically");

        let saved = std::fs::read_to_string(path).expect("read saved preferences");
        let parsed: Preferences = serde_json::from_str(&saved).expect("saved JSON must parse");
        assert_eq!(parsed.sidebar_width, 336.0);
        assert_eq!(parsed.language, Language::En);
    }

    #[test]
    fn window_geometry_round_trips() {
        let prefs = Preferences {
            window: Some(WindowGeometry {
                x: -12.5,
                y: 40.0,
                width: 1180.0,
                height: 760.0,
            }),
            settings_window: Some(WindowGeometry {
                x: 120.0,
                y: 80.0,
                width: 820.0,
                height: 560.0,
            }),
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).expect("serialize");
        let back: Preferences = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.window, prefs.window);
        assert_eq!(back.settings_window, prefs.settings_window);
    }

    /// Language keys are the stable dropdown identifiers; the round trip
    /// must hold and unknown keys must land on the default.
    #[test]
    fn language_key_round_trip() {
        for lang in Language::all() {
            assert_eq!(Language::from_key(lang.key()), lang);
        }
        assert_eq!(Language::from_key("nope"), Language::ZhCn);
        for font in ComposerFont::all() {
            assert_eq!(ComposerFont::from_key(font.key()), font);
        }
    }
}
