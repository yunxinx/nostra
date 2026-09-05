//! Row projection: the ordered row list derived from a [`Transcript`].
//!
//! The conversation list renders *rows*, not turns: one row per user bubble,
//! assistant prose part, reasoning card, tool activity, turn error, or turn
//! action bar. [`RowProjection`] derives that ordered list from the canonical
//! transcript, pairs tool results with their calls, collapses long runs of
//! activity into one group row, and caches per-row heights. Rendering must
//! never lock the transcript: the projection plus the materialized renderers
//! carry everything a frame needs.
//!
//! Outcome contract: a [`ProjectionDiff`] declares how the row *set* changed
//! and a [`ProjectionOutcome::remeasure`] declares which rows the list must
//! re-measure. The two declarations are orthogonal — neither may swallow the
//! other.

mod height;

use std::collections::HashMap;

pub(crate) use self::height::{
    Confidence, Measured, MeasurementKey, RowHeight, TypographySnapshot, current_theme_revision,
    note_theme_changed,
};
use crate::chat::transcript::{
    Part, PartChange, PartId, PartSource, Role, Transcript, TranscriptEvent, TurnId,
    has_copyable_text, stream_ended,
};
use gpui::Pixels;

/// Tool calls and results are correlated by the provider call id.
pub(crate) type CallId = String;

/// The visual kind of one transcript row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RowKind {
    UserBubble,
    AssistantProse,
    Reasoning,
    ToolActivity,
    ToolActivityGroup,
    TurnError,
    TurnActions,
}

impl RowKind {
    /// Stable selector fragment derived into debug names (AC8).
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::UserBubble => "userbubble",
            Self::AssistantProse => "prose",
            Self::Reasoning => "reasoning",
            Self::ToolActivity => "toolactivity",
            Self::ToolActivityGroup => "toolgroup",
            Self::TurnError => "turnerror",
            Self::TurnActions => "turnactions",
        }
    }
}

/// Stable identity of one row: turn, part, and visual kind.
///
/// `TurnError` / `TurnActions` rows and the synthetic wait placeholder of an
/// otherwise empty assistant turn are turn-scoped and use [`PartId::NONE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RowId {
    pub(crate) turn: TurnId,
    pub(crate) part: PartId,
    pub(crate) kind: RowKind,
}

impl RowId {
    pub(crate) fn new(turn: TurnId, part: PartId, kind: RowKind) -> Self {
        Self { turn, part, kind }
    }

    /// `row-{kind}-{turn}-{part}` — stable across list splices and cold
    /// rebuilds, and the root of every GUI debug selector in the transcript.
    pub(crate) fn debug_name(&self) -> String {
        format!(
            "row-{}-{}-{}",
            self.kind.slug(),
            self.turn.as_u64(),
            self.part.as_u64()
        )
    }
}

/// Disclosure form of a reasoning row: a finished trace is either a collapsed
/// trigger, a viewport bounded by the height budget, or natural full height.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ReasoningDisclosure {
    #[default]
    Collapsed,
    Budgeted,
    Full,
}

/// Disclosure form of one tool activity row: the body is either folded or
/// open, and an open body remembers whether its arguments section was itself
/// folded by the user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ActivityDisclosure {
    #[default]
    Collapsed,
    Open {
        arguments_open: bool,
    },
}

impl ActivityDisclosure {
    /// Whether the body's arguments section is visible.
    pub(crate) fn arguments_open(&self) -> bool {
        match self {
            Self::Collapsed => false,
            Self::Open { arguments_open } => *arguments_open,
        }
    }
}

/// Per-row disclosure kept on the projection so it survives renderer release
/// and cold rebuilds. `reasoning` carries the reasoning row's tri-state,
/// `activity` the tool activity row's two-stage fold, and `group_open` the
/// expanded/collapsed state of a tool-activity group row.
/// `reasoning_user_controlled` records that the user worked a reasoning
/// toggle, so auto expand/collapse keeps yielding to their choice even after
/// the row's renderer was released and re-materialized.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DisclosureState {
    pub(crate) reasoning: ReasoningDisclosure,
    pub(crate) reasoning_user_controlled: bool,
    pub(crate) activity: ActivityDisclosure,
    pub(crate) group_open: bool,
}

impl DisclosureState {
    /// The generic expanded fold (activity and group rows).
    pub(crate) const EXPANDED: Self = Self {
        reasoning: ReasoningDisclosure::Collapsed,
        reasoning_user_controlled: false,
        activity: ActivityDisclosure::Collapsed,
        group_open: true,
    };

