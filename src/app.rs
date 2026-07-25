//! Root `ChatApp` view: hosts conversations, top bar(s), and the fixed sidebar.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Icon, IconName, Root, Sizable as _, StyledExt as _, TITLE_BAR_HEIGHT,
    animation::{Transition, ease_in_out_cubic},
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    sidebar::SidebarToggleButton,
    v_flex,
};

use crate::actions::{NewChat, ToggleSidebar, ToggleTheme};
use crate::chat::ChatView;
use crate::preferences::{self, Preferences};

/// Minimum sidebar width when the user drags the right edge inward.
const SIDEBAR_MIN_WIDTH: Pixels = px(220.);
/// Maximum sidebar width when the user drags the right edge outward.
const SIDEBAR_MAX_WIDTH: Pixels = px(440.);
/// Width of the invisible drag hit-area on the sidebar's right edge.
const RESIZE_HANDLE_WIDTH: Pixels = px(6.);

/// Duration of the sidebar collapse/expand animation.
const SIDEBAR_ANIM: Duration = Duration::from_millis(220);

/// x-coordinate where the main column's floating content (model pill) should
/// start when the sidebar is fully collapsed.  Equals the width of the fixed
/// top-left overlay (traffic-light padding + toggle + new-chat button + gap).
const OVERLAY_INSET: Pixels = px(148.);

/// Left-pad so content sits to the right of the macOS traffic lights (x=9..77).
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_PAD: Pixels = px(80.);
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_PAD: Pixels = px(12.);

pub struct ChatApp {
    focus_handle: FocusHandle,
    conversations: Vec<Entity<ChatView>>,
    active: usize,
    collapsed: bool,
    /// True once the user has toggled the sidebar at least once.  Prevents an
    /// unwanted slide-in on the very first render.
    has_toggled: bool,
    /// Current sidebar width in the expanded state.  Users can drag the
    /// sidebar's right edge to change this within `[SIDEBAR_MIN_WIDTH,
    /// SIDEBAR_MAX_WIDTH]`.  Window resizes don't touch it.
    sidebar_width: Pixels,
    /// (mouse_x, sidebar_width) captured on `mouse_down` on the resize handle.
    /// While `Some`, window-level mouse move events adjust `sidebar_width`.
    resize_start: Option<(Pixels, Pixels)>,
    _subscriptions: Vec<Subscription>,
}

