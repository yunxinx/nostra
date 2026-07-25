//! Standalone settings window (singleton).
//!
//! Custom, macOS-style shell instead of the stock `Settings` component:
//! an immersive title-bar-less layout (the sidebar extends behind the
//! traffic lights, exactly like the main window), a flat single-level nav
//! on the left, and plain rows — no cards — on the right.  Field controls
//! reuse gpui-component widgets (buttons, popup menus, hover cards).

mod about;
mod appearance;
mod general;
mod ui;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Icon, IconName, Root, TITLE_BAR_HEIGHT, TitleBar, h_flex, v_flex,
};
use rust_i18n::t;

/// Preferred size of the settings window; clamped to 85% of the display.
const PREFERRED_SIZE: Size<Pixels> = Size {
    width: px(820.),
    height: px(560.),
};

/// Minimum size that still fits the nav plus a readable content column.
const MIN_SIZE: Size<Pixels> = Size {
    width: px(640.),
    height: px(420.),
};

/// Width of the left navigation rail.
const NAV_WIDTH: Pixels = px(200.);

/// Left-pad so nav content sits right of the macOS traffic lights.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_PAD: Pixels = px(80.);
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_PAD: Pixels = px(12.);

/// Handle of the currently open settings window.  `None` when closed.
#[derive(Default)]
struct SettingsWindowHandle(Option<WindowHandle<Root>>);

impl Global for SettingsWindowHandle {}

/// Open the settings window, or bring the existing one to the front.
pub fn open(cx: &mut App) {
    if let Some(handle) = cx
        .try_global::<SettingsWindowHandle>()
        .and_then(|state| state.0)
    {
        // `update` fails once the window has been closed; fall through and
        // open a fresh one in that case.
        let activated = handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok();
        if activated {
            return;
        }
    }

    let bounds = Bounds::centered(None, clamped_size(cx), cx);
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

    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsWindow::new(window, cx));

        // Focus the root so global keybindings (quit, toggle theme, …)
        // dispatch while this window is frontmost — same pattern as the
        // main window.
        let focus_handle = view.focus_handle(cx);
        window.defer(cx, move |window, cx| {
            if window.focused(cx).is_none() {
                focus_handle.focus(window, cx);
            }
        });

        cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
    }) {
        Ok(handle) => {
            handle
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    window.set_window_title(&t!("settings.title"));
                    // Clear the singleton when the window closes so the next
                    // OpenSettings opens a fresh one.
                    cx.on_release(|_, cx| {
                        cx.global_mut::<SettingsWindowHandle>().0 = None;
                    })
                    .detach();
                })
                .ok();
            cx.set_global(SettingsWindowHandle(Some(handle)));
        }
        Err(e) => eprintln!("failed to open settings window: {e:?}"),
    }
}

/// Re-translate the native window title after a language switch.  The
/// in-window strings update on their own (resolved in render), but the
/// OS-level title is a one-shot string.
pub fn refresh_native_title(cx: &mut App) {
    if let Some(handle) = cx
        .try_global::<SettingsWindowHandle>()
        .and_then(|state| state.0)
    {
        handle
            .update(cx, |_, window, _| {
                window.set_window_title(&t!("settings.title"));
            })
            .ok();
    }
}

fn clamped_size(cx: &App) -> Size<Pixels> {
    let mut size = PREFERRED_SIZE;
    if let Some(display) = cx.primary_display() {
        let ds = display.bounds().size;
        size.width = size.width.min(ds.width * 0.85);
        size.height = size.height.min(ds.height * 0.85);
    }
    size
}

/// The settings pages reachable from the left nav.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Appearance,
    About,
}

impl Page {
    const ALL: [Page; 3] = [Page::General, Page::Appearance, Page::About];

    fn title(self) -> String {
        match self {
            Page::General => t!("settings.page.general").to_string(),
            Page::Appearance => t!("settings.page.appearance").to_string(),
            Page::About => t!("settings.page.about").to_string(),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::General => IconName::Settings2,
            Page::Appearance => IconName::Palette,
            Page::About => IconName::Info,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Page::General => "nav-general",
            Page::Appearance => "nav-appearance",
            Page::About => "nav-about",
        }
    }
}

/// Root view of the settings window.
struct SettingsWindow {
    focus_handle: FocusHandle,
    active: Page,
}

impl SettingsWindow {
    fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            active: Page::General,
        }
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items = Page::ALL.map(|page| {
            let is_active = page == self.active;
            h_flex()
                .id(page.id())
                .h(px(30.))
                .px_2()
                .gap_2()
                .items_center()
                .rounded(cx.theme().radius)
                .text_sm()
                .text_color(cx.theme().sidebar_foreground)
                .cursor_pointer()
                .when(is_active, |this| {
                    this.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .hover(|this| {
                    this.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.active != page {
                        this.active = page;
                        cx.notify();
                    }
                }))
                .child(
                    Icon::new(page.icon())
                        .size_4()
                        .text_color(cx.theme().sidebar_foreground.opacity(0.8)),
                )
                .child(page.title())
        });

        v_flex()
            .w(NAV_WIDTH)
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            // Immersive: the rail's background extends behind the traffic
            // lights; the first item starts below them.
            .child(
                div()
                    .h(TITLE_BAR_HEIGHT)
                    .flex_shrink_0()
                    .pl(TRAFFIC_LIGHT_PAD),
            )
            .child(v_flex().px_2().pt_2().gap_1().children(items))
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.active {
            Page::General => general::render(cx),
            Page::Appearance => appearance::render(cx),
            Page::About => about::render(cx),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            // Keep the top strip empty: it doubles as the native drag region
            // of the transparent title bar.
            .child(div().h(TITLE_BAR_HEIGHT).flex_shrink_0())
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_10()
                    // No top padding: the first row's own `py_3` lands its
                    // label level with the nav's first item (`pt_2` + 30px
                    // box), keeping both columns' tops flush.
                    .pb_4()
                    .child(body),
            )
    }
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Overlay layers must render inside the window's top-level view for
        // popovers, dialogs, and notifications to appear.
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        div()
            .id("settings-window")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .size_full()
                    .items_stretch()
                    .child(self.render_nav(cx))
                    .child(self.render_content(cx)),
            )
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}