    /// The generic two-state fold of this disclosure (group rows).
    pub(crate) fn is_expanded(&self) -> bool {
        self.group_open
    }
}

/// One projected row.
#[derive(Clone, Debug)]
pub(crate) struct Row {
    id: RowId,
    turn_index: usize,
    role: Role,
    is_first_in_turn: bool,
    is_last_in_turn: bool,
    content_revision: u64,
    height: RowHeight,
    disclosure: DisclosureState,
    group_count: usize,
    source_len: usize,
    block_hint: usize,
    /// Whether this row is visible wait-ending content for the shimmer rule
    /// (P1 `has_wait_ending_content`): named tool calls, reasoning, and
    /// non-empty prose end waiting; unpaired tool results do not.
    ends_waiting: bool,
    /// The expanded tool-activity group this row belongs to. Declared on
    /// every member row while the group is expanded.
    group: Option<RowId>,
    /// Whether this row is the first member of its expanded group.
    group_leader: bool,
    /// Display name of the most recent member of the collapsed group this row
    /// stands for (`ToolActivityGroup` rows only).
    group_latest: Option<String>,
}

impl Row {
    pub(crate) fn id(&self) -> RowId {
        self.id
    }

    pub(crate) fn kind(&self) -> RowKind {
        self.id.kind
    }

    pub(crate) fn turn_index(&self) -> usize {
        self.turn_index
    }

    pub(crate) fn role(&self) -> Role {
        self.role
    }

    pub(crate) fn is_first_in_turn(&self) -> bool {
        self.is_first_in_turn
    }

    pub(crate) fn is_last_in_turn(&self) -> bool {
        self.is_last_in_turn
    }

    #[cfg(test)]
    pub(crate) fn disclosure(&self) -> DisclosureState {
        self.disclosure
    }

    pub(crate) fn group_count(&self) -> usize {
        self.group_count
    }

    /// The expanded tool-activity group this row belongs to, if any.
    pub(crate) fn group(&self) -> Option<RowId> {
        self.group
    }

    /// Whether this row is the first member of its expanded group; the row
    /// that carries the group's collapse affordance.
    pub(crate) fn leads_group(&self) -> bool {
        self.group_leader
    }

    /// The most recent member's display name for a collapsed group header.
    pub(crate) fn group_latest(&self) -> Option<&str> {
        self.group_latest.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn estimated_height(&self) -> Pixels {
        self.height.estimated
    }

    /// Confidence of the currently recorded measurement, if any.
    #[cfg(test)]
    pub(crate) fn recorded_confidence(&self) -> Option<Confidence> {
        self.height.measured.map(|measured| measured.confidence)
    }

    /// Current height: the cached measurement when fresh, the estimate
    /// otherwise.
    pub(crate) fn effective_height(&self, key: &MeasurementKey) -> Pixels {
        self.height.effective(key, self.content_revision)
    }

    /// The measurement key of any cached measurement on this row.
    pub(crate) fn measured_key(&self) -> Option<MeasurementKey> {
        self.height.measured_key()
    }

    /// Settled measured height, usable as a cold-restore first-frame
    /// placeholder.
    pub(crate) fn settled_height(&self, key: &MeasurementKey) -> Option<Pixels> {
        self.height.settled_height(key)
    }

    #[cfg(test)]
    pub(crate) fn debug_name(&self) -> String {
        self.id.debug_name()
    }
}

/// How the projection's row *set* changed, mapped one-to-one onto the
/// retained [`ListState`](gpui::ListState) operations.
///
/// Row-set changes and re-measure requests are orthogonal declarations:
/// re-measures travel in [`ProjectionOutcome::remeasure`], so a splice can
/// never swallow one (and never has to carry one).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProjectionDiff {
    #[default]
    None,
    Splice {
        range: std::ops::Range<usize>,
        inserted: usize,
    },
    Rebuild,
}

/// The shape of a content change inside one row, dispatched to
/// [`crate::chat::rows::RowRenderer::apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowChangeKind {
    /// Streaming append; the delta is re-read from the transcript part.
    Append,
    /// The part reached its stream boundary (markdown finish).
    Finished,
    /// Authoritative replacement (snapshot update, terminal reconciliation).
    Replace,
}

