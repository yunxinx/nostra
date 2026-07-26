//! Protocol selection, typed compatibility policy, and adapter dispatch.
//!
//! Enum dispatch is intentional for the two supported wire APIs. Callers use a
//! single `ProtocolSession`; protocol-specific state stays inside each adapter.

mod chat_completions;
mod responses;

use serde::{Deserialize, Serialize};

use crate::llm::{
    ContentBlock, FinishReason, GatewayError, GenerateRequest, GenerationEvent, Message,
    ProviderMetadata, Role, Usage,
};

pub(crate) use chat_completions::ChatCompletionsSession;
pub(crate) use responses::ResponsesSession;

fn validate_messages(messages: &[Message]) -> Result<(), GatewayError> {
    for message in messages {
        for block in &message.content {
            let valid = matches!(
                (message.role, block),
                (
                    Role::System | Role::Developer | Role::User | Role::Assistant,
                    ContentBlock::Text { .. }
                ) | (Role::Assistant, ContentBlock::Reasoning { .. })
                    | (Role::Assistant, ContentBlock::ToolCall { .. })
                    | (Role::Tool, ContentBlock::ToolResult { .. })
            );
            if !valid {
                return Err(GatewayError::protocol(
                    "message role does not support its content block",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod message_validation_tests {
    use super::*;
    use crate::llm::ToolResult;

    #[test]
    fn both_adapters_reject_invalid_role_content_combinations() {
        let request = GenerateRequest {
            model: "model".into(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_result: ToolResult {
                        call_id: "call_1".into(),
                        content: "result".into(),
                        is_error: false,
                    },
                }],
                provider_metadata: ProviderMetadata::default(),
            }],
            ..Default::default()
        };

        assert!(
            ChatCompletionsSession::new(CompatibilityProfile::default())
                .encode_request(&request)
                .is_err()
        );
        assert!(
            ResponsesSession::new(CompatibilityProfile::default())
                .encode_request(&request)
                .is_err()
        );

        let valid_tool_result = GenerateRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_result: ToolResult {
                        call_id: "call_1".into(),
                        content: "result".into(),
                        is_error: false,
                    },
                }],
                provider_metadata: ProviderMetadata::default(),
            }],
            ..request
        };
        assert!(
            ChatCompletionsSession::new(CompatibilityProfile::default())
                .encode_request(&valid_tool_result)
                .is_ok()
        );
        assert!(
            ResponsesSession::new(CompatibilityProfile::default())
                .encode_request(&valid_tool_result)
                .is_ok()
        );
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    #[default]
    ChatCompletions,
    Responses,
}

impl Protocol {
    pub fn endpoint_path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/chat/completions",
            Self::Responses => "/responses",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat-completions",
            Self::Responses => "responses",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "chat-completions" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxTokens,
    #[default]
    MaxCompletionTokens,
}

impl MaxTokensField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "max_tokens" => Some(Self::MaxTokens),
            "max_completion_tokens" => Some(Self::MaxCompletionTokens),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemRolePolicy {
    #[default]
    Preserve,
    System,
    Developer,
}

impl SystemRolePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::System => "system",
            Self::Developer => "developer",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "preserve" => Some(Self::Preserve),
            "system" => Some(Self::System),
            "developer" => Some(Self::Developer),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningField {
    #[default]
    Auto,
    ReasoningContent,
    Reasoning,
    ReasoningText,
}

impl ReasoningField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
            Self::ReasoningText => "reasoning_text",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "reasoning_content" => Some(Self::ReasoningContent),
            "reasoning" => Some(Self::Reasoning),
            "reasoning_text" => Some(Self::ReasoningText),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesInstructionsPolicy {
    #[default]
    TopLevel,
    InputItems,
}

