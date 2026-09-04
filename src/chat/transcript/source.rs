//! Cursor-shaped transcript pages. Phase 1 loads the full resolved tail.

use crate::session::ResolvedSessionState;

use super::model::{Turn, allocate_turn_id};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptCursor {
    pub(crate) index: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TranscriptPage {
    pub(crate) turns: Vec<Turn>,
    pub(crate) cursor_before: Option<TranscriptCursor>,
}

pub(crate) trait TranscriptSource {
    fn load_tail(&self, turns: usize) -> TranscriptPage;
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
            cursor_before: (start > 0).then_some(TranscriptCursor { index: start }),
        }
    }
}

impl TranscriptSource for ResolvedStateSource {
    fn load_tail(&self, turns: usize) -> TranscriptPage {
        let len = self.state.messages.len();
        let start = if turns == usize::MAX {
            0
        } else {
            len.saturating_sub(turns)
        };
        self.page(start, len)
    }
}
