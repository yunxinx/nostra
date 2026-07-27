//! Shared builders for the flat, macOS-style settings rows.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, Context, DismissEvent, Div, ElementId, Entity, Focusable as _,
    InteractiveElement, Interactivity, IntoElement, KeyDownEvent, MouseButton, ParentElement as _,
    RenderOnce, Role, SharedString, Stateful, StatefulInteractiveElement as _, StyleRefinement,
    Styled, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable, Sizable as _, StyledExt as _,
    button::Button,
    h_flex,
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    popover::{Popover, PopoverState},
    tooltip::Tooltip,
    v_flex,
};
use rust_i18n::t;

use crate::{preferences, ui::consume_button_key};

type ActivateHandler = Rc<dyn Fn(&mut Window, &mut App)>;
const INFO_BUTTON_SIZE: gpui::Pixels = px(18.);
const INFO_ICON_SIZE: gpui::Pixels = px(14.);
const DROPDOWN_MENU_MAX_HEIGHT: gpui::Pixels = px(190.);
// PopupMenu uses 26px medium items by default; six items plus gaps and padding
// fit below the cap, while a seventh item overflows it.
const DROPDOWN_MENU_MAX_VISIBLE_ITEMS: usize = 6;

/// Compact icon-only command with an explicit accessible name. The upstream
/// `Button` derives its accessible name only from a visible label, so settings
/// uses this local primitive where the visual contract must remain icon-only.
#[derive(IntoElement)]
pub(super) struct IconButton {
    id: ElementId,
    base: Stateful<Div>,
    icon: IconName,
    aria_label: SharedString,
    tooltip: SharedString,
    style: StyleRefinement,
    selected: bool,
    outline: bool,
    danger: bool,
    size: gpui::Pixels,
    icon_size: gpui::Pixels,
    on_click: Option<ActivateHandler>,
    on_key_activate: Option<ActivateHandler>,
}

impl IconButton {
    pub(super) fn new(
        id: impl Into<ElementId>,
        icon: IconName,
        aria_label: impl Into<SharedString>,
    ) -> Self {
        let id = id.into();
        let aria_label = aria_label.into();
        Self {
            base: div().id(id.clone()),
            id,
            icon,
            tooltip: aria_label.clone(),
            aria_label,
            style: StyleRefinement::default(),
            selected: false,
            outline: false,
            danger: false,
            size: px(20.),
            icon_size: px(16.),
            on_click: None,
            on_key_activate: None,
        }
    }

    pub(super) fn outline(mut self) -> Self {
        self.outline = true;
        self
    }

    pub(super) fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub(super) fn size(mut self, size: gpui::Pixels) -> Self {
        self.size = size;
        self
    }

    fn icon_size(mut self, size: gpui::Pixels) -> Self {
        self.icon_size = size;
        self
    }

    pub(super) fn on_activate(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        let handler = Rc::new(handler) as ActivateHandler;
        self.on_click = Some(handler.clone());
        self.on_key_activate = Some(handler);
        self
    }

    fn on_key_activate(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_key_activate = Some(Rc::new(handler));
        self
    }

