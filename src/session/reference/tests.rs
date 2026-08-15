use std::fs::OpenOptions;
use std::io::Write;

use super::*;
use crate::llm::{
    ChatReasoningField, ChatReplayMetadata, ContentBlock, Message, ProviderMetadata,
    ResponsesReplayMetadata, Role, ToolCall, ToolResult, Usage,
};
use crate::session::{
    InMemorySessionStore, LocalSessionStore, LocalStoreConfig, Reference, SessionDomain,
    SessionEntryKind, SessionFlushStore, SessionHeader, SessionLifecycleStore, SessionStore,
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

fn exercise_active_leaf_search<S>(mut store: S)
where
    S: SessionStore + ChatMessageReferenceStore,
{
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create session");
    let kept = store
        .append(&session_id, vec![sensitive_message("kept")])
        .expect("append kept")[0]
        .clone();
    let hidden = store
        .append(&session_id, vec![sensitive_message("stale secret")])
        .expect("append hidden")[0]
        .clone();
    store
        .set_leaf(&session_id, Some(&kept))
        .expect("rewind active leaf");

    assert!(
        store
            .search_chat_messages(ChatMessageSearchQuery::new("stale secret"))
            .expect("search active path")
            .messages
            .is_empty()
    );
    let hidden_reference = ChatMessageRef::new(session_id, hidden).expect("reference");
    assert!(matches!(
        store.read_chat_message(&hidden_reference),
        Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
            reason: ChatMessageUnavailableReason::MessageDeleted,
            ..
        }))
    ));
}

#[test]
fn search_and_read_follow_the_active_leaf() {
    exercise_active_leaf_search(InMemorySessionStore::new());
    let root = tempfile::tempdir().expect("tempdir");
    exercise_active_leaf_search(
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open local Chat store"),
    );
}

#[test]
fn local_and_memory_search_use_unicode_case_insensitive_matching() {
    fn exercise<S>(mut store: S)
    where
        S: SessionStore + ChatMessageReferenceStore,
    {
        let (session_id, _) = create_chat(&mut store, "МОСКВА");
        let page = store
            .search_chat_messages(ChatMessageSearchQuery::new("москва"))
            .expect("unicode search");
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].reference.session_id, session_id);
    }

    exercise(InMemorySessionStore::new());
    let root = tempfile::tempdir().expect("tempdir");
    exercise(
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open local Chat store"),
    );
}

#[test]
fn oversized_reference_reads_are_rejected_but_search_stays_bounded() {
    fn exercise<S>(mut store: S)
    where
        S: SessionStore + ChatMessageReferenceStore,
    {
        let (session_id, entry_id) =
            create_chat(&mut store, &"x".repeat(MAX_REFERENCE_MESSAGE_BYTES));
        let reference = ChatMessageRef::new(session_id, entry_id).expect("reference");
        assert!(matches!(
            store.read_chat_message(&reference),
            Err(ChatReferenceError::TooLarge { .. })
        ));
        let search = store
            .search_chat_messages(ChatMessageSearchQuery::new("xxxx"))
            .expect("bounded search projection");
        assert_eq!(search.messages.len(), 1);
        assert!(
            search.messages[0]
                .preview
                .as_ref()
                .is_some_and(|preview| preview.chars().count() <= 512)
        );
    }

    exercise(InMemorySessionStore::new());
    let root = tempfile::tempdir().expect("tempdir");
    exercise(
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open local Chat store"),
    );
}

#[test]
fn borrowed_reference_budget_matches_the_owned_redacted_shape() {
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "text".into(),
                provider_metadata: Default::default(),
            },
            ContentBlock::Reasoning {
                reasoning: crate::llm::ReasoningContent {
                    display: "reasoning".into(),
                    replay: None,
                },
            },
            ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                    raw_arguments: "ignored raw arguments".into(),
                    provider_metadata: Default::default(),
                },
            },
            ContentBlock::ToolResult {
                tool_result: ToolResult {
                    call_id: "call-1".into(),
                    content: "result".into(),
                    is_error: false,
                },
            },
        ],
        provider_metadata: Default::default(),
    };
    let borrowed = serde_json::to_vec(&BorrowedReferencedMessage(&message))
        .expect("borrowed redacted projection");
    let owned = serde_json::to_vec(&ReferencedMessage::from_message(&message))
        .expect("owned redacted projection");
    assert_eq!(borrowed, owned);
}

#[test]
fn reference_budget_covers_the_complete_tool_result() {
    let empty = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: String::new(),
            provider_metadata: Default::default(),
        }],
        provider_metadata: Default::default(),
    };
    let fixed_message_bytes = serde_json::to_vec(&ReferencedMessage::from_message(&empty))
        .expect("empty referenced message")
        .len();
    let body_len = MAX_REFERENCE_MESSAGE_BYTES
        .checked_sub(fixed_message_bytes + 1)
        .expect("reference envelope leaves room for a body");

    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create Chat session");
    let entry_id = store
        .append(
            &session_id,
            vec![SessionEntryKind::Message(crate::session::MessageEntry {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "x".repeat(body_len),
                        provider_metadata: Default::default(),
                    }],
                    provider_metadata: Default::default(),
                },
                turn_id: None,
                model: None,
                usage: Usage::default(),
            })],
        )
        .expect("append boundary message")[0]
        .clone();
    let reference = ChatMessageRef::new(session_id, entry_id).expect("reference");

    assert!(matches!(
        store.read_chat_message(&reference),
        Err(ChatReferenceError::TooLarge {
            limit: MAX_REFERENCE_MESSAGE_BYTES
        })
    ));
}

#[test]
fn unavailable_references_are_typed_for_deleted_sources_and_corruption() {
    let memory = InMemorySessionStore::new();
    let missing_session = ChatMessageRef::new(SessionId::new(SessionDomain::Chat), EntryId::new())
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
    assert!(matches!(
        local.shutdown(),
        Err(SessionError::CorruptLine { .. })
    ));
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
