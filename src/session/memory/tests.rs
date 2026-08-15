use super::*;
use crate::{
    llm::{ContentBlock, Message, ModelSelection, Role, Usage},
    session::{Compaction, ConfigChange, MessageEntry, SessionDomain},
};

fn message(text: &str) -> SessionEntryKind {
    SessionEntryKind::Message(MessageEntry {
        message: Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.into(),
                provider_metadata: Default::default(),
            }],
            provider_metadata: Default::default(),
        },
        turn_id: None,
        model: None,
        usage: Usage::default(),
    })
}

#[test]
fn create_with_entries_rejects_the_whole_invalid_session() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    let invalid = SessionEntryKind::Header(SessionHeader::new(SessionDomain::Chat, None));

    assert!(matches!(
        store.create_session_with_entries(header, vec![invalid]),
        Err(SessionError::InvalidEntryKind)
    ));
    assert!(!store.contains(&id));
}

#[test]
fn creates_appends_loads_and_flushes() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let ids = store
        .append(&id, vec![message("hello"), message("world")])
        .expect("append");
    let state = store.load_session(&id, Some(&ids[1])).expect("load");
    assert_eq!(state.messages.len(), 2);
    store.flush().expect("flush");
    store.shutdown().expect("shutdown");
}

#[test]
fn explicit_leaf_switch_does_not_change_history_facts() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let first = store.append(&id, vec![message("first")]).expect("append")[0].clone();
    let _second = store.append(&id, vec![message("second")]).expect("append");
    let facts_before_switch = store.entries(&id).expect("entries").len();
    store.set_leaf(&id, Some(&first)).expect("switch leaf");
    let state = store.load_session(&id, None).expect("load");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].entry_id, first);
    let facts = store.entries(&id).expect("entries");
    assert_eq!(facts.len(), facts_before_switch + 1);
    assert!(matches!(
        facts.last().map(|entry| &entry.kind),
        Some(SessionEntryKind::Leaf(_))
    ));
}

#[test]
fn compaction_admission_requires_an_active_message_target() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let active = store.append(&id, vec![message("active")]).expect("active")[0].clone();
    let inactive = store
        .append(&id, vec![message("inactive")])
        .expect("inactive")[0]
        .clone();
    store.set_leaf(&id, Some(&active)).expect("rewind");

    let facts_before = store.entries(&id).expect("facts").len();
    let off_path = SessionEntryKind::Compaction(Compaction {
        summary: "must not persist".into(),
        first_kept_entry_id: inactive.clone(),
        tokens_before: 10,
    });
    assert!(matches!(
        store.append(&id, vec![off_path]),
        Err(SessionError::InvalidCompactionTarget(target)) if target == inactive
    ));
    assert_eq!(store.entries(&id).expect("facts").len(), facts_before);

    let config = store
        .append(
            &id,
            vec![SessionEntryKind::ConfigChange(ConfigChange {
                model: ModelSelection {
                    profile_id: "profile".into(),
                    model_id: "model".into(),
                },
                system_prompt: None,
            })],
        )
        .expect("config")[0]
        .clone();
    let facts_before = store.entries(&id).expect("facts").len();
    let non_message = SessionEntryKind::Compaction(Compaction {
        summary: "must not persist".into(),
        first_kept_entry_id: config.clone(),
        tokens_before: 10,
    });
    assert!(matches!(
        store.append(&id, vec![non_message]),
        Err(SessionError::InvalidCompactionTarget(target)) if target == config
    ));
    assert_eq!(store.entries(&id).expect("facts").len(), facts_before);

    let continuation = store
        .append(&id, vec![message("continuation")])
        .expect("append after rejection")[0]
        .clone();
    store
        .append(
            &id,
            vec![SessionEntryKind::Compaction(Compaction {
                summary: "valid summary".into(),
                first_kept_entry_id: continuation.clone(),
                tokens_before: 10,
            })],
        )
        .expect("active message compaction");
    let restored = store.load_session(&id, None).expect("restored session");
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].entry_id, continuation);
}
