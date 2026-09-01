//! Nostra — a desktop chat client.
//!
//! The crate is organised as a library plus a thin binary so both `cargo
//! run` and downstream consumers get a clean surface.  Modules are grouped by
//! responsibility:
//!
//! * `shell` — the root view, its native window, and app-level actions.
//! * `chat` — one conversation: transcript, composer, and streaming turn.
//! * `ui` — view primitives shared across features.
//! * `appearance` — theme, fonts, and the macOS glass effect.
//! * `settings` — the standalone settings window.
//! * [`llm`] — the UI-independent model generation gateway.
//! * [`preferences`] — persisted user settings and the live `Prefs` global.
//! * [`runtime`] — typed identities and composition primitives.
//! * [`session`] — durable conversation facts and storage capabilities.
//! * `providers`, `i18n`, `assets` — provider profiles, locale management, and
//!   embedded assets.
//!
//! The linked modules form the crate's public surface; the rest are internal
//! and linked here as plain names.

mod appearance;
mod assets;
mod chat;
mod i18n;
pub mod llm;
mod logging;
mod paths;
pub mod preferences;
mod providers;
pub mod runtime;
pub mod session;
mod settings;
mod shell;
mod ui;

// Locale files live in `locales/`; English is the fallback for any key a
// locale is missing.  The active locale defaults to zh-CN via preferences.
rust_i18n::i18n!("locales", fallback = "en");

use gpui::App;
use gpui_component::ActiveTheme;
use reqwest_client::ReqwestClient;

use crate::appearance::{fonts, glass, theme};
use crate::assets::NostraAssets;
use crate::shell::actions::{
    self, DeleteChat, NewChat, OpenSettings, Quit, ToggleSidebar, ToggleTheme,
};
use crate::shell::window;

/// Entry point used by `main.rs`.
pub fn run() {
    // Diagnostics are independent from session facts and are best-effort. We
    // start them before loading preferences so startup failures are captured.
    logging::init();
    let app = gpui_platform::application()
        .with_assets(NostraAssets)
        .with_http_client(std::sync::Arc::new(ReqwestClient::new()));
    app.run(|cx| {
        let prefs = preferences::load();
        logging::set_detailed(prefs.detailed_logging);
        logging::info("app.lifecycle", "application started");
        init(prefs.clone(), cx);
        let preference_handle = preferences::handle(cx);
        window::open_main_window(prefs, preference_handle, cx);
    });
    logging::info("app.lifecycle", "application stopped");
    logging::shutdown();
}

/// One-time application setup: initialise components, prefs, locale, theme,
/// fonts, keys, menus.
fn init(prefs: preferences::Preferences, cx: &mut App) {
    gpui_component::init(cx);

    // The Prefs global is the single source of truth at runtime; seed it
    // before any subsystem reads or writes settings.
    preferences::init_global(prefs.clone(), cx);
    let preference_handle = preferences::handle(cx);
    let current = preference_handle.snapshot();
    i18n::init(current.language);
    fonts::init(current.composer_font, cx);
    glass::init(cx);
    theme::init(&prefs, cx);

    actions::bind_keys(cx);
    window::install_menus(cx);
    install_action_handlers(cx);

    // Bring the app forward on launch instead of leaving it behind other windows.
    cx.activate(true);
}

/// Application-scoped action handlers (per-view actions live in `shell::app`).
fn install_action_handlers(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| window::request_quit(cx));

    cx.on_action(|_: &ToggleTheme, cx: &mut App| {
        let next = if cx.theme().mode.is_dark() {
            preferences::ThemeMode::Light
        } else {
            preferences::ThemeMode::Dark
        };
        let preference_handle = preferences::handle(cx);
        theme::set_mode(Some(next), &preference_handle, cx);
    });

    cx.on_action(|_: &OpenSettings, cx: &mut App| settings::open(cx));
    cx.on_action(|_: &NewChat, cx: &mut App| window::new_chat(cx));
    cx.on_action(|_: &DeleteChat, cx: &mut App| window::delete_chat(cx));
    cx.on_action(|_: &ToggleSidebar, cx: &mut App| window::toggle_sidebar(cx));
}
