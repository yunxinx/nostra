//! Nostra — a desktop chat client.
//!
//! The crate is organised as a library plus a thin binary so both `cargo
//! run` and downstream consumers get a clean surface.  All platform wiring
//! lives in `window`, all keyboard actions in `actions`, theme management in
//! `theme`, locale management in `i18n`, the settings window in `settings`,
//! and persisted user preferences in `preferences`.  The UI itself is the
//! `ChatApp` view inside `app.rs`.

mod actions;
mod app;
mod assets;
mod assistant;
mod chat;
mod fonts;
mod i18n;
pub mod preferences;
mod settings;
mod theme;
mod window;

// Locale files live in `locales/`; English is the fallback for any key a
// locale is missing.  The active locale defaults to zh-CN via preferences.
rust_i18n::i18n!("locales", fallback = "en");

use gpui::App;
use gpui_component::ActiveTheme;

use crate::actions::{OpenSettings, Quit, ToggleTheme};
use crate::assets::NostraAssets;

/// Entry point used by `main.rs`.
pub fn run() {
    let app = gpui_platform::application().with_assets(NostraAssets);
    app.run(|cx| {
        let prefs = preferences::load();
        init(prefs.clone(), cx);
        window::open_main_window(prefs, cx);
    });
}

/// One-time application setup: initialise components, prefs, locale, theme,
/// fonts, keys, menus.
fn init(prefs: preferences::Preferences, cx: &mut App) {
    gpui_component::init(cx);

    // The Prefs global is the single source of truth at runtime; seed it
    // before any subsystem reads or writes settings.
    i18n::init(prefs.language);
    fonts::init(prefs.composer_font, cx);
    preferences::init_global(prefs.clone(), cx);
    theme::init(&prefs, cx);

    actions::bind_keys(cx);
    window::install_menus(cx);
    install_action_handlers(cx);

    // Bring the app forward on launch instead of leaving it behind other windows.
    cx.activate(true);
}

/// Application-scoped action handlers (per-view actions live in `app.rs`).
fn install_action_handlers(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());

    cx.on_action(|_: &ToggleTheme, cx: &mut App| {
        let next = if cx.theme().mode.is_dark() {
            preferences::ThemeMode::Light
        } else {
            preferences::ThemeMode::Dark
        };
        theme::set_mode(Some(next), cx);
    });

    cx.on_action(|_: &OpenSettings, cx: &mut App| settings::open(cx));
}