    pub(super) fn dropdown_menu_with_anchor(
        self,
        anchor: Anchor,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> IconDropdownMenu {
        IconDropdownMenu {
            id: self.id.clone(),
            style: self.style.clone(),
            trigger: self,
            anchor,
            builder: Rc::new(builder),
        }
    }
}

impl Styled for IconButton {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Selectable for IconButton {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl InteractiveElement for IconButton {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl RenderOnce for IconButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus_handle = window
            .use_keyed_state(self.id, cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let foreground = if self.danger {
            cx.theme().button_danger_foreground
        } else {
            cx.theme().foreground
        };
        let on_key_activate = self.on_key_activate;
        let tooltip = self.tooltip;

        self.base
            .role(Role::Button)
            .aria_label(self.aria_label)
            .aria_selected(self.selected)
            .track_focus(&focus_handle.tab_stop(true))
            .focus_visible(|this| this.border_1().border_color(focus_ring))
            .flex()
            .items_center()
            .justify_center()
            .size(self.size)
            .p_0()
            .rounded(cx.theme().radius)
            .cursor_default()
            .text_color(foreground)
            .when(self.outline, |this| {
                this.border_1().border_color(cx.theme().border)
            })
            .when(self.selected && !self.danger, |this| {
                this.bg(cx.theme().accent)
            })
            .when(!self.danger, |this| {
                this.hover(|this| this.bg(cx.theme().accent))
            })
            .when(self.danger, |this| {
                this.bg(cx.theme().tokens.button_danger)
                    .hover(|this| this.bg(cx.theme().tokens.button_danger_hover))
                    .active(|this| this.bg(cx.theme().tokens.button_danger_active))
            })
            .refine_style(&self.style)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .when_some(self.on_click, |this, on_click| {
                this.on_click(move |_, window, cx| on_click(window, cx))
            })
            .when_some(on_key_activate, |this, on_key_activate| {
                this.on_key_down(move |event: &KeyDownEvent, window, cx| {
                    if consume_button_key(event, window, cx) {
                        on_key_activate(window, cx);
                    }
                })
            })
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(Icon::new(self.icon).size(self.icon_size))
    }
}

type PopupMenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

#[derive(Default)]
struct IconDropdownMenuState {
    menu: Option<Entity<PopupMenu>>,
}

/// Dropdown wrapper for [`IconButton`] that preserves gpui-component's popup
/// menu appearance while adding the keyboard activation its stock wrapper
/// currently lacks.
#[derive(IntoElement)]
pub(super) struct IconDropdownMenu {
    id: ElementId,
    style: StyleRefinement,
    trigger: IconButton,
    anchor: Anchor,
    builder: PopupMenuBuilder,
}

impl RenderOnce for IconDropdownMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let menu_state_id =
            ElementId::Name(format!("settings-icon-menu-state:{:?}", self.id).into());
        let popover_id =
            ElementId::Name(format!("settings-icon-menu-popover:{:?}", self.id).into());
        let menu_state =
            window.use_keyed_state(menu_state_id, cx, |_, _| IconDropdownMenuState::default());
        let popover_state =
            window.use_keyed_state(popover_id.clone(), cx, |_, cx| PopoverState::new(false, cx));
        let keyboard_state = popover_state.clone();
        let builder = self.builder;
        let trigger = self.trigger.on_key_activate(move |window, cx| {
            keyboard_state.update(cx, |state, cx| state.show(window, cx));
            window.refresh();
        });

        Popover::new(popover_id)
            .appearance(false)
            .overlay_closable(false)
            .trigger(trigger)
            .trigger_style(self.style)
            .anchor(self.anchor)
            .content(move |_, window, cx| {
                if let Some(menu) = menu_state.read(cx).menu.clone() {
                    return menu;
                }

                let builder = builder.clone();
                let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                    builder(menu, window, cx)
                });
                menu_state.update(cx, |state, _| state.menu = Some(menu.clone()));
                menu.focus_handle(cx).focus(window, cx);

                let popover_state = cx.entity();
                window
                    .subscribe(&menu, cx, {
                        let menu_state = menu_state.clone();
                        move |_, _: &DismissEvent, window, cx| {
                            popover_state.update(cx, |state, cx| state.dismiss(window, cx));
                            menu_state.update(cx, |state, _| state.menu = None);
                        }
                    })
                    .detach();

                menu
            })
    }
}

/// One flat settings row: label (plus an optional info button) on the left,
/// the control on the right. No card and no inline description.
pub(super) fn row(
    id: &'static str,
    label: String,
    info: Option<String>,
    control: AnyElement,
    cx: &App,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_4()
        .py_3()
        // Same text style path as the nav items (`text_sm` on the row
        // container, inherited by the label) so both columns render the
        // label at exactly the same size.
        .text_sm()
        .child(
            h_flex()
                .h(INFO_BUTTON_SIZE)
                .items_center()
                .gap_1p5()
                .child(div().text_color(cx.theme().foreground).child(label))
                .when_some(
                    info.and_then(|text| info_button(id, text, cx)),
                    |this, button| this.child(button),
                ),
        )
        .child(control)
        .into_any_element()
}

