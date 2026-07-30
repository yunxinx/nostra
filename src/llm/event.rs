//! Canonical streaming events and terminal generation outcomes.
//!
//! Protocol adapters emit this vocabulary so gateway observers and GPUI code do
//! not depend on provider JSON or SSE event names.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    ContentBlock, GatewayError, Message, Protocol, ProviderMetadata, Role, ToolCall, Usage,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMetadata {
    pub response_id: Option<String>,
    pub upstream_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GenerationEvent {
    Started(StreamMetadata),
    TextStarted {
        content_index: usize,
        id: String,
    },
    TextDelta {
        content_index: usize,
        id: String,
        delta: String,
    },
    TextFinished {
        content_index: usize,
        id: String,
        replay: Option<super::ReplayMetadata>,
    },
    ReasoningStarted {
        content_index: usize,
        id: String,
    },
    ReasoningDelta {
        content_index: usize,
        id: String,
        delta: String,
    },
    ReasoningFinished {
        content_index: usize,
        id: String,
        replay: Option<super::ReplayMetadata>,
    },
    /// Replaces canonical reasoning data that arrived after its visible block
    /// finished, without reopening the block's UI lifecycle.
    ReasoningSnapshotUpdated {
        content_index: usize,
        id: String,
        reasoning: super::ReasoningContent,
    },
    ToolCallStarted {
        content_index: usize,
        index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        content_index: usize,
        index: usize,
        delta: String,
    },
    ToolCallFinished {
        content_index: usize,
        index: usize,
        tool_call: Box<ToolCall>,
    },
    UsageUpdated(Usage),
    Finished(Box<GenerationOutcome>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Incomplete(String),
    Other(String),
}

#[derive(Clone, PartialEq)]
pub struct GenerationOutcome {
    pub request_id: String,
    pub profile_id: String,
    pub model_id: String,
    pub protocol: Protocol,
    pub status: OutcomeStatus,
    pub finish_reason: Option<FinishReason>,
    pub usage: Usage,
    pub response_id: Option<String>,
    pub upstream_model: Option<String>,
    pub time_to_first_event: Option<Duration>,
    pub latency: Duration,
    pub message: Option<IndexedMessage>,
    pub error: Option<GatewayError>,
}

/// Authoritative terminal message with each canonical block bound to the
/// protocol slot that identified it during streaming.
///
/// The slot belongs to generation lifecycle rather than provider replay, so it
/// is kept out of [`Message`]. Binding it to each block avoids a parallel index
/// vector whose length could silently diverge from `content`.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexedMessage {
    pub role: Role,
    pub content: Vec<IndexedContentBlock>,
    pub provider_metadata: ProviderMetadata,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexedContentBlock {
    pub content_index: usize,
    pub block: ContentBlock,
}

impl IndexedMessage {
    pub fn from_message(message: Message) -> Self {
        Self {
            role: message.role,
            content: message
                .content
                .into_iter()
                .enumerate()
                .map(|(content_index, block)| IndexedContentBlock {
                    content_index,
                    block,
                })
                .collect(),
            provider_metadata: message.provider_metadata,
        }
    }

    pub fn into_message(self) -> Message {
        Message {
            role: self.role,
            content: self.content.into_iter().map(|part| part.block).collect(),
            provider_metadata: self.provider_metadata,
        }
    }
}

impl std::fmt::Debug for GenerationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenerationOutcome")
            .field("request_id", &self.request_id)
            .field("profile_id", &self.profile_id)
            .field("model_id", &self.model_id)
            .field("protocol", &self.protocol)
            .field("status", &self.status)
            .field("finish_reason", &self.finish_reason)
            .field("usage", &self.usage)
            .field("response_id", &self.response_id)
            .field("upstream_model", &self.upstream_model)
            .field("time_to_first_event", &self.time_to_first_event)
            .field("latency", &self.latency)
            .field("message", &self.message.as_ref().map(|_| "[REDACTED]"))
            .field("error", &self.error)
            .finish()
    }
}
