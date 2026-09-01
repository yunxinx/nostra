//! Window creation and native platform integration.
//!
//! Owns everything that a mature gpui-component app is expected to configure
//! around the top-level window: display-clamped bounds, immersive titlebar,
//! native menu bar (macOS), startup activation/focus, quit-on-last-window,
//! and restoring the previous session's window geometry.

use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Context, Focusable as _, FontWeight, Global,
    IntoElement, Menu, MenuItem, ParentElement as _, Pixels, Render, SharedString, Size,
    Styled as _, WeakEntity, Window, WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};
use gpui_component::{
    ActiveTheme, Root, TitleBar,
    button::{Button, ButtonVariants as _},
    v_flex,
};
use rust_i18n::t;
use std::{cell::RefCell, rc::Rc};

use crate::appearance::glass;
use crate::preferences::{Preferences, WindowGeometry};
use crate::runtime::{CompositionRoot, QUIT_FALLBACK_TIMEOUT};
use crate::session::SessionStores;
use crate::shell::actions::{DeleteChat, NewChat, OpenSettings, Quit, ToggleSidebar, ToggleTheme};
use crate::shell::app::ChatApp;

/// Weak handle used by application-level commands to reach the main view
/// regardless of which window currently owns focus.
#[derive(Default)]
struct MainView(Option<WeakEntity<ChatApp>>);

impl Global for MainView {}

/// Route New Chat to the main window.
pub fn new_chat(cx: &mut App) {
    update_main(cx, |app, window, cx| app.new_chat(window, cx));
}

/// Route Delete Chat to the main window.
pub fn delete_chat(cx: &mut App) {
    update_main(cx, |app, window, cx| app.request_delete_active(window, cx));
}

/// Route Toggle Sidebar to the main window.
pub fn toggle_sidebar(cx: &mut App) {
    update_main(cx, |app, _, cx| app.toggle_sidebar(cx));
}

/// Finish durable session and preference work before asking GPUI to enter its
/// short final quit phase. Native window-close still has the app-scoped quit
/// observer as a bounded fallback when no view action can run first.
pub fn request_quit(cx: &mut App) {
    let Some(view) = cx
        .try_global::<MainView>()
        .and_then(|state| state.0.clone())
    else {
        cx.quit();
        return;
    };
    if view.update(cx, |app, cx| app.request_quit(cx)).is_err() {
        cx.quit();
    }
}

fn update_main(
    cx: &mut App,
    update: impl FnOnce(&mut ChatApp, &mut Window, &mut Context<ChatApp>),
) {
    let Some(view) = cx
        .try_global::<MainView>()
        .and_then(|state| state.0.clone())
    else {
        return;
    };
    view.update_in(cx, update).ok();
}

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
pub fn open_main_window(
    prefs: Preferences,
    preference_handle: crate::preferences::PreferenceHandle,
    cx: &mut App,
) {
    let bounds = restored_bounds(prefs.window, PREFERRED_SIZE, MIN_SIZE, cx);
    // Opening SQLite catalogs and replaying a pending repair can scan every
    // source file. Keep that work off the application thread before the first
    // window and its ChatView are constructed.
    let stores = cx.background_spawn(async { SessionStores::open_default() });
    let http_client = cx.http_client();

    cx.spawn(async move |cx| {
        let stores = stores.await;
        let composition = match CompositionRoot::builder(stores)
            .with_preferences(preference_handle)
            .with_http_client(http_client)
            .build()
            .await
        {
            Ok(composition) => composition,
            Err(error) => {
                crate::logging::error(
                    "runtime.composition",
                    format_args!("failed to build application composition: {error}"),
                );
                fail_startup(error.to_string().into(), cx);
                return;
            }
        };
        let Some(services) = composition.services() else {
            crate::logging::error(
                "runtime.composition",
                "application composition did not expose active services",
            );
            fail_startup(
                "application composition did not expose active services".into(),
                cx,
            );
            return;
        };
        let composition = Rc::new(RefCell::new(Some(composition)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitleBar::title_bar_options()),
            // The app draws the interactive titlebar. On macOS this prevents
            // AppKit from treating buttons inside the transparent titlebar as
            // native window-drag regions; ChatApp supplies the blank drag area.
            app_owns_titlebar_drag: cfg!(target_os = "macos"),
            window_min_size: Some(MIN_SIZE),
            kind: WindowKind::Normal,
            #[cfg(target_os = "macos")]
            window_background: glass::window_background(prefs.glass_effect),
            #[cfg(target_os = "linux")]
            window_background: WindowBackgroundAppearance::Transparent,
            #[cfg(target_os = "linux")]
            window_decorations: Some(WindowDecorations::Client),
            ..Default::default()
        };

        let window = match cx.open_window(options, |window, cx| {
            let app_view = cx.new(|cx| ChatApp::new(prefs.clone(), services.clone(), window, cx));
            cx.set_global(MainView(Some(app_view.downgrade())));

            // Default focus to the app root so global keybindings dispatch to it
            // before any input steals focus.
            let focus_handle = app_view.focus_handle(cx);
            window.defer(cx, move |window, cx| {
                if window.focused(cx).is_none() {
                    focus_handle.focus(window, cx);
                }
            });

            cx.new(|cx| {
                Root::new(app_view, window, cx).bg(glass::root_background(cx.theme().background))
            })
        }) {
            Ok(window) => window,
            Err(error) => {
                crate::logging::error(
                    "shell.window",
                    format_args!("failed to open main window: {error:?}"),
                );
                fail_startup(error.to_string().into(), cx);
                return;
            }
        };

        if let Err(error) = window.update(cx, |_, window, cx| {
            window.activate_window();
            window.set_window_title("Nostra");
            // Quit the process when the main window closes.
            cx.on_release(|_, cx| cx.quit()).detach();
        }) {
            crate::logging::error(
                "shell.window",
                format_args!("failed to finish main-window setup: {error:?}"),
            );
        }

        let composition_for_quit = Rc::clone(&composition);
        cx.update(|cx| {
            App::on_app_quit(cx, move |cx| {
                let composition = composition_for_quit.borrow_mut().take();
                let task = composition.as_ref().map(|composition_ref| {
                    let coordinator = composition_ref.exit_coordinator();
                    let snapshot = composition_ref
                        .preferences()
                        .map(|lease| lease.handle().snapshot())
                        .unwrap_or_default();
                    cx.background_executor()
                        .spawn(coordinator.run(snapshot, QUIT_FALLBACK_TIMEOUT))
                });
                async move {
                    if let Some(mut composition) = composition {
                        let Some(task) = task else {
                            return;
                        };
                        let report = task.await;
                        if let Some(error) = report.dispose_error() {
                            crate::logging::error(
                                "runtime.composition",
                                format_args!("failed to close application composition: {error}"),
                            );
                        }
                        if report.session.is_err() {
                            return;
                        }
                        if let Err(error) = composition.close_after_exit().await {
                            crate::logging::error(
                                "runtime.composition",
                                format_args!("failed to close application composition: {error}"),
                            );
                        }
                    }
                }
            })
            .detach();
        });
    })
    .detach();
}

