use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, ElementId, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, StyledExt as _, TITLE_BAR_HEIGHT,
    button::ButtonVariants as _,
    h_flex,
    input::{Input, InputContentType},
    resizable::{h_resizable, resizable_panel},
    scroll::ScrollableElement as _,
    v_flex,
};
use rust_i18n::t;

use super::{
    DETAIL_MIN_WIDTH, LIST_MAX_WIDTH, LIST_MIN_WIDTH, ProvidersPage, ROW_HEIGHT,
    changed_list_width, dropdown_row, icon_button,
};
use crate::{llm::Protocol, preferences, providers};

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_placeholders(window, cx);
        let preference_handle = self.preference_handle.clone();

        // Two independently scrolling columns with a draggable divider.  The
        // group fills the content area, so neither column relies on the
        // settings window's own scroll view.
        h_resizable("providers-split")
            .with_state(&self.layout)
            // Fires once per drag, on mouse-up, so the divider persists at the
            // same granularity as the main window's sidebar — without a write
            // per intermediate mouse-move.
            .on_resize(move |state, _, cx| {
                let Some(width) = state.read(cx).sizes().first().copied() else {
                    return;
                };
                if let Some(width) =
                    changed_list_width(preference_handle.snapshot().provider_list_width, width)
                {
                    preferences::update_with(cx, &preference_handle, |prefs| {
                        prefs.provider_list_width = width
                    });
                }
            })
            .child(
                resizable_panel()
                    .size(self.list_width)
                    .size_range(LIST_MIN_WIDTH..LIST_MAX_WIDTH)
                    // Sized panel next to a growing sibling: opt out of the
                    // panel's internal `flex_grow: 1` so the detail column
                    // absorbs every extra pixel.
                    .flex_none()
                    .child(self.render_profile_list(window, cx)),
            )
            .child(
                resizable_panel()
                    .size_range(DETAIL_MIN_WIDTH..Pixels::MAX)
                    .child(self.render_detail(cx)),
            )
    }
}

impl ProvidersPage {
    /// Reveal toggle for the API key.  Replaces the component's built-in one so
    /// the icon reports the current state — struck-through eye while the key is
    /// hidden — rather than the action the click would perform.
    fn render_mask_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let masked = self.api_key_masked;
        let tooltip = if masked {
            t!("settings.providers.show_api_key").to_string()
        } else {
            t!("settings.providers.hide_api_key").to_string()
        };
        let weak = cx.weak_entity();

