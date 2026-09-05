//! `TranscriptView`: the retained row list over the projected transcript.
//!
//! Owns the conversation's single [`ListState`], the [`RowProjection`], and
//! one [`RowSlot`] per projected row. Streaming events flow in through
//! [`TranscriptView::handle_transcript_event`], which maps a
//! [`ProjectionDiff`] onto the list, re-measures the rows the outcome
//! declares, and dispatches content changes into the materialized renderers;
//! window synchronization (materialize/release) runs only in update phase,
//! via deferred handlers and `on_next_frame`. The render path may only
//! record the frame-observation fields `pending_materialize` /
//! `pending_typography` / `window_dirty`.

pub(in crate::chat) mod scrolling;
#[cfg(test)]
mod tests;
mod window;

use gpui::{
    AnyElement, App, Context, Entity, FollowMode, InteractiveElement as _, IntoElement, ListOffset,
    ListState, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, list, prelude::FluentBuilder as _,
    px,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, IconName, Sizable as _, StyledExt as _, button::Button,
    scroll::ScrollableElement as _, v_flex,
};
use rust_i18n::t;

use crate::chat::ChatView;
use crate::chat::projection::{
    ProjectionDiff, RowChangeKind, RowId, RowKind, RowProjection, TypographySnapshot,
    turn_has_wait_ending_row,
};
use crate::chat::rows::{
    DisclosureTarget, MaterializeContext, NestedScrollReplay, RowAction, RowActionDispatch,
    RowChange, RowRenderContext, RowRenderer, renderer_for,
};
use crate::chat::transcript::{
    Part, PartSource, Transcript, TranscriptEvent, TranscriptSnapshot, TurnId,
};
use crate::ui::markdown::MarkdownPresentation;

use self::scrolling::{SmoothScrollState, smooth_scroll_animation_enabled};

/// One list slot, index-aligned with the projection rows.
pub(in crate::chat) struct RowSlot {
    pub(in crate::chat) renderer: Box<dyn RowRenderer>,
}

/// Estimated-height hint for rows the list has not measured yet.
const ROW_HEIGHT_HINT: Pixels = px(160.);

/// Rows near the top that trigger loading an earlier page (R7).
const PREPEND_TRIGGER_ROWS: usize = 8;

pub(in crate::chat) struct TranscriptView {
    pub(in crate::chat) list_state: ListState,
    pub(in crate::chat) projection: RowProjection,
    pub(in crate::chat) slots: Vec<RowSlot>,
    /// Row id each slot was built for; parallel to `slots` and the identity
    /// key for reprojecting.
    slot_ids: Vec<RowId>,
    /// The turn whose rows the pointer is over, if any (R4). Derived from
    /// the per-turn hover counts; see `handle_hover_turn`.
    hovered_turn: Option<TurnId>,
    /// Rows per turn currently claiming the pointer (enter/leave counting).
    hover_counts: std::collections::HashMap<TurnId, u32>,
    /// The turn whose row most recently reported an enter event.
    last_entered_turn: Option<TurnId>,
    /// Pointer position captured before rows slide underneath it; new hover
    /// claims are ignored while the pointer rests there (R4).
    parked_pointer: Option<Point<Pixels>>,
    pending_materialize: Vec<RowId>,
    /// Typography observed on the render path, applied by the next
    /// update-phase window sync. Render never touches the projection or the
    /// list state directly.
    pending_typography: Option<TypographySnapshot>,
    window_dirty: bool,
    sync_scheduled: bool,
    pub(in crate::chat) typography: TypographySnapshot,
    content_width: Pixels,
    /// Conversation viewport height, recorded at prepaint of the message
    /// list. Viewport-relative height budgets (reasoning) resolve against it.
    pub(in crate::chat) viewport_height: Pixels,
    pub(in crate::chat) smooth_scroll: SmoothScrollState,
    is_generating: bool,
    streaming_turn: Option<TurnId>,
    turn_count: usize,
    waiting_turn: Option<TurnId>,
    jump_visible: bool,
    /// Reading anchor captured before a prepend moves every row down (AC3).
    prepend_anchor: Option<(RowId, Pixels)>,
    /// Pointer position as of the last painted frame; the snapshot follow
    /// scrolls park hover at (R4).
    last_pointer: Option<Point<Pixels>>,
    /// Rows the most recent remeasure request asked the list to re-measure.
    /// Test observation for the remeasure contract (AC6).
    #[cfg(test)]
    last_remeasure_request: Option<Vec<usize>>,
}

