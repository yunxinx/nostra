//! Nostra — a desktop chat client.
//!
//! The crate is organised as a library plus a thin binary so both `cargo
//! run` and downstream consumers get a clean surface.  All platform wiring
//! lives in `window`, all keyboard actions in `actions`, the palette
//! overrides in `theme`, and persisted user preferences in `preferences`.
//! The UI itself is the `ChatApp` view inside `app.rs`.

mod actions;
mod app;
mod assets;
mod assistant;
mod chat;
mod fonts;
pub mod preferences;
mod theme;
mod window;

use gpui::App;
use gpui_component::{ActiveTheme, Theme, ThemeMode};

use crate::actions::{Quit, ToggleComposerFont, ToggleTheme};
use crate::assets::NostraAssets;

/// Entry point used by `main.rs`.
pub fn run() {
    let app = gpui_platform::application().with_assets(NostraAssets);
    app.run(|cx| {
        let prefs = preferences::load();
        init(&prefs, cx);
        window::open_main_window(prefs, cx);
    });
}

/// One-time application setup: initialise components, theme, keys, menus.
fn init(prefs: &preferences::Preferences, cx: &mut App) {
    gpui_component::init(cx);
    fonts::init(prefs.composer_font, cx);

    // Start from the system appearance, then honour the saved override if any.
    Theme::sync_system_appearance(None, cx);
    apply_saved_theme_mode(prefs, cx);
    theme::apply_dark_palette(cx);

    actions::bind_keys(cx);
    window::install_menus(cx);
    install_action_handlers(cx);

    // Bring the app forward on launch instead of leaving it behind other windows.
    cx.activate(true);
}

fn apply_saved_theme_mode(prefs: &preferences::Preferences, cx: &mut App) {
    let Some(saved) = prefs.theme_mode else {
        return;
    };
    let target = if saved.is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    if cx.theme().mode != target {
        Theme::change(target, None, cx);
    }
}

/// Application-scoped action handlers (per-view actions live in `app.rs`).
fn install_action_handlers(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());

    cx.on_action(|_: &ToggleTheme, cx: &mut App| {
        let next = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(next, None, cx);
        theme::apply_dark_palette(cx);
        cx.refresh_windows();
    });

    cx.on_action(|_: &ToggleComposerFont, cx: &mut App| fonts::toggle(cx));
}
