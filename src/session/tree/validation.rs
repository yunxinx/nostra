use std::collections::{HashMap, HashSet};

#[cfg(test)]
use std::cell::Cell;

use super::super::{
    EntryId, Leaf, SessionDomain, SessionEntry, SessionEntryKind, SessionError, SessionHeader,
};

#[cfg(test)]
thread_local! {
    static COMPACTION_VALIDATION_VISITS: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(super) fn measure_compaction_validation_visits<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    COMPACTION_VALIDATION_VISITS.with(|visits| {
        assert!(visits.replace(Some(0)).is_none());
        let result = operation();
        let measured = visits.replace(None).unwrap_or_default();
        (result, measured)
    })
}

fn record_compaction_validation_visit() {
    #[cfg(test)]
    COMPACTION_VALIDATION_VISITS.with(|visits| {
        if let Some(current) = visits.get() {
            visits.set(Some(current.saturating_add(1)));
        }
    });
}

struct AncestryIntervals {
    entered_at: Vec<usize>,
    exited_at: Vec<usize>,
}

impl AncestryIntervals {
    fn contains(&self, ancestor: usize, descendant: usize) -> bool {
        self.entered_at[ancestor] <= self.entered_at[descendant]
            && self.exited_at[descendant] <= self.exited_at[ancestor]
    }
}