/// Result of applying one transcript event: the row-set diff, the rows the
/// list must re-measure, and the row-level content changes the view must
/// dispatch into materialized renderers.
#[derive(Debug, Default)]
pub(crate) struct ProjectionOutcome {
    pub(crate) diff: ProjectionDiff,
    /// Row indices whose content changed; the list re-measures exactly these
    /// (AC2), independently of any splice.
    pub(crate) remeasure: Vec<usize>,
    pub(crate) row_changes: Vec<(RowId, RowChangeKind)>,
}

/// Minimum number of consecutive activities before they collapse into one
/// group row.
pub(crate) const GROUP_THRESHOLD: usize = 3;

/// Ordered rows derived from the transcript, plus the indexes the view needs.
#[derive(Default)]
pub(crate) struct RowProjection {
    rows: Vec<Row>,
    index: HashMap<RowId, usize>,
    /// call id -> (turn index of the call, call part id)
    activity_pairs: HashMap<CallId, (usize, PartId)>,
    /// Expanded tool-activity groups, keyed by the group row id. Survives
    /// rebuilds so a released group comes back in the same state.
    group_expanded: HashMap<RowId, DisclosureState>,
    /// Disclosure of individual activity rows that are currently folded into
    /// a collapsed group row. Without this, expand → tweak one member →
    /// collapse → expand would lose the member's state. Cleared on `Reset`.
    member_disclosure: HashMap<RowId, DisclosureState>,
}

