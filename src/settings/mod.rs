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
mod logs;
mod providers;
mod ui;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, ElementId, Entity, FocusHandle, Focusable, Global,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Pixels, Render, Role,
    Size, StatefulInteractiveElement as _, Styled as _, Subscription, Window, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, div, px,
};
#[cfg(target_os = "macos")]
use gpui_component::slider::{SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme, Icon, IconName, Root, TITLE_BAR_HEIGHT, TitleBar, h_flex, v_flex,
};
use rust_i18n::t;

use crate::appearance::glass;
use crate::preferences::{self, PreferenceHandle, WindowGeometry};
use crate::shell::window;
use crate::ui::consume_button_key;

/// Preferred size of the settings window; clamped to 85% of the display.
const PREFERRED_SIZE: Size<Pixels> = Size {
    width: px(1040.),
    height: px(680.),
};

/// Minimum size that still fits the nav plus a readable content column.
const MIN_SIZE: Size<Pixels> = Size {
    width: px(640.),
    height: px(420.),
};

/// Width of the left navigation rail.
const NAV_WIDTH: Pixels = px(200.);
/// Height of a single selectable row.  Shared by the nav items and the
/// providers list so the two columns' first rows sit on the same baseline;
/// the providers form aligns its first label to the same box.
pub(super) const ROW_HEIGHT: Pixels = px(30.);
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

    let preference_handle = preferences::handle(cx);
    let saved_preferences = preference_handle.snapshot();
    let bounds = window::restored_bounds(
        saved_preferences.settings_window,
        PREFERRED_SIZE,
        MIN_SIZE,
        cx,
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitleBar::title_bar_options()),
        window_min_size: Some(MIN_SIZE),
        kind: WindowKind::Normal,
        #[cfg(target_os = "macos")]
        window_background: glass::window_background(saved_preferences.glass_effect),
        #[cfg(target_os = "linux")]
        window_background: WindowBackgroundAppearance::Transparent,
        #[cfg(target_os = "linux")]
        window_decorations: Some(WindowDecorations::Client),
        ..Default::default()
    };

    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsWindow::new(window, cx, preference_handle.clone()));

        // Focus the root so global keybindings (quit, toggle theme, …)
        // dispatch while this window is frontmost — same pattern as the
        // main window.
        let focus_handle = view.focus_handle(cx);
        window.defer(cx, move |window, cx| {
            if window.focused(cx).is_none() {
                focus_handle.focus(window, cx);
            }
        });

        cx.new(|cx| Root::new(view, window, cx).bg(glass::root_background(cx.theme().background)))
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
        Err(e) => crate::logging::error(
            "settings.window",
            format_args!("failed to open settings window: {e:?}"),
        ),
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

/// The settings pages reachable from the left nav.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Appearance,
    Providers,
    Logs,
    About,
}

impl Page {
    const ALL: [Page; 5] = [
        Page::General,
        Page::Appearance,
        Page::Providers,
        Page::Logs,
        Page::About,
    ];

    fn title(self) -> String {
        match self {
            Page::General => t!("settings.page.general").to_string(),
            Page::Appearance => t!("settings.page.appearance").to_string(),
            Page::Providers => t!("settings.page.providers").to_string(),
            Page::Logs => t!("settings.page.logs").to_string(),
            Page::About => t!("settings.page.about").to_string(),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::General => IconName::Settings2,
            Page::Appearance => IconName::Palette,
            Page::Providers => IconName::Globe,
            Page::Logs => IconName::FileText,
            Page::About => IconName::Info,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Page::General => "nav-general",
            Page::Appearance => "nav-appearance",
            Page::Providers => "nav-providers",
            Page::Logs => "nav-logs",
            Page::About => "nav-about",
        }
    }
}

