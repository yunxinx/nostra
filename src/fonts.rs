//! Bundled composer fonts: registration with gpui and the active-font global.
//!
//! Both fonts are embedded via `assets.rs` and registered into the text
//! system at startup, so `font_family("Sarasa Mono SC")` (etc.) resolves to
//! the bundled file on every platform — no system-font variation involved.

use gpui::{App, Global};

use crate::assets;
use crate::preferences::ComposerFont;

/// Embedded font files to register.  Paths are relative to the `assets/`
/// embed root.
const FONT_FILES: &[&str] = &[
    "fonts/MapleMono-CN-Regular.ttf",
    "fonts/JetBrainsMono-Regular.ttf",
];

/// App-global holding the composer font currently in effect.  Flipped by the
/// `ToggleComposerFont` action and snapshotted into preferences on quit.
pub struct ActiveComposerFont(pub ComposerFont);

impl Global for ActiveComposerFont {}

/// Register the bundled fonts and seed the active-font global from saved
/// preferences.  Must run before the first window renders the composer.
pub fn init(saved: ComposerFont, cx: &mut App) {
    let fonts = FONT_FILES
        .iter()
        .filter_map(|path| assets::embedded(path))
        .collect::<Vec<_>>();

    debug_assert_eq!(fonts.len(), FONT_FILES.len(), "bundled font missing");

    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("failed to register bundled fonts: {e:?}");
    }

    cx.set_global(ActiveComposerFont(saved));
}

/// The composer font currently in effect.
pub fn active(cx: &App) -> ComposerFont {
    cx.global::<ActiveComposerFont>().0
}

/// Switch to the other bundled font and repaint.
pub fn toggle(cx: &mut App) {
    let next = active(cx).toggled();
    cx.set_global(ActiveComposerFont(next));
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embed paths and the on-disk assets must stay in sync — a typo'd
    /// path would silently skip registration and the composer would fall
    /// back to the system font (reintroducing the wrap-drift bug).
    #[test]
    fn bundled_font_files_resolve() {
        for path in FONT_FILES {
            assert!(
                crate::assets::embedded(path).is_some(),
                "missing embedded font: {path}"
            );
        }
    }
}
