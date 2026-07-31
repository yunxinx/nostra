//! Compatibility controls for provider-specific wire-format differences.

use gpui::prelude::FluentBuilder as _;
use gpui::{Context, IntoElement, ParentElement as _, Styled as _};
use gpui_component::{
    IconName, Sizable as _,
    button::Button,
    group_box::{GroupBox, GroupBoxVariants as _},
    h_flex,
    switch::Switch,
    v_flex,
};
use rust_i18n::t;

use super::super::ui;
use super::{ProvidersPage, dropdown_row};
use crate::llm::{
    CompatibilityProfile, MaxTokensField, ReasoningField, ResponsesInstructionsPolicy,
    SystemRolePolicy,
};

impl ProvidersPage {
    /// Wire-format details, kept behind a disclosure button. Expanding drops
    /// the rows into an outlined group of related controls.
    pub(super) fn render_compatibility(
        &self,
        compatibility: CompatibilityProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.compatibility_open;

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Button::new("toggle-compatibility")
                            .outline()
                            .small()
                            .icon(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .label(t!("settings.providers.compatibility").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.compatibility_open = !this.compatibility_open;
                                cx.notify();
                            })),
                    )
                    .when_some(
                        ui::info_button(
                            "provider-compatibility",
                            t!("settings.providers.compatibility_desc").to_string(),
                            cx,
                        ),
                        |this, button| this.child(button),
                    ),
            )
            .when(open, |this| {
                this.child(
                    GroupBox::new()
                        .outline()
                        .child(self.render_compatibility_fields(compatibility, cx)),
                )
            })
    }

    fn render_compatibility_fields(
        &self,
        compatibility: CompatibilityProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let max_tokens = compatibility.max_tokens_field.as_str();
        let system_role = compatibility.system_role.as_str();
        let reasoning = compatibility.reasoning_field.as_str();
        let instructions = compatibility.responses_instructions.as_str();

        v_flex()
            .w_full()
            .gap_3()
            .text_sm()
            .child(dropdown_row(
                "compat-max-tokens-row",
                t!("settings.providers.max_tokens_field").to_string(),
                t!("settings.providers.max_tokens_field_desc").to_string(),
                ui::dropdown(
                    "compat-max-tokens",
                    vec![
                        (
                            "max_completion_tokens".into(),
                            "max_completion_tokens".into(),
                        ),
                        ("max_tokens".into(), "max_tokens".into()),
                    ],
                    max_tokens.into(),
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = MaxTokensField::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| compatibility.max_tokens_field = value,
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
                cx,
            ))
            .child(dropdown_row(
                "compat-system-role-row",
                t!("settings.providers.system_role").to_string(),
                t!("settings.providers.system_role_desc").to_string(),
                ui::dropdown(
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
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = SystemRolePolicy::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| compatibility.system_role = value,
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
                cx,
            ))
            .child(dropdown_row(
                "compat-reasoning-field-row",
                t!("settings.providers.reasoning_field").to_string(),
                t!("settings.providers.reasoning_field_desc").to_string(),
                ui::dropdown(
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
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = ReasoningField::from_key(value.as_ref()) else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| compatibility.reasoning_field = value,
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
                cx,
            ))
            .child(dropdown_row(
                "compat-responses-instructions-row",
                t!("settings.providers.responses_instructions").to_string(),
                t!("settings.providers.responses_instructions_desc").to_string(),
                ui::dropdown(
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
                    {
                        let weak = cx.weak_entity();
                        move |value, cx| {
                            let Some(value) = ResponsesInstructionsPolicy::from_key(value.as_ref())
                            else {
                                return;
                            };
                            weak.update(cx, |this, cx| {
                                this.update_compatibility(
                                    |compatibility| compatibility.responses_instructions = value,
                                    cx,
                                )
                            })
                            .ok();
                        }
                    },
                ),
                cx,
            ))
            .child(self.render_compatibility_switch(
                "compat-stream-usage",
                t!("settings.providers.stream_usage").to_string(),
                t!("settings.providers.stream_usage_desc").to_string(),
                compatibility.include_stream_usage,
                |compatibility, checked| compatibility.include_stream_usage = checked,
                cx,
            ))
            .child(self.render_compatibility_switch(
                "compat-nullable-tools",
                t!("settings.providers.nullable_tool_fields").to_string(),
                t!("settings.providers.nullable_tool_fields_desc").to_string(),
                compatibility.allow_nullable_tool_fields,
                |compatibility, checked| compatibility.allow_nullable_tool_fields = checked,
                cx,
            ))
            .child(self.render_compatibility_switch(
                "compat-object-arguments",
                t!("settings.providers.object_tool_arguments").to_string(),
                t!("settings.providers.object_tool_arguments_desc").to_string(),
                compatibility.allow_object_tool_arguments,
                |compatibility, checked| compatibility.allow_object_tool_arguments = checked,
                cx,
            ))
    }

    fn render_compatibility_switch(
        &self,
        id: &'static str,
        text: String,
        info: String,
        checked: bool,
        update: fn(&mut CompatibilityProfile, bool),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.weak_entity();
        h_flex()
            .justify_between()
            .child(ui::labelled(text, id, info, cx))
            .child(
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
}
