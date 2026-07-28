//! Stateful editor for OpenAI-compatible provider profiles and model catalogs.
//!
//! Input entities are created once and synchronized when the selected profile
//! changes. All writes go through `providers`, which owns persistence, catalog
//! revision updates, and window refreshes.

mod compatibility;
mod profile_list;

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme as _, IconName, StyledExt as _, TITLE_BAR_HEIGHT, WindowExt as _, h_flex,
    input::{Input, InputContentType, InputEvent, InputState},
    notification::NotificationType,
    resizable::{ResizableState, h_resizable, resizable_panel},
    scroll::ScrollableElement as _,
    v_flex,
};
use rust_i18n::t;

use super::{ROW_HEIGHT, ui::IconButton};
use crate::{
    llm::{CompatibilityProfile, ModelConfig, Protocol, ProviderProfile, SecretString},
    providers,
};

/// Shown as the Base URL placeholder rather than pre-filled, so the field
/// reads as a hint instead of a value the user has to notice and correct.
const BASE_URL_HINT: &str = "https://api.openai.com/v1";

/// Initial width of the profile list column.
const LIST_WIDTH: Pixels = px(220.);

/// Travel limits of the divider: narrow enough to stay compact, wide enough
/// to still show a long provider name without truncating everything.
const LIST_MIN_WIDTH: Pixels = px(160.);
const LIST_MAX_WIDTH: Pixels = px(320.);

/// Keeps the detail form readable however far the divider is dragged left.
/// Both floors together must fit the narrowest content box the window allows
/// (640px window − 200px nav − 40px inset = 400px), or the form would spill
/// past the window edge instead of the divider stopping.
const DETAIL_MIN_WIDTH: Pixels = px(220.);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "{prefix}-{nanos}-{}",
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

struct ModelEditor {
    id: String,
    name: Entity<InputState>,
    model_id: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, PartialEq, Eq)]
struct ProviderPlaceholders {
    name: SharedString,
    model_name: SharedString,
    model_id: SharedString,
}

impl ProviderPlaceholders {
    fn resolve() -> Self {
        Self {
            name: t!("settings.providers.name_placeholder").to_string().into(),
            model_name: t!("settings.providers.model_name_placeholder")
                .to_string()
                .into(),
            model_id: t!("settings.providers.model_id_placeholder")
                .to_string()
                .into(),
        }
    }
}

pub(super) struct ProvidersPage {
    selected: Option<String>,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    models: Vec<ModelEditor>,
    placeholders: ProviderPlaceholders,
    /// Divider position of the list / detail split.  Owned here (not by the
    /// element) so the width survives re-renders and page switches.
    layout: Entity<ResizableState>,
    /// Wire-format switches stay folded away until asked for.
    compatibility_open: bool,
    /// Row the pointer is over, so its remove button can appear.
    hovered: Option<String>,
    /// Mirrors the API key input's mask state.  Tracked here because the
    /// component's own toggle picks the opposite icon convention and its
    /// `masked` flag is not readable from outside the crate.
    api_key_masked: bool,
    _subscriptions: Vec<Subscription>,
}

