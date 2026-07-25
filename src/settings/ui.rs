//! Shared builders for the flat, macOS-style settings rows.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, ElementId, IntoElement, ParentElement as _, SharedString, Styled as _,
    div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::Button,
    h_flex,
    hover_card::HoverCard,
    menu::{DropdownMenu as _, PopupMenuItem},
    v_flex,
};

/// One flat settings row: label (plus an optional hover-info icon) on the
/// left, the control on the right.  No card, no inline description — the
/// explanation lives behind the info icon, shown on hover.
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
                .items_center()
                .gap_1p5()
                .child(div().text_color(cx.theme().foreground).child(label))
                .when_some(info, |this, text| this.child(info_hover(id, text, cx))),
        )
        .child(control)
        .into_any_element()
}

/// Stack rows plainly — grouping comes from typography and vertical rhythm
/// alone (no separators, no cards).
pub(super) fn section(rows: Vec<AnyElement>, _: &App) -> AnyElement {
    v_flex().w_full().children(rows).into_any_element()
}

/// Small muted info icon that reveals the description in a hover popover.
fn info_hover(id: &'static str, text: String, cx: &App) -> impl IntoElement {
    HoverCard::new(ElementId::Name(format!("info-{id}").into()))
        .anchor(Anchor::TopLeft)
        .trigger(
            Icon::new(IconName::Info)
                .size_3p5()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .max_w(px(300.))
                .text_sm()
                .text_color(cx.theme().popover_foreground)
                .child(text),
        )
}

/// Compact dropdown control: an outline button whose popup menu lists the
/// options with a check mark on the current one.
pub(super) fn dropdown(
    id: &'static str,
    options: Vec<(SharedString, SharedString)>,
    current: SharedString,
    scrollable: bool,
    on_select: impl Fn(SharedString, &mut App) + 'static,
) -> AnyElement {
    let current_label = options
        .iter()
        .find(|(value, _)| *value == current)
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| current.clone());
    let on_select = Rc::new(on_select);

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
            menu.scrollable(scrollable)
        })
        .into_any_element()
}