impl RowProjection {
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn row(&self, ix: usize) -> Option<&Row> {
        self.rows.get(ix)
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn row_index(&self, id: RowId) -> Option<usize> {
        self.index.get(&id).copied()
    }

    /// Row indices belonging to one turn.
    pub(crate) fn rows_in_turn(&self, turn_id: TurnId) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.id.turn == turn_id)
            .map(|(ix, _)| ix)
            .collect()
    }

    pub(crate) fn disclosure(&self, id: RowId) -> DisclosureState {
        self.row_index(id)
            .map(|ix| self.rows[ix].disclosure)
            .unwrap_or_default()
    }

    pub(crate) fn set_disclosure(&mut self, id: RowId, disclosure: DisclosureState) {
        if let Some(ix) = self.row_index(id) {
            let kind = self.rows[ix].id.kind;
            self.rows[ix].disclosure = disclosure;
            if kind == RowKind::ToolActivity {
                // Keep the member's state reachable while it is folded into a
                // collapsed group row.
                self.member_disclosure.insert(id, disclosure);
            }
        }
    }

    pub(crate) fn group_is_expanded(&self, group: RowId) -> bool {
        self.group_expanded
            .get(&group)
            .is_some_and(DisclosureState::is_expanded)
    }

    /// Expand or re-fold a tool activity group and re-derive the rows.
    pub(crate) fn toggle_group(
        &mut self,
        group: RowId,
        transcript: &Transcript,
        typography: &TypographySnapshot,
    ) -> ProjectionDiff {
        let next = if self.group_is_expanded(group) {
            DisclosureState::default()
        } else {
            DisclosureState::EXPANDED
        };
        self.group_expanded.insert(group, next);
        self.rebuild(transcript, typography)
    }

    /// Record one measured height against the row's current content revision.
    pub(crate) fn record_height(
        &mut self,
        id: RowId,
        height: Pixels,
        key: MeasurementKey,
        settled: bool,
    ) {
        let Some(ix) = self.row_index(id) else {
            return;
        };
        let revision = self.rows[ix].content_revision;
        self.rows[ix].height.record(Measured {
            height,
            key,
            content_revision: revision,
            confidence: if settled {
                Confidence::Settled
            } else {
                Confidence::Measured
            },
        });
    }

    pub(crate) fn bump_content_revision(&mut self, id: RowId) {
        if let Some(ix) = self.row_index(id) {
            self.rows[ix].content_revision = self.rows[ix].content_revision.saturating_add(1);
        }
    }

    /// Re-derive every row, preserving heights and disclosure for rows whose
    /// identity survived.
    pub(crate) fn rebuild(
        &mut self,
        transcript: &Transcript,
        typography: &TypographySnapshot,
    ) -> ProjectionDiff {
        self.index_activity_pairs(transcript);
        let old_ids: Vec<RowId> = self.rows.iter().map(|row| row.id).collect();
        let previous: HashMap<RowId, Row> =
            self.rows.iter().map(|row| (row.id, row.clone())).collect();
        let new_rows = derive_rows(
            transcript,
            &self.activity_pairs,
            &self.group_expanded,
            &self.member_disclosure,
            &previous,
            typography,
        );
        let new_ids: Vec<RowId> = new_rows.iter().map(|row| row.id).collect();
        self.rows = new_rows;
        self.reindex();
        splice_diff(&old_ids, &new_ids)
    }

    /// Drop every cached row state. `Reset` re-derives from scratch:
    /// `Transcript::load` re-allocates ids from 1, so a same-shaped
    /// transcript produces identical [`RowId`]s and the rebuild's previous
    /// map would otherwise resurrect cross-session heights and disclosure.
    fn drop_cached_state(&mut self) {
        self.rows.clear();
        self.index.clear();
        self.activity_pairs.clear();
        self.group_expanded.clear();
        self.member_disclosure.clear();
    }

    /// Apply one transcript event.
    pub(crate) fn apply(
        &mut self,
        transcript: &Transcript,
        event: &TranscriptEvent,
        typography: &TypographySnapshot,
    ) -> ProjectionOutcome {
        match event {
            TranscriptEvent::Reset => {
                self.drop_cached_state();
                ProjectionOutcome {
                    diff: self.rebuild(transcript, typography),
                    remeasure: Vec::new(),
                    row_changes: Vec::new(),
                }
            }
            TranscriptEvent::TailAppended { turn_ids } => {
                // Ids stay monotonic across tail appends, so cached heights
                // and disclosure remain valid. A canonically appended turn
                // can carry the result of an earlier activity row (the
                // streaming path arrives as PartInserted); the paired row
                // must re-read its content either way.
                let mut row_changes = Vec::new();
                for turn_id in turn_ids {
                    for part in transcript
                        .turn(*turn_id)
                        .into_iter()
                        .flat_map(|turn| turn.parts.iter().map(|part| part.part_id))
                    {
                        if let Some(call_row) = self.call_row_of_result(transcript, *turn_id, part)
                        {
                            row_changes.push((call_row, RowChangeKind::Replace));
                        }
                    }
                }
                let diff = self.rebuild(transcript, typography);
                let remeasure = row_changes
                    .iter()
                    .filter_map(|(id, _)| self.row_index(*id))
                    .collect();
                ProjectionOutcome {
                    diff,
                    remeasure,
                    row_changes,
                }
            }
            TranscriptEvent::PagePrepended { .. } => {
                // Prepended pages sit above the window: their rows are not
                // materialized yet and materialize fresh with their paired
                // results, so no live renderer needs re-reading.
                ProjectionOutcome {
                    diff: self.rebuild(transcript, typography),
                    remeasure: Vec::new(),
                    row_changes: Vec::new(),
                }
            }
            TranscriptEvent::PartInserted { turn_id, part_id } => {
                // A streaming insert seeds its renderer from the (empty)
                // part; a paired result fills an earlier activity row, which
                // must be re-read instead of appended to.
                let mut row_changes = Vec::new();
                if let Some(call_row) = self.call_row_of_result(transcript, *turn_id, *part_id) {
                    row_changes.push((call_row, RowChangeKind::Replace));
                }
                let diff = self.rebuild(transcript, typography);
                let remeasure = row_changes
                    .iter()
                    .filter_map(|(id, _)| self.row_index(*id))
                    .collect();
                ProjectionOutcome {
                    diff,
                    remeasure,
                    row_changes,
                }
            }
            TranscriptEvent::PartChanged {
                turn_id,
                part_id,
                change,
                ..
            } => {
                // Streaming appends cannot change the row set; touch only the
                // affected row instead of re-deriving the whole list.
                if *change == PartChange::Append
                    && let Some(fast) =
                        self.incremental_append(transcript, *turn_id, *part_id, typography)
                {
                    return fast;
                }
                let change_kind = match change {
                    PartChange::Append => RowChangeKind::Append,
                    PartChange::Finished => RowChangeKind::Finished,
                    PartChange::Replace => RowChangeKind::Replace,
                };
                let mut row_changes = self
                    .rows_for_part(*turn_id, *part_id)
                    .into_iter()
                    .map(|id| (id, change_kind))
                    .collect::<Vec<_>>();
                let diff = self.rebuild(transcript, typography);
                // Content changed: cached measurements are stale and the list
                // must remeasure exactly the affected rows (AC2).
                for (id, _) in &row_changes {
                    self.bump_content_revision(*id);
                }
                let mut remeasure: Vec<usize> = row_changes
                    .iter()
                    .filter_map(|(id, _)| self.row_index(*id))
                    .collect();
                if *change == PartChange::Replace
                    && let Some(call_row) = self.call_row_of_result(transcript, *turn_id, *part_id)
                {
                    row_changes.push((call_row, RowChangeKind::Replace));
                    if let Some(ix) = self.row_index(call_row) {
                        remeasure.push(ix);
                    }
                }
                // A tool-call part publishes exactly one PartChanged
                // (`Finished`, carrying the completed call), and
                // `rows_for_part` deliberately skips activity rows. Route it
                // here as a `Replace` so the lazy arguments section can
                // materialize while the tool is still running.
                if *change == PartChange::Finished
                    && transcript.turn(*turn_id).is_some_and(|turn| {
                        turn.parts.iter().any(|part| {
                            part.part_id == *part_id
                                && matches!(part.source, PartSource::ToolCall { .. })
                        })
                    })
                {
                    let activity = RowId::new(*turn_id, *part_id, RowKind::ToolActivity);
                    if let Some(ix) = self.row_index(activity) {
                        self.bump_content_revision(activity);
                        row_changes.push((activity, RowChangeKind::Replace));
                        remeasure.push(ix);
                    }
                }
                ProjectionOutcome {
                    diff,
                    remeasure,
                    row_changes,
                }
            }
            TranscriptEvent::TurnReplaced { turn_id } => {
                let row_changes: Vec<(RowId, RowChangeKind)> = self
                    .rows()
                    .iter()
                    .filter(|row| row.id.turn == *turn_id)
                    .map(|row| (row.id, RowChangeKind::Replace))
                    .collect();
                let diff = self.rebuild(transcript, typography);
                let remeasure = row_changes
                    .iter()
                    .filter_map(|(id, _)| self.row_index(*id))
                    .collect();
                ProjectionOutcome {
                    diff,
                    remeasure,
                    row_changes,
                }
            }
        }
    }

    /// Streaming fast path for `PartChange::Append`: the append lands on an
    /// unfinished prose/reasoning part, so `stream_ended` is (still) false
    /// and no error, actions, or wait-placeholder row can appear or
    /// disappear — only the affected row's content and estimate move.
    ///
    /// Returns `None` when the precondition does not hold (the part does not
    /// project to exactly one row); the caller falls back to a full rebuild.
    fn incremental_append(
        &mut self,
        transcript: &Transcript,
        turn_id: TurnId,
        part_id: PartId,
        typography: &TypographySnapshot,
    ) -> Option<ProjectionOutcome> {
        let rows = self.rows_for_part(turn_id, part_id);
        if rows.len() != 1 {
            return None;
        }
        // The wait placeholder only exists while the turn has no content row.
        let placeholder = RowId::new(turn_id, PartId::NONE, RowKind::AssistantProse);
        if self.row_index(placeholder).is_some() {
            return None;
        }
        let turn = transcript.turn(turn_id)?;
        let part = turn.parts.iter().find(|part| part.part_id == part_id)?;
        let (source_len, ends_waiting) = match &part.source {
            PartSource::Prose { text, .. } => (text.chars().count(), !text.trim().is_empty()),
            PartSource::Reasoning { reasoning, .. } => (
                reasoning.display.chars().count(),
                !reasoning.display.trim().is_empty(),
            ),
            _ => return None,
        };
        let id = rows[0];
        let ix = self.row_index(id)?;
        {
            let row = &mut self.rows[ix];
            row.source_len = source_len;
            row.ends_waiting = ends_waiting;
            row.height.estimated =
                RowHeight::estimate(row.id.kind, source_len, row.block_hint, typography);
        }
        self.bump_content_revision(id);
        Some(ProjectionOutcome {
            diff: ProjectionDiff::None,
            remeasure: vec![ix],
            row_changes: vec![(id, RowChangeKind::Append)],
        })
    }

    /// Invalidate cached measurements that no longer match `typography` and
    /// refresh estimates. Returns the row indices the list must remeasure.
    pub(crate) fn invalidate_typography(&mut self, typography: &TypographySnapshot) -> Vec<usize> {
        let mut changed = Vec::new();
        for (ix, row) in self.rows.iter_mut().enumerate() {
            let stale = row.height.measured.is_some_and(|measured| {
                measured.key.typography_revision != typography.typography_revision
                    || measured.key.theme_revision != typography.theme_revision
            });
            if stale {
                row.height.invalidate();
            }
            let next = RowHeight::estimate(row.id.kind, row.source_len, row.block_hint, typography);
            if (next - row.height.estimated).abs() > gpui::px(0.5) {
                row.height.estimated = next;
                changed.push(ix);
            }
        }
        changed
    }

    /// Row ids of `turn_id` that render one part's content.
    fn rows_for_part(&self, turn_id: TurnId, part_id: PartId) -> Vec<RowId> {
        self.rows
            .iter()
            .filter(|row| {
                row.id.turn == turn_id
                    && row.id.part == part_id
                    && matches!(
                        row.id.kind,
                        RowKind::AssistantProse | RowKind::Reasoning | RowKind::UserBubble
                    )
            })
            .map(|row| row.id)
            .collect()
    }

    /// The activity row a tool result belongs to, when its call is known.
    fn call_row_of_result(
        &self,
        transcript: &Transcript,
        turn_id: TurnId,
        part_id: PartId,
    ) -> Option<RowId> {
        let turn = transcript.turn(turn_id)?;
        let part = turn.parts.iter().find(|part| part.part_id == part_id)?;
        let PartSource::ToolResult(result) = &part.source else {
            return None;
        };
        let (_, call_part) = self.activity_pairs.get(&result.call_id).copied()?;
        let call_turn = transcript
            .turns()
            .iter()
            .find(|turn| turn.parts.iter().any(|part| part.part_id == call_part))?
            .turn_id;
        let activity = RowId::new(call_turn, call_part, RowKind::ToolActivity);
        if self.row_index(activity).is_some() {
            return Some(activity);
        }
        // The call sits inside a collapsed group; re-reading the group header
        // row keeps the count and heights fresh.
        self.rows
            .iter()
            .find(|row| row.id.turn == call_turn && row.id.kind == RowKind::ToolActivityGroup)
            .map(|row| row.id)
    }

    fn index_activity_pairs(&mut self, transcript: &Transcript) {
        self.activity_pairs.clear();
        for (turn_index, turn) in transcript.turns().iter().enumerate() {
            for part in &turn.parts {
                if let PartSource::ToolCall { id, .. } = &part.source {
                    self.activity_pairs
                        .entry(id.clone())
                        .or_insert((turn_index, part.part_id));
                }
            }
        }
    }

    fn reindex(&mut self) {
        self.index.clear();
        self.index.reserve(self.rows.len());
        for (ix, row) in self.rows.iter().enumerate() {
            self.index.insert(row.id, ix);
        }
    }
}

