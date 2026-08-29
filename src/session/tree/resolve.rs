use super::super::{ConfigChange, EntryId, SessionEntry, SessionEntryKind, SessionError};
use super::{
    ResolvedContextItem, ResolvedMessage, ResolvedSessionState, ResolvedTranscriptReplay,
    ResolvedTurnResult, resolve_path,
};

pub fn resolve_session(
    entries: &[SessionEntry],
    requested_leaf: Option<&EntryId>,
) -> Result<ResolvedSessionState, SessionError> {
    let (leaf_id, path, by_id) = resolve_path(entries, requested_leaf)?;

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
    let mut transcript_replays = Vec::new();
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
            SessionEntryKind::TranscriptReplay(replay) => {
                transcript_replays.push(ResolvedTranscriptReplay {
                    entry_id: entry.id.clone(),
                    replay: replay.clone(),
                });
            }
            _ => {}
        }
    }

    Ok(ResolvedSessionState {
        leaf_id,
        path: path.into_iter().map(|entry| entry.id.clone()).collect(),
        context,
        messages,
        transcript_replays,
        turn_results,
        latest_config,
        latest_compaction: compaction,
    })
}

pub(super) fn inferred_leaf(entries: &[SessionEntry]) -> Option<EntryId> {
    entries.iter().rev().find_map(|entry| match &entry.kind {
        SessionEntryKind::Leaf(leaf) => leaf.target_id.clone().or_else(|| Some(entry.id.clone())),
        _ => Some(entry.id.clone()),
    })
}