        icon_button(
            "toggle-api-key-mask",
            if masked {
                IconName::EyeOff
            } else {
                IconName::Eye
            },
            tooltip,
            px(20.),
            px(16.),
        )
        .on_click(move |_, window, cx| {
            weak.update(cx, |this, cx| {
                this.api_key_masked = !this.api_key_masked;
                let masked = this.api_key_masked;
                this.api_key
                    .update(cx, |state, cx| state.set_masked(masked, window, cx));
                cx.notify();
            })
            .ok();
        })
    }

    /// Right column: the selected profile's form, scrolling on its own, below
    /// the same reserved drag strip the list column keeps.
    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        // With nothing selected there is no form to keep clear of the drag
        // strip, so the prompt centres on the whole column instead of the
        // area below it.
        if self.selected.is_none() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .px_6()
                .text_sm()
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("settings.providers.empty").to_string()),
                )
                .into_any_element();
        }

        v_flex()
            .size_full()
            .child(div().h(TITLE_BAR_HEIGHT).flex_shrink_0())
            .child(self.render_detail_body(cx))
            .into_any_element()
    }

    fn render_detail_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(selected) = self.selected.clone() else {
            return div().into_any_element();
        };

        let profile = providers::find(&selected, cx);
        let protocol = profile.map(|profile| profile.protocol).unwrap_or_default();
        let compatibility = profile.map(|profile| profile.compatibility.clone());

        v_flex()
            .flex_1()
            .min_h_0()
            .pt_2()
            .pl_6()
            .pr_10()
            .pb_6()
            .gap_5()
            .text_sm()
            .child(field(
                "provider-name",
                t!("settings.providers.name").to_string(),
                t!("settings.providers.name_desc").to_string(),
                Input::new(&self.name),
                cx,
            ))
            .child(field(
                "provider-base-url",
                t!("settings.providers.base_url").to_string(),
                t!("settings.providers.base_url_desc").to_string(),
                Input::new(&self.base_url),
                cx,
            ))
            // A two-option dropdown needs no column of its own: label left,
            // control right, same single-line shape as the compatibility rows.
            .child(dropdown_row(
                "provider-wire-api",
                t!("settings.providers.wire_api").to_string(),
                t!("settings.providers.wire_api_desc").to_string(),
                {
                    let weak = cx.weak_entity();
                    super::super::ui::dropdown(
                        "provider-protocol",
                        vec![
                            ("chat-completions".into(), "Chat Completions".into()),
                            ("responses".into(), "Responses".into()),
                        ],
                        protocol.as_str().into(),
                        move |value, cx| {
                            let Some(protocol) = Protocol::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| this.set_protocol(protocol, cx))
                                .ok();
                        },
                    )
                },
                cx,
            ))
            .child(field(
                "provider-api-key",
                t!("settings.providers.api_key").to_string(),
                t!("settings.providers.api_key_desc").to_string(),
                Input::new(&self.api_key)
                    .content_type(InputContentType::Password)
                    .suffix(self.render_mask_toggle(cx)),
                cx,
            ))
            .child(self.render_models(cx))
            .when_some(compatibility, |this, compatibility| {
                this.child(self.render_compatibility(compatibility, cx))
            })
            .overflow_y_scrollbar()
            .into_any_element()
    }
}

impl ProvidersPage {
    /// One row per model: upstream id on the left, optional alias on the
    /// right.  Both inputs share the row's free width evenly (`flex_1` +
    /// `min_w_0`), so they stay equal at any divider position; the remove
    /// button matches their height and is centred against them.
    fn render_models(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(super::super::ui::labelled(
                        label(t!("settings.providers.models").to_string(), cx),
                        "provider-models",
                        t!("settings.providers.models_desc").to_string(),
                        cx,
                    ))
                    .child(
                        icon_button(
                            "add-model",
                            IconName::Plus,
                            t!("settings.providers.add_model").to_string(),
                            px(24.),
                            px(16.),
                        )
                        .outline()
                        .on_click({
                            let weak = cx.weak_entity();
                            move |_, window, cx| {
                                weak.update(cx, |this, cx| this.add_model(window, cx)).ok();
                            }
                        }),
                    ),
            )
            .children(self.models.iter().map(|model| {
                let id = model.id.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Input::new(&model.model_id).flex_1().min_w_0())
                    .child(Input::new(&model.name).flex_1().min_w_0())
                    .child(
                        icon_button(
                            ElementId::Name(format!("delete-model-{id}").into()),
                            IconName::Close,
                            t!("settings.providers.delete_model").to_string(),
                            px(32.),
                            px(16.),
                        )
                        .danger()
                        // A row of two text fields is cheap to retype, so
                        // this removes straight away — no confirmation.
                        .on_click({
                            let weak = cx.weak_entity();
                            move |_, _, cx| {
                                weak.update(cx, |this, cx| this.delete_model(&id, cx)).ok();
                            }
                        }),
                    )
            }))
    }
}

fn label(text: String, cx: &App) -> impl IntoElement {
    div()
        .text_sm()
        .font_medium()
        .text_color(cx.theme().foreground)
        .child(text)
}

/// Label above its input, with the field's explanation behind an info icon
/// beside it.  The label sits in a row-height box so the form's first label
/// lands on the same baseline as the first list row and nav item; the tighter
/// gap keeps the label-to-input distance where it was.
fn field(id: &str, label_text: String, info: String, input: Input, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .h(ROW_HEIGHT)
                .items_center()
                .child(super::super::ui::labelled(
                    label(label_text, cx),
                    id,
                    info,
                    cx,
                )),
        )
        .child(input)
}
