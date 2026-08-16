use thiserror::Error;

use crate::llm::{
    ContentBlock, GatewayError, GenerationOutcome, Message, ModelSelection, OutcomeStatus, Role,
    Usage,
};

use super::{
    ConfigChange, EntryId, MessageEntry, ResolvedSessionState, SafeError, SessionDomain,
    SessionEntryKind, SessionError, SessionHeader, SessionId, SessionStore, TurnResult, TurnStatus,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatTurnStart {
    pub session_id: SessionId,
    pub user_entry_id: EntryId,
}

/// Safe, storage-ready projection of a terminal generation event.
///
/// Fields stay private so failed and cancelled outcomes can never carry a
/// partial assistant message across the persistence boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatTurnTerminal {
    status: TurnStatus,
    finish_reason: Option<crate::llm::FinishReason>,
    usage: Usage,
    assistant: Option<Message>,
    error: Option<SafeError>,
}

impl ChatTurnTerminal {
    #[must_use]
    pub fn from_generation(outcome: &GenerationOutcome) -> Self {
        let status = match outcome.status {
            OutcomeStatus::Completed => TurnStatus::Completed,
            OutcomeStatus::Failed => TurnStatus::Failed,
            OutcomeStatus::Cancelled => TurnStatus::Cancelled,
        };
        let assistant = (outcome.status == OutcomeStatus::Completed)
            .then(|| {
                outcome
                    .message
                    .clone()
                    .map(|message| message.into_message())
            })
            .flatten();
        let error = (outcome.status != OutcomeStatus::Completed)
            .then(|| outcome.error.as_ref().map(SafeError::from_gateway))
            .flatten();
        Self {
            status,
            finish_reason: outcome.finish_reason.clone(),
            usage: outcome.usage.clone(),
            assistant,
            error,
        }
    }

    #[must_use]
    pub fn request_failed(error: &GatewayError) -> Self {
        Self {
            status: TurnStatus::Failed,
            finish_reason: None,
            usage: Usage::default(),
            assistant: None,
            error: Some(SafeError::from_gateway(error)),
        }
    }

    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            status: TurnStatus::Cancelled,
            finish_reason: None,
            usage: Usage::default(),
            assistant: None,
            error: None,
        }
    }

    #[must_use]
    pub fn status(&self) -> TurnStatus {
        self.status
    }
}

