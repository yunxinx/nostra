//! Stable user-data paths shared by preferences and session persistence.
//!
//! Nostra deliberately follows Hunea's single-directory layout.  The
//! application uses `~/.config/nostra` on every platform instead of letting
//! each subsystem choose a platform-specific location independently.

use std::path::PathBuf;

/// Return the user's portable `.config` directory.
pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".config"))
        })
}

/// Return Nostra's unified user data root.
pub(crate) fn nostra_config_dir() -> Option<PathBuf> {
    config_dir().map(|path| path.join("nostra"))
}