impl ProvidersPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selected = providers::profiles(cx)
            .first()
            .map(|profile| profile.id.clone());
        let profile = selected
            .as_deref()
            .and_then(|id| providers::find(id, cx))
            .cloned();
        let placeholders = ProviderPlaceholders::resolve();
        let name = input_with_placeholder(
            window,
            cx,
            profile.as_ref().map_or("", |p| p.name.as_str()),
            placeholders.name.clone(),
        );
        let base_url = input_with_placeholder(
            window,
            cx,
            profile.as_ref().map_or("", |p| p.base_url.as_str()),
            BASE_URL_HINT.into(),
        );
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .default_value(profile.as_ref().map_or("", |p| p.api_key.expose()))
        });

        let mut this = Self {
            selected,
            name,
            base_url,
            api_key,
            models: Vec::new(),
            placeholders,
            layout: cx.new(|_| ResizableState::default()),
            compatibility_open: false,
            hovered: None,
            api_key_masked: true,
            _subscriptions: Vec::new(),
        };
        this.subscribe_profile_fields(window, cx);
        this.rebuild_models(profile.as_ref(), window, cx);
        this
    }

    fn subscribe_profile_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = [
            (self.name.clone(), ProfileField::Name),
            (self.base_url.clone(), ProfileField::BaseUrl),
            (self.api_key.clone(), ProfileField::ApiKey),
        ];
        for (input, field) in fields {
            self._subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event, _, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    let Some(selected) = this.selected.clone() else {
                        return;
                    };
                    let value = input.read(cx).value().to_string();
                    providers::update(&selected, cx, |profile| match field {
                        ProfileField::Name => profile.name = value,
                        ProfileField::BaseUrl => profile.base_url = value,
                        ProfileField::ApiKey => profile.api_key = SecretString::new(value),
                    });
                    cx.notify();
                },
            ));
        }
    }

    fn rebuild_models(
        &mut self,
        profile: Option<&ProviderProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.models.clear();
        let Some(profile) = profile else {
            return;
        };
        for model in &profile.models {
            self.push_model_editor(&profile.id, model, window, cx);
        }
    }

    fn push_model_editor(
        &mut self,
        profile_id: &str,
        model: &ModelConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = input_with_placeholder(
            window,
            cx,
            model.display_name.as_deref().unwrap_or_default(),
            self.placeholders.model_name.clone(),
        );
        let model_id = input_with_placeholder(
            window,
            cx,
            &model.model_id,
            self.placeholders.model_id.clone(),
        );
        let mut subscriptions = Vec::new();
        for (state, field) in [
            (name.clone(), ModelField::DisplayName),
            (model_id.clone(), ModelField::UpstreamId),
        ] {
            let mut binding = ModelFieldBinding::new(profile_id, &model.id, field);
            subscriptions.push(cx.subscribe_in(
                &state,
                window,
                move |_, input, event, window, cx| match event {
                    InputEvent::Focus => {
                        binding.remember_focus(input.read(cx).value().as_ref());
                    }
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        let Some(profile) = providers::find(&binding.profile_id, cx) else {
                            return;
                        };
                        let Some(value) = binding.accepted_value(profile, value) else {
                            return;
                        };
                        let field = binding.field;
                        providers::update_model(
                            &binding.profile_id,
                            &binding.config_id,
                            cx,
                            |model| set_model_field(model, field, value),
                        );
                        cx.notify();
                    }
                    InputEvent::Blur | InputEvent::PressEnter { .. } => {
                        let value = input.read(cx).value().to_string();
                        let duplicate = providers::find(&binding.profile_id, cx)
                            .is_some_and(|profile| !binding.value_is_available(profile, &value));
                        if !duplicate {
                            if matches!(event, InputEvent::PressEnter { .. }) {
                                binding.remember_focus(&value);
                            }
                            return;
                        }

                        let previous = binding.value_on_focus.clone();
                        let input = input.clone();
                        let profile_id = binding.profile_id.clone();
                        let config_id = binding.config_id.clone();
                        let field = binding.field;
                        let message = match binding.field {
                            ModelField::DisplayName => {
                                t!("settings.providers.duplicate_model_name").to_string()
                            }
                            ModelField::UpstreamId => {
                                t!("settings.providers.duplicate_model_id").to_string()
                            }
                        };
                        cx.defer_in(window, move |_, window, cx| {
                            input.update(cx, |input, cx| {
                                input.set_value(previous.clone(), window, cx)
                            });
                            providers::update_model(&profile_id, &config_id, cx, |model| {
                                set_model_field(model, field, previous.clone())
                            });
                            window.push_notification((NotificationType::Error, message), cx);
                            cx.notify();
                        });
                    }
                },
            ));
        }
        self.models.push(ModelEditor {
            id: model.id.clone(),
            name,
            model_id,
            _subscriptions: subscriptions,
        });
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(&id) {
            return;
        }
        self.selected = Some(id.clone());
        let profile = providers::find(&id, cx).cloned();
        self.name.update(cx, |state, cx| {
            state.set_value(profile.as_ref().map_or("", |p| p.name.as_str()), window, cx)
        });
        self.base_url.update(cx, |state, cx| {
            state.set_value(
                profile.as_ref().map_or("", |p| p.base_url.as_str()),
                window,
                cx,
            )
        });
        // Re-mask on every switch: a key revealed for one provider must not
        // stay on screen once a different provider's key loads into the field.
        self.api_key_masked = true;
        self.api_key.update(cx, |state, cx| {
            state.set_value(
                profile.as_ref().map_or("", |p| p.api_key.expose()),
                window,
                cx,
            );
            state.set_masked(true, window, cx);
        });
        self.rebuild_models(profile.as_ref(), window, cx);
        cx.notify();
    }

    fn add_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = ProviderProfile {
            id: next_id("profile"),
            // Both fields start empty and rely on their placeholders.  A
            // stored default would be frozen in whichever language was active
            // when the row was created; the list falls back to a localized
            // "unnamed" label instead, which follows the current locale.
            name: String::new(),
            base_url: String::new(),
            api_key: SecretString::default(),
            protocol: Protocol::ChatCompletions,
            compatibility: CompatibilityProfile::default(),
            models: Vec::new(),
        };
        let id = profile.id.clone();
        providers::add(profile, cx);
        self.select(id, window, cx);
    }

    /// Drops a profile and moves the selection to whatever is left.  The row
    /// menu is the intentional direct-delete surface, so this runs unguarded.
    fn delete_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        providers::remove(id, cx);
        if self.hovered.as_deref() == Some(id) {
            self.hovered = None;
        }
        if self.selected.as_deref() != Some(id) {
            cx.notify();
            return;
        }

        let next = providers::profiles(cx)
            .first()
            .map(|profile| profile.id.clone());
        self.selected = None;
        if let Some(next) = next {
            self.select(next, window, cx);
        } else {
            self.name
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.base_url
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.api_key
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.models.clear();
            cx.notify();
        }
    }

    fn add_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile_id) = self.selected.clone() else {
            return;
        };
        let model = ModelConfig {
            id: next_id("model"),
            model_id: String::new(),
            display_name: None,
        };
        providers::add_model(&profile_id, model.clone(), cx);
        self.push_model_editor(&profile_id, &model, window, cx);
        cx.notify();
    }

    fn delete_model(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile_id) = self.selected.clone() else {
            return;
        };
        providers::remove_model(&profile_id, id, cx);
        self.models.retain(|model| model.id != id);
        cx.notify();
    }

    fn set_protocol(&mut self, protocol: Protocol, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        providers::update(&id, cx, |profile| profile.protocol = protocol);
        cx.notify();
    }

    fn update_compatibility(
        &mut self,
        update: impl FnOnce(&mut CompatibilityProfile),
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        providers::update(&id, cx, |profile| update(&mut profile.compatibility));
        cx.notify();
    }

    fn sync_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = ProviderPlaceholders::resolve();

        if self.placeholders.name != next.name {
            self.name.update(cx, |state, cx| {
                state.set_placeholder(next.name.clone(), window, cx)
            });
        }
        if self.placeholders.model_name != next.model_name {
            for model in &self.models {
                model.name.update(cx, |state, cx| {
                    state.set_placeholder(next.model_name.clone(), window, cx)
                });
            }
        }
        if self.placeholders.model_id != next.model_id {
            for model in &self.models {
                model.model_id.update(cx, |state, cx| {
                    state.set_placeholder(next.model_id.clone(), window, cx)
                });
            }
        }

        self.placeholders = next;
    }
}

