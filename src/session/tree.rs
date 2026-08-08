use std::collections::{HashMap, HashSet};

use crate::llm::{Message, ModelSelection, Usage};

use super::{
    Compaction, ConfigChange, EntryId, Reference, SessionDomain, SessionEntry, SessionEntryKind,
    SessionError, SessionHeader, TurnResult,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMessage {
    pub entry_id: EntryId,
    pub message: Message,
    pub turn_id: Option<String>,
    pub model: Option<ModelSelection>,
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTurnResult {
    pub entry_id: EntryId,
    pub result: TurnResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedContextItem {
    Message(EntryId),
    Reference {
        entry_id: EntryId,
        reference: Reference,
    },
    Summary {
        entry_id: EntryId,
        summary: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSessionState {
    pub leaf_id: EntryId,
    pub path: Vec<EntryId>,
    pub context: Vec<ResolvedContextItem>,
    pub messages: Vec<ResolvedMessage>,
    pub turn_results: Vec<ResolvedTurnResult>,
    pub latest_config: Option<ConfigChange>,
    pub latest_compaction: Option<Compaction>,
}

/// Validate the header, id uniqueness, parent links, and the append-only header
/// position.  The returned header is borrowed from the original entry list.
pub fn validate_session_entries(entries: &[SessionEntry]) -> Result<&SessionHeader, SessionError> {
    let Some(first) = entries.first() else {
        return Err(SessionError::MissingHeader);
    };
    let SessionEntryKind::Header(header) = &first.kind else {
        return Err(SessionError::HeaderNotFirst);
    };
    header.validate()?;
    if first.parent_id.is_some() {
        return Err(SessionError::HeaderNotFirst);
    }

    let mut ids = HashSet::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        if !ids.insert(entry.id.clone()) {
            return Err(SessionError::DuplicateId(entry.id.clone()));
        }
        if index > 0 && matches!(entry.kind, SessionEntryKind::Header(_)) {
            return Err(SessionError::DuplicateHeader);
        }
    }

    let by_id = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect::<HashMap<_, _>>();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 && entry.parent_id.is_none() {
            return Err(SessionError::MissingParent(entry.id.clone()));
        }
        if let Some(parent) = &entry.parent_id
            && !by_id.contains_key(parent)
        {
            return Err(SessionError::DanglingParent(parent.clone()));
        }
        if let SessionEntryKind::BranchSummary(summary) = &entry.kind
            && !by_id.contains_key(&summary.from_id)
        {
            return Err(SessionError::InvalidBranchTarget(summary.from_id.clone()));
        }
        if let SessionEntryKind::Reference(reference) = &entry.kind {
            reference.source.validate()?;
            if header.domain != SessionDomain::Agent {
                return Err(SessionError::ReferenceOutsideAgent);
            }
        }
    }

    let mut validated = HashSet::with_capacity(entries.len());
    for entry in entries {
        if validated.contains(&entry.id) {
            continue;
        }
        let mut current = Some(&entry.id);
        let mut ancestors = HashSet::new();
        while let Some(id) = current {
            if validated.contains(id) {
                break;
            }
            if !ancestors.insert(id.clone()) {
                return Err(SessionError::CycleDetected);
            }
            current = by_id
                .get(id)
                .and_then(|candidate| candidate.parent_id.as_ref());
        }
        validated.extend(ancestors);
    }
    Ok(header)
}

pub(crate) fn validate_appended_kind(
    kind: &SessionEntryKind,
    known_ids: &HashSet<EntryId>,
    domain: SessionDomain,
) -> Result<(), SessionError> {
    match kind {
        SessionEntryKind::Header(_) => Err(SessionError::InvalidEntryKind),
        SessionEntryKind::Compaction(compaction)
            if !known_ids.contains(&compaction.first_kept_entry_id) =>
        {
            Err(SessionError::InvalidCompactionTarget(
                compaction.first_kept_entry_id.clone(),
            ))
        }
        SessionEntryKind::BranchSummary(summary) if !known_ids.contains(&summary.from_id) => {
            Err(SessionError::InvalidBranchTarget(summary.from_id.clone()))
        }
        SessionEntryKind::Reference(reference) => {
            reference.source.validate()?;
            if domain != SessionDomain::Agent {
                return Err(SessionError::ReferenceOutsideAgent);
            }
            Ok(())
        }
        SessionEntryKind::Leaf(super::Leaf {
            target_id: Some(target),
        }) if !known_ids.contains(target) => Err(SessionError::LeafNotFound(target.clone())),
        _ => Ok(()),
    }
}

pub fn resolve_session(
    entries: &[SessionEntry],
    requested_leaf: Option<&EntryId>,
) -> Result<ResolvedSessionState, SessionError> {
    validate_session_entries(entries)?;
    let by_id = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect::<HashMap<_, _>>();

    let leaf_id = requested_leaf
        .cloned()
        .or_else(|| inferred_leaf(entries))
        .ok_or(SessionError::MissingHeader)?;
    if !by_id.contains_key(&leaf_id) {
        return Err(SessionError::LeafNotFound(leaf_id));
    }

    let mut path = Vec::new();
    let mut current = Some(leaf_id.clone());
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            return Err(SessionError::CycleDetected);
        }
        let entry = by_id
            .get(&id)
            .ok_or_else(|| SessionError::LeafNotFound(id.clone()))?;
        path.push((*entry).clone());
        current = entry.parent_id.clone();
    }
    path.reverse();

    let mut latest_config = path.first().and_then(|entry| match &entry.kind {
        SessionEntryKind::Header(header) => {
            header.initial_model.clone().map(|model| ConfigChange {
                model,
                system_prompt: header.initial_system_prompt.clone(),
            })
        }
        _ => None,
    });
    for entry in &path {
        if let SessionEntryKind::ConfigChange(change) = &entry.kind {
            latest_config = Some(change.clone());
        }
    }

    let compaction = path.iter().rev().find_map(|entry| match &entry.kind {
        SessionEntryKind::Compaction(compaction) => Some(compaction.clone()),
        _ => None,
    });
    let start = if let Some(compaction) = &compaction {
        let Some(index) = path.iter().position(|entry| {
            entry.id == compaction.first_kept_entry_id
                && matches!(entry.kind, SessionEntryKind::Message(_))
        }) else {
            return Err(SessionError::InvalidCompactionTarget(
                compaction.first_kept_entry_id.clone(),
            ));
        };
        index
    } else {
        0
    };

    let mut context = Vec::new();
    let mut messages = Vec::new();
    let turn_results = path
        .iter()
        .filter_map(|entry| match &entry.kind {
            SessionEntryKind::TurnResult(result) => Some(ResolvedTurnResult {
                entry_id: entry.id.clone(),
                result: result.clone(),
            }),
            _ => None,
        })
        .collect();
    if let Some(compaction) = &compaction {
        let compaction_id = path
            .iter()
            .rev()
            .find(|entry| matches!(entry.kind, SessionEntryKind::Compaction(_)))
            .map(|entry| entry.id.clone())
            .ok_or(SessionError::InvalidEntryKind)?;
        context.push(ResolvedContextItem::Summary {
            entry_id: compaction_id,
            summary: compaction.summary.clone(),
        });
    }
    for entry in &path[start..] {
        match &entry.kind {
            SessionEntryKind::Message(message) => {
                context.push(ResolvedContextItem::Message(entry.id.clone()));
                messages.push(ResolvedMessage {
                    entry_id: entry.id.clone(),
                    message: message.message.clone(),
                    turn_id: message.turn_id.clone(),
                    model: message.model.clone(),
                    usage: message.usage.clone(),
                });
            }
            SessionEntryKind::Reference(reference) => {
                context.push(ResolvedContextItem::Reference {
                    entry_id: entry.id.clone(),
                    reference: reference.clone(),
                })
            }
            SessionEntryKind::BranchSummary(summary) => {
                if !by_id.contains_key(&summary.from_id) {
                    return Err(SessionError::InvalidBranchTarget(summary.from_id.clone()));
                }
                context.push(ResolvedContextItem::Summary {
                    entry_id: entry.id.clone(),
                    summary: summary.summary.clone(),
                })
            }
            _ => {}
        }
    }

    Ok(ResolvedSessionState {
        leaf_id,
        path: path.into_iter().map(|entry| entry.id).collect(),
        context,
        messages,
        turn_results,
        latest_config,
        latest_compaction: compaction,
    })
}

fn inferred_leaf(entries: &[SessionEntry]) -> Option<EntryId> {
    entries.iter().rev().find_map(|entry| match &entry.kind {
        SessionEntryKind::Leaf(leaf) => leaf.target_id.clone().or_else(|| Some(entry.id.clone())),
        _ => Some(entry.id.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        llm::{ContentBlock, Message, Role, Usage},
        session::{
            BranchSummary, EntryId, MessageEntry, ProjectIdentity, SessionDomain, SessionEntry,
            SessionHeader, TurnResult, TurnStatus,
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
}