impl TranscriptView {
    pub(in crate::chat) fn new(
        transcript: &Entity<Transcript>,
        snapshot: &TranscriptSnapshot,
        typography: TypographySnapshot,
        restore: Option<(RowProjection, Option<ListOffset>)>,
        cx: &mut Context<ChatView>,
    ) -> Self {
        let anchor = restore.as_ref().and_then(|(_, anchor)| *anchor);
        let mut projection = restore
            .map(|(projection, _)| projection)
            .unwrap_or_default();
        projection.rebuild(transcript.read(cx), &typography);
        let slots: Vec<RowSlot> = projection
            .rows()
            .iter()
            .map(|row| RowSlot {
                renderer: renderer_for(row.kind()),
            })
            .collect();
        let slot_ids: Vec<RowId> = projection.rows().iter().map(|row| row.id()).collect();
        let list_state = ListState::new(
            0,
            gpui::ListAlignment::Top,
            crate::chat::MESSAGE_LIST_OVERDRAW,
        )
        .with_uniform_item_height(ROW_HEIGHT_HINT);
        let mut view = Self {
            list_state,
            projection,
            slots,
            slot_ids,
            hovered_turn: None,
            hover_counts: std::collections::HashMap::new(),
            last_entered_turn: None,
            parked_pointer: None,
            pending_materialize: Vec::new(),
            pending_typography: None,
            window_dirty: false,
            sync_scheduled: false,
            typography,
            content_width: px(720.),
            viewport_height: px(0.),
            smooth_scroll: SmoothScrollState::default(),
            is_generating: false,
            streaming_turn: snapshot.streaming().map(|(turn, _)| turn),
            turn_count: snapshot.turn_count(),
            waiting_turn: None,
            jump_visible: false,
            prepend_anchor: None,
            last_pointer: None,
            #[cfg(test)]
            last_remeasure_request: None,
        };
        view.refresh_waiting_turn();

        match anchor {
            Some(anchor) => {
                // Cold restore: first-frame placeholders come from settled
                // heights where available, and the anchor wins (AC5/AC6).
                let hint = view.restore_hint();
                view.list_state
                    .reset_with_uniform_height(view.projection.len(), hint);
                view.list_state.scroll_to(anchor);
            }
            None => {
                let hint = view.uniform_hint();
                view.list_state
                    .reset_with_uniform_height(view.projection.len(), hint);
                view.list_state.set_follow_mode(FollowMode::Tail);
                view.list_state.scroll_to_end();
            }
        }
        view
    }

    /// Uniform estimate hint for rows the list has not measured: the P1
    /// `MESSAGE_HEIGHT_HINT`, which keeps the first-frame scrollbar sized
    /// from turn-height heuristics rather than per-row estimates.
    fn uniform_hint(&self) -> Pixels {
        crate::chat::MESSAGE_HEIGHT_HINT
    }

    /// Cold-restore hint: coverage-weighted blend of the settled mean and
    /// the uniform estimate. A plain settled mean collapses toward zero when
    /// few rows carry measurements (unsettled rows contribute nothing), which
    /// systematically under-sizes the first-frame scrollbar and mis-places
    /// `scroll_to(anchor)`.
    fn restore_hint(&self) -> Pixels {
        // The saved measurements were taken under the cold session's width
        // bucket; the current viewport is not laid out yet and may differ.
        let key = self
            .projection
            .rows()
            .iter()
            .find_map(|row| row.measured_key())
            .unwrap_or_else(|| self.current_key());
        let settled: Vec<Pixels> = self
            .projection
            .rows()
            .iter()
            .filter_map(|row| row.settled_height(&key))
            .collect();
        if settled.is_empty() {
            return self.uniform_hint();
        }
        let settled_mean = settled.iter().copied().sum::<Pixels>().as_f32() / settled.len() as f32;
        let coverage = settled.len() as f32 / self.projection.len().max(1) as f32;
        px(settled_mean * coverage + self.uniform_hint().as_f32() * (1. - coverage))
    }

    fn refresh_waiting_turn(&mut self) {
        self.waiting_turn = None;
        if !self.is_generating {
            return;
        }
        let rows = self.projection.rows();
        let Some(last) = rows.last() else {
            return;
        };
        let turn_index = last.turn_index();
        if turn_index + 1 != self.turn_count {
            return;
        }
        if last.role() != crate::chat::transcript::Role::Assistant {
            return;
        }
        let turn_rows: Vec<&crate::chat::projection::Row> = rows
            .iter()
            .filter(|row| row.turn_index() == turn_index)
            .collect();
        if turn_rows.iter().any(|row| row.kind() == RowKind::TurnError) {
            return;
        }
        if turn_has_wait_ending_row(&turn_rows) {
            return;
        }
        self.waiting_turn = Some(last.id().turn);
    }

    // ------------------------------------------------------------------
    // Transcript event flow
    // ------------------------------------------------------------------

    /// Fold one transcript event into the projection, the retained list, and
    /// the materialized renderers.
    pub(in crate::chat) fn handle_transcript_event(
        &mut self,
        event: &TranscriptEvent,
        snapshot: &TranscriptSnapshot,
        transcript: &Entity<Transcript>,
        presentation: &MarkdownPresentation,
        user_message_markdown: bool,
        cx: &mut Context<ChatView>,
    ) {
        let outcome = {
            let guard = transcript.read(cx);
            self.projection.apply(guard, event, &self.typography)
        };

        // Keep slots index-aligned with rows, preserving renderers by id.
        self.reproject_slots();

        self.streaming_turn = snapshot.streaming().map(|(turn, _)| turn);
        self.turn_count = snapshot.turn_count();
        self.refresh_waiting_turn();

        // Dispatch content changes into materialized renderers.
        for (row_id, change_kind) in &outcome.row_changes {
            let Some(ix) = self.projection.row_index(*row_id) else {
                continue;
            };
            if ix >= self.slots.len() || !self.slots[ix].renderer.is_materialized() {
                // Rows of the streaming turn are (re)materialized below.
                continue;
            }
            let change = match change_kind {
                RowChangeKind::Append => {
                    let TranscriptEvent::PartChanged { delta, .. } = event else {
                        continue;
                    };
                    RowChange::Append {
                        delta: delta.as_ref(),
                    }
                }
                RowChangeKind::Finished => RowChange::Finished,
                RowChangeKind::Replace => RowChange::Replace,
            };
            let content = self.read_row_content(*row_id, transcript, cx);
            let ctx = MaterializeContext {
                row_id: *row_id,
                part: content.part.as_ref(),
                paired_result: content.paired_result.as_ref(),
                error: content.error.as_ref(),
                presentation,
                user_message_markdown,
                owner_id: crate::chat::next_body_owner_id(),
                append_replays_part: false,
            };
            self.slots[ix].renderer.apply(&change, &ctx, cx);
        }

        // Apply the list diff, then re-measure exactly the rows whose
        // content changed. The two declarations are orthogonal: a splice
        // must not swallow a remeasure.
        self.apply_diff(&outcome.diff);
        self.remeasure_rows(&outcome.remeasure);

        // Streaming rows are always materialized (R6): do it immediately so
        // the update phase owns entity creation. Tail appends and terminal
        // rewrites are equally on-screen, so their rows materialize now too;
        // the next laid-out sync releases anything outside the window.
        // Only the inserted part's row seeds empty — the following Append
        // event replays its content; every other row re-reads what it has.
        let replay_part = match event {
            TranscriptEvent::PartInserted { part_id, .. } => Some(*part_id),
            _ => None,
        };
        let mut eager_turns: Vec<TurnId> = Vec::new();
        if let Some(turn) = self.streaming_turn {
            eager_turns.push(turn);
        }
        match event {
            TranscriptEvent::TailAppended { turn_ids } => {
                eager_turns.extend(turn_ids.iter().copied());
            }
            TranscriptEvent::TurnReplaced { turn_id } => eager_turns.push(*turn_id),
            _ => {}
        }
        for turn in eager_turns {
            let indices = self.projection.rows_in_turn(turn);
            for ix in indices {
                if ix < self.slots.len() && !self.slots[ix].renderer.is_materialized() {
                    let replay = self
                        .projection
                        .row(ix)
                        .is_some_and(|row| Some(row.id().part) == replay_part);
                    self.materialize_row(
                        ix,
                        transcript,
                        presentation,
                        user_message_markdown,
                        replay,
                        cx,
                    );
                }
            }
        }

        self.window_dirty = true;
        self.schedule_sync(cx);
    }