impl ResponsesInstructionsPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopLevel => "top_level",
            Self::InputItems => "input_items",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        match value {
            "top_level" => Some(Self::TopLevel),
            "input_items" => Some(Self::InputItems),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityProfile {
    pub max_tokens_field: MaxTokensField,
    pub system_role: SystemRolePolicy,
    pub reasoning_field: ReasoningField,
    pub include_stream_usage: bool,
    pub allow_nullable_tool_fields: bool,
    pub allow_object_tool_arguments: bool,
    pub responses_instructions: ResponsesInstructionsPolicy,
    pub responses_store: bool,
    pub responses_include_encrypted_reasoning: bool,
}

impl Default for CompatibilityProfile {
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            system_role: SystemRolePolicy::Preserve,
            reasoning_field: ReasoningField::Auto,
            include_stream_usage: true,
            allow_nullable_tool_fields: true,
            allow_object_tool_arguments: true,
            responses_instructions: ResponsesInstructionsPolicy::TopLevel,
            responses_store: false,
            responses_include_encrypted_reasoning: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProtocolTerminal {
    pub status: ProtocolTerminalStatus,
    pub finish_reason: FinishReason,
    pub usage: Usage,
    pub response_id: Option<String>,
    pub upstream_model: Option<String>,
    pub provider_metadata: ProviderMetadata,
    pub error: Option<GatewayError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolTerminalStatus {
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProtocolUpdate {
    pub events: Vec<GenerationEvent>,
    pub terminal: Option<ProtocolTerminal>,
}

impl ProtocolUpdate {
    pub fn events(events: Vec<GenerationEvent>) -> Self {
        Self {
            events,
            terminal: None,
        }
    }
}

pub(crate) enum ProtocolSession {
    Chat(ChatCompletionsSession),
    Responses(ResponsesSession),
}

impl ProtocolSession {
    pub fn new(protocol: Protocol, compatibility: CompatibilityProfile) -> Self {
        match protocol {
            Protocol::ChatCompletions => Self::Chat(ChatCompletionsSession::new(compatibility)),
            Protocol::Responses => Self::Responses(ResponsesSession::new(compatibility)),
        }
    }

    pub fn encode_request(
        &self,
        request: &GenerateRequest,
    ) -> Result<serde_json::Value, GatewayError> {
        match self {
            Self::Chat(session) => session.encode_request(request),
            Self::Responses(session) => session.encode_request(request),
        }
    }

    pub fn ingest_sse_data(&mut self, data: &str) -> Result<ProtocolUpdate, GatewayError> {
        match self {
            Self::Chat(session) => session.ingest(data),
            Self::Responses(session) => session.ingest(data),
        }
    }

    pub fn finish_eof(&mut self) -> Result<Option<ProtocolTerminal>, GatewayError> {
        match self {
            Self::Chat(session) => session.finish_eof(),
            Self::Responses(session) => session.finish_eof(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_keys_round_trip_for_all_compatibility_enums() {
        for value in [Protocol::ChatCompletions, Protocol::Responses] {
            assert_eq!(Protocol::from_key(value.as_str()), Some(value));
        }
        for value in [
            MaxTokensField::MaxTokens,
            MaxTokensField::MaxCompletionTokens,
        ] {
            assert_eq!(MaxTokensField::from_key(value.as_str()), Some(value));
        }
        for value in [
            SystemRolePolicy::Preserve,
            SystemRolePolicy::System,
            SystemRolePolicy::Developer,
        ] {
            assert_eq!(SystemRolePolicy::from_key(value.as_str()), Some(value));
        }
        for value in [
            ReasoningField::Auto,
            ReasoningField::ReasoningContent,
            ReasoningField::Reasoning,
            ReasoningField::ReasoningText,
        ] {
            assert_eq!(ReasoningField::from_key(value.as_str()), Some(value));
        }
        for value in [
            ResponsesInstructionsPolicy::TopLevel,
            ResponsesInstructionsPolicy::InputItems,
        ] {
            assert_eq!(
                ResponsesInstructionsPolicy::from_key(value.as_str()),
                Some(value)
            );
        }
    }

    #[test]
    fn settings_keys_reject_unknown_values() {
        assert_eq!(Protocol::from_key("unknown"), None);
        assert_eq!(MaxTokensField::from_key("unknown"), None);
        assert_eq!(SystemRolePolicy::from_key("unknown"), None);
        assert_eq!(ReasoningField::from_key("unknown"), None);
        assert_eq!(ResponsesInstructionsPolicy::from_key("unknown"), None);
    }
}
