use std::sync::Arc;

use gpui::Context;

use crate::{
    chat::{
        conversation_runtime::ConversationRuntime,
        persistence::JsonlOffsetSource,
        transcript::{TranscriptCursor, TranscriptPage},
    },
    llm::ModelSelection,
    session::{ChatSessionControllerError, ResolvedSessionState, SessionId},
};

/// How many turns the paged open path loads for the first screen, and how
/// many each backward scroll page loads (design §3.6).
pub(crate) const TAIL_PAGE_TURNS: usize = 50;
pub(crate) const PREPEND_PAGE_TURNS: usize = 50;

/// Everything the runtime needs to adopt a previously persisted session in
/// one shot: the page of turns to display, its backward cursor, the optional
/// cursor source that keeps serving earlier pages, the model the composer
/// should seed from, and the next `turn-<n>` seed.
///
/// A `None` source with a full page is the R6 fallback shape: the whole
/// transcript is already in `page` and no earlier content exists to load.
pub(crate) struct ChatSessionRestore {
    pub session_id: SessionId,
    pub page: TranscriptPage,
    pub cursor: Option<TranscriptCursor>,
    /// `None` when the fallback path loaded the full transcript.
    pub source: Option<Arc<JsonlOffsetSource>>,
    pub model: Option<ModelSelection>,
    pub next_turn_seed: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatRestoreError {
    #[error("chat session controller lock is poisoned")]
    ControllerLockPoisoned,
    #[error("conversation runtime is busy with a pending turn, persistence, or deletion")]
    Busy,
    #[error("chat session storage has not been initialized")]
    StorageUnavailable,
    #[error(transparent)]
    Controller(#[from] ChatSessionControllerError),
}

impl ConversationRuntime {
    /// Bind this runtime to a previously persisted session and replace the
    /// transcript with the provided tail page. The view is not on this path:
    /// it observes [`super::super::transcript::TranscriptEvent::Reset`].
    ///
    /// The runtime retains the paged source so the view can keep prepending
    /// earlier turns after warm/cold transitions (the source snapshot's
    /// prefix is immutable, so it stays valid while the conversation exists).
    pub(crate) fn restore_session(
        &mut self,
        restore: &ChatSessionRestore,
        cx: &mut Context<Self>,
    ) -> Result<(), ChatRestoreError> {
        if self.generating
            || self.persistence_pending
            || self.deletion_requested
            || self.deletion_pending
            || self.shutdown_requested
            || self.pending_turn_id.is_some()
            || self.terminal_persistence.is_some()
            || self.pending_terminal.is_some()
        {
            return Err(ChatRestoreError::Busy);
        }
        let controller = self
            .session_controller
            .clone()
            .ok_or(ChatRestoreError::StorageUnavailable)?;
        {
            let mut guard = controller
                .lock()
                .map_err(|_| ChatRestoreError::ControllerLockPoisoned)?;
            // The binding validates domain, existence, and the absence of a
            // pending turn; the transcript bodies arrive through `page`.
            guard.restore_binding(&restore.session_id, restore.model.clone())?;
        }
        self.advance_generation();
        self.session_id = Some(restore.session_id.clone());
        self.next_turn_id = restore.next_turn_seed.max(1);
        self.transcript_source = restore.source.clone();
        // `Transcript::load` adopts the caller cursor or, failing that, the
        // page's own `cursor_before`, so an exhausted page clears `has_earlier`
        // while a paged tail keeps the earlier-page anchor.
        let page_cursor = restore
            .cursor
            .clone()
            .or(restore.page.cursor_before.clone());
        let page = clone_page(&restore.page);
        self.transcript.update(cx, |transcript, cx| {
            transcript.load(page, page_cursor, cx);
        });
        self.publish_state(cx);
        Ok(())
    }
}

/// Pages are consumed by adoption: clone one so the caller's restore bundle
/// stays reusable (tests and retry paths re-read it).
fn clone_page(page: &TranscriptPage) -> TranscriptPage {
    TranscriptPage {
        turns: page.turns.clone(),
        cursor_before: page.cursor_before.clone(),
    }
}

/// Seed the runtime's `turn-<n>` counter from a resolved tail page: every
/// durable turn's user message entry is on the active path, and turn ids are
/// monotonic, so the path maximum necessarily lands in the tail page
/// (design §5.3).
pub(crate) fn next_turn_seed_from_messages(messages: &[crate::session::ResolvedMessage]) -> u64 {
    messages
        .iter()
        .filter_map(|message| message.turn_id.as_deref())
        .filter_map(turn_id_index)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

pub(crate) fn next_turn_seed_from_state(state: &ResolvedSessionState) -> u64 {
    let from_messages = next_turn_seed_from_messages(&state.messages);
    let from_results = state
        .turn_results
        .iter()
        .filter_map(|result| result.result.turn_id.as_deref())
        .filter_map(turn_id_index)
        .max()
        .unwrap_or(0);
    from_messages.max(from_results).saturating_add(1)
}

/// Resolve the turn seed from a paged open's entry-path snapshot: turn ids
/// are monotonic, so the path's last message entry names the maximum. One
/// targeted fact read; `None` means the index could not serve the read and
/// the caller should treat the index as unusable.
pub(crate) fn next_turn_seed_from_path(
    store: &Arc<dyn crate::session::SessionTreeStore + Send + Sync>,
    session_id: &SessionId,
    path: &[crate::session::PathEntryRecord],
) -> Option<u64> {
    let last_message = path
        .iter()
        .rev()
        .find(|record| record.kind == crate::session::EntryKindTag::Message)?;
    let entries = store
        .read_entries(session_id, std::slice::from_ref(&last_message.entry_id))
        .ok()?;
    let entry = entries.first()?;
    match &entry.kind {
        crate::session::SessionEntryKind::Message(message) => message
            .turn_id
            .as_deref()
            .and_then(turn_id_index)
            .map(|index| index.saturating_add(1)),
        _ => None,
    }
}

fn turn_id_index(turn_id: &str) -> Option<u64> {
    turn_id
        .strip_prefix("turn-")
        .and_then(|rest| rest.parse::<u64>().ok())
}
