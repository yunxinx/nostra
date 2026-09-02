//! Root layout and sidebar rendering for the application shell.

use super::*;

impl ChatApp {
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

    fn render_sidebar_panel(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .bg(glass::background(
                contrast::sidebar_surface(cx),
                self.preference_snapshot.glass_effect,
                self.preference_snapshot.glass_tint_opacity,
                cx,
            ))
            .text_color(contrast::sidebar_text(cx))
            .child(self.render_sidebar_top_row(cx))
            .child(self.render_sidebar_content(window, cx))
            .child(self.render_sidebar_footer(cx))
            .into_any_element()
    }

    /// Reserved space at the top of the sidebar column so the sidebar's
    /// background extends behind the traffic lights and the fixed overlay.
    /// The interactive buttons themselves live in the app-level overlay so
    /// they don't move when the sidebar collapses.
    fn render_sidebar_top_row(&self, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "sidebar-top-reserved".to_string())
            .h(TITLE_BAR_HEIGHT)
            .flex_shrink_0()
    }

    fn render_sidebar_content(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .px(SIDEBAR_CONTENT_INSET)
            .pt(SIDEBAR_CONTENT_INSET)
            .child(match self.workspace_id {
                CHAT_WORKSPACE_ID => self.render_history_content(window, cx),
                PROJECT_WORKSPACE_ID => self.render_agent_content(window, cx),
                _ => self.render_history_content(window, cx),
            })
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();
        let workspace_id = self.workspace_id;
        let app = cx.weak_entity();

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
                            .text_color(contrast::sidebar_text(cx))
                            .child("yuewei"),
                    ),
            )
            .dropdown_menu_with_anchor(gpui::Anchor::BottomLeft, move |menu, window, cx| {
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
                .submenu_with_icon(
                    Some(Icon::new(IconName::ChevronsUpDown)),
                    t!("account.work_mode").to_string(),
                    window,
                    cx,
                    {
                        let app = app.clone();
                        move |submenu, _, _| {
                            let chat_app = app.clone();
                            let project_app = app.clone();
                            submenu
                                .item(
                                    gpui_component::menu::PopupMenuItem::new(
                                        t!("sidebar.chats").to_string(),
                                    )
                                    .checked(workspace_id == CHAT_WORKSPACE_ID)
                                    .on_click(
                                        move |_, window, cx| {
                                            chat_app
                                                .update(cx, |this, cx| {
                                                    this.switch_workspace(
                                                        CHAT_WORKSPACE_ID,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                                .item(
                                    gpui_component::menu::PopupMenuItem::new(
                                        t!("agent.mode").to_string(),
                                    )
                                    .checked(workspace_id == PROJECT_WORKSPACE_ID)
                                    .on_click(
                                        move |_, window, cx| {
                                            project_app
                                                .update(cx, |this, cx| {
                                                    this.switch_workspace(
                                                        PROJECT_WORKSPACE_ID,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                        }
                    },
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
            .px(SIDEBAR_CONTENT_INSET)
            .child(
                div()
                    .debug_selector(|| "sidebar-account-boundary".to_string())
                    .child(account),
            )
            .child(div().flex_1())
            .child(
                div()
                    .debug_selector(|| "sidebar-search-boundary".to_string())
                    .child(
                        Button::new("search")
                            .ghost()
                            .small()
                            .icon(IconName::Search)
                            .tooltip(t!("sidebar.search").to_string()),
                    ),
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
        let active_view = self.active_view();
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
            .child(self.render_sidebar_panel(window, cx))
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
            EffectTransition::new(SIDEBAR_ANIM)
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
        let main_content: AnyElement = match self.workspace_id {
            CHAT_WORKSPACE_ID => div()
                .flex_1()
                .min_h_0()
                .when_some(active_view, |this, view| this.child(view))
                .when(!has_active, |this| this.child(render_empty_workspace(cx)))
                .into_any_element(),
            PROJECT_WORKSPACE_ID => self.render_agent_main(window, cx),
            _ => div()
                .flex_1()
                .min_h_0()
                .when_some(active_view, |this, view| this.child(view))
                .when(!has_active, |this| this.child(render_empty_workspace(cx)))
                .into_any_element(),
        };

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
            .child(main_content);

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
                    .tooltip(if self.workspace_id == CHAT_WORKSPACE_ID {
                        t!("sidebar.new_chat").to_string()
                    } else {
                        t!("agent.open_folder").to_string()
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
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
            EffectTransition::new(SIDEBAR_ANIM)
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

/// Rendered in the main column when the workspace has no opened conversation
/// (startup state or after the last conversation is closed).  Distinct from
/// `ChatView`'s in-conversation empty state, which shows while a draft view
/// exists but has not yet received its first message.
fn render_empty_workspace(cx: &mut Context<ChatApp>) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(
            div()
                .text_2xl()
                .font_semibold()
                .text_color(theme.foreground)
                .child(t!("chat.workspace_empty_title").to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("chat.workspace_empty_hint").to_string()),
        )
}