/// Compare old and new row identities and produce the cheapest diff.
fn splice_diff(old: &[RowId], new: &[RowId]) -> ProjectionDiff {
    if old == new {
        return ProjectionDiff::None;
    }
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let removed = old.len() - prefix - suffix;
    let inserted = new.len() - prefix - suffix;
    if prefix == 0 && suffix == 0 && (!old.is_empty() || !new.is_empty()) {
        // Nothing in common: the retained list is cheapest off a reset.
        return ProjectionDiff::Rebuild;
    }
    ProjectionDiff::Splice {
        range: prefix..prefix + removed,
        inserted,
    }
}

/// Everything the row derivation needs before heights are attached.
struct RowDraft {
    id: RowId,
    turn_index: usize,
    role: Role,
    source_len: usize,
    block_hint: usize,
    group_count: usize,
    ends_waiting: bool,
    /// The expanded tool-activity group this row belongs to.
    group: Option<RowId>,
    /// Whether this row is the first member of its expanded group.
    group_leader: bool,
    /// Tool call display name, carried so a collapsed group header can name
    /// its most recent member.
    name: Option<String>,
    /// Most recent member's name for a collapsed group row.
    group_latest: Option<String>,
}

fn draft(
    id: RowId,
    turn_index: usize,
    role: Role,
    source_len: usize,
    block_hint: usize,
) -> RowDraft {
    RowDraft {
        id,
        turn_index,
        role,
        source_len,
        block_hint,
        group_count: 0,
        ends_waiting: false,
        group: None,
        group_leader: false,
        name: None,
        group_latest: None,
    }
}

