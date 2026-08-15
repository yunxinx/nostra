use std::collections::{HashMap, HashSet};

#[cfg(test)]
use std::cell::Cell;

use crate::llm::{ContentBlock, Message, Role};

use super::{EntryId, Leaf, SessionEntry, SessionEntryKind, SessionError, TranscriptReplay};

type SessionEntryIndex<'a> = HashMap<EntryId, &'a SessionEntry>;
type ResolvedEntryPath<'a> = (EntryId, Vec<&'a SessionEntry>, SessionEntryIndex<'a>);

#[cfg(test)]
thread_local! {
    static TOPOLOGY_PARENT_VISITS: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
fn measure_topology_parent_visits<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    TOPOLOGY_PARENT_VISITS.with(|visits| {
        assert!(visits.replace(Some(0)).is_none());
        let result = operation();
        let measured = visits.replace(None).unwrap_or_default();
        (result, measured)
    })
}

fn record_topology_parent_visit() {
    #[cfg(test)]
    TOPOLOGY_PARENT_VISITS.with(|visits| {
        if let Some(current) = visits.get() {
            visits.set(Some(current.saturating_add(1)));
        }
    });
}

mod resolve;
mod types;
mod validation;

use resolve::inferred_leaf;
pub use resolve::resolve_session;
pub use types::*;
pub(crate) use validation::AppendValidationState;
#[cfg(test)]
use validation::measure_compaction_validation_visits;
pub use validation::validate_session_entries;

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
    let visible_parents = build_visible_parent_topology(entries, &by_id, &visible_ids)?;
    let choices_by_parent = branch_choices_by_parent(
        entries,
        &by_id,
        &visible_entries,
        &visible_ids,
        &visible_parents,
        &active_ids,
    )?;

    let rows = path
        .iter()
        .filter(|entry| visible_row_kind(entry).is_some())
        .map(|entry| {
            let kind = visible_row_kind(entry).ok_or(SessionError::InvalidEntryKind)?;
            let parent_id = visible_parents
                .get(&entry.id)
                .cloned()
                .ok_or_else(|| SessionError::BranchNotFound(entry.id.clone()))?;
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
    let visible_parents = build_visible_parent_topology(entries, &by_id, &visible_ids)?;
    let root = by_id
        .get(branch_root_id)
        .copied()
        .ok_or_else(|| SessionError::BranchNotFound(branch_root_id.clone()))?;
    let common_parent_id = visible_parents
        .get(&root.id)
        .cloned()
        .ok_or_else(|| SessionError::BranchNotFound(root.id.clone()))?;
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
    session_branch_tree_snapshot_impl(entries, requested_leaf, &mut BranchTraversalWork::default())
}

fn session_branch_tree_snapshot_impl(
    entries: &[SessionEntry],
    requested_leaf: Option<&EntryId>,
    work: &mut BranchTraversalWork,
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
    let visible_parents = build_visible_parent_topology(entries, &by_id, &visible_ids)?;
    let branch_root_ids = branch_root_ids(entries, &visible_entries, &visible_parents)?;
    let root_set = branch_root_ids.iter().cloned().collect::<HashSet<_>>();
    let branch_stats = branch_stats(entries, &by_id, &visible_ids, &root_set, work)?;
    let branch_parents =
        build_branch_parent_topology(&branch_root_ids, &visible_parents, &root_set)?;
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
        let stats = branch_stats
            .get(&root_id)
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
        let latest_row = stats
            .latest_row
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
        let subtree_leaf = stats
            .subtree_leaf
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
        let parent_branch_root_id = branch_parents
            .get(&root.id)
            .cloned()
            .ok_or_else(|| SessionError::BranchNotFound(root.id.clone()))?;
        nodes.push(SessionBranchTreeNode {
            parent_branch_root_id,
            branch: SessionBranchSummary {
                branch_root_id: root_id.clone(),
                subtree_leaf_id: subtree_leaf.id.clone(),
                latest_row_id: latest_row.id.clone(),
                preview: row_preview(latest_row),
                is_current: current_branch_root_id.as_ref() == Some(&root_id)
                    || active_ids.contains(&root_id) && current_branch_root_id.is_none(),
                row_count: stats.row_count,
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

fn row_preview(entry: &SessionEntry) -> Option<String> {
    match &entry.kind {
        SessionEntryKind::Message(message) => message_preview(&message.message),
        SessionEntryKind::TranscriptReplay(replay) => replay.preview(),
        SessionEntryKind::Reference(reference) => reference.label.clone(),
        _ => None,
    }
}

pub(crate) fn message_preview(message: &Message) -> Option<String> {
    let mut text = String::new();
    let mut raw_content_seen = false;
    let mut started = false;
    let mut length = 0;
    for block in &message.content {
        let value = match block {
            ContentBlock::Text { text: value, .. } => value.as_str(),
            ContentBlock::Reasoning { reasoning } => reasoning.display.as_str(),
            ContentBlock::ToolCall { tool_call } => {
                if raw_content_seen {
                    push_preview_char(&mut text, '\n', &mut started, &mut length);
                }
                tool_call.name.as_str()
            }
            ContentBlock::ToolResult { tool_result } => tool_result.content.as_str(),
        };
        raw_content_seen |= !value.is_empty();
        for character in value.chars() {
            push_preview_char(&mut text, character, &mut started, &mut length);
            if length >= 512 {
                break;
            }
        }
        if length >= 512 {
            break;
        }
    }
    while text.chars().last().is_some_and(char::is_whitespace) {
        text.pop();
    }
    (!text.is_empty()).then_some(text)
}

fn push_preview_char(text: &mut String, character: char, started: &mut bool, length: &mut usize) {
    if !*started && character.is_whitespace() {
        return;
    }
    if *length >= 512 {
        return;
    }
    *started = true;
    text.push(character);
    *length += 1;
}

fn branch_choices_by_parent(
    entries: &[SessionEntry],
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_entries: &[&SessionEntry],
    visible_ids: &HashSet<EntryId>,
    visible_parents: &HashMap<EntryId, Option<EntryId>>,
    active_ids: &HashSet<EntryId>,
) -> Result<HashMap<EntryId, Vec<SessionTreeBranchChoice>>, SessionError> {
    let branch_root_ids = branch_root_ids(entries, visible_entries, visible_parents)?;
    let root_set = branch_root_ids.iter().cloned().collect::<HashSet<_>>();
    let branch_stats = branch_stats(
        entries,
        by_id,
        visible_ids,
        &root_set,
        &mut BranchTraversalWork::default(),
    )?;
    let mut by_parent = HashMap::<EntryId, Vec<EntryId>>::new();
    for root_id in branch_root_ids {
        let Some(parent) = visible_parents
            .get(&root_id)
            .cloned()
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?
        else {
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
                let stats = branch_stats
                    .get(&root_id)
                    .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
                let latest_row = stats
                    .latest_row
                    .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
                let subtree_leaf = stats
                    .subtree_leaf
                    .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
                Ok(SessionTreeBranchChoice {
                    branch: SessionBranchSummary {
                        branch_root_id: root_id.clone(),
                        subtree_leaf_id: subtree_leaf.id.clone(),
                        latest_row_id: latest_row.id.clone(),
                        preview: row_preview(latest_row),
                        is_current: active_ids.contains(&root_id),
                        row_count: stats.row_count,
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
    visible_entries: &[&SessionEntry],
    visible_parents: &HashMap<EntryId, Option<EntryId>>,
) -> Result<Vec<EntryId>, SessionError> {
    let mut children_by_parent = HashMap::<Option<EntryId>, Vec<EntryId>>::new();
    for entry in visible_entries {
        let parent = visible_parents
            .get(&entry.id)
            .cloned()
            .ok_or_else(|| SessionError::BranchNotFound(entry.id.clone()))?;
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

fn build_visible_parent_topology(
    entries: &[SessionEntry],
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_ids: &HashSet<EntryId>,
) -> Result<HashMap<EntryId, Option<EntryId>>, SessionError> {
    let mut parents = HashMap::with_capacity(entries.len());
    for entry in entries {
        resolve_visible_parent(entry, by_id, visible_ids, &mut parents)?;
    }
    Ok(parents)
}

fn resolve_visible_parent(
    entry: &SessionEntry,
    by_id: &HashMap<EntryId, &SessionEntry>,
    visible_ids: &HashSet<EntryId>,
    parents: &mut HashMap<EntryId, Option<EntryId>>,
) -> Result<Option<EntryId>, SessionError> {
    if let Some(parent) = parents.get(&entry.id) {
        return Ok(parent.clone());
    }

    let mut unresolved = vec![entry.id.clone()];
    let mut current = entry.parent_id.clone();
    let mut seen = HashSet::new();
    let resolved = loop {
        let Some(id) = current else {
            break None;
        };
        record_topology_parent_visit();
        if !seen.insert(id.clone()) {
            return Err(SessionError::CycleDetected);
        }
        if visible_ids.contains(&id) {
            break Some(id);
        }
        if let Some(parent) = parents.get(&id) {
            break parent.clone();
        }
        let parent = by_id
            .get(&id)
            .copied()
            .ok_or_else(|| SessionError::DanglingParent(id.clone()))?;
        unresolved.push(id);
        current = parent.parent_id.clone();
    };
    for id in unresolved {
        parents.insert(id, resolved.clone());
    }
    Ok(resolved)
}

fn build_branch_parent_topology(
    branch_root_ids: &[EntryId],
    visible_parents: &HashMap<EntryId, Option<EntryId>>,
    branch_root_set: &HashSet<EntryId>,
) -> Result<HashMap<EntryId, Option<EntryId>>, SessionError> {
    let mut parents: HashMap<EntryId, Option<EntryId>> =
        HashMap::with_capacity(branch_root_ids.len());
    for root_id in branch_root_ids {
        let mut unresolved = vec![root_id.clone()];
        let mut current = visible_parents
            .get(root_id)
            .cloned()
            .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))?;
        let mut seen = HashSet::new();
        let resolved = loop {
            let Some(id) = current else {
                break None;
            };
            record_topology_parent_visit();
            if !seen.insert(id.clone()) {
                return Err(SessionError::CycleDetected);
            }
            if branch_root_set.contains(&id) {
                break Some(id);
            }
            if let Some(parent) = parents.get(&id) {
                break parent.clone();
            }
            unresolved.push(id.clone());
            current = visible_parents
                .get(&id)
                .cloned()
                .ok_or_else(|| SessionError::BranchNotFound(id.clone()))?;
        };
        for id in unresolved {
            parents.insert(id, resolved.clone());
        }
    }
    Ok(parents)
}

#[derive(Clone, Copy, Default)]
struct BranchStats<'a> {
    latest_row: Option<&'a SessionEntry>,
    subtree_leaf: Option<&'a SessionEntry>,
    row_count: usize,
}

#[derive(Default)]
struct BranchTraversalWork {
    ancestor_visits: usize,
}

fn branch_stats<'a>(
    entries: &'a [SessionEntry],
    by_id: &HashMap<EntryId, &'a SessionEntry>,
    visible_ids: &HashSet<EntryId>,
    branch_root_ids: &HashSet<EntryId>,
    work: &mut BranchTraversalWork,
) -> Result<HashMap<EntryId, BranchStats<'a>>, SessionError> {
    let append_order = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut children = HashMap::<EntryId, Vec<EntryId>>::new();
    let mut roots = Vec::new();
    for entry in entries {
        if let Some(parent) = &entry.parent_id {
            children
                .entry(parent.clone())
                .or_default()
                .push(entry.id.clone());
        } else {
            roots.push(entry.id.clone());
        }
    }

    // Aggregate each subtree exactly once in post-order. The previous
    // implementation walked every entry back through all ancestors, turning
    // a linear conversation into quadratic work on the UI-facing tree path.
    let mut stack = roots
        .into_iter()
        .rev()
        .map(|id| (id, false))
        .collect::<Vec<_>>();
    let mut aggregates = HashMap::<EntryId, BranchStats<'a>>::new();
    while let Some((id, expanded)) = stack.pop() {
        work.ancestor_visits = work.ancestor_visits.saturating_add(1);
        if !expanded {
            stack.push((id.clone(), true));
            if let Some(descendants) = children.get(&id) {
                for child in descendants.iter().rev() {
                    stack.push((child.clone(), false));
                }
            }
            continue;
        }

        let entry = by_id
            .get(&id)
            .copied()
            .ok_or_else(|| SessionError::DanglingParent(id.clone()))?;
        let mut aggregate = BranchStats {
            latest_row: visible_ids.contains(&id).then_some(entry),
            subtree_leaf: (!matches!(entry.kind, SessionEntryKind::Leaf(_))).then_some(entry),
            row_count: usize::from(visible_ids.contains(&id)),
        };
        if let Some(descendants) = children.get(&id) {
            for child in descendants {
                work.ancestor_visits = work.ancestor_visits.saturating_add(1);
                let child_stats = aggregates
                    .get(child)
                    .copied()
                    .ok_or_else(|| SessionError::DanglingParent(child.clone()))?;
                aggregate.row_count = aggregate.row_count.saturating_add(child_stats.row_count);
                aggregate.latest_row =
                    later_entry(aggregate.latest_row, child_stats.latest_row, &append_order)?;
                aggregate.subtree_leaf = later_entry(
                    aggregate.subtree_leaf,
                    child_stats.subtree_leaf,
                    &append_order,
                )?;
            }
        }
        aggregates.insert(id, aggregate);
    }

    branch_root_ids
        .iter()
        .map(|root_id| {
            aggregates
                .get(root_id)
                .copied()
                .map(|stats| (root_id.clone(), stats))
                .ok_or_else(|| SessionError::BranchNotFound(root_id.clone()))
        })
        .collect()
}

fn later_entry<'a>(
    current: Option<&'a SessionEntry>,
    candidate: Option<&'a SessionEntry>,
    append_order: &HashMap<EntryId, usize>,
) -> Result<Option<&'a SessionEntry>, SessionError> {
    Ok(match (current, candidate) {
        (None, candidate) => candidate,
        (current, None) => current,
        (Some(current), Some(candidate)) => {
            let current_order = append_order
                .get(&current.id)
                .copied()
                .ok_or_else(|| SessionError::BranchNotFound(current.id.clone()))?;
            let candidate_order = append_order
                .get(&candidate.id)
                .copied()
                .ok_or_else(|| SessionError::BranchNotFound(candidate.id.clone()))?;
            if candidate_order > current_order {
                Some(candidate)
            } else {
                Some(current)
            }
        }
    })
}

#[cfg(test)]
mod tests;