    /// Rebuild `slots` so index i holds the renderer for row i, preserving
    /// renderer identity by [`RowId`].
    pub(in crate::chat) fn reproject_slots(&mut self) {
        let previous: Vec<RowSlot> = std::mem::take(&mut self.slots);
        let previous_ids: Vec<RowId> = std::mem::take(&mut self.slot_ids);
        let mut map: std::collections::HashMap<RowId, RowSlot> =
            previous_ids.into_iter().zip(previous).collect();
        self.slots = self
            .projection
            .rows()
            .iter()
            .map(|row| {
                map.remove(&row.id()).unwrap_or(RowSlot {
                    renderer: renderer_for(row.kind()),
                })
            })
            .collect();
        self.slot_ids = self.projection.rows().iter().map(|row| row.id()).collect();
    }

    pub(in crate::chat) fn apply_diff(&mut self, diff: &ProjectionDiff) {
        match diff {
            ProjectionDiff::None => {}
            ProjectionDiff::Splice { range, inserted } => {
                if *inserted > 0 {
                    self.list_state.splice_with_uniform_height(
                        range.clone(),
                        *inserted,
                        self.uniform_hint(),
                    );
                } else {
                    self.list_state.splice(range.clone(), 0);
                }
            }
            ProjectionDiff::Rebuild => {
                self.list_state
                    .reset_with_uniform_height(self.projection.len(), self.uniform_hint());
            }
        }
    }

    /// Ask the retained list to re-measure `rows`, merged into the fewest
    /// contiguous ranges. Remeasure requests are orthogonal to row-set
    /// diffs: this never changes the row count or the scroll anchor.
    pub(in crate::chat) fn remeasure_rows(&mut self, rows: &[usize]) {
        let mut rows = rows.to_vec();
        rows.sort_unstable();
        rows.dedup();
        #[cfg(test)]
        {
            self.last_remeasure_request = Some(rows.clone());
        }
        let mut run: Option<std::ops::Range<usize>> = None;
        for ix in rows {
            match &mut run {
                Some(r) if r.end == ix => r.end = ix + 1,
                _ => {
                    if let Some(r) = run.take() {
                        self.list_state.remeasure_items(r);
                    }
                    run = Some(ix..ix + 1);
                }
            }
        }
        if let Some(r) = run.take() {
            self.list_state.remeasure_items(r);
        }
    }

    // ------------------------------------------------------------------
    // Materialization
    // ------------------------------------------------------------------

    /// Snapshot the transcript-backed content one row renders.
    fn read_row_content(
        &self,
        row_id: RowId,
        transcript: &Entity<Transcript>,
        cx: &Context<ChatView>,
    ) -> RowContent {
        let guard = transcript.read(cx);
        let turn = guard.turn(row_id.turn);
        let part = turn.and_then(|turn| {
            turn.parts
                .iter()
                .find(|part| part.part_id == row_id.part)
                .cloned()
        });
        // Tool activity rows show the paired result; an unpaired result row
        // *is* its own result.
        let mut paired_result = None;
        if row_id.kind == RowKind::ToolActivity {
            match part.as_ref().map(|part| &part.source) {
                Some(PartSource::ToolCall { id, .. }) => {
                    paired_result = guard.turns().iter().rev().find_map(|turn| {
                        turn.parts.iter().find_map(|part| match &part.source {
                            PartSource::ToolResult(result) if &result.call_id == id => {
                                Some(result.clone())
                            }
                            _ => None,
                        })
                    });
                }
                Some(PartSource::ToolResult(result)) => {
                    paired_result = Some(result.clone());
                }
                _ => {}
            }
        }
        let error = turn.and_then(|turn| turn.error.clone());
        RowContent {
            part,
            paired_result,
            error,
        }
    }

