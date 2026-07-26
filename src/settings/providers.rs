//! Stateful editor for OpenAI-compatible provider profiles and model catalogs.
//!
//! Input entities are created once and synchronized when the selected profile
//! changes. All writes go through `providers`, which owns persistence, catalog
//! revision updates, and window refreshes.

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    switch::Switch,
    v_flex,
};
use rust_i18n::t;

use crate::{
    llm::{
        CompatibilityProfile, MaxTokensField, ModelConfig, Protocol, ProviderProfile,
        ReasoningField, ResponsesInstructionsPolicy, SecretString, SystemRolePolicy,
    },
    providers,
};

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

pub(super) struct ProvidersPage {
    selected: Option<String>,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    models: Vec<ModelEditor>,
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
        let name = input(window, cx, profile.as_ref().map_or("", |p| p.name.as_str()));
        let base_url = input(
            window,
            cx,
            profile.as_ref().map_or("", |p| p.base_url.as_str()),
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
        let models = profile.map_or(&[][..], |profile| profile.models.as_slice());
        for model in models {
            self.push_model_editor(model, window, cx);
        }
    }

    fn push_model_editor(
        &mut self,
        model: &ModelConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = input_with_placeholder(
            window,
            cx,
            model.display_name.as_deref().unwrap_or_default(),
            &t!("settings.providers.model_name_placeholder"),
        );
        let model_id = input_with_placeholder(
            window,
            cx,
            &model.model_id,
            &t!("settings.providers.model_id_placeholder"),
        );
        let mut subscriptions = Vec::new();
        for (state, field) in [
            (name.clone(), ModelField::DisplayName),
            (model_id.clone(), ModelField::UpstreamId),
        ] {
            let id = model.id.clone();
            subscriptions.push(cx.subscribe_in(
                &state,
                window,
                move |this, input, event, _, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    let Some(profile_id) = this.selected.clone() else {
                        return;
                    };
                    let value = input.read(cx).value().to_string();
                    providers::update_model(&profile_id, &id, cx, |model| match field {
                        ModelField::DisplayName => {
                            model.display_name = (!value.trim().is_empty()).then_some(value)
                        }
                        ModelField::UpstreamId => model.model_id = value,
                    });
                    cx.notify();
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
        self.api_key.update(cx, |state, cx| {
            state.set_value(
                profile.as_ref().map_or("", |p| p.api_key.expose()),
                window,
                cx,
            )
        });
        self.rebuild_models(profile.as_ref(), window, cx);
        cx.notify();
    }

    fn add_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = ProviderProfile {
            id: next_id("profile"),
            name: t!("settings.providers.new_profile").to_string(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::ChatCompletions,
            compatibility: CompatibilityProfile::default(),
            models: Vec::new(),
        };
        let id = profile.id.clone();
        providers::add(profile, cx);
        self.select(id, window, cx);
    }

    fn request_delete_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let id = id.clone();
            let weak = weak.clone();
            alert
                .title(t!("settings.providers.delete_profile_title").to_string())
                .description(t!("settings.providers.delete_profile_desc").to_string())
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(t!("settings.providers.delete").to_string())
                        .cancel_text(t!("settings.providers.cancel").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    providers::remove(&id, cx);
                    weak.update(cx, |this, cx| {
                        let next = providers::profiles(cx)
                            .first()
                            .map(|profile| profile.id.clone());
                        this.selected = None;
                        if let Some(next) = next {
                            this.select(next, window, cx);
                        } else {
                            this.name
                                .update(cx, |state, cx| state.set_value("", window, cx));
                            this.base_url
                                .update(cx, |state, cx| state.set_value("", window, cx));
                            this.api_key
                                .update(cx, |state, cx| state.set_value("", window, cx));
                            this.models.clear();
                            cx.notify();
                        }
                    })
                    .ok();
                    true
                })
        });
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
        self.push_model_editor(&model, window, cx);
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

    fn request_delete_model(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let id = id.clone();
            let weak = weak.clone();
            alert
                .title(t!("settings.providers.delete_model_title").to_string())
                .description(t!("settings.providers.delete_model_desc").to_string())
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(t!("settings.providers.delete").to_string())
                        .cancel_text(t!("settings.providers.cancel").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    weak.update(cx, |this, cx| this.delete_model(&id, cx)).ok();
                    true
                })
        });
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

