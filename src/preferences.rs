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
