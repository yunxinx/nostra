//! Canonical turn and part types owned by [`super::Transcript`].

use crate::llm::{
    ContentBlock, GatewayError, IndexedMessage, Message as LlmMessage, ProviderMetadata,
    ReasoningContent, ToolCall, ToolResult,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TurnId(u64);

impl TurnId {
    #[must_use]
    pub(crate) const fn as_u64(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn from_u64_for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PartId(u64);

impl PartId {
    #[must_use]
    pub(crate) const fn as_u64(self) -> u64 {
        self.0
    }

    /// Placeholder for turn-scoped rows (`TurnError`, `TurnActions`, the
    /// synthetic wait placeholder) that do not belong to one part.
    pub(crate) const NONE: PartId = PartId(0);

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn from_u64_for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    #[must_use]
    pub(crate) fn from_llm(role: crate::llm::Role) -> Self {
        match role {
            crate::llm::Role::Assistant => Self::Assistant,
            crate::llm::Role::Tool => Self::Tool,
            crate::llm::Role::User | crate::llm::Role::System | crate::llm::Role::Developer => {
                Self::User
            }
        }
    }

    #[must_use]
    pub(crate) const fn to_llm(self) -> crate::llm::Role {
        match self {
            Self::User => crate::llm::Role::User,
            Self::Assistant => crate::llm::Role::Assistant,
            Self::Tool => crate::llm::Role::Tool,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartKind {
    Prose,
    Reasoning,
    ToolCall,
    ToolResult,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PartSource {
    Prose {
        text: String,
        replay: ProviderMetadata,
        stream_id: String,
    },
    Reasoning {
        reasoning: ReasoningContent,
        stream_id: String,
    },
    ToolCall {
        index: usize,
        id: String,
        name: String,
        tool_call: Option<ToolCall>,
    },
    ToolResult(ToolResult),
}

impl PartSource {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn prose_text(&self) -> Option<&str> {
        match self {
            Self::Prose { text, .. } => Some(text.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> PartKind {
        match self {
            Self::Prose { .. } => PartKind::Prose,
            Self::Reasoning { .. } => PartKind::Reasoning,
            Self::ToolCall { .. } => PartKind::ToolCall,
            Self::ToolResult(_) => PartKind::ToolResult,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Part {
    pub(crate) part_id: PartId,
    pub(crate) content_index: usize,
    pub(crate) source: PartSource,
    pub(crate) finished: bool,
}

impl Part {
    #[must_use]
    pub(super) fn new(
        part_id: PartId,
        content_index: usize,
        source: PartSource,
        finished: bool,
    ) -> Self {
        Self {
            part_id,
            content_index,
            source,
            finished,
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> PartKind {
        self.source.kind()
    }

    #[must_use]
    pub(super) fn from_block(content_index: usize, block: ContentBlock, part_id: PartId) -> Self {
        let (source, finished) = match block {
            ContentBlock::Text {
                text,
                provider_metadata,
            } => (
                PartSource::Prose {
                    stream_id: format!("terminal-text-{content_index}"),
                    text,
                    replay: provider_metadata,
                },
                true,
            ),
            ContentBlock::Reasoning { reasoning } => (
                PartSource::Reasoning {
                    stream_id: format!("terminal-reasoning-{content_index}"),
                    reasoning,
                },
                true,
            ),
            ContentBlock::ToolCall { tool_call } => (
                PartSource::ToolCall {
                    index: content_index,
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    tool_call: Some(tool_call),
                },
                true,
            ),
            ContentBlock::ToolResult { tool_result } => (PartSource::ToolResult(tool_result), true),
        };
        Self::new(part_id, content_index, source, finished)
    }

    #[must_use]
    pub(crate) fn canonical(&self) -> Option<ContentBlock> {
        match &self.source {
            PartSource::Prose { text, replay, .. } if !text.is_empty() => {
                Some(ContentBlock::Text {
                    text: text.clone(),
                    provider_metadata: replay.clone(),
                })
            }
            PartSource::Reasoning { reasoning, .. }
                if !reasoning.display.is_empty() || reasoning.replay.is_some() =>
            {
                Some(ContentBlock::Reasoning {
                    reasoning: reasoning.clone(),
                })
            }
            PartSource::ToolCall {
                tool_call: Some(tool_call),
                ..
            } => Some(ContentBlock::ToolCall {
                tool_call: tool_call.clone(),
            }),
            PartSource::ToolResult(tool_result) => Some(ContentBlock::ToolResult {
                tool_result: tool_result.clone(),
            }),
            _ => None,
        }
    }

    #[must_use]
    pub(super) fn matches_text(&self, content_index: usize, stream_id: &str) -> bool {
        matches!(
            &self.source,
            PartSource::Prose { stream_id: current, .. }
                if self.content_index == content_index && current == stream_id
        )
    }

    #[must_use]
    pub(super) fn matches_reasoning(&self, content_index: usize, stream_id: &str) -> bool {
        matches!(
            &self.source,
            PartSource::Reasoning { stream_id: current, .. }
                if self.content_index == content_index && current == stream_id
        )
    }

    #[must_use]
    pub(super) fn matches_tool_call(&self, content_index: usize) -> bool {
        matches!(
            &self.source,
            PartSource::ToolCall { .. } if self.content_index == content_index
        )
    }

    #[must_use]
    pub(super) fn matches_tool_call_index(&self, content_index: usize, index: usize) -> bool {
        matches!(
            &self.source,
            PartSource::ToolCall { index: current, .. }
                if self.content_index == content_index && *current == index
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Turn {
    pub(crate) turn_id: TurnId,
    pub(crate) role: Role,
    pub(crate) parts: Vec<Part>,
    pub(crate) error: Option<GatewayError>,
    pub(crate) provider_metadata: ProviderMetadata,
}

impl Turn {
    #[must_use]
    pub(super) fn empty(role: Role, turn_id: TurnId) -> Self {
        Self {
            turn_id,
            role,
            parts: Vec::new(),
            error: None,
            provider_metadata: ProviderMetadata::default(),
        }
    }

    #[must_use]
    pub(super) fn from_llm(message: LlmMessage, turn_id: TurnId, next_part_id: &mut u64) -> Self {
        let role = Role::from_llm(message.role);
        let parts = message
            .content
            .into_iter()
            .enumerate()
            .map(|(index, block)| {
                let part_id = allocate_part_id(next_part_id);
                Part::from_block(index, block, part_id)
            })
            .collect();
        Self {
            turn_id,
            role,
            parts,
            error: None,
            provider_metadata: message.provider_metadata,
        }
    }

    #[must_use]
    pub(crate) fn to_llm(&self) -> LlmMessage {
        LlmMessage {
            role: self.role.to_llm(),
            content: self.parts.iter().filter_map(Part::canonical).collect(),
            provider_metadata: self.provider_metadata.clone(),
        }
    }

    pub(super) fn finish_reasoning(&mut self, stream_id: Option<&str>) {
        for part in &mut self.parts {
            let PartSource::Reasoning {
                stream_id: part_stream_id,
                ..
            } = &part.source
            else {
                continue;
            };
            if stream_id.is_none_or(|id| id == part_stream_id) {
                part.finished = true;
            }
        }
    }
}

#[must_use]
pub(super) fn allocate_turn_id(next: &mut u64) -> TurnId {
    let id = TurnId(*next);
    *next = next.saturating_add(1);
    id
}

#[must_use]
pub(super) fn allocate_part_id(next: &mut u64) -> PartId {
    let id = PartId(*next);
    *next = next.saturating_add(1);
    id
}

#[must_use]
pub(crate) fn is_replayable(message: &LlmMessage) -> bool {
    message.role != crate::llm::Role::Assistant || !message.content.is_empty()
}

pub(super) fn apply_indexed_message(
    turn: &mut Turn,
    message: IndexedMessage,
    next_part_id: &mut u64,
) {
    turn.parts = super::reconcile::reconcile_parts(
        std::mem::take(&mut turn.parts),
        message.content,
        next_part_id,
    );
    turn.provider_metadata = message.provider_metadata;
    turn.role = Role::from_llm(message.role);
}
