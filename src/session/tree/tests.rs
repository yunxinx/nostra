use super::*;
use crate::{
    llm::{ContentBlock, Message, Role, Usage},
    session::{
        BranchSummary, ChatMessageRef, Compaction, ConfigChange, EntryId, MessageEntry,
        ProjectIdentity, Reference, SessionDomain, SessionEntry, SessionHeader, SessionId,
        TranscriptReplay, TurnResult, TurnStatus,
    },
};

fn message(text: &str, parent: Option<EntryId>) -> SessionEntry {
    SessionEntry::new(
        EntryId::new(),
        parent,
        SessionEntryKind::Message(MessageEntry {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            turn_id: None,
            model: None,
            usage: Usage::default(),
        }),
    )
}

#[test]
fn resolves_linear_history_and_leaf_branch() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let first = message("first", Some(header.id.clone()));
    let second = message("second", Some(first.id.clone()));
    let branch = message("branch", Some(first.id.clone()));
    let entries = vec![header, first.clone(), second, branch.clone()];
    let state = resolve_session(&entries, Some(&branch.id)).expect("branch resolves");
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.leaf_id, branch.id);
    assert_eq!(state.messages[0].entry_id, first.id);
}

#[test]
fn compaction_replaces_prefix_with_summary() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let first = message("first", Some(header.id.clone()));
    let second = message("second", Some(first.id.clone()));
    let terminal = SessionEntry::new(
        EntryId::new(),
        Some(second.id.clone()),
        SessionEntryKind::TurnResult(TurnResult {
            turn_id: Some("turn-1".into()),
            status: TurnStatus::Completed,
            finish_reason: None,
            error: None,
            usage: Usage {
                total_tokens: 10,
                ..Usage::default()
            },
        }),
    );
    let compaction = SessionEntry::new(
        EntryId::new(),
        Some(terminal.id.clone()),
        SessionEntryKind::Compaction(Compaction {
            summary: "old context".into(),
            first_kept_entry_id: second.id.clone(),
            tokens_before: 100,
        }),
    );
    let third = message("third", Some(compaction.id.clone()));
    let state = resolve_session(
        &[header, first, second.clone(), terminal, compaction, third],
        None,
    )
    .expect("compaction resolves");
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.turn_results.len(), 1);
    assert_eq!(
        state.turn_results[0].result.turn_id.as_deref(),
        Some("turn-1")
    );
    assert!(matches!(
        state.context[0],
        ResolvedContextItem::Summary { .. }
    ));
    assert_eq!(state.messages[0].entry_id, second.id);
}

#[test]
fn branch_summary_is_included_when_resolving_a_new_branch() {
    let header = SessionEntry::header(SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    ));
    let root = message("root", Some(header.id.clone()));
    let abandoned = message("abandoned", Some(root.id.clone()));
    let summary = SessionEntry::new(
        EntryId::new(),
        Some(root.id.clone()),
        SessionEntryKind::BranchSummary(BranchSummary {
            from_id: abandoned.id.clone(),
            summary: "abandoned branch summary".into(),
        }),
    );
    let replacement = message("replacement", Some(summary.id.clone()));
    let state = resolve_session(
        &[header, root.clone(), abandoned, summary, replacement],
        None,
    )
    .expect("branch resolves");
    assert_eq!(state.messages.len(), 2);
    assert!(state.context.iter().any(|item| {
        matches!(
            item,
            ResolvedContextItem::Summary { summary, .. }
                if summary == "abandoned branch summary"
        )
    }));
    assert_eq!(state.messages[0].entry_id, root.id);
}