/// Root view of the settings window.
struct SettingsWindow {
    focus_handle: FocusHandle,
    active: Page,
    providers: Entity<providers::ProvidersPage>,
    logs: Entity<logs::LogsPage>,
    #[cfg(target_os = "macos")]
    glass_opacity: Entity<SliderState>,
    window_geometry: WindowGeometry,
    preference_handle: PreferenceHandle,
    preference_snapshot: preferences::Preferences,
    _subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        preference_handle: PreferenceHandle,
    ) -> Self {
        #[cfg(target_os = "macos")]
        let glass_opacity = {
            let initial_opacity =
                glass::tint_opacity(preference_handle.snapshot().glass_tint_opacity, cx) * 100.;
            cx.new(|_| {
                SliderState::new()
                    .min(glass::MIN_TINT_PERCENT)
                    .max(glass::MAX_TINT_PERCENT)
                    .step(1.)
                    .default_value(initial_opacity)
            })
        };
        let bounds_subscription = cx.observe_window_bounds(window, |this, window, _| {
            this.window_geometry = WindowGeometry::from_window(window);
        });
        let preferences_for_observer = preference_handle.clone();
        let preference_subscription =
            cx.observe_global_in::<preferences::Prefs>(window, move |this, _, cx| {
                let snapshot = preferences_for_observer.snapshot();
                if this.preference_snapshot == snapshot {
                    return;
                }
                this.preference_snapshot = snapshot;
                cx.notify();
            });
        let preferences_for_release = preference_handle.clone();
        cx.on_release(move |this, cx| {
            let geometry = this.window_geometry;
            preferences::update_with(cx, &preferences_for_release, |prefs| {
                prefs.settings_window = Some(geometry)
            });
            #[cfg(target_os = "macos")]
            glass::commit_tint_preview(&preferences_for_release, cx);
        })
        .detach();

        let mut subscriptions = vec![bounds_subscription];
        #[cfg(target_os = "macos")]
        let preference_handle_for_slider = preference_handle.clone();
        subscriptions.push(cx.subscribe_in(
            &glass_opacity,
            window,
            move |_, _, event: &SliderEvent, _, cx| match event {
                SliderEvent::Change(value) => {
                    glass::preview_tint_opacity(value.start() / 100., cx);
                }
                SliderEvent::Release(value) => {
                    glass::persist_tint_opacity(
                        value.start() / 100.,
                        &preference_handle_for_slider,
                        cx,
                    );
                }
            },
        ));

        let preference_snapshot = preference_handle.snapshot();
        Self {
            focus_handle: cx.focus_handle(),
            active: Page::General,
            providers: cx
                .new(|cx| providers::ProvidersPage::new(preference_handle.clone(), window, cx)),
            logs: cx.new(|cx| logs::LogsPage::new(window, cx)),
            #[cfg(target_os = "macos")]
            glass_opacity,
            window_geometry: WindowGeometry::from_window(window),
            preference_handle,
            preference_snapshot,
            _subscriptions: {
                subscriptions.push(preference_subscription);
                subscriptions
            },
        }
    }

    fn set_active(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.active == page {
            return;
        }
        if self.active == Page::Logs {
            self.logs.update(cx, |logs, cx| logs.set_visible(false, cx));
        }
        self.active = page;
        if page == Page::Logs {
            self.logs.update(cx, |logs, cx| logs.set_visible(true, cx));
        }
        cx.notify();
    }

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = Page::ALL.map(|page| {
            let is_active = page == self.active;
            let title = page.title();
            let id: ElementId = page.id().into();
            let focus_handle = window
                .use_keyed_state(id.clone(), cx, |_, cx| cx.focus_handle())
                .read(cx)
                .clone();
            let focus_ring = cx.theme().ring.opacity(0.2);

            h_flex()
                .id(id)
                .role(Role::Button)
                .aria_label(title.clone())
                .aria_selected(is_active)
                .track_focus(&focus_handle.tab_stop(true))
                .focus_visible(|this| this.border_1().border_color(focus_ring))
                .h(ROW_HEIGHT)
                .px_2()
                .gap_2()
                .items_center()
                .rounded(cx.theme().radius)
                .text_sm()
                .text_color(cx.theme().sidebar_foreground)
                .cursor_default()
                .when(is_active, |this| {
                    this.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .hover(|this| {
                    this.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if consume_button_key(event, window, cx) {
                        this.set_active(page, cx);
                    }
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_active(page, cx);
                }))
                .child(
                    Icon::new(page.icon())
                        .size_4()
                        .text_color(cx.theme().sidebar_foreground.opacity(0.8)),
                )
                .child(title)
        });

        v_flex()
            .w(NAV_WIDTH)
            .h_full()
            .flex_shrink_0()
            .bg(glass::background(
                cx.theme().sidebar,
                self.preference_snapshot.glass_effect,
                self.preference_snapshot.glass_tint_opacity,
                cx,
            ))
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
            Page::General => {
                general::render(cx, &self.preference_handle, &self.preference_snapshot)
            }
            Page::Appearance => appearance::render(
                cx,
                &self.preference_handle,
                &self.preference_snapshot,
                #[cfg(target_os = "macos")]
                &self.glass_opacity,
            ),
            Page::Providers => self.providers.clone().into_any_element(),
            Page::Logs => self.logs.clone().into_any_element(),
            Page::About => about::render(cx),
        };

        // The providers page is a split view whose two columns scroll
        // independently, so it takes the content box at full height and owns
        // its own scrolling. The logs page owns a readonly editor the same way.
        // Every other page is a plain column inside one shared scroll view.
        let immersive = matches!(self.active, Page::Providers | Page::Logs);
        let content = match self.active {
            Page::Providers => div()
                .flex_1()
                .min_h_0()
                // Left inset matches the gap the list keeps on its own right
                // edge, so the column sits evenly between nav and divider.
                // The columns supply the rest of their padding, and the detail
                // column's scrollbar rides the window edge.
                .pl_3()
                .child(body)
                .into_any_element(),
            Page::Logs => div().flex_1().min_h_0().child(body).into_any_element(),
            _ => div()
                .id("settings-content")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_10()
                // No top padding: the first row's own `py_3` lands its
                // label level with the nav's first item (`pt_2` + 30px
                // box), keeping both columns' tops flush.
                .pb_4()
                .child(body)
                .into_any_element(),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            // Keep the top strip empty: it doubles as the native drag region
            // of the transparent title bar.  The providers split reserves the
            // same strip inside each of its columns instead, so its divider
            // can run the full height of the window.
            .when(!immersive, |this| {
                this.child(div().h(TITLE_BAR_HEIGHT).flex_shrink_0())
            })
            .child(content)
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
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .size_full()
                    .items_stretch()
                    .child(self.render_nav(window, cx))
                    .child(self.render_content(cx)),
            )
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}
