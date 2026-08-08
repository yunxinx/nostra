use std::collections::{HashMap, HashSet};

use crate::llm::{ContentBlock, Message, ModelSelection, Role, Usage};

use super::{
    Compaction, ConfigChange, EntryId, Leaf, Reference, SessionDomain, SessionEntry,
    SessionEntryKind, SessionError, SessionHeader, TranscriptReplay, TurnResult,
};

type SessionEntryIndex<'a> = HashMap<EntryId, &'a SessionEntry>;
type ResolvedEntryPath<'a> = (EntryId, Vec<&'a SessionEntry>, SessionEntryIndex<'a>);

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

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTranscriptReplay {
    pub entry_id: EntryId,
    pub replay: TranscriptReplay,
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
    pub transcript_replays: Vec<ResolvedTranscriptReplay>,
    pub turn_results: Vec<ResolvedTurnResult>,
    pub latest_config: Option<ConfigChange>,
    pub latest_compaction: Option<Compaction>,
}

/// A display-oriented row in the durable session graph. State-only entries
/// such as configuration, compaction, and leaf markers remain in the graph
/// but do not become rows of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTreeRowKind {
    UserMessage,
    AssistantMessage,
    ToolMessage,
    SystemMessage,
    DeveloperMessage,
    ToolActivity,
    TerminalActivity,
    ChatReference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTreeRow {
    pub entry_id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: i64,
    pub kind: SessionTreeRowKind,
    pub preview: String,
    pub is_active_path: bool,
    pub is_current: bool,
    pub branch_choices: Vec<SessionTreeBranchChoice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBranchSummary {
    pub branch_root_id: EntryId,
    pub subtree_leaf_id: EntryId,
    pub latest_row_id: EntryId,
    pub preview: String,
    pub is_current: bool,
    pub row_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTreeBranchChoice {
    pub branch: SessionBranchSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTreeSnapshot {
    pub rows: Vec<SessionTreeRow>,
    pub current_row_id: Option<EntryId>,
    pub active_row_ids: HashSet<EntryId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBranchTreeNode {
    pub parent_branch_root_id: Option<EntryId>,
    pub branch: SessionBranchSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBranchTreeSnapshot {
    pub nodes: Vec<SessionBranchTreeNode>,
    pub current_branch_root_id: Option<EntryId>,
    pub total_row_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionBranchPreview {
    pub branch: SessionBranchSummary,
    pub common_parent_id: Option<EntryId>,
    pub snapshot: SessionTreeSnapshot,
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
        if let SessionEntryKind::TranscriptReplay(replay) = &entry.kind {
            replay
                .validate()
                .map_err(|error| SessionError::InvalidTranscriptReplay(error.to_string()))?;
            if header.domain != SessionDomain::Agent {
                return Err(SessionError::InvalidEntryKind);
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
        SessionEntryKind::TranscriptReplay(replay) => {
            replay
                .validate()
                .map_err(|error| SessionError::InvalidTranscriptReplay(error.to_string()))?;
            if domain != SessionDomain::Agent {
                return Err(SessionError::InvalidEntryKind);
            }
            Ok(())
        }
        SessionEntryKind::Leaf(super::Leaf {
            target_id: Some(target),
        }) if !known_ids.contains(target) => Err(SessionError::LeafNotFound(target.clone())),
        _ => Ok(()),
    }
}

/// Build the visible tree for the requested branch. A missing leaf selects the
/// latest durable leaf without modifying any session facts.
pub fn session_tree_snapshot(
    entries: &[SessionEntry],
    requested_leaf: Option<&EntryId>,
) -> Result<SessionTreeSnapshot, SessionError> {
    let (_, path, by_id) = resolve_path(entries, requested_leaf)?;
    let active_ids = path
        .iter()
        .filter_map(|entry| visible_row_kind(entry).map(|_| entry.id.clone()))
        .collect::<HashSet<_>>();
    let current_row_id = path
        .iter()
        .rev()
        .find(|entry| visible_row_kind(entry).is_some())
        .map(|entry| entry.id.clone());
    let visible_entries = entries
        .iter()
        .filter(|entry| visible_row_kind(entry).is_some())
        .collect::<Vec<_>>();
    let visible_ids = visible_entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let choices_by_parent =
        branch_choices_by_parent(entries, &by_id, &visible_entries, &visible_ids, &active_ids)?;

    let rows = path
        .iter()
        .filter(|entry| visible_row_kind(entry).is_some())
        .map(|entry| {
            let kind = visible_row_kind(entry).ok_or(SessionError::InvalidEntryKind)?;
            let parent_id = nearest_visible_parent(entry, &by_id, &visible_ids)?;
            Ok(SessionTreeRow {
                entry_id: entry.id.clone(),
                parent_id: parent_id.clone(),
                timestamp: entry.timestamp,
                kind,
                preview: row_preview(entry),
                is_active_path: active_ids.contains(&entry.id),
                is_current: current_row_id.as_ref() == Some(&entry.id),
                branch_choices: choices_by_parent
                    .get(&entry.id)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>, SessionError>>()?;

    Ok(SessionTreeSnapshot {
        rows,
        current_row_id,
        active_row_ids: active_ids,
    })
}

/// Resolve the latest path under a branch root and return only the common
/// parent plus the branch's visible rows. This lets a future UI inspect a
/// candidate branch without changing the durable active leaf.
pub fn session_branch_preview(
    entries: &[SessionEntry],
    branch_root_id: &EntryId,
) -> Result<SessionBranchPreview, SessionError> {
    let branch_tree = session_branch_tree_snapshot(entries, None)?;
    let branch = branch_tree
        .nodes
        .iter()
        .find(|node| node.branch.branch_root_id == *branch_root_id)
        .map(|node| node.branch.clone())
        .ok_or_else(|| SessionError::BranchNotFound(branch_root_id.clone()))?;
    let (_, _, by_id) = resolve_path(entries, None)?;
    let visible_ids = entries
        .iter()
        .filter(|entry| visible_row_kind(entry).is_some())
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let root = by_id
        .get(branch_root_id)
        .copied()
        .ok_or_else(|| SessionError::BranchNotFound(branch_root_id.clone()))?;
    let common_parent_id = nearest_visible_parent(root, &by_id, &visible_ids)?;
    let mut snapshot = session_tree_snapshot(entries, Some(&branch.subtree_leaf_id))?;
    let first_row_id = common_parent_id.as_ref().unwrap_or(&branch.branch_root_id);
    if let Some(index) = snapshot
        .rows
        .iter()
        .position(|row| row.entry_id == *first_row_id)
    {
        snapshot.rows.drain(..index);
        snapshot.active_row_ids = snapshot
            .rows
            .iter()
            .map(|row| row.entry_id.clone())
            .collect();
    }
    Ok(SessionBranchPreview {
        branch,
        common_parent_id,
        snapshot,
    })
}

/// Project every durable branch root and its latest recorded descendant.
/// Entries are returned in append order, which is deterministic for a JSONL
/// source and avoids mutating the selected branch while merely inspecting it.
pub fn session_branch_tree_snapshot(
    entries: &[SessionEntry],
    requested_leaf: Option<&EntryId>,
) -> Result<SessionBranchTreeSnapshot, SessionError> {
    let (_, active_path, by_id) = resolve_path(entries, requested_leaf)?;
    let active_ids = active_path
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let visible_entries = entries
        .iter()
        .filter(|entry| visible_row_kind(entry).is_some())
        .collect::<Vec<_>>();
    let visible_ids = visible_entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let branch_root_ids = branch_root_ids(entries, &by_id, &visible_entries, &visible_ids)?;
    let root_set = branch_root_ids.iter().cloned().collect::<HashSet<_>>();
    let current_branch_root_id = active_path
        .iter()
        .rev()
        .map(|entry| &entry.id)
        .find(|entry_id| root_set.contains(*entry_id))
        .cloned();
    let mut nodes = Vec::with_capacity(branch_root_ids.len());
    for root_id in branch_root_ids {
        let root = by_id
            .get(&root_id)
            .copied()
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
        let latest_row = latest_visible_descendant(entries, &by_id, &visible_ids, &root_id)?;
        let subtree_leaf = latest_non_leaf_descendant(entries, &by_id, &root_id)?;
        let row_count = visible_entries
            .iter()
            .filter(|entry| is_descendant(entry, &root_id, &by_id))
            .count();
        let parent_branch_root_id = nearest_branch_parent(root, &by_id, &visible_ids, &root_set)?;
        nodes.push(SessionBranchTreeNode {
            parent_branch_root_id,
            branch: SessionBranchSummary {
                branch_root_id: root_id.clone(),
                subtree_leaf_id: subtree_leaf.id.clone(),
                latest_row_id: latest_row.id.clone(),
                preview: row_preview(latest_row),
                is_current: current_branch_root_id.as_ref() == Some(&root_id)
                    || active_ids.contains(&root_id) && current_branch_root_id.is_none(),
                row_count,
                created_at: root.timestamp,
                updated_at: latest_row.timestamp,
            },
        });
    }
    Ok(SessionBranchTreeSnapshot {
        nodes,
        current_branch_root_id,
        total_row_count: visible_entries.len(),
    })
}

fn resolve_path<'a>(
    entries: &'a [SessionEntry],
    requested_leaf: Option<&EntryId>,
) -> Result<ResolvedEntryPath<'a>, SessionError> {
    validate_session_entries(entries)?;
    let by_id = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect::<HashMap<_, _>>();
    let requested = requested_leaf
        .cloned()
        .or_else(|| inferred_leaf(entries))
        .ok_or(SessionError::MissingHeader)?;
    let leaf_id = match by_id.get(&requested).map(|entry| &entry.kind) {
        Some(SessionEntryKind::Leaf(Leaf {
            target_id: Some(target),
        })) => target.clone(),
        Some(_) => requested,
        None => return Err(SessionError::LeafNotFound(requested)),
    };
    let mut path = Vec::new();
    let mut current = Some(leaf_id.clone());
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            return Err(SessionError::CycleDetected);
        }
        let entry = by_id
            .get(&id)
            .copied()
            .ok_or_else(|| SessionError::LeafNotFound(id.clone()))?;
        path.push(entry);
        current = entry.parent_id.clone();
    }
    path.reverse();
    Ok((leaf_id, path, by_id))
}

fn visible_row_kind(entry: &SessionEntry) -> Option<SessionTreeRowKind> {
    match &entry.kind {
        SessionEntryKind::Message(message) => Some(match message.message.role {
            Role::User => SessionTreeRowKind::UserMessage,
            Role::Assistant => SessionTreeRowKind::AssistantMessage,
            Role::Tool => SessionTreeRowKind::ToolMessage,
            Role::System => SessionTreeRowKind::SystemMessage,
            Role::Developer => SessionTreeRowKind::DeveloperMessage,
        }),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::TerminalSnapshot { .. }) => {
            Some(SessionTreeRowKind::TerminalActivity)
        }
        SessionEntryKind::TranscriptReplay(_) => Some(SessionTreeRowKind::ToolActivity),
        SessionEntryKind::Reference(_) => Some(SessionTreeRowKind::ChatReference),
        _ => None,
    }
}

fn row_preview(entry: &SessionEntry) -> String {
    match &entry.kind {
        SessionEntryKind::Message(message) => message_preview(&message.message)
            .unwrap_or_else(|| "Message without visible content".to_string()),
        SessionEntryKind::TranscriptReplay(replay) => replay.preview(),
        SessionEntryKind::Reference(reference) => reference.label.clone().unwrap_or_else(|| {
            format!(
                "Chat message {}:{}",
                reference.source.session_id, reference.source.entry_id
            )
        }),
        _ => "Session entry".to_string(),
    }
}

pub(crate) fn message_preview(message: &Message) -> Option<String> {
    let mut text = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: value, .. } => text.push_str(value),
            ContentBlock::Reasoning { reasoning } => text.push_str(&reasoning.display),
            ContentBlock::ToolCall { tool_call } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&tool_call.name);
            }
            ContentBlock::ToolResult { tool_result } => text.push_str(&tool_result.content),
        }
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(512).collect())
}

fn branch_choices_by_parent(
    entries: &[SessionEntry],
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_entries: &[&SessionEntry],
    visible_ids: &HashSet<EntryId>,
    active_ids: &HashSet<EntryId>,
) -> Result<HashMap<EntryId, Vec<SessionTreeBranchChoice>>, SessionError> {
    let branch_root_ids = branch_root_ids(entries, by_id, visible_entries, visible_ids)?;
    let mut by_parent = HashMap::<EntryId, Vec<EntryId>>::new();
    for root_id in branch_root_ids {
        let root = by_id
            .get(&root_id)
            .copied()
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
        let Some(parent) = nearest_visible_parent(root, by_id, visible_ids)? else {
            continue;
        };
        by_parent.entry(parent).or_default().push(root_id);
    }

    let mut choices = HashMap::new();
    for (parent_id, root_ids) in by_parent {
        if root_ids.len() < 2 {
            continue;
        }
        let summaries = root_ids
            .into_iter()
            .map(|root_id| {
                let root = by_id
                    .get(&root_id)
                    .copied()
                    .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
                let latest_row = latest_visible_descendant(entries, by_id, visible_ids, &root_id)?;
                let subtree_leaf = latest_non_leaf_descendant(entries, by_id, &root_id)?;
                let row_count = visible_entries
                    .iter()
                    .filter(|entry| is_descendant(entry, &root_id, by_id))
                    .count();
                Ok(SessionTreeBranchChoice {
                    branch: SessionBranchSummary {
                        branch_root_id: root_id.clone(),
                        subtree_leaf_id: subtree_leaf.id.clone(),
                        latest_row_id: latest_row.id.clone(),
                        preview: row_preview(latest_row),
                        is_current: active_ids.contains(&root_id),
                        row_count,
                        created_at: root.timestamp,
                        updated_at: latest_row.timestamp,
                    },
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        choices.insert(parent_id, summaries);
    }
    Ok(choices)
}

fn branch_root_ids(
    entries: &[SessionEntry],
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_entries: &[&SessionEntry],
    visible_ids: &HashSet<EntryId>,
) -> Result<Vec<EntryId>, SessionError> {
    let mut children_by_parent = HashMap::<Option<EntryId>, Vec<EntryId>>::new();
    for entry in visible_entries {
        let parent = nearest_visible_parent(entry, by_id, visible_ids)?;
        children_by_parent
            .entry(parent)
            .or_default()
            .push(entry.id.clone());
    }
    let mut roots = children_by_parent
        .get(&None)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>();
    for children in children_by_parent.values() {
        if children.len() >= 2 {
            roots.extend(children.iter().cloned());
        }
    }
    Ok(entries
        .iter()
        .filter(|entry| roots.contains(&entry.id))
        .map(|entry| entry.id.clone())
        .collect())
}

fn nearest_visible_parent(
    entry: &SessionEntry,
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_ids: &HashSet<EntryId>,
) -> Result<Option<EntryId>, SessionError> {
    let mut current = entry.parent_id.clone();
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            return Err(SessionError::CycleDetected);
        }
        if visible_ids.contains(&id) {
            return Ok(Some(id));
        }
        current = by_id
            .get(&id)
            .copied()
            .ok_or_else(|| SessionError::DanglingParent(id.clone()))?
            .parent_id
            .clone();
    }
    Ok(None)
}

fn nearest_branch_parent(
    entry: &SessionEntry,
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_ids: &HashSet<EntryId>,
    branch_root_ids: &HashSet<EntryId>,
) -> Result<Option<EntryId>, SessionError> {
    let mut current = nearest_visible_parent(entry, by_id, visible_ids)?;
    while let Some(id) = current {
        if branch_root_ids.contains(&id) {
            return Ok(Some(id));
        }
        let parent = by_id
            .get(&id)
            .copied()
            .ok_or_else(|| SessionError::DanglingParent(id.clone()))?;
        current = nearest_visible_parent(parent, by_id, visible_ids)?;
    }
    Ok(None)
}

fn latest_visible_descendant<'a>(
    entries: &'a [SessionEntry],
    by_id: &HashMap<EntryId, &'a SessionEntry>,
    visible_ids: &HashSet<EntryId>,
    root_id: &EntryId,
) -> Result<&'a SessionEntry, SessionError> {
    entries
        .iter()
        .rev()
        .find(|entry| visible_ids.contains(&entry.id) && is_descendant(entry, root_id, by_id))
        .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))
}

fn latest_non_leaf_descendant<'a>(
    entries: &'a [SessionEntry],
    by_id: &HashMap<EntryId, &'a SessionEntry>,
    root_id: &EntryId,
) -> Result<&'a SessionEntry, SessionError> {
    entries
        .iter()
        .rev()
        .find(|entry| {
            !matches!(entry.kind, SessionEntryKind::Leaf(_)) && is_descendant(entry, root_id, by_id)
        })
        .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))
}

fn is_descendant(
    entry: &SessionEntry,
    root_id: &EntryId,
    by_id: &HashMap<EntryId, &SessionEntry>,
) -> bool {
    if entry.id == *root_id {
        return true;
    }
    let mut current = entry.parent_id.clone();
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if id == *root_id {
            return true;
        }
        if !seen.insert(id.clone()) {
            return false;
        }
        let Some(parent) = by_id.get(&id).copied() else {
            return false;
        };
        current = parent.parent_id.clone();
    }
    false
}

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
            SessionHeader, TranscriptReplay, TurnResult, TurnStatus,
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