#[test]
fn transcript_replay_restores_outside_model_context_and_is_agent_only() {
    let header = SessionEntry::header(SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    ));
    let root = message("inspect the project", Some(header.id.clone()));
    let replay = SessionEntry::new(
        EntryId::new(),
        Some(root.id.clone()),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::TerminalSnapshot {
            terminal_id: "terminal-1".into(),
            title: Some("cargo test".into()),
            content: "all tests passed".into(),
        }),
    );
    let state = resolve_session(&[header, root.clone(), replay.clone()], None)
        .expect("agent replay resolves");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.transcript_replays.len(), 1);
    assert_eq!(state.transcript_replays[0].entry_id, replay.id);
    assert!(
        state
            .context
            .iter()
            .all(|item| !matches!(item, ResolvedContextItem::Summary { .. }))
    );

    let chat_header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let chat_root = message("chat", Some(chat_header.id.clone()));
    let chat_replay = SessionEntry::new(
        EntryId::new(),
        Some(chat_root.id.clone()),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::ToolActivity {
            title: "read file".into(),
            content: String::new(),
        }),
    );
    assert!(matches!(
        resolve_session(&[chat_header, chat_root, chat_replay], None),
        Err(SessionError::InvalidEntryKind)
    ));
}

#[test]
fn tree_and_branch_preview_follow_logical_message_forks() {
    let header = SessionEntry::header(SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    ));
    let root = message("root", Some(header.id.clone()));
    let original = message("original", Some(root.id.clone()));
    let selected_root = SessionEntry::new(
        EntryId::new(),
        Some(original.id.clone()),
        SessionEntryKind::Leaf(Leaf {
            target_id: Some(root.id.clone()),
        }),
    );
    let replacement = message("replacement", Some(root.id.clone()));
    let terminal = SessionEntry::new(
        EntryId::new(),
        Some(replacement.id.clone()),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::TerminalSnapshot {
            terminal_id: "terminal-1".into(),
            title: None,
            content: "done".into(),
        }),
    );
    let entries = vec![
        header,
        root.clone(),
        original.clone(),
        selected_root,
        replacement.clone(),
        terminal,
    ];

    let tree = session_tree_snapshot(&entries, None).expect("active tree");
    assert_eq!(tree.rows.len(), 3);
    assert_eq!(tree.rows[0].entry_id, root.id);
    assert_eq!(tree.rows[0].branch_choices.len(), 2);
    assert_eq!(tree.rows[1].entry_id, replacement.id);

    let branch_tree = session_branch_tree_snapshot(&entries, None).expect("branch tree");
    assert_eq!(branch_tree.nodes.len(), 3);
    assert_eq!(
        branch_tree.current_branch_root_id,
        Some(replacement.id.clone())
    );
    let original_branch = branch_tree
        .nodes
        .iter()
        .find(|node| node.branch.branch_root_id == original.id)
        .expect("original branch");
    assert_eq!(original_branch.parent_branch_root_id, Some(root.id.clone()));
    assert_eq!(original_branch.branch.row_count, 1);

    let preview = session_branch_preview(&entries, &original.id).expect("branch preview");
    assert_eq!(preview.common_parent_id, Some(root.id.clone()));
    assert_eq!(preview.snapshot.rows.len(), 2);
    assert_eq!(preview.snapshot.rows[1].entry_id, original.id);
}

#[test]
fn tree_preview_preserves_missing_content_for_the_presentation_layer() {
    let header = SessionEntry::header(SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    ));
    let empty_message = SessionEntry::new(
        EntryId::new(),
        Some(header.id.clone()),
        SessionEntryKind::Message(MessageEntry {
            message: Message {
                role: Role::Assistant,
                content: Vec::new(),
                provider_metadata: Default::default(),
            },
            turn_id: None,
            model: None,
            usage: Usage::default(),
        }),
    );
    let tool_call = SessionEntry::new(
        EntryId::new(),
        Some(empty_message.id.clone()),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::ToolCall {
            call_id: "call-1".into(),
            tool_name: "read_file".into(),
            arguments: serde_json::json!({}),
        }),
    );
    let terminal = SessionEntry::new(
        EntryId::new(),
        Some(tool_call.id.clone()),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::TerminalSnapshot {
            terminal_id: "terminal-1".into(),
            title: None,
            content: String::new(),
        }),
    );
    let reference = SessionEntry::new(
        EntryId::new(),
        Some(terminal.id.clone()),
        SessionEntryKind::Reference(Reference {
            source: ChatMessageRef::new(SessionId::new(SessionDomain::Chat), EntryId::new())
                .expect("valid Chat reference"),
            label: None,
        }),
    );

    let snapshot = session_tree_snapshot(
        &[header, empty_message, tool_call, terminal, reference],
        None,
    )
    .expect("tree snapshot");
    assert_eq!(snapshot.rows.len(), 4);
    assert_eq!(snapshot.rows[0].preview, None);
    assert_eq!(snapshot.rows[1].preview.as_deref(), Some("read_file"));
    assert_eq!(snapshot.rows[2].preview, None);
    assert_eq!(snapshot.rows[3].preview, None);
}

