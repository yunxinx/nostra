//! Provider selection list and its row actions.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Role, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT,
    button::{Button, ButtonVariants as _},
    h_flex,
    list::ListItem,
    menu::{DropdownMenu as _, PopupMenuItem},
    popover::Popover,
    scroll::ScrollableElement as _,
    v_flex,
};
use rust_i18n::t;

use super::{ProvidersPage, ROW_HEIGHT, icon_button};
use crate::{llm::ProviderProfile, providers};

impl ProvidersPage {
    /// Left column: one row per profile, then the add row as the last item.
    pub(super) fn render_profile_list(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let profiles = providers::profiles(cx).to_vec();
        let selected = self.selected.clone();
        let rows = profiles
            .iter()
            .enumerate()
            .map(|(ix, profile)| {
                self.render_profile_row(profile.clone(), ix, &selected, window, cx)
            })
            .collect::<Vec<_>>();

        v_flex()
            .size_full()
            .child(div().h(TITLE_BAR_HEIGHT).flex_shrink_0())
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .pt_2()
                    .pr_3()
                    .pb_4()
                    .gap_1()
                    .children(rows)
                    .child(self.render_add_item(!profiles.is_empty(), window, cx))
                    .overflow_y_scrollbar(),
            )
            .into_any_element()
    }

    fn render_profile_row(
        &self,
        profile: ProviderProfile,
        ix: usize,
        selected: &Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = profile.id.clone();
        let active = selected.as_deref() == Some(id.as_str());
        let is_confirming = self.confirming.as_deref() == Some(id.as_str());
        let actions_visible =
            active || self.hovered.as_deref() == Some(id.as_str()) || is_confirming;
        let row_id = ElementId::Name(format!("provider-row-{id}").into());
        let focus_handle = window
            .use_keyed_state(row_id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let name = if profile.name.trim().is_empty() {
            t!("settings.providers.unnamed", index = ix + 1).to_string()
        } else {
            profile.name
        };

        div()
            .id(ElementId::Name(
                format!("provider-row-container-{id}").into(),
            ))
            .relative()
            .w_full()
            .h(ROW_HEIGHT)
            .on_hover(cx.listener({
                let id = id.clone();
                move |this, hovered: &bool, _, cx| {
                    let entered = *hovered;
                    if !entered && this.hovered.as_deref() != Some(id.as_str()) {
                        return;
                    }
                    let next = entered.then(|| id.clone());
                    if this.hovered != next {
                        this.hovered = next;
                        cx.notify();
                    }
                }
            }))
            .child(
                div()
                    .id(row_id)
                    .role(Role::Button)
                    .aria_label(name.clone())
                    .aria_selected(active)
                    .track_focus(&focus_handle.tab_stop(true))
                    .focus_visible(|this| this.border_1().border_color(focus_ring))
                    .cursor_default()
                    .w_full()
                    .h(ROW_HEIGHT)
                    .rounded(cx.theme().radius)
                    .on_key_down(cx.listener({
                        let id = id.clone();
                        move |this, event: &KeyDownEvent, window, cx| {
                            if crate::ui::consume_button_key(event, window, cx) {
                                this.select(id.clone(), window, cx);
                            }
                        }
                    }))
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, window, cx| this.select(id.clone(), window, cx)
                    }))
                    .child(
                        ListItem::new(ElementId::Name(format!("provider-{id}").into()))
                            .w_full()
                            .h(ROW_HEIGHT)
                            .items_center()
                            .px_2()
                            .text_sm()
                            .rounded(cx.theme().radius)
                            .selected(active)
                            .child(div().w_full().min_w_0().pr_6().truncate().child(name)),
                    )
                    .when(!actions_visible, |this| {
                        this.child(div().absolute().right_2().top(px(5.)).size_5().occlude())
                    }),
            )
            .child(
                div()
                    .absolute()
                    .right_2()
                    .top(px(5.))
                    .size_5()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(self.render_profile_actions(&id, actions_visible, is_confirming, cx)),
            )
            .into_any_element()
    }

    fn render_profile_actions(
        &self,
        id: &str,
        visible: bool,
        confirming: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let id = id.to_string();

        let trigger = icon_button(
            ElementId::Name(format!("provider-actions-{id}").into()),
            IconName::Ellipsis,
            t!("settings.providers.more_actions").to_string(),
            px(20.),
            px(16.),
        );

        if confirming {
            let on_change_id = id.clone();
            Popover::new(ElementId::Name(
                format!("provider-delete-confirm-{id}").into(),
            ))
            .open(true)
            .anchor(Anchor::TopRight)
            .p_0()
            .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                if !*open && this.confirming.as_deref() == Some(&on_change_id) {
                    this.confirming = None;
                    cx.notify();
                }
            }))
            .trigger(trigger)
            .content({
                let weak = weak.clone();
                let id = id.clone();
                move |_, _, _| {
                    let weak = weak.clone();
                    let id = id.clone();
                    v_flex()
                        .gap_1()
                        .p_2()
                        .child(
                            div()
                                .w_full()
                                .text_center()
                                .text_sm()
                                .child(t!("settings.providers.delete_profile_title").to_string()),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new(ElementId::Name(
                                        format!("provider-cancel-delete-{id}").into(),
                                    ))
                                    .ghost()
                                    .small()
                                    .flex_1()
                                    .label(
                                        t!("settings.providers.delete_profile_cancel").to_string(),
                                    )
                                    .on_click({
                                        let weak = weak.clone();
                                        move |_, _, cx| {
                                            weak.update(cx, |this, cx| {
                                                this.confirming = None;
                                                cx.notify();
                                            })
                                            .ok();
                                        }
                                    }),
                                )
                                .child(
                                    Button::new(ElementId::Name(
                                        format!("provider-confirm-delete-{id}").into(),
                                    ))
                                    .danger()
                                    .small()
                                    .flex_1()
                                    .label(
                                        t!("settings.providers.delete_profile_confirm").to_string(),
                                    )
                                    .on_click({
                                        let weak = weak.clone();
                                        move |_, window, cx| {
                                            weak.update(cx, |this, cx| {
                                                this.confirming = None;
                                                this.delete_profile(&id, window, cx);
                                            })
                                            .ok();
                                        }
                                    }),
                                ),
                        )
                }
            })
            .into_any_element()
        } else {
            let menu_id = id.clone();
            trigger
                .when(!visible, |this| this.invisible())
                .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                    let weak = weak.clone();
                    let id = menu_id.clone();
                    menu.item(
                        PopupMenuItem::new(t!("settings.providers.delete_profile").to_string())
                            .on_click(move |_, _, cx| {
                                weak.update(cx, |this, cx| {
                                    this.begin_delete_confirmation(id.clone(), cx)
                                })
                                .ok();
                            }),
                    )
                })
                .into_any_element()
        }
    }

    fn render_add_item(
        &self,
        after_rows: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id: ElementId = "add-provider-action".into();
        let focus_handle = window
            .use_keyed_state(id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let label = t!("settings.providers.add_profile").to_string();

        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label.clone())
            .track_focus(&focus_handle.tab_stop(true))
            .focus_visible(|this| this.border_1().border_color(focus_ring))
            .cursor_default()
            .w_full()
            .h(ROW_HEIGHT)
            .rounded(cx.theme().radius)
            .when(after_rows, |this| this.mt_1p5())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if crate::ui::consume_button_key(event, window, cx) {
                    this.add_profile(window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.add_profile(window, cx)))
            .child(
                ListItem::new("add-provider")
                    .w_full()
                    .h(ROW_HEIGHT)
                    .items_center()
                    .px_2()
                    .text_sm()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border.opacity(0.7))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .gap_1p5()
                            .items_center()
                            .child(Icon::new(IconName::Plus).size_4())
                            .child(label),
                    ),
            )
    }
}