impl ChatApp {
    /// Build the root app view from persisted preferences.  Sidebar width is
    /// clamped into the allowed range, and a save-on-quit hook is registered
    /// so the current UI state survives across restarts.
    pub fn new(prefs: Preferences, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar_width = px(prefs.sidebar_width)
            .max(SIDEBAR_MIN_WIDTH)
            .min(SIDEBAR_MAX_WIDTH);

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            conversations: Vec::new(),
            active: 0,
            collapsed: prefs.sidebar_collapsed,
            has_toggled: false,
            sidebar_width,
            resize_start: None,
            _subscriptions: Vec::new(),
        };
        this.spawn_conversation(window, cx);
        this.register_save_on_quit(cx);
        this
    }

    /// Persist current preferences (sidebar width, collapse state, theme mode)
    /// when the app quits.  File I/O happens on the background executor so it
    /// doesn't stall shutdown.
    fn register_save_on_quit(&self, cx: &mut Context<Self>) {
        cx.on_app_quit(|this, cx| {
            let snapshot = this.snapshot_preferences(cx);
            cx.background_executor().spawn(async move {
                if let Err(e) = preferences::save(&snapshot) {
                    eprintln!("failed to save preferences: {e:?}");
                }
            })
        })
        .detach();
    }

    fn snapshot_preferences(&self, cx: &App) -> Preferences {
        let theme_mode = if cx.theme().mode.is_dark() {
            preferences::ThemeMode::Dark
        } else {
            preferences::ThemeMode::Light
        };
        Preferences {
            sidebar_width: self.sidebar_width.as_f32(),
            sidebar_collapsed: self.collapsed,
            theme_mode: Some(theme_mode),
        }
    }

    fn spawn_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = ChatView::view("New chat", window, cx);
        let sub = cx.observe(&view, |_, _, cx| cx.notify());
        self.conversations.push(view);
        self._subscriptions.push(sub);
        self.active = self.conversations.len() - 1;
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.conversations.len() && ix != self.active {
            self.active = ix;
            cx.notify();
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        self.has_toggled = true;
        cx.notify();
    }

    fn on_new_chat(&mut self, _: &NewChat, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_conversation(window, cx);
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_sidebar(cx);
    }

    /// Vertical drag hit-area at the sidebar's right edge.  Uses gpui's
    /// native drag session (`on_drag` + `on_drag_move`) so the drag keeps
    /// tracking the cursor even if it leaves the narrow hit-area.
    fn render_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-resize-handle")
            .absolute()
            .top_0()
            .right(-(RESIZE_HANDLE_WIDTH * 0.5))
            .w(RESIZE_HANDLE_WIDTH)
            .h_full()
            .cursor_col_resize()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.resize_start = Some((ev.position.x, this.sidebar_width));
                    cx.notify();
                }),
            )
            .on_drag(SidebarResize, |_, _, _, cx| cx.new(|_| EmptyView))
            .on_drag_move(
                cx.listener(|this, ev: &DragMoveEvent<SidebarResize>, _, cx| {
                    let Some((start_x, start_width)) = this.resize_start else {
                        return;
                    };
                    let delta = ev.event.position.x - start_x;
                    let clamped = (start_width + delta)
                        .max(SIDEBAR_MIN_WIDTH)
                        .min(SIDEBAR_MAX_WIDTH);
                    if clamped != this.sidebar_width {
                        this.sidebar_width = clamped;
                        cx.notify();
                    }
                }),
            )
    }

    // ---------- Sidebar rendering ----------

    fn render_sidebar_panel(&self, active: usize, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .child(self.render_sidebar_top_row(cx))
            .child(self.render_sidebar_content(active, cx))
            .child(self.render_sidebar_footer(cx))
            .into_any_element()
    }

    /// Reserved space at the top of the sidebar column so the sidebar's
    /// background extends behind the traffic lights and the fixed overlay.
    /// The interactive buttons themselves live in the app-level overlay so
    /// they don't move when the sidebar collapses.
    fn render_sidebar_top_row(&self, _: &mut Context<Self>) -> impl IntoElement {
        div().h(TITLE_BAR_HEIGHT).flex_shrink_0()
    }

    fn render_sidebar_content(&self, active: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let items = self.conversations.iter().enumerate().map(|(i, view)| {
            let title = view.read(cx).title().clone();
            let is_active = i == active;
            div()
                .id(("conv", i))
                .flex_shrink_0()
                .h(px(32.))
                .px_2()
                .flex()
                .items_center()
                .rounded(cx.theme().radius)
                .text_sm()
                .text_color(cx.theme().sidebar_foreground)
                .cursor_pointer()
                .overflow_hidden()
                .whitespace_nowrap()
                .when(is_active, |this| {
                    this.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .hover(|this| {
                    this.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select(i, cx)))
                .child(div().overflow_hidden().text_ellipsis().child(title))
        });

        v_flex()
            .id("chats")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .pt_2()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(cx.theme().sidebar_foreground.opacity(0.6))
                    .child("Chats"),
            )
            .children(items)
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();

        let account = Button::new("account")
            .ghost()
            .compact()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size_6()
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .text_xs()
                            .font_semibold()
                            .child("Y"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .text_color(cx.theme().sidebar_foreground)
                            .child("yuewei"),
                    ),
            )
            .dropdown_menu_with_anchor(gpui::Anchor::BottomLeft, move |menu, _, _| {
                let label = if is_dark {
                    "Switch to light mode"
                } else {
                    "Switch to dark mode"
                };
                menu.menu(label, Box::new(ToggleTheme))
            });

        h_flex()
            .items_center()
            .gap_2()
            .h(px(52.))
            .flex_shrink_0()
            .px_2()
            .child(account)
            .child(div().flex_1())
            .child(
                Button::new("search")
                    .ghost()
                    .small()
                    .icon(IconName::Search)
                    .tooltip("Search chats"),
            )
    }
}

