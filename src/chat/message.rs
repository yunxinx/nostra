use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

pub enum MessagePart {
    Text {
        content_index: usize,
        ui_id: u64,
        id: String,
        text: String,
        replay: ProviderMetadata,
        finished: bool,
        body: MarkdownBody,
    },
    Reasoning {
        content_index: usize,
        ui_id: u64,
        id: String,
        reasoning: ReasoningContent,
        finished: bool,
        trace: Option<ReasoningTrace>,
    },
    ToolCall {
        content_index: usize,
        ui_id: u64,
        index: usize,
        id: String,
        name: String,
        tool_call: Option<ToolCall>,
    },
    ToolResult {
        content_index: usize,
        tool_result: ToolResult,
        body: MarkdownBody,
    },
}

pub struct Message {
    pub(super) ui_id: u64,
    pub role: Role,
    pub parts: Vec<MessagePart>,
    pub provider_metadata: ProviderMetadata,
    /// Set when the generation for this turn failed. Rendered as a card below
    /// whatever text streamed before the failure, and deliberately kept out of
    /// the parts so a provider's error text is never replayed as conversation
    /// history on the next turn.
    pub error: Option<TurnError>,
}

impl Message {
    pub(super) fn empty(role: Role) -> Self {
        Self {
            ui_id: NEXT_MESSAGE_UI_ID.fetch_add(1, Ordering::Relaxed),
            role,
            parts: Vec::new(),
            provider_metadata: ProviderMetadata::default(),
            error: None,
        }
    }

    pub(super) fn from_canonical(message: LlmMessage, cx: &mut App) -> Self {
        let role = match message.role {
            crate::llm::Role::Assistant => Role::Assistant,
            _ => Role::User,
        };
        let parts = message
            .content
            .into_iter()
            .enumerate()
            .map(|(index, block)| MessagePart::from_canonical(index, block, cx))
            .collect();
        Self {
            ui_id: NEXT_MESSAGE_UI_ID.fetch_add(1, Ordering::Relaxed),
            role,
            parts,
            provider_metadata: message.provider_metadata,
            error: None,
        }
    }

    pub(super) fn canonical(&self) -> LlmMessage {
        LlmMessage {
            role: match self.role {
                Role::User => crate::llm::Role::User,
                Role::Assistant => crate::llm::Role::Assistant,
            },
            content: self
                .parts
                .iter()
                .filter_map(MessagePart::canonical)
                .collect(),
            provider_metadata: self.provider_metadata.clone(),
        }
    }

    pub(super) fn replace_with_canonical(&mut self, message: IndexedMessage, cx: &mut App) {
        let mut previous = std::mem::take(&mut self.parts)
            .into_iter()
            .map(|part| (part.content_index(), part))
            .collect::<std::collections::BTreeMap<_, _>>();
        self.parts = message
            .content
            .into_iter()
            .map(|part| {
                let old = previous.remove(&part.content_index);
                MessagePart::reconcile(part.content_index, old, part.block, cx)
            })
            .collect();
        self.provider_metadata = message.provider_metadata;
    }

    pub(super) fn finish_reasoning(&mut self, id: Option<&str>) {
        for part in &mut self.parts {
            let MessagePart::Reasoning {
                id: part_id,
                finished,
                trace,
                ..
            } = part
            else {
                continue;
            };
            if id.is_none_or(|id| id == part_id) {
                *finished = true;
                if let Some(trace) = trace {
                    trace.finish();
                }
            }
        }
    }
}

impl MessagePart {
    pub(super) fn from_canonical(index: usize, block: ContentBlock, cx: &mut App) -> Self {
        let ui_id = NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed);
        match block {
            ContentBlock::Text {
                text,
                provider_metadata,
            } => Self::Text {
                content_index: index,
                ui_id,
                id: format!("terminal-text-{index}"),
                body: MarkdownBody::new(&text, ui_id, cx),
                text,
                replay: provider_metadata,
                finished: true,
            },
            ContentBlock::Reasoning { reasoning } => Self::Reasoning {
                content_index: index,
                ui_id,
                id: format!("terminal-reasoning-{index}"),
                finished: true,
                trace: (!reasoning.display.is_empty())
                    .then(|| ReasoningTrace::completed(reasoning.display.clone(), ui_id, cx)),
                reasoning,
            },
            ContentBlock::ToolCall { tool_call } => Self::ToolCall {
                content_index: index,
                ui_id,
                index,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                tool_call: Some(tool_call),
            },
            ContentBlock::ToolResult { tool_result } => Self::ToolResult {
                content_index: index,
                body: MarkdownBody::new(&tool_result.content, ui_id, cx),
                tool_result,
            },
        }
    }

    pub(super) fn canonical(&self) -> Option<ContentBlock> {
        match self {
            Self::Text { text, replay, .. } if !text.is_empty() => Some(ContentBlock::Text {
                text: text.clone(),
                provider_metadata: replay.clone(),
            }),
            Self::Reasoning { reasoning, .. }
                if !reasoning.display.is_empty() || reasoning.replay.is_some() =>
            {
                Some(ContentBlock::Reasoning {
                    reasoning: reasoning.clone(),
                })
            }
            Self::ToolCall {
                tool_call: Some(tool_call),
                ..
            } => Some(ContentBlock::ToolCall {
                tool_call: tool_call.clone(),
            }),
            Self::ToolResult { tool_result, .. } => Some(ContentBlock::ToolResult {
                tool_result: tool_result.clone(),
            }),
            _ => None,
        }
    }

    pub(super) fn content_index(&self) -> usize {
        match self {
            Self::Text { content_index, .. }
            | Self::Reasoning { content_index, .. }
            | Self::ToolCall { content_index, .. }
            | Self::ToolResult { content_index, .. } => *content_index,
        }
    }

    pub(super) fn reconcile(
        index: usize,
        old: Option<Self>,
        block: ContentBlock,
        cx: &mut App,
    ) -> Self {
        match (old, block) {
            (
                Some(Self::Text {
                    ui_id, id, body, ..
                }),
                ContentBlock::Text {
                    text,
                    provider_metadata,
                },
            ) => {
                let mut body = body;
                body.set_text(&text, cx);
                Self::Text {
                    content_index: index,
                    ui_id,
                    id,
                    text,
                    replay: provider_metadata,
                    finished: true,
                    body,
                }
            }
            (
                Some(Self::Reasoning {
                    ui_id,
                    id,
                    trace: Some(mut trace),
                    ..
                }),
                ContentBlock::Reasoning { reasoning },
            ) if !reasoning.display.is_empty() => {
                trace.set_source(&reasoning.display, cx);
                trace.finish();
                Self::Reasoning {
                    content_index: index,
                    ui_id,
                    id,
                    reasoning,
                    finished: true,
                    trace: Some(trace),
                }
            }
            (
                Some(Self::ToolCall {
                    ui_id,
                    index,
                    id,
                    name,
                    ..
                }),
                ContentBlock::ToolCall { tool_call },
            ) => Self::ToolCall {
                content_index: index,
                ui_id,
                index,
                id,
                name: if tool_call.name.is_empty() {
                    name
                } else {
                    tool_call.name.clone()
                },
                tool_call: Some(tool_call),
            },
            (_, block) => Self::from_canonical(index, block, cx),
        }
    }
}