fn derive_rows(
    transcript: &Transcript,
    activity_pairs: &HashMap<CallId, (usize, PartId)>,
    group_expanded: &HashMap<RowId, DisclosureState>,
    member_disclosure: &HashMap<RowId, DisclosureState>,
    previous: &HashMap<RowId, Row>,
    typography: &TypographySnapshot,
) -> Vec<Row> {
    let turns = transcript.turns();
    let mut drafts: Vec<RowDraft> = Vec::new();

    // Unpaired results render inside their own turn.
    let mut unpaired: HashMap<usize, Vec<&Part>> = HashMap::new();
    for (turn_index, turn) in turns.iter().enumerate() {
        for part in &turn.parts {
            let PartSource::ToolResult(result) = &part.source else {
                continue;
            };
            if !activity_pairs.contains_key(&result.call_id) {
                unpaired.entry(turn_index).or_default().push(part);
            }
        }
    }

    for (turn_index, turn) in turns.iter().enumerate() {
        let turn_id = turn.turn_id;
        let mut turn_drafts: Vec<RowDraft> = Vec::new();

        for part in &turn.parts {
            match (&part.source, turn.role) {
                (PartSource::Prose { text, .. }, Role::User) => {
                    turn_drafts.push(draft(
                        RowId::new(turn_id, part.part_id, RowKind::UserBubble),
                        turn_index,
                        turn.role,
                        text.chars().count(),
                        1,
                    ));
                }
                (PartSource::Prose { text, .. }, Role::Assistant) => {
                    let mut item = draft(
                        RowId::new(turn_id, part.part_id, RowKind::AssistantProse),
                        turn_index,
                        turn.role,
                        text.chars().count(),
                        1,
                    );
                    item.ends_waiting = !text.trim().is_empty();
                    turn_drafts.push(item);
                }
                (PartSource::Reasoning { reasoning, .. }, _) => {
                    let mut item = draft(
                        RowId::new(turn_id, part.part_id, RowKind::Reasoning),
                        turn_index,
                        turn.role,
                        reasoning.display.chars().count(),
                        1,
                    );
                    item.ends_waiting = !reasoning.display.trim().is_empty();
                    turn_drafts.push(item);
                }
                (PartSource::ToolCall { name, .. }, Role::Assistant) => {
                    let mut item = draft(
                        RowId::new(turn_id, part.part_id, RowKind::ToolActivity),
                        turn_index,
                        turn.role,
                        name.chars().count(),
                        1,
                    );
                    item.ends_waiting = !name.is_empty();
                    item.name = Some(name.clone());
                    turn_drafts.push(item);
                }
                // A prose part inside a tool turn has no row of its own.
                (PartSource::Prose { .. }, Role::Tool) => {}
                (PartSource::ToolResult(_), _) => {}
                (PartSource::ToolCall { .. }, _) => {}
            }
        }
        for part in unpaired.get(&turn_index).into_iter().flatten() {
            let PartSource::ToolResult(result) = &part.source else {
                continue;
            };
            turn_drafts.push(draft(
                RowId::new(turn_id, part.part_id, RowKind::ToolActivity),
                turn_index,
                turn.role,
                result.content.chars().count(),
                1,
            ));
        }

        let mut turn_drafts = collapse_groups(turn_drafts, turn_id, turn.role, group_expanded);

        if turn.error.is_some() {
            turn_drafts.push(draft(
                RowId::new(turn_id, PartId::NONE, RowKind::TurnError),
                turn_index,
                turn.role,
                0,
                0,
            ));
        }
        // Same gate as P1: any turn whose stream ended with copyable prose
        // and no error offers the action row (user messages included —
        // design.md keeps the P1 condition).
        if turn.error.is_none() && stream_ended(turn) && has_copyable_text(turn) {
            turn_drafts.push(draft(
                RowId::new(turn_id, PartId::NONE, RowKind::TurnActions),
                turn_index,
                turn.role,
                0,
                0,
            ));
        }

        // Synthetic wait placeholder: an assistant turn with no other rows
        // still owns one row so the waiting shimmer has a home.
        if turn.role == Role::Assistant && turn_drafts.is_empty() {
            turn_drafts.push(draft(
                RowId::new(turn_id, PartId::NONE, RowKind::AssistantProse),
                turn_index,
                turn.role,
                0,
                0,
            ));
        }

        drafts.extend(turn_drafts);
    }

    attach_rows(drafts, member_disclosure, previous, typography)
}