    /// Create the renderer's content entities. Update phase only.
    ///
    /// `append_replays_part` marks the row of a part a live `PartInserted`
    /// just created: its renderer seeds empty because the following `Append`
    /// event replays the part from the beginning. Window sync and cold
    /// restores pass `false` and re-read the accumulated content.
    pub(super) fn materialize_row(
        &mut self,
        ix: usize,
        transcript: &Entity<Transcript>,
        presentation: &MarkdownPresentation,
        user_message_markdown: bool,
        append_replays_part: bool,
        cx: &mut Context<ChatView>,
    ) {
        let Some(row) = self.projection.row(ix) else {
            return;
        };
        let row_id = row.id();
        let content = self.read_row_content(row_id, transcript, cx);
        let ctx = MaterializeContext {
            row_id,
            part: content.part.as_ref(),
            paired_result: content.paired_result.as_ref(),
            error: content.error.as_ref(),
            presentation,
            user_message_markdown,
            owner_id: crate::chat::next_body_owner_id(),
            append_replays_part,
        };
        let slot = &mut self.slots[ix];
        // The disclosure arrives before materialization so the renderer
        // rebuilds exactly the lazy bodies its restored state calls for
        // (an activity row restored open re-creates its result body here).
        slot.renderer
            .sync_disclosure(self.projection.disclosure(row_id));
        slot.renderer.materialize(&ctx, cx);
    }

