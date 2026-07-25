//! Persistent user preferences (sidebar width, collapse state, theme mode).
//!
//! Prefs are written to a platform-specific config directory on app quit
//! and read back on startup.  Any deserialize failure falls back to
//! `Preferences::default`, so users never see a startup error just because
//! the on-disk format has drifted.

use std::path::PathBuf;

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
    #[serde(default)]
    pub theme_mode: Option<ThemeMode>,
    /// Which bundled font the composer input uses.  Tolerant of values from
    /// older preference files (unknown names fall back to the default font
    /// instead of invalidating the whole file).
    #[serde(default, deserialize_with = "composer_font_or_default")]
    pub composer_font: ComposerFont,
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

    /// Human-readable label for menus.
    pub fn label(self) -> &'static str {
        match self {
            ComposerFont::MapleMonoCn => "Maple Mono 圆体",
            ComposerFont::JetBrainsMono => "JetBrains Mono + 系统中文",
        }
    }

    /// The other choice, for a toggle-style menu item.
    pub fn toggled(self) -> Self {
        match self {
            ComposerFont::MapleMonoCn => ComposerFont::JetBrainsMono,
            ComposerFont::JetBrainsMono => ComposerFont::MapleMonoCn,
        }
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
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(prefs)?;
    std::fs::write(&p, json)?;
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