#[derive(Clone, Copy)]
enum ProfileField {
    Name,
    BaseUrl,
    ApiKey,
}

#[derive(Clone, Copy)]
enum ModelField {
    DisplayName,
    UpstreamId,
}

struct ModelFieldBinding {
    profile_id: String,
    config_id: String,
    field: ModelField,
    value_on_focus: String,
}

impl ModelFieldBinding {
    fn new(profile_id: &str, config_id: &str, field: ModelField) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            config_id: config_id.to_string(),
            field,
            value_on_focus: String::new(),
        }
    }

    fn remember_focus(&mut self, value: &str) {
        value.clone_into(&mut self.value_on_focus);
    }

    fn value_is_available(&self, profile: &ProviderProfile, value: &str) -> bool {
        model_field_is_available(profile, &self.config_id, self.field, value)
    }

    fn accepted_value(&self, profile: &ProviderProfile, value: String) -> Option<String> {
        self.value_is_available(profile, &value).then_some(value)
    }
}

fn model_field_is_available(
    profile: &ProviderProfile,
    config_id: &str,
    field: ModelField,
    value: &str,
) -> bool {
    match field {
        ModelField::DisplayName => profile.display_name_is_available(value, config_id),
        ModelField::UpstreamId => profile.upstream_model_id_is_available(value, config_id),
    }
}

fn set_model_field(model: &mut ModelConfig, field: ModelField, value: String) {
    match field {
        ModelField::DisplayName => model.display_name = (!value.trim().is_empty()).then_some(value),
        ModelField::UpstreamId => model.model_id = value,
    }
}

