//! Root `ChatApp` view: hosts conversations, top bar(s), and the fixed sidebar.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Icon, IconName, InteractiveElementExt as _, Root, Sizable as _, StyledExt as _,
    TITLE_BAR_HEIGHT,
    animation::{Transition, ease_in_out_cubic},
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    sidebar::SidebarToggleButton,
    v_flex,
};
use rust_i18n::t;

use crate::actions::{OpenSettings, ToggleTheme};
use crate::chat::{ChatEvent, ChatView};
use crate::llm::ModelSelection;
use crate::model_select::ModelPicker;
use crate::preferences::{self, Preferences, WindowGeometry};
use crate::{glass, theme, ui};

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
    conversations: Vec<Conversation>,
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
    /// Set on pointer-down in the empty titlebar layer. The first move hands
    /// the gesture to the platform and clears this flag.
    titlebar_move_pending: bool,
    /// Latest main-window restore bounds, kept fresh by a window-bounds
    /// observer and persisted on quit.
    window_geometry: Option<WindowGeometry>,
    model_picker: Entity<ModelPicker>,
    _subscriptions: Vec<Subscription>,
}

struct Conversation {
    view: Entity<ChatView>,
    title: SharedString,
    selection: Option<ModelSelection>,
}

impl ChatApp {
    /// Build the root app view from persisted preferences.  Sidebar width is
    /// clamped into the allowed range, and a save-on-quit hook is registered
    /// so the current UI state survives across restarts.
    pub fn new(prefs: Preferences, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar_width = px(prefs.sidebar_width)
            .max(SIDEBAR_MIN_WIDTH)
            .min(SIDEBAR_MAX_WIDTH);

        let parent = cx.weak_entity();
        let model_picker = cx.new(|cx| {
            ModelPicker::new(
                prefs.last_model_selection.clone(),
                move |selection, cx| {
                    parent
                        .update(cx, |app, cx| app.select_model_from_picker(selection, cx))
                        .unwrap_or(false)
                },
                window,
                cx,
            )
        });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            conversations: Vec::new(),
            active: 0,
            collapsed: prefs.sidebar_collapsed,
            has_toggled: false,
            sidebar_width,
            resize_start: None,
            titlebar_move_pending: false,
            window_geometry: Some(WindowGeometry::from_window(window)),
            model_picker,
            _subscriptions: Vec::new(),
        };
        this.track_window_geometry(window, cx);
        this.track_system_appearance(window, cx);
        this.spawn_conversation(window, cx);
        this.register_save_on_quit(cx);
        this
    }

    /// Keep `window_geometry` current across moves and resizes.  The
    /// observer fires for both, because the platform window reports either
    /// as a bounds change.
    fn track_window_geometry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.observe_window_bounds(window, |this, window, _| {
            this.window_geometry = Some(WindowGeometry::from_window(window));
        });
        self._subscriptions.push(sub);
    }

    /// Keep a "follow system" theme live after startup. The subscription is
    /// window-scoped and drops with the root view.
    fn track_system_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.observe_window_appearance(window, |_, window, cx| {
            theme::sync_system_appearance(window, cx);
        });
        self._subscriptions.push(sub);
    }

    /// Persist current preferences (sidebar state, window geometry) when the
    /// app quits.  Settings-window changes are already live in the `Prefs`
    /// global, so folding in the exit-only state completes the snapshot.
    /// File I/O happens on the background executor so it doesn't stall
    /// shutdown; gpui awaits the returned task before exiting.
    fn register_save_on_quit(&self, cx: &mut Context<Self>) {
        cx.on_app_quit(|this, cx| {
            let sidebar_width = this.sidebar_width.as_f32();
            let sidebar_collapsed = this.collapsed;
            let window = this.window_geometry;
            let snapshot = preferences::snapshot_with(cx, |prefs| {
                prefs.sidebar_width = sidebar_width;
                prefs.sidebar_collapsed = sidebar_collapsed;
                prefs.window = window;
            });
            cx.background_executor().spawn(async move {
                if let Err(e) = preferences::save(&snapshot) {
                    eprintln!("failed to save preferences: {e:?}");
                }
            })
        })
        .detach();
    }

    fn spawn_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        let title: SharedString = t!("chat.default_title").to_string().into();
        let view = ChatView::view(window, cx);
        let sub = cx.subscribe_in(&view, window, |this, view, event, window, cx| {
            let Some(index) = this
                .conversations
                .iter()
                .position(|conversation| conversation.view.entity_id() == view.entity_id())
            else {
                return;
            };
            let conversation = &mut this.conversations[index];
            match event {
                ChatEvent::TitleChanged(title) => {
                    if conversation.title != *title {
                        conversation.title = title.clone();
                        cx.notify();
                    }
                }
                ChatEvent::SelectionChanged(selection) => {
                    if conversation.selection.as_ref() != Some(selection) {
                        conversation.selection = Some(selection.clone());
                        if index == this.active {
                            this.sync_model_picker_to_active(window, cx);
                        }
                        cx.notify();
                    }
                }
            }
        });
        let selection = view.read(cx).selection();
        self.conversations.push(Conversation {
            view,
            title,
            selection,
        });
        self._subscriptions.push(sub);
        self.active = self.conversations.len() - 1;
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.conversations.len() && ix != self.active {
            self.model_picker
                .update(cx, |picker, cx| picker.dismiss(window, cx));
            self.active = ix;
            self.sync_model_picker_to_active(window, cx);
            cx.notify();
        }
    }

    fn sync_model_picker_to_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversations.get(self.active) else {
            return;
        };
        let selection = conversation.selection.clone();
        self.model_picker.update(cx, |picker, cx| {
            picker.set_conversation(selection, window, cx)
        });
    }

    fn select_model_from_picker(
        &mut self,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(conversation) = self.conversations.get(self.active) else {
            return false;
        };
        let view = conversation.view.clone();
        view.update(cx, |chat, cx| chat.select_model(selection, cx));
        true
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        self.has_toggled = true;
        cx.notify();
    }

    pub(crate) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_conversation(window, cx);
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

    fn render_sidebar_panel(
        &self,
        active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .bg(glass::background(cx.theme().sidebar, cx))
            .text_color(cx.theme().sidebar_foreground)
            .child(self.render_sidebar_top_row(cx))
            .child(self.render_sidebar_content(active, window, cx))
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

    fn render_sidebar_content(
        &self,
        active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let items = self
            .conversations
            .iter()
            .enumerate()
            .map(|(i, conversation)| {
                let title = conversation.title.clone();
                let is_active = i == active;
                let id = ElementId::NamedInteger("conv".into(), i as u64);
                let focus_handle = window
                    .use_keyed_state(id.clone(), cx, |_, cx| cx.focus_handle())
                    .read(cx)
                    .clone();
                let focus_ring = cx.theme().ring.opacity(0.2);

                div()
                    .id(id)
                    .role(Role::Button)
                    .aria_label(title.clone())
                    .aria_selected(is_active)
                    .track_focus(&focus_handle.tab_stop(true))
                    .focus_visible(|this| this.border_1().border_color(focus_ring))
                    .flex_shrink_0()
                    .h(px(32.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded(cx.theme().radius)
                    .text_sm()
                    .text_color(cx.theme().sidebar_foreground)
                    .cursor_default()
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
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if ui::consume_button_key(event, window, cx) {
                            this.select(i, window, cx);
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.select(i, window, cx)))
                    .child(div().overflow_hidden().text_ellipsis().child(title))
            })
            .collect::<Vec<_>>();

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
                    .child(t!("sidebar.chats").to_string()),
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
                let (theme_label, theme_icon) = if is_dark {
                    (t!("account.switch_to_light"), IconName::Sun)
                } else {
                    (t!("account.switch_to_dark"), IconName::Moon)
                };
                menu.menu_with_icon(
                    t!("account.settings").to_string(),
                    IconName::Settings,
                    Box::new(OpenSettings),
                )
                .menu_with_icon(
                    theme_label.to_string(),
                    theme_icon,
                    Box::new(ToggleTheme),
                )
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
                    .tooltip(t!("sidebar.search").to_string()),
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
        let active_view = self
            .conversations
            .get(active)
            .map(|conversation| conversation.view.clone());
        let has_active = active_view.is_some();

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
            .child(self.render_sidebar_panel(active, window, cx))
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
        // Keep the title row in normal layout flow with an opaque background.
        // The model pill is positioned over this reserved row, while message
        // content and its scrollbar remain entirely below it.
        let main_column = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            .child(
                div()
                    .h(TITLE_BAR_HEIGHT)
                    .flex_shrink_0()
                    .bg(cx.theme().background),
            )
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
            .occlude()
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
                    .tooltip(t!("sidebar.new_chat").to_string())
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
            .occlude()
            .when(has_active, |this| this.child(self.model_picker.clone()));

        // AppKit otherwise treats every point in a transparent native
        // titlebar as draggable, including controls. This layer sits behind
        // the controls, so only genuinely empty titlebar space starts a move.
        let titlebar_drag_area = div()
            .id("main-titlebar-drag-area")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(TITLE_BAR_HEIGHT)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_move_pending = true),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.titlebar_move_pending = false;
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_move_pending = false),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.titlebar_move_pending {
                    this.titlebar_move_pending = false;
                    window.start_window_move();
                }
            }))
            .when(cfg!(target_os = "macos"), |this| {
                this.on_double_click(|_, window, _| window.titlebar_double_click())
            })
            .when(cfg!(target_os = "linux"), |this| {
                this.on_double_click(|_, window, _| window.zoom_window())
            });

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
            .child(
                h_flex()
                    .size_full()
                    .child(sidebar_column)
                    .child(main_column),
            )
            .child(titlebar_drag_area)
            .child(pill_element)
            .child(overlay)
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}

/// Zero-sized marker used as the payload type of the sidebar-resize drag
/// session.  The presence of `DragMoveEvent<SidebarResize>` is what tells the
/// handler that this drag is ours.
#[derive(Clone)]
struct SidebarResize;