#[derive(Debug, Error)]
pub enum ChatSessionControllerError {
    #[error("chat session storage failed: {0}")]
    Storage(#[from] SessionError),
    #[error("chat user message must use the user role, got `{role:?}`")]
    InvalidUserRole { role: Role },
    #[error("chat assistant message must use the assistant role, got `{role:?}`")]
    InvalidAssistantRole { role: Role },
    #[error("chat user message has no non-empty content")]
    EmptyUserMessage,
    #[error("chat turn id must not be empty")]
    EmptyTurnId,
    #[error("chat turn `{turn_id}` is already in progress")]
    TurnInProgress { turn_id: String },
    #[error("chat turn `{turn_id}` has not finished persisting its user message")]
    UserMessagePersistenceIncomplete { turn_id: String },
    #[error("chat has no turn in progress")]
    NoTurnInProgress,
    #[error("chat terminal turn id `{actual}` does not match active turn `{expected}`")]
    TurnIdMismatch { expected: String, actual: String },
    #[error("chat turn id `{turn_id}` was already used in this session")]
    TurnIdAlreadyUsed { turn_id: String },
    #[error("chat terminal retry for turn `{turn_id}` does not match the first terminal")]
    TerminalRetryMismatch { turn_id: String },
    #[error("session `{session_id}` is not a Chat session")]
    NotChatSession { session_id: SessionId },
    #[error("chat controller is missing its pending session state")]
    MissingSessionState,
    #[error("session store did not return an id for the appended user message")]
    MissingUserEntryId,
    #[error("chat session has been permanently deleted")]
    Deleted,
}

#[derive(Clone, Debug)]
struct PendingTurn {
    session_id: SessionId,
    turn_id: String,
    model: ModelSelection,
    user_message: Message,
    user_entries: Vec<SessionEntryKind>,
    session_header: Option<SessionHeader>,
    session_created: bool,
    user_entry_id: Option<EntryId>,
    terminal: Option<ChatTurnTerminal>,
}

/// UI-independent owner of one Chat session's durable lifecycle.
///
/// The synchronous capability is intentional. GPUI integration schedules the
/// controller on its background executor instead of performing storage I/O in
/// an entity update or render pass.
pub struct ChatSessionController<S> {
    store: S,
    session_id: Option<SessionId>,
    current_model: Option<ModelSelection>,
    pending_turn: Option<PendingTurn>,
    deleted: bool,
}

impl<S> ChatSessionController<S> {
    /// Run one controller operation against a temporary capability.
    ///
    /// A persistence reservation can outlive the normal store's accepting
    /// boundary while application shutdown is in progress.  The controller
    /// still owns the serialized turn state, so the reserved capability must
    /// be swapped in only for the synchronous operation and restored before
    /// the controller lock is released.  Keeping this scope here prevents a
    /// caller from accidentally leaving the shutdown-authorized store on the
    /// long-lived controller.
    pub(crate) fn with_replaced_store<R>(
        &mut self,
        replacement: S,
        operation: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let original = std::mem::replace(&mut self.store, replacement);
        let result = operation(self);
        self.store = original;
        result
    }
}

impl<S> ChatSessionController<S>
where
    S: SessionStore,
{
    #[must_use]
    pub fn new(store: S) -> Self {
        Self {
            store,
            session_id: None,
            current_model: None,
            pending_turn: None,
            deleted: false,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    #[must_use]
    pub fn pending_turn_id(&self) -> Option<&str> {
        self.pending_turn
            .as_ref()
            .map(|pending| pending.turn_id.as_str())
    }

    /// The model most recently applied to this controller's session, either by
    /// a durable turn or by [`Self::restore`].  `None` for a fresh controller
    /// that has not yet begun or restored a turn.
    #[must_use]
    pub fn current_model(&self) -> Option<&ModelSelection> {
        self.current_model.as_ref()
    }

    pub fn begin_turn(
        &mut self,
        user_message: Message,
        model: ModelSelection,
        turn_id: impl Into<String>,
    ) -> Result<ChatTurnStart, ChatSessionControllerError> {
        if self.deleted {
            return Err(ChatSessionControllerError::Deleted);
        }
        if user_message.role != Role::User {
            return Err(ChatSessionControllerError::InvalidUserRole {
                role: user_message.role,
            });
        }
        if !message_has_content(&user_message) {
            return Err(ChatSessionControllerError::EmptyUserMessage);
        }
        let turn_id = turn_id.into();
        if turn_id.trim().is_empty() {
            return Err(ChatSessionControllerError::EmptyTurnId);
        }
        if let Some(pending) = &self.pending_turn {
            if pending.user_entry_id.is_none()
                && pending.turn_id == turn_id
                && pending.model == model
                && pending.user_message == user_message
            {
                return self.resume_pending_start();
            }
            return Err(ChatSessionControllerError::TurnInProgress {
                turn_id: pending.turn_id.clone(),
            });
        }

        if let Some(session_id) = &self.session_id {
            let state = self.store.load_session(session_id, None)?;
            if state
                .messages
                .iter()
                .any(|message| message.turn_id.as_deref() == Some(turn_id.as_str()))
                || state
                    .turn_results
                    .iter()
                    .any(|result| result.result.turn_id.as_deref() == Some(turn_id.as_str()))
            {
                return Err(ChatSessionControllerError::TurnIdAlreadyUsed { turn_id });
            }
        }

        let created = self.session_id.is_none();
        let (session_id, session_header, session_created) = if created {
            let mut header = SessionHeader::new(SessionDomain::Chat, None);
            header.initial_model = Some(model.clone());
            (header.session_id.clone(), Some(header), false)
        } else {
            (
                self.session_id
                    .clone()
                    .ok_or(ChatSessionControllerError::MissingSessionState)?,
                None,
                true,
            )
        };
        let model_changed = !created && self.current_model.as_ref() != Some(&model);
        let mut entries = Vec::with_capacity(usize::from(model_changed) + 1);
        if model_changed {
            entries.push(SessionEntryKind::ConfigChange(ConfigChange {
                model: model.clone(),
                system_prompt: None,
            }));
        }
        entries.push(SessionEntryKind::Message(MessageEntry {
            message: user_message.clone(),
            turn_id: Some(turn_id.clone()),
            model: Some(model.clone()),
            usage: Usage::default(),
        }));

        self.pending_turn = Some(PendingTurn {
            session_id,
            turn_id,
            model,
            user_message,
            user_entries: entries,
            session_header,
            session_created,
            user_entry_id: None,
            terminal: None,
        });
        if !session_created {
            return self.create_pending_session_with_user();
        }
        self.append_pending_user()
    }

    pub fn finish_turn(
        &mut self,
        turn_id: &str,
        terminal: &ChatTurnTerminal,
    ) -> Result<(), ChatSessionControllerError> {
        if self.deleted {
            return Err(ChatSessionControllerError::Deleted);
        }
        let pending = self
            .pending_turn
            .as_ref()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        if pending.turn_id != turn_id {
            return Err(ChatSessionControllerError::TurnIdMismatch {
                expected: pending.turn_id.clone(),
                actual: turn_id.to_string(),
            });
        }
        if pending.user_entry_id.is_none() {
            return Err(
                ChatSessionControllerError::UserMessagePersistenceIncomplete {
                    turn_id: pending.turn_id.clone(),
                },
            );
        }
        if let Some(assistant) = &terminal.assistant
            && assistant.role != Role::Assistant
        {
            return Err(ChatSessionControllerError::InvalidAssistantRole {
                role: assistant.role,
            });
        }
        if let Some(previous) = &pending.terminal {
            if previous != terminal {
                return Err(ChatSessionControllerError::TerminalRetryMismatch {
                    turn_id: turn_id.to_string(),
                });
            }
            return self.resume_pending_terminal();
        }
        let pending = self
            .pending_turn
            .as_mut()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        pending.terminal = Some(terminal.clone());
        self.append_pending_terminal()
    }

    fn resume_pending_start(&mut self) -> Result<ChatTurnStart, ChatSessionControllerError> {
        self.store.flush()?;
        let pending = self
            .pending_turn
            .clone()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        if !pending.session_created {
            match self.store.load_session(&pending.session_id, None) {
                Ok(_) => self.mark_pending_session_created(pending.session_id.clone())?,
                Err(SessionError::SessionNotFound(_)) => {
                    return self.create_pending_session_with_user();
                }
                Err(error) => return Err(error.into()),
            }
        }

        let pending = self
            .pending_turn
            .clone()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        let state = self.store.load_session(&pending.session_id, None)?;
        if let Some(user_entry_id) = state.messages.iter().rev().find_map(|message| {
            (message.turn_id.as_deref() == Some(pending.turn_id.as_str())
                && message.message == pending.user_message)
                .then(|| message.entry_id.clone())
        }) {
            let current = self
                .pending_turn
                .as_mut()
                .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
            current.user_entry_id = Some(user_entry_id.clone());
            self.current_model = Some(current.model.clone());
            return Ok(ChatTurnStart {
                session_id: current.session_id.clone(),
                user_entry_id,
            });
        }
        self.append_pending_user()
    }

    fn mark_pending_session_created(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), ChatSessionControllerError> {
        let pending = self
            .pending_turn
            .as_mut()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        pending.session_id = session_id.clone();
        pending.session_created = true;
        self.session_id = Some(session_id);
        self.current_model = Some(pending.model.clone());
        Ok(())
    }

    fn append_pending_user(&mut self) -> Result<ChatTurnStart, ChatSessionControllerError> {
        let pending = self
            .pending_turn
            .clone()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        let entry_ids = self
            .store
            .append(&pending.session_id, pending.user_entries)?;
        self.finish_pending_user(entry_ids)
    }

    fn create_pending_session_with_user(
        &mut self,
    ) -> Result<ChatTurnStart, ChatSessionControllerError> {
        let pending = self
            .pending_turn
            .clone()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        let header = pending
            .session_header
            .ok_or(ChatSessionControllerError::MissingSessionState)?;
        let (session_id, entry_ids) = self
            .store
            .create_session_with_entries(header, pending.user_entries)?;
        self.mark_pending_session_created(session_id)?;
        self.finish_pending_user(entry_ids)
    }

    fn finish_pending_user(
        &mut self,
        entry_ids: Vec<EntryId>,
    ) -> Result<ChatTurnStart, ChatSessionControllerError> {
        let user_entry_id = entry_ids
            .last()
            .cloned()
            .ok_or(ChatSessionControllerError::MissingUserEntryId)?;
        let current = self
            .pending_turn
            .as_mut()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        current.user_entry_id = Some(user_entry_id.clone());
        self.current_model = Some(current.model.clone());
        Ok(ChatTurnStart {
            session_id: current.session_id.clone(),
            user_entry_id,
        })
    }

    fn resume_pending_terminal(&mut self) -> Result<(), ChatSessionControllerError> {
        self.store.flush()?;
        let pending = self
            .pending_turn
            .clone()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        let terminal = pending
            .terminal
            .as_ref()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        let expected = turn_result(&pending.turn_id, terminal);
        let state = self.store.load_session(&pending.session_id, None)?;
        if state
            .turn_results
            .iter()
            .any(|resolved| resolved.result == expected)
        {
            self.pending_turn = None;
            return Ok(());
        }
        self.append_pending_terminal()
    }

    fn append_pending_terminal(&mut self) -> Result<(), ChatSessionControllerError> {
        let pending = self
            .pending_turn
            .clone()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        let terminal = pending
            .terminal
            .as_ref()
            .ok_or(ChatSessionControllerError::NoTurnInProgress)?;
        self.store.append(
            &pending.session_id,
            terminal_entries(&pending.turn_id, &pending.model, terminal),
        )?;
        self.pending_turn = None;
        Ok(())
    }

    pub fn restore(
        &mut self,
        session_id: &SessionId,
    ) -> Result<ResolvedSessionState, ChatSessionControllerError> {
        if self.deleted {
            return Err(ChatSessionControllerError::Deleted);
        }
        if let Some(pending) = &self.pending_turn {
            return Err(ChatSessionControllerError::TurnInProgress {
                turn_id: pending.turn_id.clone(),
            });
        }
        if session_id.domain() != SessionDomain::Chat {
            return Err(ChatSessionControllerError::NotChatSession {
                session_id: session_id.clone(),
            });
        }
        let state = self.store.load_session(session_id, None)?;
        self.current_model = state
            .latest_config
            .as_ref()
            .map(|config| config.model.clone())
            .or_else(|| {
                state
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| message.model.clone())
            });
        self.session_id = Some(session_id.clone());
        Ok(state)
    }

    /// Permanently delete this controller's durable session, if one exists.
    ///
    /// A failed first-turn create can publish JSONL before returning an error,
    /// so the pending turn's preallocated session id is also a deletion
    /// candidate. The controller is marked deleted only after storage confirms
    /// the idempotent removal, allowing a failed delete to be retried exactly.
    pub fn delete_session(&mut self) -> Result<(), ChatSessionControllerError> {
        if self.deleted {
            return Ok(());
        }
        let session_id = self.session_id.clone().or_else(|| {
            self.pending_turn
                .as_ref()
                .map(|pending| pending.session_id.clone())
        });
        if let Some(session_id) = session_id {
            self.store.delete_session(&session_id)?;
        }
        self.pending_turn = None;
        self.session_id = None;
        self.current_model = None;
        self.deleted = true;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), ChatSessionControllerError> {
        self.store.flush().map_err(Into::into)
    }

    pub fn shutdown(&mut self) -> Result<(), ChatSessionControllerError> {
        self.store.shutdown().map_err(Into::into)
    }
}

fn message_has_content(message: &Message) -> bool {
    message.content.iter().any(|block| match block {
        ContentBlock::Text { text, .. } => !text.trim().is_empty(),
        ContentBlock::Reasoning { reasoning } => {
            !reasoning.display.trim().is_empty() || reasoning.replay.is_some()
        }
        ContentBlock::ToolCall { .. } => true,
        ContentBlock::ToolResult { tool_result } => !tool_result.content.trim().is_empty(),
    })
}

fn terminal_entries(
    turn_id: &str,
    model: &ModelSelection,
    terminal: &ChatTurnTerminal,
) -> Vec<SessionEntryKind> {
    let mut entries = Vec::with_capacity(usize::from(terminal.assistant.is_some()) + 1);
    if let Some(assistant) = &terminal.assistant {
        entries.push(SessionEntryKind::Message(MessageEntry {
            message: assistant.clone(),
            turn_id: Some(turn_id.to_string()),
            model: Some(model.clone()),
            usage: terminal.usage.clone(),
        }));
    }
    entries.push(SessionEntryKind::TurnResult(turn_result(turn_id, terminal)));
    entries
}

fn turn_result(turn_id: &str, terminal: &ChatTurnTerminal) -> TurnResult {
    TurnResult {
        turn_id: Some(turn_id.to_string()),
        status: terminal.status,
        finish_reason: terminal.finish_reason.clone(),
        error: terminal.error.clone(),
        usage: terminal.usage.clone(),
    }
}

#[cfg(test)]
mod tests;
