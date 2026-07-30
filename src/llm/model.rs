//! Protocol-neutral request, message, tool, replay, and usage types.
//!
//! Messages retain ordered content blocks. Provider replay metadata is scoped
//! by wire protocol and redacted from formatting while remaining serializable
//! for the next request.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub model: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub turn_id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
    pub provider_metadata: ProviderMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    Reasoning {
        reasoning: ReasoningContent,
    },
    ToolCall {
        tool_call: ToolCall,
    },
    ToolResult {
        tool_result: ToolResult,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningContent {
    #[serde(default)]
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
    pub provider_metadata: ProviderMetadata,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<ChatReplayMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ResponsesReplayMetadata>,
}

/// Protocol-scoped opaque state retained only for a later request to the same wire API.
pub type ReplayMetadata = ProviderMetadata;

impl ProviderMetadata {
    pub fn is_empty(&self) -> bool {
        self.chat.is_none() && self.responses.is_none()
    }
}

impl fmt::Debug for ProviderMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderMetadata([REDACTED])")
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatReplayMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<ChatReasoningField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatReasoningField {
    ReasoningContent,
    Reasoning,
    ReasoningText,
}

impl ChatReasoningField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
            Self::ReasoningText => "reasoning_text",
        }
    }
}

impl fmt::Debug for ChatReplayMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChatReplayMetadata([REDACTED])")
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponsesReplayMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_reasoning: Option<String>,
}

impl fmt::Debug for ResponsesReplayMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResponsesReplayMetadata([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    Reported,
    Estimated,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub provenance: UsageProvenance,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            provenance: UsageProvenance::Unavailable,
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 0,
        }
    }
}

impl Usage {
    pub fn add_assign(&mut self, other: &Self) {
        self.provenance = match (self.provenance, other.provenance) {
            (UsageProvenance::Estimated, _) | (_, UsageProvenance::Estimated) => {
                UsageProvenance::Estimated
            }
            (UsageProvenance::Reported, _) | (_, UsageProvenance::Reported) => {
                UsageProvenance::Reported
            }
            _ => UsageProvenance::Unavailable,
        };
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn replay_metadata_never_exposes_opaque_values_in_debug_output() {
        let chat_secret = "chat-opaque-secret";
        let responses_secret = "responses-opaque-secret";
        let metadata = ProviderMetadata {
            chat: Some(ChatReplayMetadata {
                reasoning_field: Some(ChatReasoningField::ReasoningContent),
                reasoning_details: Some(json!({"opaque": chat_secret})),
            }),
            responses: Some(ResponsesReplayMetadata {
                response_id: Some("resp-secret".into()),
                item_id: Some("item-secret".into()),
                message_id: Some("message-secret".into()),
                call_id: Some("call-secret".into()),
                encrypted_reasoning: Some(responses_secret.into()),
            }),
        };
        let event = crate::llm::GenerationEvent::ReasoningFinished {
            content_index: 0,
            id: "reasoning".into(),
            replay: Some(metadata.clone()),
        };

        for formatted in [
            format!("{metadata:?}"),
            format!("{:?}", metadata.chat.as_ref().expect("chat metadata")),
            format!(
                "{:?}",
                metadata.responses.as_ref().expect("Responses metadata")
            ),
            format!("{event:?}"),
        ] {
            assert!(formatted.contains("[REDACTED]"));
            assert!(!formatted.contains(chat_secret));
            assert!(!formatted.contains(responses_secret));
            assert!(!formatted.contains("resp-secret"));
            assert!(!formatted.contains("item-secret"));
            assert!(!formatted.contains("message-secret"));
            assert!(!formatted.contains("call-secret"));
        }
    }
}
