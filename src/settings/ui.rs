//! Shared builders for the flat, macOS-style settings rows.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, ElementId, IntoElement, ParentElement as _, RenderOnce, SharedString,
    Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    popover::Popover,
    v_flex,
};
use rust_i18n::t;

use crate::preferences;

const INFO_BUTTON_SIZE: gpui::Pixels = px(18.);
const INFO_ICON_SIZE: gpui::Pixels = px(14.);
const DROPDOWN_MENU_MAX_HEIGHT: gpui::Pixels = px(190.);
// PopupMenu uses 26px medium items by default; six items plus gaps and padding
// fit below the cap, while a seventh item overflows it.
const DROPDOWN_MENU_MAX_VISIBLE_ITEMS: usize = 6;

/// Standard gpui-component icon button with Nostra's compact settings metrics.
///
/// Button derives its icon size from its component size. A final style size
/// keeps the outer hit target exact while the component size preserves the
/// requested icon pixels and all standard focus/keyboard/disabled behavior.
pub(super) fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    aria_label: impl Into<SharedString>,
    size: gpui::Pixels,
    icon_size: gpui::Pixels,
) -> Button {
    let aria_label = aria_label.into();
    Button::new(id)
        .ghost()
        .with_size(icon_size * (4. / 3.))
        .size(size)
        .p_0()
        .icon(Icon::new(icon))
        .aria_label(aria_label.clone())
        .tooltip(aria_label)
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
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let popover_id = ElementId::Name(format!("settings-info-popover-{}", self.id).into());
        let text = self.text;

        let trigger = icon_button(
            ElementId::Name(format!("settings-info-trigger-{}", self.id).into()),
            IconName::Info,
            self.aria_label,
            INFO_BUTTON_SIZE,
            INFO_ICON_SIZE,
        );

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
