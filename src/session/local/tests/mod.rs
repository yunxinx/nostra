use super::*;

use crate::llm::{ContentBlock, Message, ModelSelection, Role, Usage};
use crate::session::{
    BranchSummary, ChatMessageRef, ChatMessageUnavailable, ChatSessionController, Compaction,
    ConfigChange, InMemorySessionStore, JsonlWriter, ProjectIdentity, SessionStore,
    TranscriptReplay, TurnResult, TurnStatus,
};
use std::io::Write;

fn message(text: &str) -> SessionEntryKind {
    SessionEntryKind::Message(super::super::MessageEntry {
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

fn message_with_metadata(
    text: &str,
    model: Option<ModelSelection>,
    tokens: u64,
) -> SessionEntryKind {
    SessionEntryKind::Message(super::super::MessageEntry {
        message: Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.into(),
                provider_metadata: Default::default(),
            }],
            provider_metadata: Default::default(),
        },
        turn_id: Some("turn-1".into()),
        model,
        usage: Usage {
            total_tokens: tokens,
            ..Usage::default()
        },
    })
}

fn role_message_with_metadata(
    role: Role,
    text: &str,
    model: ModelSelection,
    tokens: u64,
    turn_id: &str,
) -> SessionEntryKind {
    SessionEntryKind::Message(super::super::MessageEntry {
        message: Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.into(),
                provider_metadata: Default::default(),
            }],
            provider_metadata: Default::default(),
        },
        turn_id: Some(turn_id.into()),
        model: Some(model),
        usage: Usage {
            total_tokens: tokens,
            ..Usage::default()
        },
    })
}

fn exercise_agent_tree_contract<S>(store: &mut S, project: ProjectIdentity) -> SessionId
where
    S: SessionStore + SessionCatalogStore,
{
    let project_id = project.project_id.clone();
    let mut header = SessionHeader::new(SessionDomain::Agent, Some(project));
    header.initial_model = Some(ModelSelection {
        profile_id: "initial-profile".into(),
        model_id: "initial-model".into(),
    });
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create agent session");
    store
        .append(
            &session_id,
            vec![SessionEntryKind::ConfigChange(ConfigChange {
                model: ModelSelection {
                    profile_id: "current-profile".into(),
                    model_id: "current-model".into(),
                },
                system_prompt: Some("current system".into()),
            })],
        )
        .expect("append config");
    let root = store
        .append(&session_id, vec![message("root")])
        .expect("append root")[0]
        .clone();
    let original = store
        .append(&session_id, vec![message("original")])
        .expect("append original")[0]
        .clone();
    store
        .set_leaf(&session_id, Some(&root))
        .expect("select root");
    store
        .append(
            &session_id,
            vec![SessionEntryKind::BranchSummary(BranchSummary {
                from_id: original.clone(),
                summary: "summarized original branch".into(),
            })],
        )
        .expect("append branch summary");
    let replacement = store
        .append(&session_id, vec![message("replacement")])
        .expect("append replacement")[0]
        .clone();
    store
        .append(
            &session_id,
            vec![SessionEntryKind::TranscriptReplay(
                TranscriptReplay::TerminalSnapshot {
                    terminal_id: "terminal-1".into(),
                    title: Some("cargo test".into()),
                    content: "ok".into(),
                },
            )],
        )
        .expect("append transcript replay");
    store
        .append(
            &session_id,
            vec![SessionEntryKind::Compaction(Compaction {
                summary: "older work".into(),
                first_kept_entry_id: replacement.clone(),
                tokens_before: 50,
            })],
        )
        .expect("append compaction");

    let state = store
        .load_session(&session_id, None)
        .expect("restore agent");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].entry_id, replacement);
    assert_eq!(state.transcript_replays.len(), 1);
    assert_eq!(
        state.latest_compaction.expect("compaction").tokens_before,
        50
    );
    assert_eq!(
        state.latest_config.expect("config").model.model_id,
        "current-model"
    );

    let tree = store.load_session_tree(&session_id).expect("tree");
    assert_eq!(tree.rows[0].branch_choices.len(), 2);
    let branches = store.load_branch_tree(&session_id).expect("branch tree");
    assert_eq!(branches.nodes.len(), 3);
    let preview = store
        .load_branch_preview(&session_id, &original)
        .expect("branch preview");
    assert_eq!(preview.common_parent_id, Some(root));
    assert_eq!(preview.snapshot.rows.len(), 2);

    let page = store
        .list_sessions(
            SessionDomain::Agent,
            CatalogQuery {
                project_id: Some(project_id.clone()),
                ..CatalogQuery::first_page()
            },
        )
        .expect("project catalog");
    assert_eq!(page.sessions.len(), 1);
    assert!(
        store
            .list_sessions(
                SessionDomain::Agent,
                CatalogQuery {
                    project_id: Some("project-018f0000-0000-7000-8000-000000000000".into()),
                    ..CatalogQuery::first_page()
                },
            )
            .expect("unknown project")
            .sessions
            .is_empty()
    );
    session_id
}

fn assert_project_scoped_restore_isolation<S>(store: &mut S)
where
    S: SessionStore + ProjectSessionStore,
{
    let project_a = ProjectIdentity::new("/tmp/agent-project-a", "Agent Project A");
    let project_b = ProjectIdentity::new("/tmp/agent-project-b", "Agent Project B");
    let project_a_id = project_a.project_id.clone();
    let project_b_id = project_b.project_id.clone();

    let header_a = SessionHeader::new(SessionDomain::Agent, Some(project_a));
    let session_a = header_a.session_id.clone();
    store
        .create_session(header_a)
        .expect("create project A session");
    store
        .append(&session_a, vec![message("project A discussion")])
        .expect("append project A message");

    let header_b = SessionHeader::new(SessionDomain::Agent, Some(project_b));
    let session_b = header_b.session_id.clone();
    store
        .create_session(header_b)
        .expect("create project B session");

    let page = store
        .list_project_sessions(&project_a_id, CatalogQuery::first_page())
        .expect("list project A sessions");
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].session_id, session_a);

    let restored = store
        .load_project_session(&project_a_id, &session_a, None)
        .expect("restore project A session");
    assert_eq!(restored.messages.len(), 1);
    assert!(matches!(
        store.load_project_session(&project_a_id, &session_b, None),
        Err(SessionError::ProjectMismatch {
            expected,
            actual,
            ..
        }) if expected == project_a_id && actual == project_b_id
    ));
}

mod catalog;
mod contracts;
mod durability;
mod path_safety;
mod pending;
mod repair;
mod source_authority;
