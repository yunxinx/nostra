//! Stateful editor for OpenAI-compatible provider profiles and model catalogs.
//!
//! Input entities are created once and synchronized when the selected profile
//! changes. All writes go through `providers`, which owns persistence, catalog
//! revision updates, and window refreshes.

mod compatibility;
mod profile_list;
mod render;

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Pixels,
    SharedString, Styled as _, Subscription, Window, px,
};
use gpui_component::{
    WindowExt as _, h_flex,
    input::{InputEvent, InputState},
    notification::NotificationType,
    resizable::ResizableState,
};
use rust_i18n::t;

use super::{ROW_HEIGHT, ui::icon_button};
use crate::{
    llm::{CompatibilityProfile, ModelConfig, Protocol, ProviderProfile, SecretString},
    preferences, providers,
    ui::inline_delete_confirmation::InlineDeleteConfirmationHandle,
};

/// Shown as the Base URL placeholder rather than pre-filled, so the field
/// reads as a hint instead of a value the user has to notice and correct.
const BASE_URL_HINT: &str = "https://api.openai.com/v1";

/// Travel limits of the divider: narrow enough to stay compact, wide enough
/// to still show a long provider name without truncating everything.
const LIST_MIN_WIDTH: Pixels = px(160.);
const LIST_MAX_WIDTH: Pixels = px(320.);

/// Gate for the persisted divider position, applied in both directions: a
/// hand-edited or stale preferences value can't push the list outside the
/// range the drag itself allows, and a measured width is normalized the same
/// way before it is written back.
fn clamp_list_width(width: Pixels) -> Pixels {
    width.clamp(LIST_MIN_WIDTH, LIST_MAX_WIDTH)
}

/// Normalize a measured width and return it only when persistence is needed.
fn changed_list_width(current: f32, measured: Pixels) -> Option<f32> {
    let measured = clamp_list_width(measured).as_f32();
    (current != measured).then_some(measured)
}

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
    /// element) so the width survives re-renders and page switches; the value
    /// itself is seeded from and written back to preferences, so it also
    /// survives closing the settings window.
    layout: Entity<ResizableState>,
    /// Restored list width, used as the split's initial size on first render.
    list_width: Pixels,
    preference_handle: preferences::PreferenceHandle,
    /// Wire-format switches stay folded away until asked for.
    compatibility_open: bool,
    /// Row the pointer is over, so its remove button can appear.
    hovered: Option<String>,
    /// Profile awaiting inline delete confirmation.  While set, its row shows
    /// a Popover confirm card anchored to the actions button.
    confirming: Option<String>,
    delete_confirmation: InlineDeleteConfirmationHandle,
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
            preference_handle: preferences::handle(cx),
            list_width: clamp_list_width(px(preferences::handle(cx)
                .snapshot()
                .provider_list_width)),
            compatibility_open: false,
            hovered: None,
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
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

    /// Arm inline delete confirmation for a profile.  The row's actions button
    /// becomes a Popover trigger showing a confirm card anchored to it.
    fn begin_delete_confirmation(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_confirmation.dismiss_for_unmount(window, cx);
        self.confirming = Some(id);
        cx.notify();
    }

    /// Drop a profile after its caller has completed the confirmation flow.
    fn delete_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming.as_deref() == Some(id) {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
        }
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
mod tests;
