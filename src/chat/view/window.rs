//! Materialization-window computation and synchronization.
//!
//! Three zones around the viewport, expressed in *screens* of estimated row
//! heights:
//!
//! - **viewport** — rows the list paints;
//! - **materialize** — viewport ± 1 screen: renderers are created here;
//! - **retain** — viewport ± 3 screens: renderers survive here; anything
//!   beyond is released, keeping heights, disclosure, and takeover flags in
//!   the projection.
//!
//! The gap between the two boundaries is the hysteresis that keeps scrolling
//! back and forth from thrashing entity creation. Rows of the streaming turn
//! are always materialized. `bounds_for_item` returns `None` for items the
//! list has not rendered, so pixel ranges are estimated from cumulative
//! effective heights (estimate, or cached measurement when fresh).

use gpui::{Context, Entity, ListOffset, Pixels, px};

use super::TranscriptView;
use crate::chat::projection::{MeasurementKey, Row, TypographySnapshot};
use crate::chat::transcript::Transcript;
use crate::ui::markdown::MarkdownPresentation;

/// How many viewports above/below the viewport rows are materialized.
pub(super) const MATERIALIZE_SCREENS: f32 = 1.;
/// How many viewports above/below the viewport rows are retained.
pub(super) const RETAIN_SCREENS: f32 = 3.;

impl TranscriptView {
    /// Reconcile materialized renderers with the scroll window.
    ///
    /// Runs only in update phase (deferred handlers, `on_next_frame`, and
    /// scroll synchronization) — never inside a render closure.
    pub(super) fn sync_window(
        &mut self,
        transcript: &Entity<Transcript>,
        presentation: &MarkdownPresentation,
        user_message_markdown: bool,
        cx: &mut Context<crate::chat::ChatView>,
    ) {
        self.sync_scheduled = false;
        // Typography changes observed on the render path are applied here,
        // in update phase (AC6): invalidation and remeasure never run in
        // render.
        let typography_changed = self.apply_pending_typography();
        let viewport = self.list_state.viewport_bounds().size;
        self.content_width = viewport.width;

        let key = self.current_key();
        let laid_out = viewport.height > px(0.);
        let (materialize, retain) = if laid_out {
            let scroll_top = self.list_state.logical_scroll_top();
            compute_zones(self.projection.rows(), scroll_top, viewport.height, &key)
        } else {
            // Nothing laid out yet: zone math needs a viewport. Keep every
            // renderer (no release) and let the pending set drive synthesis.
            (0..0, 0..self.slots.len())
        };
        // Release pass first so a scrolling viewport frees before it
        // allocates. Rows of the streaming turn are exempt (R6).
        let streaming_turn = self.streaming_turn;
        if laid_out {
            for ix in 0..self.slots.len() {
                let in_retain = retain.contains(&ix);
                if in_retain {
                    continue;
                }
                if !self.slots[ix].renderer.is_materialized() {
                    continue;
                }
                if self
                    .projection
                    .row(ix)
                    .is_some_and(|row| Some(row.id().turn) == streaming_turn)
                {
                    continue;
                }
                let row_id = self.projection.rows()[ix].id();
                let disclosure = self.slots[ix].renderer.disclosure();
                self.projection.set_disclosure(row_id, disclosure);
                self.slots[ix].renderer.release(cx);
            }
        }

        // Materialize pass: window rows, anything the render path requested,
        // and every row of the streaming turn.
        let mut wanted: Vec<usize> = materialize.clone().collect();
        wanted.extend(
            self.pending_materialize
                .drain(..)
                .filter_map(|row_id| self.projection.row_index(row_id)),
        );
        if let Some(turn) = streaming_turn {
            wanted.extend(self.projection.rows_in_turn(turn));
        }
        wanted.sort_unstable();
        wanted.dedup();
        for ix in wanted.iter().copied() {
            if ix >= self.slots.len() || self.slots[ix].renderer.is_materialized() {
                continue;
            }
            // Late materialization: no live Append will replay the part, so
            // renderers re-read the authoritative accumulated content.
            self.materialize_row(
                ix,
                transcript,
                presentation,
                user_message_markdown,
                false,
                cx,
            );
            self.window_dirty = true;
        }

        // The jump affordance is a pure function of the scroll state; it is
        // refreshed here because the scroll handler cannot touch the list
        // state while the list is laying out.
        self.refresh_jump_visibility(cx);

        if self.window_dirty || typography_changed {
            self.window_dirty = false;
            cx.notify();
        }
    }

    /// Measurement key measurements and lookups are currently taken under.
    pub(super) fn current_key(&self) -> MeasurementKey {
        self.typography
            .measurement_key(TypographySnapshot::width_bucket(self.content_width))
    }
}

/// Materialize and retain row-index ranges for the current scroll position.
pub(super) fn compute_zones(
    rows: &[Row],
    scroll_top: ListOffset,
    viewport_height: Pixels,
    key: &MeasurementKey,
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let count = rows.len();
    if count == 0 {
        return (0..0, 0..0);
    }

    // Cumulative heights: cum[i] is the top edge of row i; cum[count] the
    // bottom edge of the last row.
    let mut cum = Vec::with_capacity(count + 1);
    cum.push(px(0.));
    for row in rows {
        let last = cum[cum.len() - 1];
        cum.push(last + row.effective_height(key));
    }

    let item_ix = scroll_top.item_ix.min(count);
    let scroll_px = cum[item_ix] + scroll_top.offset_in_item;

    let zone = |screens_above: f32, screens_below: f32| -> std::ops::Range<usize> {
        let top = scroll_px - viewport_height * screens_above;
        let bottom = scroll_px + viewport_height * screens_below;
        // First row whose bottom edge is below `top`.
        let start = (0..count).find(|&ix| cum[ix + 1] > top).unwrap_or(count);
        // First row whose top edge is at or beyond `bottom`.
        let end = (start..count)
            .find(|&ix| cum[ix] >= bottom)
            .unwrap_or(count);
        start..end
    };

    let materialize = zone(MATERIALIZE_SCREENS, MATERIALIZE_SCREENS + 1.);
    let retain = zone(RETAIN_SCREENS, RETAIN_SCREENS + 1.);
    // The retain zone always spans at least the materialize zone.
    let retain = retain.start.min(materialize.start)..retain.end.max(materialize.end);
    (materialize, retain)
}
