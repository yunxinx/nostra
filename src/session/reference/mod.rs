use std::{fmt, io::Write};

use serde::{
    Deserialize, Serialize,
    ser::{SerializeSeq, SerializeStruct, SerializeStructVariant},
};
use thiserror::Error;

use crate::llm::{ContentBlock, Message, Role};

use super::{
    CatalogError, ChatMessageRef, EntryId, SessionEntryKind, SessionError, SessionId,
    SessionSummary,
};

pub const DEFAULT_REFERENCE_PAGE_SIZE: usize = 30;
pub const MAX_REFERENCE_MESSAGE_BYTES: usize = 50 * 1024;
const MAX_SEARCHABLE_TEXT_CHARS: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageSearchCursor {
    pub timestamp: i64,
    pub session_id: SessionId,
    pub entry_id: EntryId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessageSearchQuery {
    pub text: String,
    pub cursor: Option<ChatMessageSearchCursor>,
    pub limit: usize,
}

impl ChatMessageSearchQuery {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cursor: None,
            limit: DEFAULT_REFERENCE_PAGE_SIZE,
        }
    }

    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.clamp(1, DEFAULT_REFERENCE_PAGE_SIZE);
        self
    }

    #[must_use]
    pub(crate) fn bounded_limit(&self) -> usize {
        self.limit.clamp(1, DEFAULT_REFERENCE_PAGE_SIZE)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageSearchPage {
    pub messages: Vec<ChatMessagePreview>,
    pub next_cursor: Option<ChatMessageSearchCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessagePreview {
    pub reference: ChatMessageRef,
    /// `None` preserves the absence of a durable title; picker UI supplies a
    /// localized placeholder for the active locale when it renders the row.
    pub session_title: Option<String>,
    pub session_created_at: i64,
    pub timestamp: i64,
    pub role: Role,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferencedMessage {
    pub role: Role,
    pub content: Vec<ReferencedContentBlock>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReferencedContentBlock {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl ReferencedMessage {
    #[must_use]
    pub fn from_message(message: &Message) -> Self {
        let content = message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => ReferencedContentBlock::Text(text.clone()),
                ContentBlock::Reasoning { reasoning } => {
                    ReferencedContentBlock::Reasoning(reasoning.display.clone())
                }
                ContentBlock::ToolCall { tool_call } => ReferencedContentBlock::ToolCall {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.clone(),
                },
                ContentBlock::ToolResult { tool_result } => ReferencedContentBlock::ToolResult {
                    call_id: tool_result.call_id.clone(),
                    content: tool_result.content.clone(),
                    is_error: tool_result.is_error,
                },
            })
            .collect();
        Self {
            role: message.role,
            content,
        }
    }

    #[must_use]
    pub fn searchable_text(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
            if !text.is_empty() {
                text.push('\n');
            }
            match block {
                ReferencedContentBlock::Text(value)
                | ReferencedContentBlock::Reasoning(value)
                | ReferencedContentBlock::ToolResult { content: value, .. } => text.push_str(value),
                ReferencedContentBlock::ToolCall {
                    name, arguments, ..
                } => {
                    text.push_str(name);
                    text.push('\n');
                    text.push_str(&arguments.to_string());
                }
            }
        }
        text.chars().take(16_384).collect()
    }

    #[must_use]
    pub fn preview(&self) -> Option<String> {
        let text = self.searchable_text();
        let text = text.trim();
        (!text.is_empty()).then(|| text.chars().take(512).collect())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessageRead {
    pub reference: ChatMessageRef,
    /// Presentation fallback text is intentionally not part of the reference
    /// contract, so an untitled source remains `None` across storage backends.
    pub session_title: Option<String>,
    pub session_created_at: i64,
    pub timestamp: i64,
    pub message: ReferencedMessage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatMessageUnavailableReason {
    SessionDeleted,
    MessageDeleted,
    SourceCorrupt,
}

impl fmt::Display for ChatMessageUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SessionDeleted => "source Chat session was deleted",
            Self::MessageDeleted => "source Chat message was deleted",
            Self::SourceCorrupt => "source Chat transcript is corrupt or unavailable",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageUnavailable {
    pub reference: ChatMessageRef,
    pub reason: ChatMessageUnavailableReason,
}

impl fmt::Display for ChatMessageUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{} ({})",
            self.reference.session_id, self.reference.entry_id, self.reason
        )
    }
}

#[derive(Debug, Error)]
pub enum ChatReferenceError {
    #[error("invalid Chat message reference: {0}")]
    InvalidReference(#[from] SessionError),
    #[error("Chat message is unavailable: {0}")]
    Unavailable(ChatMessageUnavailable),
    #[error("Chat reference search failed: {0}")]
    Catalog(#[from] CatalogError),
    #[error("Chat reference read failed: {0}")]
    Storage(#[source] SessionError),
    #[error("Chat message exceeds the {limit}-byte reference limit")]
    TooLarge { limit: usize },
}

pub trait ChatMessageReferenceStore {
    fn search_chat_messages(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError>;
    fn read_chat_message(
        &self,
        reference: &ChatMessageRef,
    ) -> Result<ChatMessageRead, ChatReferenceError>;
}

/// Read-only adapter exposed to a future Agent tool runtime. It owns only the
/// Chat capability and therefore cannot append or mutate either session domain.
pub struct AgentChatReferenceTool<S> {
    chat_store: S,
}

impl<S> AgentChatReferenceTool<S>
where
    S: ChatMessageReferenceStore,
{
    #[must_use]
    pub fn new(chat_store: S) -> Self {
        Self { chat_store }
    }

    pub fn search(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError> {
        self.chat_store.search_chat_messages(query)
    }

    pub fn read(&self, reference: &ChatMessageRef) -> Result<ChatMessageRead, ChatReferenceError> {
        self.chat_store.read_chat_message(reference)
    }
}

pub(crate) fn preview_from_node(
    session_id: SessionId,
    entry_id: EntryId,
    timestamp: i64,
    session_title: Option<String>,
    session_created_at: i64,
    role: Role,
    preview: Option<String>,
) -> ChatMessagePreview {
    ChatMessagePreview {
        reference: ChatMessageRef {
            session_id,
            entry_id,
        },
        session_title,
        session_created_at,
        timestamp,
        role,
        preview,
    }
}

pub(crate) fn unavailable(
    reference: &ChatMessageRef,
    reason: ChatMessageUnavailableReason,
) -> ChatReferenceError {
    ChatReferenceError::Unavailable(ChatMessageUnavailable {
        reference: reference.clone(),
        reason,
    })
}

pub(crate) fn validate_reference(reference: &ChatMessageRef) -> Result<(), ChatReferenceError> {
    reference
        .validate()
        .map_err(ChatReferenceError::InvalidReference)
}

pub(crate) fn message_from_entry(
    reference: &ChatMessageRef,
    summary: &SessionSummary,
    entry: &super::SessionEntry,
) -> Result<ChatMessageRead, ChatReferenceError> {
    let SessionEntryKind::Message(message) = &entry.kind else {
        return Err(unavailable(
            reference,
            ChatMessageUnavailableReason::MessageDeleted,
        ));
    };
    // Measure the complete tool result, including its envelope, before cloning
    // any message body. Limiting only the nested message would still allow the
    // serialized reference metadata to push the actual Agent input over the
    // documented byte budget.
    bounded_chat_message_read(reference, summary, entry.timestamp, &message.message)
}

fn bounded_chat_message_read(
    reference: &ChatMessageRef,
    summary: &SessionSummary,
    timestamp: i64,
    message: &Message,
) -> Result<ChatMessageRead, ChatReferenceError> {
    let borrowed = BorrowedChatMessageRead {
        reference,
        session_title: summary.title.as_deref(),
        session_created_at: summary.created_at,
        timestamp,
        message,
    };
    ensure_reference_budget(&borrowed)?;
    Ok(ChatMessageRead {
        reference: reference.clone(),
        session_title: summary.title.clone(),
        session_created_at: summary.created_at,
        timestamp,
        message: ReferencedMessage::from_message(message),
    })
}

pub(crate) fn searchable_text_from_message(message: &Message) -> String {
    let mut text = String::new();
    let mut remaining = MAX_SEARCHABLE_TEXT_CHARS;
    for block in &message.content {
        if remaining == 0 {
            break;
        }
        if !text.is_empty() {
            text.push('\n');
            remaining = remaining.saturating_sub(1);
        }
        let value = match block {
            ContentBlock::Text { text, .. } => text.as_str(),
            ContentBlock::Reasoning { reasoning } => reasoning.display.as_str(),
            ContentBlock::ToolCall { tool_call } => tool_call.name.as_str(),
            ContentBlock::ToolResult { tool_result } => tool_result.content.as_str(),
        };
        for character in value.chars().take(remaining) {
            text.push(character);
            remaining -= 1;
        }
    }
    text
}

fn ensure_reference_budget(value: &impl Serialize) -> Result<(), ChatReferenceError> {
    let mut counter = ByteBudgetWriter::new(MAX_REFERENCE_MESSAGE_BYTES);
    if let Err(error) = serde_json::to_writer(&mut counter, value) {
        if counter.exceeded {
            return Err(ChatReferenceError::TooLarge {
                limit: MAX_REFERENCE_MESSAGE_BYTES,
            });
        }
        return Err(ChatReferenceError::Storage(SessionError::io(
            std::io::Error::other(error),
        )));
    }
    Ok(())
}

struct ByteBudgetWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl ByteBudgetWriter {
    fn new(limit: usize) -> Self {
        Self {
            written: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for ByteBudgetWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(std::io::Error::other("reference byte budget exceeded"));
        }
        self.written += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct BorrowedReferencedMessage<'a>(&'a Message);

impl Serialize for BorrowedReferencedMessage<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("ReferencedMessage", 2)?;
        state.serialize_field("role", &self.0.role)?;
        state.serialize_field("content", &BorrowedContentBlocks(&self.0.content))?;
        state.end()
    }
}

struct BorrowedChatMessageRead<'a> {
    reference: &'a ChatMessageRef,
    session_title: Option<&'a str>,
    session_created_at: i64,
    timestamp: i64,
    message: &'a Message,
}

impl Serialize for BorrowedChatMessageRead<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("ChatMessageRead", 5)?;
        state.serialize_field("reference", self.reference)?;
        state.serialize_field("session_title", &self.session_title)?;
        state.serialize_field("session_created_at", &self.session_created_at)?;
        state.serialize_field("timestamp", &self.timestamp)?;
        state.serialize_field("message", &BorrowedReferencedMessage(self.message))?;
        state.end()
    }
}

struct BorrowedContentBlocks<'a>(&'a [ContentBlock]);

impl Serialize for BorrowedContentBlocks<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for block in self.0 {
            sequence.serialize_element(&BorrowedContentBlock(block))?;
        }
        sequence.end()
    }
}