impl Focusable for ChatApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ChatApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        let active_view = self.conversations.get(active).cloned();
        let active_title = active_view
            .as_ref()
            .map(|v| v.read(cx).title().clone())
            .unwrap_or_else(|| "Chat".into());

        // Root overlays (sheets, dialogs, notifications) must be rendered
        // inside the top-level view of the window.
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        // ---------- Sidebar column (animated width) ----------
        //
        // Inner panel is always laid out at `self.sidebar_width` so its
        // contents don't reflow while collapsing — the wrapper simply clips.
        // The right-edge drag handle sits inside the inner div too, so it's
        // clipped along with the sidebar during the collapse animation.
        let sidebar_width = self.sidebar_width;
        let sidebar_inner = div()
            .w(sidebar_width)
            .h_full()
            .relative()
            .child(self.render_sidebar_panel(active, cx))
            .child(self.render_resize_handle(cx));

        let (from_w, to_w) = if self.collapsed {
            (sidebar_width, px(0.))
        } else {
            (px(0.), sidebar_width)
        };

        let sidebar_wrapper = div()
            .h_full()
            .flex_shrink_0()
            .overflow_hidden()
            .child(sidebar_inner);

        let sidebar_column: AnyElement = if self.has_toggled {
            Transition::new(SIDEBAR_ANIM)
                .ease(ease_in_out_cubic)
                .width(from_w, to_w)
                .apply(
                    sidebar_wrapper,
                    ElementId::NamedInteger("sidebar-anim".into(), self.collapsed as u64),
                )
                .into_any_element()
        } else {
            sidebar_wrapper.w(to_w).into_any_element()
        };

        // ---------- Main column ----------
        //
        // Just a top-bar spacer and the chat body.  The floating widgets on
        // top of the window (overlay buttons, model pill) are rendered
        // separately as absolute elements so they don't shift with the layout.
        let main_column = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            .child(div().h(TITLE_BAR_HEIGHT).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when_some(active_view, |this, view| this.child(view)),
            );

        // ---------- Fixed top-left overlay: never moves ----------
        //
        // Sidebar-toggle and New-chat live here so they stay put no matter
        // whether the sidebar is showing, hiding, or mid-animation.
        let overlay = h_flex()
            .absolute()
            .top_0()
            .left_0()
            .h(TITLE_BAR_HEIGHT)
            .pl(TRAFFIC_LIGHT_PAD)
            .pr(px(6.))
            .items_center()
            .gap_1()
            .child(
                SidebarToggleButton::new()
                    .collapsed(self.collapsed)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .child(
                Button::new("new-chat")
                    .ghost()
                    .small()
                    .icon(Icon::default().path("icons/square-pen.svg"))
                    .tooltip("New chat")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.spawn_conversation(window, cx)),
                    ),
            );

        // ---------- Floating model pill (animated left position) ----------
        //
        // Left position animates in sync with the sidebar width so the pill
        // stays visually attached to the main column's top-left corner while
        // the sidebar opens or closes.  Expanded: pill sits just right of the
        // sidebar.  Collapsed: pill sits just right of the fixed overlay.
        let expanded_left = sidebar_width + px(6.);
        let (pill_from, pill_to) = if self.collapsed {
            (expanded_left, OVERLAY_INSET)
        } else {
            (OVERLAY_INSET, expanded_left)
        };
        let pill_target_left = if self.collapsed {
            OVERLAY_INSET
        } else {
            expanded_left
        };

        let pill_wrapper = div()
            .absolute()
            .top_0()
            .h(TITLE_BAR_HEIGHT)
            .flex()
            .items_center()
            .child(model_pill(active_title, cx));

        let pill_element: AnyElement = if self.has_toggled {
            Transition::new(SIDEBAR_ANIM)
                .ease(ease_in_out_cubic)
                .slide_x(pill_from, pill_to)
                .apply(
                    pill_wrapper,
                    ElementId::NamedInteger("pill-anim".into(), self.collapsed as u64),
                )
                .into_any_element()
        } else {
            pill_wrapper.left(pill_target_left).into_any_element()
        };

        div()
            .id("nostra-app")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .on_action(cx.listener(Self::on_new_chat))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .child(
                h_flex()
                    .size_full()
                    .child(sidebar_column)
                    .child(main_column),
            )
            .child(pill_element)
            .child(overlay)
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}

fn model_pill(title: SharedString, cx: &App) -> impl IntoElement {
    h_flex()
        .id("model-pill")
        .items_center()
        .gap_1()
        .px_2()
        .h(px(26.))
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .hover(|this| this.bg(cx.theme().accent))
        .child(div().text_sm().font_medium().child(title))
        .child(
            Icon::new(IconName::ChevronDown)
                .size_3()
                .text_color(cx.theme().muted_foreground),
        )
}

/// Zero-sized marker used as the payload type of the sidebar-resize drag
/// session.  The presence of `DragMoveEvent<SidebarResize>` is what tells the
/// handler that this drag is ours.
#[derive(Clone)]
struct SidebarResize;