#[test]
fn branch_tree_aggregation_has_a_linear_traversal_bound() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let mut parent = header.id.clone();
    let mut entries = vec![header];
    for index in 0..2_000 {
        let entry = message(&format!("message-{index}"), Some(parent));
        parent = entry.id.clone();
        entries.push(entry);
    }

    let mut work = BranchTraversalWork::default();
    let snapshot =
        session_branch_tree_snapshot_impl(&entries, None, &mut work).expect("linear branch tree");
    assert_eq!(snapshot.nodes.len(), 1);
    assert_eq!(snapshot.nodes[0].branch.row_count, 2_000);
    assert!(
        work.ancestor_visits <= entries.len().saturating_mul(4),
        "branch aggregation visited {} ancestors for {} entries",
        work.ancestor_visits,
        entries.len()
    );
}

#[test]
fn branch_topology_reuses_shared_invisible_ancestry() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let mut parent = header.id.clone();
    let mut entries = vec![header];
    for index in 0..1_000 {
        let state = SessionEntry::new(
            EntryId::new(),
            Some(parent),
            SessionEntryKind::ConfigChange(ConfigChange {
                model: crate::llm::ModelSelection {
                    profile_id: "profile".into(),
                    model_id: format!("model-{index}"),
                },
                system_prompt: None,
            }),
        );
        parent = state.id.clone();
        entries.push(state);
    }
    for index in 0..1_000 {
        entries.push(message(&format!("branch-{index}"), Some(parent.clone())));
    }

    let (snapshot, parent_visits) = measure_topology_parent_visits(|| {
        session_branch_tree_snapshot(&entries, None).expect("shared-ancestry branch tree")
    });
    assert_eq!(snapshot.nodes.len(), 1_000);
    assert!(
        parent_visits <= entries.len().saturating_mul(8),
        "branch topology revisited {parent_visits} parents for {} entries",
        entries.len()
    );
}

#[test]
fn compaction_validation_has_a_linear_traversal_bound() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let first = message("first", Some(header.id.clone()));
    let first_id = first.id.clone();
    let mut parent = first.id.clone();
    let mut entries = vec![header, first];
    for index in 0..1_000 {
        let compaction = SessionEntry::new(
            EntryId::new(),
            Some(parent),
            SessionEntryKind::Compaction(Compaction {
                summary: format!("summary-{index}"),
                first_kept_entry_id: first_id.clone(),
                tokens_before: index,
            }),
        );
        parent = compaction.id.clone();
        entries.push(compaction);
    }

    let (snapshot, validation_visits) = measure_compaction_validation_visits(|| {
        session_branch_tree_snapshot(&entries, None).expect("compacted branch tree")
    });
    assert_eq!(snapshot.total_row_count, 1);
    assert!(
        validation_visits <= entries.len().saturating_mul(4),
        "compaction validation revisited {validation_visits} ancestors for {} entries",
        entries.len()
    );
}