struct BorrowedContentBlock<'a>(&'a ContentBlock);

impl Serialize for BorrowedContentBlock<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            ContentBlock::Text { text, .. } => {
                serializer.serialize_newtype_variant("ReferencedContentBlock", 0, "Text", text)
            }
            ContentBlock::Reasoning { reasoning } => serializer.serialize_newtype_variant(
                "ReferencedContentBlock",
                1,
                "Reasoning",
                &reasoning.display,
            ),
            ContentBlock::ToolCall { tool_call } => {
                let mut state = serializer.serialize_struct_variant(
                    "ReferencedContentBlock",
                    2,
                    "ToolCall",
                    3,
                )?;
                state.serialize_field("id", &tool_call.id)?;
                state.serialize_field("name", &tool_call.name)?;
                state.serialize_field("arguments", &tool_call.arguments)?;
                state.end()
            }
            ContentBlock::ToolResult { tool_result } => {
                let mut state = serializer.serialize_struct_variant(
                    "ReferencedContentBlock",
                    3,
                    "ToolResult",
                    3,
                )?;
                state.serialize_field("call_id", &tool_result.call_id)?;
                state.serialize_field("content", &tool_result.content)?;
                state.serialize_field("is_error", &tool_result.is_error)?;
                state.end()
            }
        }
    }
}

#[cfg(test)]
mod tests;