/// Terminal startup-failure view: the composition could not be built, so no
/// main window exists. Shows the error and offers the only remaining action.
struct StartupFailure {
    message: SharedString,
}

impl Render for StartupFailure {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .p_8()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("startup.failed_title").to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.message.clone()),
            )
            .child(
                Button::new("startup-quit")
                    .primary()
                    .label(t!("startup.quit").to_string())
                    .on_click(|_, _, cx| cx.quit()),
            )
    }
}

/// Surface a fatal startup error in a dedicated window instead of leaving a
/// windowless process behind. Quits when the user dismisses the window, and
/// quits immediately when even that window cannot be opened.
fn fail_startup(message: SharedString, cx: &mut AsyncApp) {
    let bounds = cx.update(|cx| Bounds::centered(None, size(px(480.), px(240.)), cx));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(360.), px(180.))),
        kind: WindowKind::Normal,
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        window.set_window_title("Nostra");
        let view = cx.new(|_| StartupFailure { message });
        cx.new(|cx| Root::new(view, window, cx))
    });
    match opened {
        Ok(window) => {
            let _ = window.update(cx, |_, window, cx| {
                window.activate_window();
                cx.on_release(|_, cx| cx.quit()).detach();
            });
        }
        Err(error) => {
            crate::logging::error(
                "shell.window",
                format_args!("failed to open startup-failure window: {error:?}"),
            );
            cx.update(|cx| cx.quit());
        }
    }
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
                items: vec![
                    MenuItem::action(t!("menu.new_chat").to_string(), NewChat),
                    MenuItem::action(t!("menu.delete_chat").to_string(), DeleteChat),
                ],
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
pub(crate) fn restored_bounds(
    saved: Option<WindowGeometry>,
    preferred_size: Size<Pixels>,
    minimum_size: Size<Pixels>,
    cx: &App,
) -> Bounds<Pixels> {
    if let Some(g) = saved {
        let finite = [g.x, g.y, g.width, g.height].iter().all(|v| v.is_finite());
        if finite {
            let mut bounds = Bounds {
                origin: point(px(g.x), px(g.y)),
                size: size(
                    px(g.width).max(minimum_size.width),
                    px(g.height).max(minimum_size.height),
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

    let mut size = preferred_size;
    if let Some(display) = cx.primary_display() {
        let ds = display.bounds().size;
        size.width = size.width.min(ds.width * 0.85);
        size.height = size.height.min(ds.height * 0.85);
    }
    Bounds::centered(None, size, cx)
}