/// Collapse runs of ≥ GROUP_THRESHOLD consecutive tool-activity rows into one
/// group row. An expanded group renders its member rows again, each tagged
/// with the owning group so the header can re-fold.
fn collapse_groups(
    drafts: Vec<RowDraft>,
    turn_id: TurnId,
    role: Role,
    group_expanded: &HashMap<RowId, DisclosureState>,
) -> Vec<RowDraft> {
    if role != Role::Assistant {
        return drafts;
    }
    let mut out: Vec<RowDraft> = Vec::new();
    let mut run: Vec<RowDraft> = Vec::new();
    fn flush(
        run: &mut Vec<RowDraft>,
        out: &mut Vec<RowDraft>,
        turn_id: TurnId,
        group_expanded: &HashMap<RowId, DisclosureState>,
    ) {
        if run.is_empty() {
            return;
        }
        if run.len() >= GROUP_THRESHOLD {
            let group = RowId::new(turn_id, run[0].id.part, RowKind::ToolActivityGroup);
            if group_expanded
                .get(&group)
                .is_some_and(DisclosureState::is_expanded)
            {
                // Every member declares its group; the first one leads it, so
                // the expand/collapse round trip uses one stable group id.
                for (index, item) in run.iter_mut().enumerate() {
                    item.group = Some(group);
                    item.group_leader = index == 0;
                }
                out.append(run);
            } else {
                let count = run.len();
                // The header names the step the user last saw happen.
                let group_latest = run.iter().rev().find_map(|item| item.name.clone());
                let first = run.remove(0);
                out.push(RowDraft {
                    id: group,
                    group_count: count,
                    group_latest,
                    ..first
                });
                run.clear();
            }
        } else {
            out.append(run);
        }
    }
    for item in drafts {
        if item.id.kind == RowKind::ToolActivity {
            run.push(item);
        } else {
            flush(&mut run, &mut out, turn_id, group_expanded);
            out.push(item);
        }
    }
    flush(&mut run, &mut out, turn_id, group_expanded);
    out
}