    /// The diagnostics surface for materialized-row counts (review gates).
    #[cfg(test)]
    pub(in crate::chat) fn materialized_row_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.renderer.is_materialized())
            .count()
    }

    #[cfg(test)]
    pub(in crate::chat) fn materialized_row_indices(&self) -> std::collections::BTreeSet<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.renderer.is_materialized())
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Re-materialize user bubbles after the markdown/plain preference
    /// flips: the choice is made when content entities are created.
    pub(in crate::chat) fn release_user_bubble_rows(&mut self, cx: &mut Context<ChatView>) {
        for slot in &mut self.slots {
            if slot.renderer.kind() == RowKind::UserBubble && slot.renderer.is_materialized() {
                slot.renderer.release(cx);
            }
        }
        self.window_dirty = true;
        self.schedule_sync(cx);
    }

    // ------------------------------------------------------------------
    // Window synchronization scheduling
    // ------------------------------------------------------------------

    /// Defer one window synchronization to the end of the update cycle.
    pub(in crate::chat) fn schedule_sync(&mut self, cx: &mut Context<ChatView>) {
        if self.sync_scheduled {
            return;
        }
        self.sync_scheduled = true;
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| this.sync_window_now(cx));
        });
    }

    /// Schedule one window synchronization from the render path. The render
    /// closure only records `pending_materialize` / `window_dirty`; the sync
    /// itself runs in update phase, after the update cycle ends.
    fn schedule_sync_frame(&mut self, window: &mut Window, cx: &mut Context<ChatView>) {
        let _ = window;
        self.schedule_sync(cx);
    }

    // ------------------------------------------------------------------
    // Render path
    // ------------------------------------------------------------------

    /// Render one list item. Contract: only `pending_materialize` /
    /// `window_dirty` writes plus frame scheduling — no entity creation, no
    /// transcript locks, no external entity access.
    fn render_row(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) -> AnyElement {
        let Some(row) = self.projection.row(ix).cloned() else {
            return div().into_any_element();
        };
        let waiting = self.waiting_turn == Some(row.id().turn);
        let hovering = self.hovered_turn == Some(row.id().turn);
        let group_count = row.group_count();
        let group = row.group();
        let group_leader = row.leads_group();
        let group_latest = row.group_latest().map(SharedString::from);
        let key = self.current_key();
        let dispatch = RowActionDispatch::new(cx.weak_entity());

        let materialized = self
            .slots
            .get(ix)
            .is_some_and(|slot| slot.renderer.is_materialized());
        if !materialized {
            self.pending_materialize.push(row.id());
            self.window_dirty = true;
        }
        // Every rendered frame ensures one window synchronization follows:
        // placeholders request materialization, and steady-state frames give
        // the sync a chance to release rows that left the window.
        self.schedule_sync_frame(window, cx);
        let content: AnyElement = if !materialized {
            // Placeholder keeps the estimate stable for this frame; the next
            // frame materializes through `sync_window`.
            div().h(row.effective_height(&key)).into_any_element()
        } else if waiting && !row.is_first_in_turn() {
            // The waiting turn renders one shimmer in place of all its rows.
            div().into_any_element()
        } else if waiting {
            shimmer_row(&row, cx)
        } else {
            let ctx = RowRenderContext {
                row_id: row.id(),
                role: row.role(),
                turn_hovered: hovering,
                waiting,
                viewport_height: self.viewport_height,
                group_count,
                group_latest,
                group,
                group_leader,
                dispatch,
            };
            self.slots[ix].renderer.render(&ctx, window, cx)
        };

        let row_id = row.id();
        let dispatch_hover = RowActionDispatch::new(cx.weak_entity());
        let dispatch_measure = dispatch_hover.clone();
        let turn_id = row.id().turn;
        let settled = self.streaming_turn != Some(row.id().turn);

        let max_width = crate::chat::CONTENT_MAX_WIDTH;
        // An idle (non-waiting) placeholder row is visually nothing; keep it
        // out of the debug selector map so tests see it as absent.
        let invisible_row = !waiting
            && row.kind() == RowKind::AssistantProse
            && row.id().part == crate::chat::transcript::PartId::NONE;
        let mut wrapper = div().id(("row", ix as u64)).w_full();
        if !invisible_row {
            wrapper = wrapper.debug_selector(move || row_id.debug_name());
        }
        wrapper
            .when(row.is_last_in_turn(), |this| this.pb_5())
            .when(!row.is_last_in_turn(), |this| this.pb_1())
            .on_hover(move |hovering: &bool, window, cx| {
                dispatch_hover.send(
                    RowAction::HoverTurn {
                        turn: turn_id,
                        entered: *hovering,
                    },
                    window,
                    cx,
                );
            })
            .on_prepaint(move |bounds, window, cx| {
                dispatch_measure.send(
                    RowAction::Measured {
                        row_id,
                        height: bounds.size.height,
                        settled,
                    },
                    window,
                    cx,
                );
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .px_6()
                    .child(div().w_full().max_w(max_width).child(content)),
            )
            .into_any_element()
    }

    /// `HoverTurn` handling: honor the parked pointer, then track hover with
    /// per-turn counts (R4). One mouse move re-evaluates every hover element
    /// with the same pointer position, so enter/leave events for different
    /// rows arrive in arbitrary order — order-insensitive counting keeps the
    /// hovered turn stable while the pointer walks down the transcript.
    pub(in crate::chat) fn handle_hover_turn(
        &mut self,
        turn: TurnId,
        entered: bool,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) {
        let pointer = window.mouse_position();
        if let Some(parked) = self.parked_pointer {
            if pointer == parked {
                return;
            }
            self.parked_pointer = None;
        }
        if entered {
            *self.hover_counts.entry(turn).or_insert(0) += 1;
            self.last_entered_turn = Some(turn);
        } else if let Some(count) = self.hover_counts.get_mut(&turn) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.hover_counts.remove(&turn);
            }
        }
        // The most recently entered turn wins while its rows still claim the
        // pointer; once its count reaches zero, fall back to any other turn
        // still under the pointer before clearing.
        let resolved = self
            .last_entered_turn
            .filter(|turn| self.hover_counts.contains_key(turn))
            .or_else(|| self.hover_counts.keys().next().copied());
        if self.hovered_turn != resolved {
            self.hovered_turn = resolved;
            cx.notify();
        }
    }

    /// Freeze hover at the current pointer before rows slide underneath (R4).
    pub(in crate::chat) fn park_pointer(&mut self, window: &Window) {
        self.parked_pointer = Some(window.mouse_position());
    }

    /// Freeze hover at the last painted pointer position. Used from update
    /// paths that have no window (subscription handlers): the pointer cannot
    /// have moved between frames without a repaint.
    pub(in crate::chat) fn park_at_last_pointer(&mut self) {
        if let Some(pointer) = self.last_pointer {
            self.parked_pointer = Some(pointer);
        }
    }

    /// Snapshot the pointer each frame so update-phase scrolls can park.
    pub(in crate::chat) fn observe_pointer(&mut self, window: &Window) {
        self.last_pointer = Some(window.mouse_position());
    }

    // ------------------------------------------------------------------
    // Measurement
    // ------------------------------------------------------------------

    pub(in crate::chat) fn record_measured(
        &mut self,
        row_id: RowId,
        height: Pixels,
        settled: bool,
    ) {
        let key = self.current_key();
        self.projection.record_height(row_id, height, key, settled);
    }

    /// Replay state for a row's own scrollable viewport, if it has one.
    pub(in crate::chat) fn nested_scroll_replay(
        &mut self,
        row_id: RowId,
    ) -> Option<NestedScrollReplay<'_>> {
        let ix = self.projection.row_index(row_id)?;
        self.slots.get_mut(ix)?.renderer.nested_scroll_replay()
    }

    // ------------------------------------------------------------------
    // Follow & jump (R7)
    // ------------------------------------------------------------------

    pub(in crate::chat) fn follow_stream(&mut self) {
        if self.list_state.is_following_tail() {
            // Rows slide under a stationary pointer; freeze hover first (R4).
            self.park_at_last_pointer();
            self.list_state.scroll_to_end();
        }
    }

    fn jump_visible_now(&self) -> bool {
        if self.list_state.is_following_tail() {
            return false;
        }
        if self.list_state.max_offset_for_scrollbar().y <= px(0.) {
            return false;
        }
        self.list_state.is_scrolled_to_end() == Some(false)
    }

    /// Called from the list's scroll handler. The list state is borrowed
    /// while this fires, so nothing here may touch it: defer all work to the
    /// window synchronization, which also refreshes the jump affordance.
    pub(in crate::chat) fn note_scrolled(&mut self, cx: &mut Context<ChatView>) {
        self.schedule_sync(cx);
    }

    pub(in crate::chat) fn jump_to_latest(
        &mut self,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) {
        // The jump slides rows underneath a stationary pointer (R4).
        self.park_pointer(window);
        self.jump_visible = false;
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        cx.notify();
    }

    pub(in crate::chat) fn show_jump_button(&self) -> bool {
        self.jump_visible
    }

    /// Refresh the jump button from the current scroll state. Call only from
    /// update phase (the scroll handler must not borrow the list state).
    pub(in crate::chat) fn refresh_jump_visibility(&mut self, cx: &mut Context<ChatView>) {
        let visible = self.jump_visible_now();
        if visible != self.jump_visible {
            self.jump_visible = visible;
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // Prepend (R7 / AC3)
    // ------------------------------------------------------------------

    /// Whether the reader is close enough to the top to load an earlier
    /// page, and the transcript reports one exists.
    pub(in crate::chat) fn wants_prepend(&self, snapshot: &TranscriptSnapshot) -> bool {
        if !snapshot.has_earlier() {
            return false;
        }
        self.list_state.logical_scroll_top().item_ix < PREPEND_TRIGGER_ROWS
    }

    /// Capture the reading anchor and freeze hover before rows slide down.
    pub(in crate::chat) fn capture_prepend_anchor(&mut self) {
        self.park_at_last_pointer();
        let offset = self.list_state.logical_scroll_top();
        if let Some(row) = self.projection.row(offset.item_ix) {
            self.prepend_anchor = Some((row.id(), offset.offset_in_item));
        }
    }

    /// After rows were prepended, restore the anchor onto the same content
    /// (AC3). The anchor row is re-located by identity; the offset within it
    /// is dropped, since the prepend may have shifted which measured height
    /// the list had settled on.
    pub(in crate::chat) fn restore_prepend_anchor(&mut self) {
        if let Some((row_id, _)) = self.prepend_anchor.take()
            && let Some(ix) = self.projection.row_index(row_id)
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
        }
    }

    // ------------------------------------------------------------------
    // Cold / warm (R8)
    // ------------------------------------------------------------------

    /// Release every renderer and hand the projection plus the scroll anchor
    /// back to the host conversation.
    pub(in crate::chat) fn cool_down(
        &mut self,
        cx: &mut Context<ChatView>,
    ) -> (RowProjection, Option<ListOffset>) {
        for slot in &mut self.slots {
            if slot.renderer.is_materialized() {
                slot.renderer.release(cx);
            }
        }
        let projection = std::mem::take(&mut self.projection);
        let anchor = if self.list_state.item_count() > 0 {
            Some(self.list_state.logical_scroll_top())
        } else {
            None
        };
        self.slots.clear();
        self.slot_ids.clear();
        (projection, anchor)
    }

    // ------------------------------------------------------------------
    // Typography (AC6)
    // ------------------------------------------------------------------

    /// Compare the live typography with the cached snapshot. Render-phase
    /// observation only: a change is declared into `pending_typography`
    /// (fresh snapshot, typography revision advanced) and applied by
    /// [`Self::apply_pending_typography`] in the next update-phase window
    /// sync — the render path never touches the projection or the list.
    pub(in crate::chat) fn observe_typography_frame(
        &mut self,
        window: &Window,
        cx: &mut Context<ChatView>,
    ) {
        let line_height = window.line_height();
        let font_size = window.rem_size();
        let theme_revision = crate::chat::projection::current_theme_revision();
        if self.typography.line_height == line_height
            && self.typography.font_size == font_size
            && self.typography.theme_revision == theme_revision
        {
            return;
        }
        self.pending_typography = Some(TypographySnapshot {
            line_height,
            font_size,
            typography_revision: self.typography.typography_revision.saturating_add(1),
            theme_revision,
        });
        self.schedule_sync(cx);
    }

    /// Apply a typography change declared by the render path. Update phase
    /// only: refresh the snapshot, invalidate measurements taken under the
    /// old typography, and re-measure the rows whose estimates moved.
    /// Returns whether anything changed so the caller can fold it into its
    /// notify.
    pub(in crate::chat) fn apply_pending_typography(&mut self) -> bool {
        let Some(next) = self.pending_typography.take() else {
            return false;
        };
        self.typography = next;
        let changed = self.projection.invalidate_typography(&next);
        self.remeasure_rows(&changed);
        true
    }

    pub(in crate::chat) fn set_generating(&mut self, generating: bool) {
        if self.is_generating != generating {
            self.is_generating = generating;
            self.refresh_waiting_turn();
        }
    }

    #[cfg(test)]
    pub(in crate::chat) fn hovered_turn(&self) -> Option<TurnId> {
        self.hovered_turn
    }
}

