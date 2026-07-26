//! Canonical streaming events and terminal generation outcomes.
//!
//! Protocol adapters emit this vocabulary so gateway observers and GPUI code do
//! not depend on provider JSON or SSE event names.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{GatewayError, Message, Protocol, ToolCall, Usage};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMetadata {
    pub response_id: Option<String>,
    pub upstream_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GenerationEvent {
    Started(StreamMetadata),
    TextStarted {
        id: String,
    },
    TextDelta {
        id: String,
        delta: String,
    },
    TextFinished {
        id: String,
        replay: Option<super::ReplayMetadata>,
    },
    ReasoningStarted {
        id: String,
    },
    ReasoningDelta {
        id: String,
        delta: String,
    },
    ReasoningFinished {
        id: String,
        replay: Option<super::ReplayMetadata>,
    },
    ToolCallStarted {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    ToolCallFinished {
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
    pub message: Option<Message>,
    pub error: Option<GatewayError>,
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
