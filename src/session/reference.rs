use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::llm::{ContentBlock, Message, Role};

use super::{
    CatalogError, ChatMessageRef, EntryId, SessionEntryKind, SessionError, SessionId,
    SessionSummary,
};

pub const DEFAULT_REFERENCE_PAGE_SIZE: usize = 30;

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
    pub session_title: String,
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
    pub session_title: String,
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
    session_title: String,
    session_created_at: i64,
    message: ReferencedMessage,
) -> ChatMessagePreview {
    ChatMessagePreview {
        reference: ChatMessageRef {
            session_id,
            entry_id,
        },
        session_title,
        session_created_at,
        timestamp,
        role: message.role,
        preview: message.preview(),
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
    Ok(ChatMessageRead {
        reference: reference.clone(),
        session_title: summary.title.clone(),
        session_created_at: summary.created_at,
        timestamp: entry.timestamp,
        message: ReferencedMessage::from_message(&message.message),
    })
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::*;
    use crate::llm::{
        ChatReasoningField, ChatReplayMetadata, ContentBlock, Message, ProviderMetadata,
        ResponsesReplayMetadata, Role, Usage,
    };
    use crate::session::{
        InMemorySessionStore, LocalSessionStore, LocalStoreConfig, Reference, SessionDomain,
        SessionEntryKind, SessionFlushStore, SessionHeader, SessionStore,
    };

    fn sensitive_message(text: &str) -> SessionEntryKind {
        SessionEntryKind::Message(crate::session::MessageEntry {
            message: Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: text.into(),
                        provider_metadata: ProviderMetadata {
                            chat: Some(ChatReplayMetadata {
                                reasoning_field: Some(ChatReasoningField::Reasoning),
                                reasoning_details: Some(serde_json::json!({
                                    "provider_key": "provider-secret"
                                })),
                            }),
                            responses: None,
                        },
                    },
                    ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "visible reasoning".into(),
                            replay: Some(ProviderMetadata {
                                chat: None,
                                responses: Some(ResponsesReplayMetadata {
                                    encrypted_reasoning: Some("opaque-transport-secret".into()),
                                    ..Default::default()
                                }),
                            }),
                        },
                    },
                ],
                provider_metadata: ProviderMetadata::default(),
            },
            turn_id: None,
            model: None,
            usage: Usage::default(),
        })
    }

    fn create_chat<S: SessionStore>(store: &mut S, text: &str) -> (SessionId, EntryId) {
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create Chat session");
        let entry_id = store
            .append(&session_id, vec![sensitive_message(text)])
            .expect("append Chat message")[0]
            .clone();
        (session_id, entry_id)
    }

    fn exercise_exact_read_and_redaction<S>(mut store: S)
    where
        S: SessionStore + ChatMessageReferenceStore,
    {
        let (session_id, entry_id) = create_chat(&mut store, "canonical discussion");
        let reference = ChatMessageRef::new(session_id, entry_id).expect("Chat reference");
        let read = store.read_chat_message(&reference).expect("read reference");
        assert_eq!(read.message.role, Role::Assistant);
        assert_eq!(read.message.content.len(), 2);
        let encoded = serde_json::to_string(&read.message).expect("safe message JSON");
        assert!(encoded.contains("canonical discussion"));
        assert!(encoded.contains("visible reasoning"));
        assert!(!encoded.contains("provider-secret"));
        assert!(!encoded.contains("opaque-transport-secret"));
        assert!(!encoded.contains("raw upstream"));
    }

    #[test]
    fn memory_and_local_exact_read_share_safe_canonical_projection() {
        exercise_exact_read_and_redaction(InMemorySessionStore::new());
        let root = tempfile::tempdir().expect("tempdir");
        exercise_exact_read_and_redaction(
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open local Chat store"),
        );
    }

    fn exercise_bounded_search<S>(mut store: S)
    where
        S: SessionStore + ChatMessageReferenceStore,
    {
        for index in 0..35 {
            let header = SessionHeader::new(SessionDomain::Chat, None);
            let session_id = header.session_id.clone();
            store.create_session(header).expect("create search session");
            store
                .append(
                    &session_id,
                    vec![SessionEntryKind::Message(crate::session::MessageEntry {
                        message: Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: format!("searchable item {index}"),
                                provider_metadata: Default::default(),
                            }],
                            provider_metadata: Default::default(),
                        },
                        turn_id: None,
                        model: None,
                        usage: Usage::default(),
                    })],
                )
                .expect("append search message");
        }
        let first = store
            .search_chat_messages(ChatMessageSearchQuery::new("searchable"))
            .expect("first search page");
        assert_eq!(first.messages.len(), DEFAULT_REFERENCE_PAGE_SIZE);
        let second = store
            .search_chat_messages(ChatMessageSearchQuery {
                cursor: first.next_cursor.clone(),
                ..ChatMessageSearchQuery::new("searchable")
            })
            .expect("second search page");
        assert_eq!(second.messages.len(), 5);
        let first_ids = first
            .messages
            .iter()
            .map(|message| message.reference.clone())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            second
                .messages
                .iter()
                .all(|message| !first_ids.contains(&message.reference))
        );
        assert!(first.messages.windows(2).all(|pair| {
            (
                pair[0].timestamp,
                &pair[0].reference.session_id,
                &pair[0].reference.entry_id,
            ) >= (
                pair[1].timestamp,
                &pair[1].reference.session_id,
                &pair[1].reference.entry_id,
            )
        }));
    }

    #[test]
    fn memory_and_local_search_are_bounded_and_deterministically_ordered() {
        exercise_bounded_search(InMemorySessionStore::new());
        let root = tempfile::tempdir().expect("tempdir");
        exercise_bounded_search(
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open local Chat store"),
        );
    }

    #[test]
    fn unavailable_references_are_typed_for_deleted_sources_and_corruption() {
        let memory = InMemorySessionStore::new();
        let missing_session =
            ChatMessageRef::new(SessionId::new(SessionDomain::Chat), EntryId::new())
                .expect("Chat reference");
        assert!(matches!(
            memory.read_chat_message(&missing_session),
            Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
                reason: ChatMessageUnavailableReason::SessionDeleted,
                ..
            }))
        ));

        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut local = LocalSessionStore::open(config).expect("open local Chat store");
        let (deleted_session, deleted_entry) = create_chat(&mut local, "deleted session");
        let deleted_reference =
            ChatMessageRef::new(deleted_session.clone(), deleted_entry).expect("Chat reference");
        local
            .delete_session(&deleted_session)
            .expect("delete source session");
        assert!(matches!(
            local.read_chat_message(&deleted_reference),
            Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
                reason: ChatMessageUnavailableReason::SessionDeleted,
                ..
            }))
        ));

        let (session_id, _deleted_entry) = create_chat(&mut local, "message deleted");
        let missing_message =
            ChatMessageRef::new(session_id.clone(), EntryId::new()).expect("Chat reference");
        assert!(matches!(
            local.read_chat_message(&missing_message),
            Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
                reason: ChatMessageUnavailableReason::MessageDeleted,
                ..
            }))
        ));
        let (session_id, entry_id) = create_chat(&mut local, "corrupt me");
        let path = local
            .get_summary(&session_id)
            .expect("summary")
            .expect("session row")
            .jsonl_path
            .clone();
        OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open source")
            .write_all(b"{not-json}\n")
            .expect("corrupt source");
        let reference = ChatMessageRef::new(session_id, entry_id).expect("Chat reference");
        assert!(matches!(
            local.read_chat_message(&reference),
            Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
                reason: ChatMessageUnavailableReason::SourceCorrupt,
                ..
            }))
        ));
        local.shutdown().expect("shutdown");
    }

    #[test]
    fn agent_reference_entry_keeps_only_the_source_pointer() {
        let mut chat = InMemorySessionStore::new();
        let (chat_id, entry_id) = create_chat(&mut chat, "do not copy this body");
        let reference = ChatMessageRef::new(chat_id, entry_id).expect("Chat reference");
        let entry = SessionEntryKind::Reference(Reference {
            source: reference,
            label: Some("discussion".into()),
        });
        let encoded = serde_json::to_string(&entry).expect("reference JSON");
        assert!(encoded.contains("session_id"));
        assert!(encoded.contains("entry_id"));
        assert!(!encoded.contains("do not copy this body"));
    }

    #[test]
    fn reference_tool_is_read_only_and_rejects_agent_domain_sources() {
        let mut chat = InMemorySessionStore::new();
        let (session_id, entry_id) = create_chat(&mut chat, "tool search");
        let tool = AgentChatReferenceTool::new(chat);
        let page = tool
            .search(ChatMessageSearchQuery::new("tool"))
            .expect("search through tool");
        assert_eq!(page.messages.len(), 1);
        let reference = ChatMessageRef::new(session_id, entry_id).expect("Chat reference");
        assert!(tool.read(&reference).is_ok());

        let agent_reference = ChatMessageRef {
            session_id: SessionId::new(SessionDomain::Agent),
            entry_id: EntryId::new(),
        };
        assert!(matches!(
            tool.read(&agent_reference),
            Err(ChatReferenceError::InvalidReference(
                SessionError::ReferenceSourceNotChat
            ))
        ));
    }
}