#[test]
fn restore_projection_keeps_message_metadata_and_terminal_results() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let message_id = EntryId::new();
    let turn_id = "turn-1".to_string();
    let model = crate::llm::ModelSelection {
        profile_id: "profile".into(),
        model_id: "model".into(),
    };
    let message = SessionEntry::new(
        message_id.clone(),
        Some(header.id.clone()),
        SessionEntryKind::Message(MessageEntry {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            turn_id: Some(turn_id.clone()),
            model: Some(model.clone()),
            usage: Usage {
                total_tokens: 3,
                ..Usage::default()
            },
        }),
    );
    let result = SessionEntry::new(
        EntryId::new(),
        Some(message_id.clone()),
        SessionEntryKind::TurnResult(TurnResult {
            turn_id: Some(turn_id),
            status: TurnStatus::Cancelled,
            finish_reason: None,
            error: None,
            usage: Usage {
                total_tokens: 3,
                ..Usage::default()
            },
        }),
    );
    let state = resolve_session(&[header, message, result.clone()], None).expect("resolve");
    assert_eq!(state.messages[0].entry_id, message_id);
    assert_eq!(state.messages[0].model, Some(model));
    assert_eq!(state.messages[0].usage.total_tokens, 3);
    assert_eq!(state.turn_results[0].entry_id, result.id);
    assert_eq!(state.turn_results[0].result.status, TurnStatus::Cancelled);
}

#[test]
fn rejects_dangling_parent_and_cycle() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let missing = message("missing", Some(EntryId::new()));
    assert!(matches!(
        validate_session_entries(&[header.clone(), missing]),
        Err(SessionError::DanglingParent(_))
    ));
    let mut cyclic = message("cycle", Some(header.id.clone()));
    cyclic.parent_id = Some(cyclic.id.clone());
    assert!(matches!(
        resolve_session(&[header, cyclic], None),
        Err(SessionError::DanglingParent(_)) | Err(SessionError::CycleDetected)
    ));
}

#[test]
fn rejects_an_acyclic_parent_that_appears_later_in_the_log() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let later_parent = message("parent", Some(header.id.clone()));
    let child = message("child", Some(later_parent.id.clone()));

    assert!(matches!(
        validate_session_entries(&[header, child.clone(), later_parent.clone()]),
        Err(SessionError::ParentOutOfOrder {
            entry_id,
            parent_id,
        }) if entry_id == child.id && parent_id == later_parent.id
    ));
}

#[test]
fn rejects_a_leaf_fact_that_targets_an_unknown_entry() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let message = message("message", Some(header.id.clone()));
    let missing = EntryId::new();
    let leaf = SessionEntry::new(
        EntryId::new(),
        Some(message.id.clone()),
        SessionEntryKind::Leaf(Leaf {
            target_id: Some(missing.clone()),
        }),
    );

    assert!(matches!(
        validate_session_entries(&[header, message, leaf]),
        Err(SessionError::LeafNotFound(target)) if target == missing
    ));
}

#[test]
fn rejects_a_leaf_fact_that_targets_a_later_entry() {
    let header = SessionEntry::header(SessionHeader::new(SessionDomain::Chat, None));
    let later = message("later", Some(header.id.clone()));
    let leaf = SessionEntry::new(
        EntryId::new(),
        Some(header.id.clone()),
        SessionEntryKind::Leaf(Leaf {
            target_id: Some(later.id.clone()),
        }),
    );

    assert!(matches!(
        validate_session_entries(&[header, leaf, later.clone()]),
        Err(SessionError::LeafNotFound(target)) if target == later.id
    ));
}

#[test]
fn rejects_a_branch_summary_that_targets_a_later_entry() {
    let header = SessionEntry::header(SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    ));
    let later = message("later", Some(header.id.clone()));
    let summary = SessionEntry::new(
        EntryId::new(),
        Some(header.id.clone()),
        SessionEntryKind::BranchSummary(BranchSummary {
            from_id: later.id.clone(),
            summary: "future branch".into(),
        }),
    );

    assert!(matches!(
        validate_session_entries(&[header, summary, later.clone()]),
        Err(SessionError::InvalidBranchTarget(target)) if target == later.id
    ));
}