fn input(window: &mut Window, cx: &mut App, value: &str) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).default_value(value))
}

fn input_with_placeholder(
    window: &mut Window,
    cx: &mut App,
    value: &str,
    placeholder: &str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(value)
            .placeholder(placeholder.to_string())
    })
}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let profiles = providers::profiles(cx).to_vec();
        let selected = self.selected.clone();
        let protocol = selected
            .as_deref()
            .and_then(|id| providers::find(id, cx))
            .map(|profile| profile.protocol);
        let compatibility = selected
            .as_deref()
            .and_then(|id| providers::find(id, cx))
            .map(|profile| profile.compatibility.clone());

        h_flex()
            .size_full()
            .items_stretch()
            .child(
                v_flex()
                    .w(px(210.))
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .pr_3()
                            .children(profiles.into_iter().map(|profile| {
                                let id = profile.id.clone();
                                let active = selected.as_deref() == Some(id.as_str());
                                Button::new(ElementId::Name(format!("provider-{id}").into()))
                                    .ghost()
                                    .label(if profile.name.trim().is_empty() {
                                        t!("settings.providers.unnamed").to_string()
                                    } else {
                                        profile.name
                                    })
                                    .selected(active)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select(id.clone(), window, cx)
                                    }))
                            })),
                    )
                    .child(
                        h_flex()
                            .pt_3()
                            .pr_3()
                            .gap_1()
                            .child(
                                Button::new("add-provider")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Plus)
                                    .tooltip(t!("settings.providers.add_profile").to_string())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_profile(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("delete-provider")
                                    .ghost()
                                    .danger()
                                    .small()
                                    .icon(IconName::Close)
                                    .disabled(self.selected.is_none())
                                    .tooltip(t!("settings.providers.delete_profile").to_string())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.request_delete_profile(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .pl_7()
                    .gap_5()
                    .when(self.selected.is_none(), |this| {
                        this.child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("settings.providers.empty").to_string()),
                        )
                    })
                    .when(self.selected.is_some(), |this| {
                        this.child(field(
                            t!("settings.providers.name").to_string(),
                            Input::new(&self.name),
                        ))
                        .child(field(
                            t!("settings.providers.base_url").to_string(),
                            Input::new(&self.base_url),
                        ))
                        .child(
                            v_flex()
                                .gap_2()
                                .child(label(t!("settings.providers.wire_api").to_string(), cx))
                                .child({
                                    let weak = cx.weak_entity();
                                    super::ui::dropdown(
                                        "provider-protocol",
                                        vec![
                                            ("chat-completions".into(), "Chat Completions".into()),
                                            ("responses".into(), "Responses".into()),
                                        ],
                                        protocol.unwrap_or_default().as_str().into(),
                                        false,
                                        move |value, cx| {
                                            let Some(protocol) = Protocol::from_key(value.as_ref())
                                            else {
                                                return;
                                            };
                                            weak.update(cx, |this, cx| {
                                                this.set_protocol(protocol, cx)
                                            })
                                            .ok();
                                        },
                                    )
                                }),
                        )
                        .child(field(
                            t!("settings.providers.api_key").to_string(),
                            Input::new(&self.api_key).mask_toggle(),
                        ))
                        .when_some(compatibility, |this, compatibility| {
                            this.child(self.render_compatibility(compatibility, cx))
                        })
                        .child(self.render_models(window, cx))
                    }),
            )
    }
}