fn build_ancestry_intervals(
    entries: &[SessionEntry],
    positions: &HashMap<EntryId, usize>,
) -> Result<AncestryIntervals, SessionError> {
    let mut children = vec![Vec::new(); entries.len()];
    for (entry_index, entry) in entries.iter().enumerate().skip(1) {
        let parent_id = entry
            .parent_id
            .as_ref()
            .ok_or_else(|| SessionError::MissingParent(entry.id.clone()))?;
        let parent_index = positions
            .get(parent_id)
            .copied()
            .ok_or_else(|| SessionError::DanglingParent(parent_id.clone()))?;
        children[parent_index].push(entry_index);
    }

    let mut entered_at = vec![0; entries.len()];
    let mut exited_at = vec![0; entries.len()];
    let mut next_interval = 0usize;
    let mut stack = vec![(0usize, false)];
    while let Some((entry_index, expanded)) = stack.pop() {
        record_compaction_validation_visit();
        if expanded {
            exited_at[entry_index] = next_interval;
            continue;
        }

        entered_at[entry_index] = next_interval;
        next_interval = next_interval.saturating_add(1);
        stack.push((entry_index, true));
        for child in children[entry_index].iter().rev() {
            stack.push((*child, false));
        }
    }

    Ok(AncestryIntervals {
        entered_at,
        exited_at,
    })
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

    let mut positions = HashMap::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        if positions.insert(entry.id.clone(), index).is_some() {
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
            && positions
                .get(&summary.from_id)
                .is_none_or(|target_index| *target_index >= index)
        {
            return Err(SessionError::InvalidBranchTarget(summary.from_id.clone()));
        }
        if let SessionEntryKind::Leaf(Leaf {
            target_id: Some(target),
        }) = &entry.kind
            && positions
                .get(target)
                .is_none_or(|target_index| *target_index >= index)
        {
            return Err(SessionError::LeafNotFound(target.clone()));
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

    // Cycle detection deliberately runs first so a self-link or mutual cycle
    // retains its stronger corruption classification. The ordering pass then
    // enforces the append-only invariant for otherwise acyclic graphs.
    for (entry_index, entry) in entries.iter().enumerate() {
        let Some(parent_id) = &entry.parent_id else {
            continue;
        };
        let parent_index = positions
            .get(parent_id)
            .copied()
            .ok_or_else(|| SessionError::DanglingParent(parent_id.clone()))?;
        if parent_index >= entry_index {
            return Err(SessionError::ParentOutOfOrder {
                entry_id: entry.id.clone(),
                parent_id: parent_id.clone(),
            });
        }
    }

    // Parent links are immutable once appended, so one DFS interval index can
    // answer every compaction ancestry query in constant time. Walking from
    // each compaction back to the root makes a valid compacted history
    // quadratic before any tree or restore projection can be produced.
    let ancestry = entries
        .iter()
        .any(|entry| matches!(entry.kind, SessionEntryKind::Compaction(_)))
        .then(|| build_ancestry_intervals(entries, &positions))
        .transpose()?;
    for entry in entries {
        let SessionEntryKind::Compaction(compaction) = &entry.kind else {
            continue;
        };
        record_compaction_validation_visit();
        let target_index = positions.get(&compaction.first_kept_entry_id).copied();
        let target_is_message = target_index
            .is_some_and(|index| matches!(entries[index].kind, SessionEntryKind::Message(_)));
        let parent_index = entry
            .parent_id
            .as_ref()
            .and_then(|parent_id| positions.get(parent_id))
            .copied();
        let target_is_ancestor = target_index
            .zip(parent_index)
            .is_some_and(|(target, parent)| {
                ancestry
                    .as_ref()
                    .is_some_and(|ancestry| ancestry.contains(target, parent))
            });
        if !target_is_message || !target_is_ancestor {
            return Err(SessionError::InvalidCompactionTarget(
                compaction.first_kept_entry_id.clone(),
            ));
        }
    }
    Ok(header)
}

#[derive(Clone, Debug)]
struct AppendValidationNode {
    parent_id: Option<EntryId>,
    is_message: bool,
}

/// Compact semantic index used at the append boundary.
///
/// The source log remains authoritative. This index retains only the graph
/// facts needed to reject an invalid batch before any bytes are written,
/// without cloning or rescanning every prior message on each append.
#[derive(Clone, Debug, Default)]
pub(crate) struct AppendValidationState {
    nodes: HashMap<EntryId, AppendValidationNode>,
}

pub(crate) struct ValidatedAppend {
    nodes: Vec<(EntryId, AppendValidationNode)>,
}

impl AppendValidationState {
    pub(crate) fn from_entries(entries: &[SessionEntry]) -> Result<Self, SessionError> {
        validate_session_entries(entries)?;
        Ok(Self {
            nodes: entries
                .iter()
                .map(|entry| {
                    (
                        entry.id.clone(),
                        AppendValidationNode {
                            parent_id: entry.parent_id.clone(),
                            is_message: matches!(entry.kind, SessionEntryKind::Message(_)),
                        },
                    )
                })
                .collect(),
        })
    }

    pub(crate) fn validate_entries<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a SessionEntry>,
        domain: SessionDomain,
    ) -> Result<ValidatedAppend, SessionError> {
        let mut staged = HashMap::<EntryId, AppendValidationNode>::new();
        for entry in entries {
            let lookup = |id: &EntryId| staged.get(id).or_else(|| self.nodes.get(id));
            if lookup(&entry.id).is_some() {
                return Err(SessionError::DuplicateId(entry.id.clone()));
            }

            let is_first = self.nodes.is_empty() && staged.is_empty();
            if is_first {
                if !matches!(entry.kind, SessionEntryKind::Header(_)) || entry.parent_id.is_some() {
                    return Err(SessionError::MissingHeader);
                }
            } else {
                if matches!(entry.kind, SessionEntryKind::Header(_)) {
                    return Err(SessionError::InvalidEntryKind);
                }
                let parent = entry
                    .parent_id
                    .as_ref()
                    .ok_or_else(|| SessionError::MissingParent(entry.id.clone()))?;
                if lookup(parent).is_none() {
                    return Err(SessionError::DanglingParent(parent.clone()));
                }
            }

            match &entry.kind {
                SessionEntryKind::Header(_) => {}
                SessionEntryKind::Compaction(compaction) => {
                    let target_is_message = lookup(&compaction.first_kept_entry_id)
                        .is_some_and(|target| target.is_message);
                    let mut current = entry.parent_id.clone();
                    let mut target_is_ancestor = false;
                    while let Some(id) = current {
                        if id == compaction.first_kept_entry_id {
                            target_is_ancestor = true;
                            break;
                        }
                        current = lookup(&id).and_then(|node| node.parent_id.clone());
                    }
                    if !target_is_message || !target_is_ancestor {
                        return Err(SessionError::InvalidCompactionTarget(
                            compaction.first_kept_entry_id.clone(),
                        ));
                    }
                }
                SessionEntryKind::BranchSummary(summary) if lookup(&summary.from_id).is_none() => {
                    return Err(SessionError::InvalidBranchTarget(summary.from_id.clone()));
                }
                SessionEntryKind::Reference(reference) => {
                    reference.source.validate()?;
                    if domain != SessionDomain::Agent {
                        return Err(SessionError::ReferenceOutsideAgent);
                    }
                }
                SessionEntryKind::TranscriptReplay(replay) => {
                    replay.validate().map_err(|error| {
                        SessionError::InvalidTranscriptReplay(error.to_string())
                    })?;
                    if domain != SessionDomain::Agent {
                        return Err(SessionError::InvalidEntryKind);
                    }
                }
                SessionEntryKind::Leaf(Leaf {
                    target_id: Some(target),
                }) if lookup(target).is_none() => {
                    return Err(SessionError::LeafNotFound(target.clone()));
                }
                _ => {}
            }

            staged.insert(
                entry.id.clone(),
                AppendValidationNode {
                    parent_id: entry.parent_id.clone(),
                    is_message: matches!(entry.kind, SessionEntryKind::Message(_)),
                },
            );
        }
        Ok(ValidatedAppend {
            nodes: staged.into_iter().collect(),
        })
    }

    pub(crate) fn contains(&self, entry_id: &EntryId) -> bool {
        self.nodes.contains_key(entry_id)
    }

    pub(crate) fn commit(&mut self, append: ValidatedAppend) {
        self.nodes.extend(append.nodes);
    }
}
