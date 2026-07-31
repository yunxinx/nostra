//! Application-level actions and their keybindings.
//!
//! Keeping these in one place makes it easy to wire them to menus in
//! `window::install_menus` and to global handlers in `lib::install_action_handlers`.

use gpui::{App, KeyBinding, actions};

actions!(
    nostra,
    [NewChat, Quit, ToggleSidebar, ToggleTheme, OpenSettings]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-n", NewChat, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-n", NewChat, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-l", ToggleTheme, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-l", ToggleTheme, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-,", OpenSettings, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-,", OpenSettings, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-q", Quit, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-f4", Quit, None),
    ]);
}
