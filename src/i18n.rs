//! Locale management: bridges the saved [`Language`] preference to
//! rust-i18n (this crate's own strings) and to gpui-component's built-in
//! strings (dropdown placeholders, settings search box, reset tooltips, …).
//!
//! Both crates resolve `t!` through the shared rust-i18n global locale, but
//! we set it via both entry points so a future dependency-version split
//! cannot silently desynchronize the two.

use gpui::App;

use crate::preferences::{self, Language};
use crate::shell::window;

/// Apply the saved language before any window opens.
pub fn init(lang: Language) {
    set_locale(lang);
}

/// The language currently persisted in preferences.
pub fn current(cx: &App) -> Language {
    preferences::handle(cx).snapshot().language
}

/// Switch the UI language: persist, retranslate native menus and window
/// titles, and repaint every window.  In-window strings re-resolve on the
/// next frame because they are looked up inside `render`.
pub fn change(lang: Language, cx: &mut App) {
    if current(cx) == lang {
        return;
    }
    let handle = preferences::handle(cx);
    preferences::update_with(cx, &handle, |p| p.language = lang);
    set_locale(lang);
    window::install_menus(cx);
    crate::settings::refresh_native_title(cx);
    cx.refresh_windows();
}

fn set_locale(lang: Language) {
    rust_i18n::set_locale(lang.locale());
    gpui_component::set_locale(lang.locale());
}
