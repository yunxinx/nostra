use super::*;

use crate::session::{ResolvedSessionState, SessionId};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ChatRestoreError {
    #[error("chat session controller lock is poisoned")]
    ControllerLockPoisoned,
    #[error("chat view is busy with a pending turn, persistence, or deletion")]
    Busy,
    #[error("chat session storage has not been initialized")]
    StorageUnavailable,
    #[error(transparent)]
    Controller(#[from] ChatSessionControllerError),
}

impl ChatView {
    /// Hydrate this view from a previously persisted [`ResolvedSessionState`].
    ///
    /// The view must be idle: no pending provider generation, durable begin,
    /// terminal persistence, deletion, or shutdown.  The controller's
    /// [`ChatSessionController::restore`] is invoked first so the durable
    /// lifecycle shares one entry point with normal turns, then the canonical
    /// messages are converted into retained [`Message`] entities with fresh
    /// Markdown bodies.
    ///
    /// The next turn id is derived from the largest `turn-N` already present in
    /// the resolved state so a subsequent send never reuses a durable turn id.
    #[allow(dead_code)]
    pub(crate) fn restore_from_session(
        &mut self,
        session_id: &SessionId,
        state: &ResolvedSessionState,
        cx: &mut Context<Self>,
    ) -> Result<(), ChatRestoreError> {
        if self.pending
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

        let messages = state
            .messages
            .iter()
            .map(|resolved| Message::from_canonical(resolved.message.clone(), cx))
            .collect::<Vec<_>>();
        let previous_len = self.messages.len();
        self.messages = messages;
        let new_len = self.messages.len();
        if previous_len != new_len {
            self.list_state.splice(previous_len..previous_len, new_len);
        } else {
            cx.notify();
        }

        self.conversation_id = session_id.to_string();
        self.next_turn_id = next_turn_id_for(state).saturating_add(1);

        if let Some(model) = restored_model {
            self.selection = Some(model);
            self.selection_available =
                providers::selection_is_available(self.selection.as_ref(), cx);
            self.provider_catalog_revision = providers::catalog_revision();
        }

        if let Some(title) = derive_title_from_state(state) {
            cx.emit(ChatEvent::TitleChanged(title));
        }
        cx.emit(ChatEvent::SessionBound(session_id.clone()));
        if let Some(model) = self.selection.clone() {
            cx.emit(ChatEvent::SelectionChanged(model));
        }
        cx.notify();
        Ok(())
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
fn turn_id_index(turn_id: &str) -> Option<u64> {
    turn_id
        .strip_prefix("turn-")
        .and_then(|rest| rest.parse::<u64>().ok())
}

#[allow(dead_code)]
fn derive_title_from_state(state: &ResolvedSessionState) -> Option<SharedString> {
    state
        .messages
        .iter()
        .find(|message| message.message.role == crate::llm::Role::User)
        .and_then(|message| {
            message
                .message
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                        Some(derive_title(text))
                    }
                    _ => None,
                })
        })
}