/// Stack rows plainly — grouping comes from typography and vertical rhythm
/// alone (no separators, no cards).
pub(super) fn section(rows: Vec<AnyElement>, _: &App) -> AnyElement {
    v_flex().w_full().children(rows).into_any_element()
}

/// Small muted info button that opens a focus-managed popover on click, Enter,
/// or Space.
/// Shared by the flat rows above and the providers form, so every settings
/// field explains itself the same way and in the same place.
pub(super) fn info_button(id: &str, text: String, cx: &App) -> Option<AnyElement> {
    (!info_buttons_hidden(cx)).then(|| {
        InfoButton {
            id: id.to_string(),
            text: text.into(),
            aria_label: t!("settings.more_information").to_string().into(),
        }
        .into_any_element()
    })
}

pub(super) fn info_buttons_hidden(cx: &App) -> bool {
    preferences::get(cx).hide_settings_info_buttons
}

pub(super) fn set_info_buttons_hidden(hidden: bool, cx: &mut App) {
    preferences::update(cx, |prefs| prefs.hide_settings_info_buttons = hidden);
    cx.refresh_windows();
}

#[derive(IntoElement)]
struct InfoButton {
    id: String,
    text: SharedString,
    aria_label: SharedString,
}

impl RenderOnce for InfoButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let popover_id = ElementId::Name(format!("settings-info-popover-{}", self.id).into());
        let popover_state =
            window.use_keyed_state(popover_id.clone(), cx, |_, cx| PopoverState::new(false, cx));
        let keyboard_state = popover_state.clone();
        let text = self.text;

        let trigger = IconButton::new(
            ElementId::Name(format!("settings-info-trigger-{}", self.id).into()),
            IconName::Info,
            self.aria_label,
        )
        .size(INFO_BUTTON_SIZE)
        .icon_size(INFO_ICON_SIZE)
        .on_key_activate(move |window, cx| {
            keyboard_state.update(cx, |state, cx| {
                state.show(window, cx);
            });
            window.refresh();
        });

        Popover::new(popover_id)
            .anchor(Anchor::TopLeft)
            .trigger(trigger)
            .child(
                div()
                    .max_w(px(300.))
                    .text_sm()
                    .text_color(cx.theme().popover_foreground)
                    .child(text),
            )
    }
}

/// A label followed by its info button, laid out as one inline unit. Callers
/// pass an already-styled label so the row keeps whatever weight and colour it
/// used before the icon existed.
pub(super) fn labelled(
    label: impl IntoElement,
    id: &str,
    info: String,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .h(INFO_BUTTON_SIZE)
        .items_center()
        .gap_1p5()
        .child(label)
        .when_some(info_button(id, info, cx), |this, button| this.child(button))
}

/// Compact dropdown control: an outline button whose popup menu lists the
/// options with a check mark on the current one.
///
/// Scrolling is enabled automatically only when the option count would
/// overflow the menu's max height — callers never decide this, so a short
/// list can never end up with a spurious scrollbar.
pub(super) fn dropdown(
    id: &'static str,
    options: Vec<(SharedString, SharedString)>,
    current: SharedString,
    on_select: impl Fn(SharedString, &mut App) + 'static,
) -> AnyElement {
    let current_label = options
        .iter()
        .find(|(value, _)| *value == current)
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| current.clone());
    let on_select = Rc::new(on_select);
    let scrollable = options.len() > DROPDOWN_MENU_MAX_VISIBLE_ITEMS;

    Button::new(id)
        .outline()
        .small()
        .label(current_label)
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let menu = options.iter().fold(menu, |menu, (value, label)| {
                let checked = *value == current;
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(checked)
                        .on_click({
                            let value = value.clone();
                            let on_select = on_select.clone();
                            move |_, _, cx| on_select(value.clone(), cx)
                        }),
                )
            });
            menu.scrollable(scrollable).max_h(DROPDOWN_MENU_MAX_HEIGHT)
        })
        .into_any_element()
}
