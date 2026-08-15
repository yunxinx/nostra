//! Stable user-data paths shared by preferences and session persistence.
//!
//! Nostra deliberately follows Hunea's single-directory layout.  The
//! application uses `~/.config/nostra` on every platform instead of letting
//! each subsystem choose a platform-specific location independently.

use std::{ffi::OsString, path::PathBuf};

/// Return the user's portable `.config` directory.
pub(crate) fn config_dir() -> Option<PathBuf> {
    config_dir_from_env(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

fn config_dir_from_env(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    xdg_config_home
        .and_then(absolute_path)
        .or_else(|| {
            home.and_then(absolute_path)
                .map(|path| path.join(".config"))
        })
        .or_else(|| {
            user_profile
                .and_then(absolute_path)
                .map(|path| path.join(".config"))
        })
}

fn absolute_path(value: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    // Environment roots are untrusted process input. Accepting a relative
    // value would make durable data move with the process working directory.
    path.is_absolute().then_some(path)
}

/// Return Nostra's unified user data root.
pub(crate) fn nostra_config_dir() -> Option<PathBuf> {
    config_dir().map(|path| path.join("nostra"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    const ABSOLUTE_XDG: &str = "/var/lib/nostra-config";
    #[cfg(windows)]
    const ABSOLUTE_XDG: &str = r"C:\nostra-config";
    #[cfg(not(windows))]
    const ABSOLUTE_HOME: &str = "/Users/example";
    #[cfg(windows)]
    const ABSOLUTE_HOME: &str = r"C:\Users\example";

    #[test]
    fn absolute_xdg_config_home_takes_priority() {
        assert_eq!(
            config_dir_from_env(Some(ABSOLUTE_XDG.into()), Some(ABSOLUTE_HOME.into()), None,),
            Some(PathBuf::from(ABSOLUTE_XDG))
        );
    }

    #[test]
    fn empty_xdg_config_home_falls_back_to_absolute_home() {
        assert_eq!(
            config_dir_from_env(Some("".into()), Some(ABSOLUTE_HOME.into()), None),
            Some(PathBuf::from(ABSOLUTE_HOME).join(".config"))
        );
    }

    #[test]
    fn relative_xdg_config_home_falls_back_to_absolute_home() {
        assert_eq!(
            config_dir_from_env(
                Some("relative-config".into()),
                Some(ABSOLUTE_HOME.into()),
                None,
            ),
            Some(PathBuf::from(ABSOLUTE_HOME).join(".config"))
        );
    }

    #[test]
    fn relative_or_empty_home_values_never_resolve_against_the_working_directory() {
        assert_eq!(
            config_dir_from_env(None, Some("relative-home".into()), Some("".into())),
            None
        );
    }

    #[test]
    fn invalid_home_falls_back_to_an_absolute_user_profile() {
        assert_eq!(
            config_dir_from_env(
                None,
                Some("relative-home".into()),
                Some(ABSOLUTE_HOME.into()),
            ),
            Some(PathBuf::from(ABSOLUTE_HOME).join(".config"))
        );
    }
}