struct RowContent {
    part: Option<Part>,
    paired_result: Option<crate::llm::ToolResult>,
    error: Option<crate::llm::GatewayError>,
}

fn shimmer_row(row: &crate::chat::projection::Row, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .w_full()
        .child(
            gpui_component::shimmer::ShimmerText::new(t!("chat.generating").to_string())
                .id(("assistant-waiting", row.id().turn.as_u64()))
                .text_color(theme.muted_foreground),
        )
        .into_any_element()
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Typography revisions are observed here because only render sees
        // the live window; the change itself is applied in update phase
        // (AC6: font-size and theme switches invalidate and remeasure).
        self.view.observe_typography_frame(window, cx);
        self.view.observe_pointer(window);

        // Re-resolve the placeholder so a language switch reaches the
        // already-built input; guarded to avoid a notify cycle.
        let placeholder: gpui::SharedString = if self.references_enabled {
            t!("reference_picker.composer_placeholder").to_string()
        } else {
            t!("chat.placeholder").to_string()
        }
        .into();
        if self.placeholder != placeholder {
            self.placeholder = placeholder.clone();
            self.composer.update(cx, |composer, cx| {
                composer.set_placeholder(placeholder, window, cx)
            });
        }

        let has_rows = !self.view.projection.is_empty();
        let send_disabled = self.runtime_snapshot.is_generating()
            || self.runtime_snapshot.persistence_pending()
            || self.runtime_snapshot.deletion_pending()
            || self.runtime_snapshot.shutdown_requested()
            || self.input_blank
            || !self.selection_available;
        let composer_height = self.composer_height;
        let base_composer_height = self.base_composer_height;
        self.composer_status
            .set(crate::ui::reference_picker::ComposerStatus {
                pending: self.runtime_snapshot.is_generating(),
                send_disabled,
            });
        let view = cx.weak_entity();

        div()
            .relative()
            .size_full()
            .child(if has_rows {
                self.render_message_list(composer_height, window, cx)
                    .into_any_element()
            } else {
                render_empty_state(base_composer_height, cx).into_any_element()
            })
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(self.composer.clone())
                    .on_prepaint(move |bounds, _, cx| {
                        view.update(cx, |this, cx| {
                            if this.record_composer_height(bounds.size.height) {
                                cx.notify();
                            }
                        })
                        .ok();
                    }),
            )
    }
}

