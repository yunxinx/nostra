//! Cursor-shaped transcript pages. Phase 1 loads the full resolved tail; the
//! P5 storage cursor adds entry-anchored backward paging over a source.

use crate::session::{EntryId, ResolvedSessionState};

use super::model::{Turn, allocate_turn_id};

/// Anchor for backward paging: the entry id of the earliest message already
/// held by the model. `None` in a page means the path start was reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptCursor {
    pub(crate) entry_id: EntryId,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TranscriptPage {
    pub(crate) turns: Vec<Turn>,
    pub(crate) cursor_before: Option<TranscriptCursor>,
}

pub(crate) trait TranscriptSource {
    /// Total number of turns the source can eventually serve, when known.
    fn total_hint(&self) -> Option<usize>;
    fn load_tail(&self, turns: usize) -> TranscriptPage;
    fn load_before(&self, cursor: &TranscriptCursor, turns: usize) -> TranscriptPage;
}

pub(crate) struct ResolvedStateSource {
    state: ResolvedSessionState,
}

impl ResolvedStateSource {
    #[must_use]
    pub(crate) fn new(state: ResolvedSessionState) -> Self {
        Self { state }
    }

    fn page(&self, start: usize, end: usize) -> TranscriptPage {
        let messages = &self.state.messages;
        let end = end.min(messages.len());
        let start = start.min(end);
        // Placeholder ids: `Transcript::load` / `prepend` adopt pages through
        // the model counters, so page-local ids never reach the model.
        let mut next_turn_id = 1;
        let mut next_part_id = 1;
        let turns = messages[start..end]
            .iter()
            .map(|resolved| {
                let turn_id = allocate_turn_id(&mut next_turn_id);
                Turn::from_llm(resolved.message.clone(), turn_id, &mut next_part_id)
            })
            .collect();
        TranscriptPage {
            turns,
            cursor_before: (start > 0).then(|| TranscriptCursor {
                entry_id: messages[start].entry_id.clone(),
            }),
        }
    }
}

impl TranscriptSource for ResolvedStateSource {
    fn total_hint(&self) -> Option<usize> {
        Some(self.state.messages.len())
    }

    fn load_tail(&self, turns: usize) -> TranscriptPage {
        let len = self.state.messages.len();
        let start = if turns == usize::MAX {
            0
        } else {
            len.saturating_sub(turns)
        };
        self.page(start, len)
    }

    fn load_before(&self, cursor: &TranscriptCursor, turns: usize) -> TranscriptPage {
        let messages = &self.state.messages;
        let Some(position) = messages
            .iter()
            .position(|message| message.entry_id == cursor.entry_id)
        else {
            // The cursor no longer names a message of this snapshot (for
            // example after a path rewind). Return an empty page and keep the
            // cursor so the loader stops instead of skipping content.
            return TranscriptPage {
                turns: Vec::new(),
                cursor_before: Some(cursor.clone()),
            };
        };
        let start = if turns == usize::MAX {
            0
        } else {
            position.saturating_sub(turns)
        };
        self.page(start, position)
    }
}
