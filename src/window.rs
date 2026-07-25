//! Window creation and native platform integration.
//!
//! Owns everything that a mature gpui-component app is expected to configure
//! around the top-level window: display-clamped bounds, immersive titlebar,
//! native menu bar (macOS), startup activation/focus, quit-on-last-window,
//! and restoring the previous session's window geometry.

use anyhow::{Context as _, Result};
use gpui::*;
use gpui_component::{ActiveTheme, Root, TitleBar};
use rust_i18n::t;

use crate::actions::{NewChat, OpenSettings, Quit, ToggleSidebar, ToggleTheme};
use crate::app::ChatApp;
use crate::preferences::{Preferences, WindowGeometry};

/// Preferred initial window size; clamped to 85% of the primary display.
const PREFERRED_SIZE: Size<Pixels> = Size {
    width: px(1180.),
    height: px(760.),
};

/// Hard minimum so the layout never collapses below usability.
const MIN_SIZE: Size<Pixels> = Size {
    width: px(720.),
    height: px(480.),
};

/// Open the main chat window and wire up per-window platform hooks.
pub fn open_main_window(prefs: Preferences, cx: &mut App) {
    let bounds = initial_bounds(prefs.window, cx);

    cx.spawn(async move |cx| -> Result<()> {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitleBar::title_bar_options()),
            window_min_size: Some(MIN_SIZE),
            kind: WindowKind::Normal,
            #[cfg(target_os = "linux")]
            window_background: WindowBackgroundAppearance::Transparent,
            #[cfg(target_os = "linux")]
            window_decorations: Some(WindowDecorations::Client),
            ..Default::default()
        };

        let window = cx
            .open_window(options, |window, cx| {
                let app_view = cx.new(|cx| ChatApp::new(prefs.clone(), window, cx));

                // Default focus to the app root so global keybindings dispatch to it
                // before any input steals focus.
                let focus_handle = app_view.focus_handle(cx);
                window.defer(cx, move |window, cx| {
                    if window.focused(cx).is_none() {
                        focus_handle.focus(window, cx);
                    }
                });

                cx.new(|cx| Root::new(app_view, window, cx).bg(cx.theme().background))
            })
            .context("failed to open main window")?;

        window.update(cx, |_, window, cx| {
            window.activate_window();
            window.set_window_title("Nostra");
            // Quit the process when the main window closes.
            cx.on_release(|_, cx| cx.quit()).detach();
        })?;

        Ok(())
    })
    .detach_and_log_err(cx);
}

/// Install (or re-install after a language change) the macOS native menu
/// bar.  No-op on other platforms.
pub fn install_menus(_cx: &mut App) {
    #[cfg(target_os = "macos")]
    {
        _cx.set_menus(vec![
            Menu {
                name: "Nostra".into(),
                items: vec![
                    MenuItem::action(t!("menu.settings").to_string(), OpenSettings),
                    MenuItem::action(t!("menu.toggle_theme").to_string(), ToggleTheme),
                    MenuItem::separator(),
                    MenuItem::action(t!("menu.quit").to_string(), Quit),
                ],
                disabled: false,
            },
            Menu {
                name: t!("menu.file").to_string().into(),
                items: vec![MenuItem::action(t!("menu.new_chat").to_string(), NewChat)],
                disabled: false,
            },
            Menu {
                name: t!("menu.view").to_string().into(),
                items: vec![MenuItem::action(
                    t!("menu.toggle_sidebar").to_string(),
                    ToggleSidebar,
                )],
                disabled: false,
            },
        ]);
    }
}

/// Bounds for the main window: the saved geometry when it is still sane and
/// visible on some connected display, the default centered rect otherwise.
/// The restored size is clamped to the display it lands on, so a saved
/// geometry from a larger (since disconnected) monitor never produces an
/// oversized window.
fn initial_bounds(saved: Option<WindowGeometry>, cx: &App) -> Bounds<Pixels> {
    if let Some(g) = saved {
        let finite = [g.x, g.y, g.width, g.height].iter().all(|v| v.is_finite());
        if finite {
            let mut bounds = Bounds {
                origin: point(px(g.x), px(g.y)),
                size: size(
                    px(g.width).max(MIN_SIZE.width),
                    px(g.height).max(MIN_SIZE.height),
                ),
            };
            // A monitor may have been unplugged since the last run; only
            // restore geometry that still lands on a connected display.
            if let Some(display) = cx
                .displays()
                .into_iter()
                .find(|d| d.bounds().intersects(&bounds))
            {
                let ds = display.bounds().size;
                bounds.size.width = bounds.size.width.min(ds.width);
                bounds.size.height = bounds.size.height.min(ds.height);
                return bounds;
            }
        }
    }

    clamped_bounds(cx)
}

fn clamped_bounds(cx: &App) -> Bounds<Pixels> {
    let mut size = PREFERRED_SIZE;
    if let Some(display) = cx.primary_display() {
        let ds = display.bounds().size;
        size.width = size.width.min(ds.width * 0.85);
        size.height = size.height.min(ds.height * 0.85);
    }
    Bounds::centered(None, size, cx)
}