impl ChatView {
    /// Full-height message viewport with a floating composer stacked on top
    /// (gpui-component absolute overlay pattern).
    pub(super) fn render_message_list(
        &mut self,
        composer_height: Pixels,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let wheel_scroll_anchor = self.view.list_state.logical_scroll_top();
        let render_item = cx.processor(move |this, index: usize, window, cx| {
            this.view.render_row(index, window, cx)
        });
        let show_jump = self.view.show_jump_button();
        // Record the conversation viewport height for viewport-relative row
        // budgets. The composer height is recorded the same way; a change
        // repaints the list once.
        let view = cx.weak_entity();

        div()
            .relative()
            .size_full()
            .when(show_jump, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(composer_height + px(12.))
                        .flex()
                        .justify_center()
                        .child(
                            div().debug_selector(|| "jump-to-latest".into()).child(
                                Button::new("jump-to-latest")
                                    .outline()
                                    .small()
                                    .icon(IconName::ArrowDown)
                                    .label(t!("chat.jump_to_latest").to_string())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.view.jump_to_latest(window, cx);
                                    })),
                            ),
                        ),
                )
            })
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(
                        div()
                            .id("messages")
                            .size_full()
                            .on_prepaint(move |bounds, _, cx| {
                                view.update(cx, |this, cx| {
                                    if this.view.viewport_height != bounds.size.height {
                                        this.view.viewport_height = bounds.size.height;
                                        cx.notify();
                                    }
                                })
                                .ok();
                            })
                            .on_scroll_wheel(cx.listener(
                                move |this, event: &gpui::ScrollWheelEvent, window, cx| {
                                    this.handle_message_scroll_wheel(
                                        event,
                                        wheel_scroll_anchor,
                                        window,
                                        cx,
                                    );
                                },
                            ))
                            .child(
                                list(self.view.list_state.clone(), render_item)
                                    .size_full()
                                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                                    // Match the scrollbar thumb's 4px top inset so the
                                    // first row aligns with the top of the thumb.
                                    .pt(px(4.))
                                    // Leave exactly enough room for the measured composer.
                                    .pb(composer_height),
                            ),
                    )
                    .vertical_scrollbar(&self.view.list_state),
            )
    }

    pub(super) fn handle_message_scroll_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        native_anchor: ListOffset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Inactive macOS windows may still receive wheel hit-tests, but their
        // frame delivery is throttled. Keep those events on GPUI's native
        // path instead of queueing an animation that cannot advance smoothly.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
            || event.delta.precise()
        {
            self.view.smooth_scroll.cancel_motion();
            return;
        }

        let distance = -event.delta.pixel_delta(window.line_height()).y;
        if distance == Pixels::ZERO {
            return;
        }

        // GPUI's list handles the wheel event first during bubbling. Restore
        // the pre-event anchor before starting the eased movement so the
        // native jump never reaches a painted frame.
        self.view.list_state.scroll_to(native_anchor);
        self.view.smooth_scroll.enqueue(distance);
        self.schedule_smooth_scroll_frame(window, cx);
        cx.stop_propagation();
    }

    pub(super) fn schedule_smooth_scroll_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view.smooth_scroll.frame_scheduled {
            return;
        }
        self.view.smooth_scroll.frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.advance_smooth_scroll(window, cx);
        });
    }

    pub(super) fn advance_smooth_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view.smooth_scroll.frame_scheduled = false;
        // A window can lose activation after the wheel event but before its
        // scheduled frame. Drop the queued motion rather than invalidating a
        // throttled, inactive window on every frame.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
        {
            self.view.smooth_scroll.cancel_motion();
            return;
        }

        let Some(step) = self.view.smooth_scroll.next_step() else {
            return;
        };
        let before = self.view.list_state.logical_scroll_top();
        self.view.list_state.scroll_by(step);
        let after = self.view.list_state.logical_scroll_top();
        if before.item_ix == after.item_ix && before.offset_in_item == after.offset_in_item {
            self.view.smooth_scroll.cancel_motion();
        }

        cx.notify();
        if self.view.smooth_scroll.remaining != Pixels::ZERO {
            self.schedule_smooth_scroll_frame(window, cx);
        }
    }

    /// Reasoning-viewport easing: one frame step per scheduled frame, per
    /// row. The renderer owns the queued distance and the follow flag; the
    /// view owns the easing constants and the window-activation contract.
    pub(super) fn schedule_reasoning_scroll_frame(
        &mut self,
        row_id: RowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(nested) = self.view.nested_scroll_replay(row_id) else {
            return;
        };
        if nested.smooth.frame_scheduled {
            return;
        }
        nested.smooth.frame_scheduled = true;
        drop(nested);
        cx.on_next_frame(window, move |this, window, cx| {
            this.advance_reasoning_scroll(row_id, window, cx);
        });
    }

    pub(super) fn advance_reasoning_scroll(
        &mut self,
        row_id: RowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nested viewport motion shares the inactive-window frame throttling
        // with the transcript: cancel queued card easing when focus moved to
        // a different window instead of invalidating a throttled window.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
        {
            if let Some(nested) = self.view.nested_scroll_replay(row_id) {
                nested.smooth.frame_scheduled = false;
                nested.smooth.cancel_motion();
            }
            return;
        }
        let Some(nested) = self.view.nested_scroll_replay(row_id) else {
            return;
        };
        nested.smooth.frame_scheduled = false;
        let Some(step) = nested.smooth.next_step() else {
            return;
        };
        let offset = nested.scroll.scroll_px_offset_for_scrollbar();
        let max_offset = nested.scroll.max_offset_for_scrollbar();
        let next_y = (offset.y + step).clamp(-max_offset.y, gpui::Pixels::ZERO);
        nested
            .scroll
            .set_offset_from_scrollbar(gpui::point(offset.x, next_y));
        if next_y == offset.y {
            nested.smooth.cancel_motion();
        }
        let has_remaining = nested.smooth.remaining != Pixels::ZERO;
        drop(nested);
        #[cfg(test)]
        scrolling::record_reasoning_smooth_invalidation();
        cx.notify();
        if has_remaining {
            self.schedule_reasoning_scroll_frame(row_id, window, cx);
        }
    }

    // ------------------------------------------------------------------
    // Row action router
    // ------------------------------------------------------------------

    pub(in crate::chat) fn handle_row_action(
        &mut self,
        action: RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            RowAction::HoverTurn { turn, entered } => {
                self.view.handle_hover_turn(turn, entered, window, cx)
            }
            RowAction::Measured {
                row_id,
                height,
                settled,
            } => self.view.record_measured(row_id, height, settled),
            RowAction::ToggleDisclosure { row_id, target } => {
                self.toggle_row_disclosure(row_id, target, window, cx)
            }
            RowAction::ReplayNestedScroll {
                row_id,
                anchor,
                dy,
                precise,
            } => self.replay_nested_scroll(row_id, anchor, dy, precise, window, cx),
        }
    }

    fn toggle_row_disclosure(
        &mut self,
        row_id: RowId,
        target: DisclosureTarget,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An expanded group routes by its group id alone: its member rows
        // replaced the header row, so the id has no index of its own.
        if target == DisclosureTarget::Group {
            let transcript = self.transcript.clone();
            let diff = {
                let guard = transcript.read(cx);
                self.view
                    .projection
                    .toggle_group(row_id, guard, &self.view.typography)
            };
            self.view.reproject_slots();
            self.view.apply_diff(&diff);
            self.view.schedule_sync(cx);
            cx.notify();
            return;
        }
        let Some(ix) = self.view.projection.row_index(row_id) else {
            return;
        };
        if let Some(slot) = self.view.slots.get_mut(ix) {
            slot.renderer.toggle_disclosure(target, cx);
            let disclosure = slot.renderer.disclosure();
            self.view.projection.set_disclosure(row_id, disclosure);
        }
        cx.notify();
    }

    fn replay_nested_scroll(
        &mut self,
        row_id: RowId,
        anchor: gpui::Point<Pixels>,
        dy: Pixels,
        precise: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nested viewport motion cancels any queued transcript easing first.
        self.view.smooth_scroll.cancel_motion();
        let smooth =
            smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
                && !precise;
        let Some(nested) = self.view.nested_scroll_replay(row_id) else {
            return;
        };
        // Follow semantics: an upward gesture hands the viewport to the
        // reader; a downward gesture at the end re-arms tail following.
        if dy > Pixels::ZERO {
            *nested.follow = false;
            nested.scroll.set_follow_mode(FollowMode::Normal);
        } else if dy < Pixels::ZERO && nested.near_bottom() {
            *nested.follow = true;
            nested.scroll.set_follow_mode(FollowMode::Tail);
        }
        if !smooth {
            // One-shot native path: precise (touchpad) input, the disabled
            // preference, or an inactive window must still move the viewport,
            // so apply the delta directly instead of queueing eased frames.
            // The containment handler owns the gesture at this pin — the
            // inner list's own wheel listener does not fire past it.
            nested.smooth.cancel_motion();
            if dy == Pixels::ZERO {
                return;
            }
            let offset = nested.scroll.scroll_px_offset_for_scrollbar();
            let max_offset = nested.scroll.max_offset_for_scrollbar();
            let next_y = (offset.y + dy).clamp(-max_offset.y, Pixels::ZERO);
            nested
                .scroll
                .set_offset_from_scrollbar(gpui::point(offset.x, next_y));
            return;
        }
        if dy == Pixels::ZERO {
            return;
        }
        // Restore the painted-frame anchor before easing so the native jump
        // the list already applied never reaches a painted frame.
        nested.scroll.set_offset_from_scrollbar(anchor);
        nested.smooth.enqueue(dy);
        drop(nested);
        self.schedule_reasoning_scroll_frame(row_id, window, cx);
    }

    // ------------------------------------------------------------------
    // Window synchronization entry points
    // ------------------------------------------------------------------

    /// Run one window synchronization now. Update phase only.
    pub(in crate::chat) fn sync_window_now(&mut self, cx: &mut Context<Self>) {
        let transcript = self.transcript.clone();
        let user_message_markdown = self.preference_snapshot.user_message_markdown;
        // Field-disjoint borrows: the presentation is read while the view is
        // updated.
        let view = &mut self.view;
        let presentation = &self.markdown_presentation;
        view.sync_window(&transcript, presentation, user_message_markdown, cx);
    }
}

/// Greeting shown before the first turn. Takes the composer's *resting*
/// height (see `ChatView::base_composer_height`) so the block stays anchored
/// while a multi-line draft grows the composer over it.
fn render_empty_state(base_composer_height: Pixels, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .pb(base_composer_height)
        .gap_2()
        .child(
            div()
                .text_2xl()
                .font_semibold()
                .text_color(theme.foreground)
                .child(t!("chat.empty_title").to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("chat.empty_hint").to_string()),
        )
}