fn input_with_placeholder(
    window: &mut Window,
    cx: &mut App,
    value: &str,
    placeholder: SharedString,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(value)
            .placeholder(placeholder)
    })
}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_placeholders(window, cx);

        // Two independently scrolling columns with a draggable divider.  The
        // group fills the content area, so neither column relies on the
        // settings window's own scroll view.
        h_resizable("providers-split")
            .with_state(&self.layout)
            .child(
                resizable_panel()
                    .size(LIST_WIDTH)
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

        IconButton::new(
            "toggle-api-key-mask",
            if masked {
                IconName::EyeOff
            } else {
                IconName::Eye
            },
            tooltip,
        )
        .on_activate(move |window, cx| {
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
                    super::ui::dropdown(
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
                    .child(super::ui::labelled(
                        label(t!("settings.providers.models").to_string(), cx),
                        "provider-models",
                        t!("settings.providers.models_desc").to_string(),
                        cx,
                    ))
                    .child(
                        IconButton::new(
                            "add-model",
                            IconName::Plus,
                            t!("settings.providers.add_model").to_string(),
                        )
                        .outline()
                        .size(px(24.))
                        .on_activate({
                            let weak = cx.weak_entity();
                            move |window, cx| {
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
                        IconButton::new(
                            ElementId::Name(format!("delete-model-{id}").into()),
                            IconName::Close,
                            t!("settings.providers.delete_model").to_string(),
                        )
                        .danger()
                        .size(px(32.))
                        // A row of two text fields is cheap to retype, so
                        // this removes straight away — no confirmation.
                        .on_activate({
                            let weak = cx.weak_entity();
                            move |_, cx| {
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
                .child(super::ui::labelled(label(label_text, cx), id, info, cx)),
        )
        .child(input)
}

/// Single-line control row: label plus info icon on the left, control on the
/// right.
fn dropdown_row(
    id: &str,
    label: String,
    info: String,
    control: AnyElement,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(super::ui::labelled(label, id, info, cx))
        .child(control)
}

#[cfg(test)]
mod tests {
    use rust_i18n::t;

    use super::{ModelField, ModelFieldBinding};
    use crate::llm::{CompatibilityProfile, ModelConfig, Protocol, ProviderProfile, SecretString};

    fn edit_profile(id: &str, upstream_id: &str, display_name: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            name: id.into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![
                ModelConfig {
                    id: "edited".into(),
                    model_id: upstream_id.into(),
                    display_name: Some(display_name.into()),
                },
                ModelConfig {
                    id: "existing".into(),
                    model_id: "vendor/taken".into(),
                    display_name: Some("Taken".into()),
                },
            ],
        }
    }

    /// Every field on this page explains itself through a hover icon whose
    /// text is looked up at render time.  A missing or misspelled key resolves
    /// to the key path itself and would ship as visible gibberish, so each one
    /// must exist in both locales.
    #[test]
    fn every_field_description_resolves_in_both_locales() {
        const KEYS: [&str; 13] = [
            "name_desc",
            "base_url_desc",
            "wire_api_desc",
            "api_key_desc",
            "models_desc",
            "compatibility_desc",
            "max_tokens_field_desc",
            "system_role_desc",
            "reasoning_field_desc",
            "responses_instructions_desc",
            "stream_usage_desc",
            "nullable_tool_fields_desc",
            "object_tool_arguments_desc",
        ];

        for key in KEYS {
            let path = format!("settings.providers.{key}");
            for locale in ["zh-CN", "en"] {
                let text = t!(&path, locale = locale);
                assert_ne!(text, path, "{path} is missing for {locale}");
                assert!(!text.is_empty(), "{path} is empty for {locale}");
            }
        }
    }

    /// The unnamed-row label is resolved at render time and numbered, so it
    /// must interpolate rather than leak a `%{index}` placeholder, and it must
    /// follow whichever locale is active.
    #[test]
    fn unnamed_provider_label_is_numbered_per_locale() {
        for (locale, expected) in [("zh-CN", "未命名供应商 2"), ("en", "Unnamed provider 2")]
        {
            assert_eq!(
                t!("settings.providers.unnamed", locale = locale, index = 2),
                expected
            );
        }
    }

    #[test]
    fn duplicate_model_notifications_resolve_in_both_locales() {
        for key in ["duplicate_model_id", "duplicate_model_name"] {
            let path = format!("settings.providers.{key}");
            for locale in ["zh-CN", "en"] {
                let text = t!(&path, locale = locale);
                assert_ne!(text, path, "{path} is missing for {locale}");
                assert!(!text.is_empty(), "{path} is empty for {locale}");
            }
        }
    }

    #[test]
    fn duplicate_edits_retain_the_value_captured_on_focus() {
        let profile = edit_profile("owner", "vendor/original", "Original");
        for field in [ModelField::DisplayName, ModelField::UpstreamId] {
            let (initial, intermediate, duplicate) = match field {
                ModelField::DisplayName => ("Original", "Take", "Taken"),
                ModelField::UpstreamId => ("vendor/original", "vendor/take", "vendor/taken"),
            };
            let mut binding = ModelFieldBinding::new(&profile.id, "edited", field);
            binding.remember_focus(initial);

            assert_eq!(
                binding.accepted_value(&profile, intermediate.to_string()),
                Some(intermediate.to_string())
            );
            assert_eq!(
                binding.accepted_value(&profile, duplicate.to_string()),
                None
            );
            assert_eq!(binding.value_on_focus, initial);
        }
    }
}
