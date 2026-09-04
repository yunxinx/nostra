use gpui::Context;

use crate::{
    chat::conversation_runtime::ConversationRuntime,
    chat::transcript::{ResolvedStateSource, TranscriptSource as _},
    llm::ModelSelection,
    session::{ChatSessionControllerError, ResolvedSessionState, SessionId},
};

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
    /// transcript with the resolved tail. The view is not on this path: it
    /// observes [`super::super::transcript::TranscriptEvent::Reset`].
    pub(crate) fn restore_session(
        &mut self,
        session_id: &SessionId,
        state: &ResolvedSessionState,
        cx: &mut Context<Self>,
    ) -> Result<Option<ModelSelection>, ChatRestoreError> {
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
        let restored_model = {
            let mut guard = controller
                .lock()
                .map_err(|_| ChatRestoreError::ControllerLockPoisoned)?;
            guard.restore(session_id)?;
            guard.current_model().cloned()
        };
        self.advance_generation();
        self.session_id = Some(session_id.clone());
        self.next_turn_id = next_turn_id_for(state).saturating_add(1);
        let page = ResolvedStateSource::new(state.clone()).load_tail(usize::MAX);
        self.transcript.update(cx, |transcript, cx| {
            transcript.load(page, None, cx);
        });
        self.publish_state(cx);
        Ok(restored_model)
    }
}

fn next_turn_id_for(state: &ResolvedSessionState) -> u64 {
    let from_messages = state
        .messages
        .iter()
        .filter_map(|message| message.turn_id.as_deref())
        .filter_map(turn_id_index);
    let from_results = state
        .turn_results
        .iter()
        .filter_map(|result| result.result.turn_id.as_deref())
        .filter_map(turn_id_index);
    from_messages.chain(from_results).max().unwrap_or(0)
}

fn turn_id_index(turn_id: &str) -> Option<u64> {
    turn_id
        .strip_prefix("turn-")
        .and_then(|rest| rest.parse::<u64>().ok())
}