fn attach_rows(
    drafts: Vec<RowDraft>,
    member_disclosure: &HashMap<RowId, DisclosureState>,
    previous: &HashMap<RowId, Row>,
    typography: &TypographySnapshot,
) -> Vec<Row> {
    let total = drafts.len();
    let mut first_seen: HashMap<usize, usize> = HashMap::with_capacity(total);
    let mut last_seen: HashMap<usize, usize> = HashMap::with_capacity(total);
    for (ix, item) in drafts.iter().enumerate() {
        first_seen.entry(item.turn_index).or_insert(ix);
        last_seen.insert(item.turn_index, ix);
    }
    drafts
        .into_iter()
        .enumerate()
        .map(|(ix, item)| {
            let estimate =
                RowHeight::estimate(item.id.kind, item.source_len, item.block_hint, typography);
            // Disclosure comes from the surviving row when the identity is
            // still projected, otherwise from the member side map so a
            // member folded into a collapsed group keeps its state.
            let disclosure = previous
                .get(&item.id)
                .map(|row| row.disclosure)
                .or_else(|| member_disclosure.get(&item.id).copied())
                .unwrap_or_default();
            let (content_revision, measured) = match previous.get(&item.id) {
                // Identity survived: keep the cached measurement (its
                // freshness is guarded by the content revision), but
                // re-estimate for the current content.
                Some(row) => (row.content_revision, row.height.measured),
                None => (0, None),
            };
            let height = RowHeight {
                estimated: estimate,
                measured,
            };
            Row {
                id: item.id,
                turn_index: item.turn_index,
                role: item.role,
                is_first_in_turn: first_seen.get(&item.turn_index) == Some(&ix),
                is_last_in_turn: last_seen.get(&item.turn_index) == Some(&ix),
                content_revision,
                height,
                disclosure,
                group_count: item.group_count,
                source_len: item.source_len,
                block_hint: item.block_hint,
                ends_waiting: item.ends_waiting,
                group: item.group,
                group_leader: item.group_leader,
                group_latest: item.group_latest,
            }
        })
        .collect()
}

/// Whether one turn's rows contain content that ends the assistant wait
/// shimmer (P1 `has_wait_ending_content` semantics).
pub(crate) fn turn_has_wait_ending_row(rows: &[&Row]) -> bool {
    rows.iter().any(|row| row.ends_waiting)
}

#[cfg(test)]
mod tests;