impl ProvidersPage {
    fn render_compatibility(
        &self,
        compatibility: CompatibilityProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let max_tokens = compatibility.max_tokens_field.as_str();
        let system_role = compatibility.system_role.as_str();
        let reasoning = compatibility.reasoning_field.as_str();
        let instructions = compatibility.responses_instructions.as_str();

        v_flex()
            .gap_2()
            .child(label(
                t!("settings.providers.compatibility").to_string(),
                cx,
            ))
            .child(compatibility_dropdown_row(
                t!("settings.providers.max_tokens_field").to_string(),
                super::ui::dropdown(
                    "compat-max-tokens",
                    vec![
                        (
                            "max_completion_tokens".into(),
                            "max_completion_tokens".into(),
                        ),
                        ("max_tokens".into(), "max_tokens".into()),
                    ],
                    max_tokens.into(),
                    false,
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = MaxTokensField::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| {
                                        compatibility.max_tokens_field = value;
                                    },
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
            ))
            .child(compatibility_dropdown_row(
                t!("settings.providers.system_role").to_string(),
                super::ui::dropdown(
                    "compat-system-role",
                    vec![
                        (
                            "preserve".into(),
                            t!("settings.providers.option.preserve").to_string().into(),
                        ),
                        ("system".into(), "system".into()),
                        ("developer".into(), "developer".into()),
                    ],
                    system_role.into(),
                    false,
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = SystemRolePolicy::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| {
                                        compatibility.system_role = value;
                                    },
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
            ))
            .child(compatibility_dropdown_row(
                t!("settings.providers.reasoning_field").to_string(),
                super::ui::dropdown(
                    "compat-reasoning-field",
                    vec![
                        (
                            "auto".into(),
                            t!("settings.providers.option.auto").to_string().into(),
                        ),
                        ("reasoning_content".into(), "reasoning_content".into()),
                        ("reasoning".into(), "reasoning".into()),
                        ("reasoning_text".into(), "reasoning_text".into()),
                    ],
                    reasoning.into(),
                    false,
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = ReasoningField::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| {
                                        compatibility.reasoning_field = value;
                                    },
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
            ))
            .child(compatibility_dropdown_row(
                t!("settings.providers.responses_instructions").to_string(),
                super::ui::dropdown(
                    "compat-responses-instructions",
                    vec![
                        (
                            "top_level".into(),
                            t!("settings.providers.option.top_level").to_string().into(),
                        ),
                        (
                            "input_items".into(),
                            t!("settings.providers.option.input_items")
                                .to_string()
                                .into(),
                        ),
                    ],
                    instructions.into(),
                    false,
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = ResponsesInstructionsPolicy::from_key(value.as_ref())
                            else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| {
                                        compatibility.responses_instructions = value;
                                    },
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
            ))
            .child(self.compatibility_switch(
                "compat-stream-usage",
                t!("settings.providers.stream_usage").to_string(),
                compatibility.include_stream_usage,
                |compatibility, checked| compatibility.include_stream_usage = checked,
                cx,
            ))
            .child(self.compatibility_switch(
                "compat-nullable-tools",
                t!("settings.providers.nullable_tool_fields").to_string(),
                compatibility.allow_nullable_tool_fields,
                |compatibility, checked| compatibility.allow_nullable_tool_fields = checked,
                cx,
            ))
            .child(self.compatibility_switch(
                "compat-object-arguments",
                t!("settings.providers.object_tool_arguments").to_string(),
                compatibility.allow_object_tool_arguments,
                |compatibility, checked| compatibility.allow_object_tool_arguments = checked,
                cx,
            ))
    }

    fn compatibility_switch(
        &self,
        id: &'static str,
        text: String,
        checked: bool,
        update: fn(&mut CompatibilityProfile, bool),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.weak_entity();
        h_flex().justify_between().child(text).child(
            Switch::new(id)
                .small()
                .checked(checked)
                .on_click(move |checked, _, cx| {
                    weak.update(cx, |this, cx| {
                        this.update_compatibility(
                            |compatibility| update(compatibility, *checked),
                            cx,
                        )
                    })
                    .ok();
                }),
        )
    }

    fn render_models(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(label(t!("settings.providers.models").to_string(), cx))
                    .child(
                        Button::new("add-model")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(t!("settings.providers.add_model").to_string())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_model(window, cx)),
                            ),
                    ),
            )
            .children(self.models.iter().map(|model| {
                let id = model.id.clone();
                h_flex()
                    .gap_2()
                    .child(Input::new(&model.name))
                    .child(Input::new(&model.model_id).flex_1())
                    .child(
                        Button::new(ElementId::Name(format!("delete-model-{id}").into()))
                            .ghost()
                            .danger()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t!("settings.providers.delete_model").to_string())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_delete_model(id.clone(), window, cx)
                            })),
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

fn field(label_text: String, input: Input) -> impl IntoElement {
    v_flex().gap_2().child(label_text).child(input)
}

fn compatibility_dropdown_row(label: String, control: AnyElement) -> impl IntoElement {
    h_flex()
        .justify_between()
        .gap_4()
        .child(label)
        .child(control)
}
