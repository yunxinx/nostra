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

    pub fn begin_turn(
        &mut self,
        user_message: Message,
        model: ModelSelection,
        turn_id: impl Into<String>,
    ) -> Result<ChatTurnStart, ChatSessionControllerError> {
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
            let header = self
                .pending_turn
                .as_ref()
                .and_then(|pending| pending.session_header.clone())
                .ok_or(ChatSessionControllerError::MissingSessionState)?;
            let session_id = self.store.create_session(header)?;
            self.mark_pending_session_created(session_id)?;
        }
        self.append_pending_user()
    }

    pub fn finish_turn(
        &mut self,
        turn_id: &str,
        terminal: &ChatTurnTerminal,
    ) -> Result<(), ChatSessionControllerError> {
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
                    let header = pending
                        .session_header
                        .ok_or(ChatSessionControllerError::MissingSessionState)?;
                    let session_id = self.store.create_session(header)?;
                    self.mark_pending_session_created(session_id)?;
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
mod tests {
    use std::{fs, time::Duration};

    use super::*;
    use crate::llm::{FinishReason, IndexedMessage, Protocol, UsageProvenance};
    use crate::session::{
        InMemorySessionStore, LocalSessionStore, LocalStoreConfig, SessionBranchPreview,
        SessionBranchTreeSnapshot, SessionTreeSnapshot,
    };
    use crate::session::{SessionFlushStore, SessionLifecycleStore, SessionTreeStore};

    fn model(id: &str) -> ModelSelection {
        ModelSelection {
            profile_id: "profile".into(),
            model_id: id.into(),
        }
    }

    fn text_message(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.into(),
                provider_metadata: Default::default(),
            }],
            provider_metadata: Default::default(),
        }
    }

    fn usage(total_tokens: u64) -> Usage {
        Usage {
            provenance: UsageProvenance::Reported,
            input_tokens: total_tokens.saturating_sub(2),
            output_tokens: 2,
            total_tokens,
            ..Usage::default()
        }
    }

    fn generation(
        status: OutcomeStatus,
        message: Option<Message>,
        usage: Usage,
        error: Option<GatewayError>,
    ) -> GenerationOutcome {
        GenerationOutcome {
            request_id: "request-1".into(),
            profile_id: "profile".into(),
            model_id: "model-a".into(),
            protocol: Protocol::ChatCompletions,
            status,
            finish_reason: (status == OutcomeStatus::Completed).then_some(FinishReason::Stop),
            usage,
            response_id: Some("response-1".into()),
            upstream_model: None,
            time_to_first_event: Some(Duration::from_millis(1)),
            latency: Duration::from_millis(2),
            message: message.map(IndexedMessage::from_message),
            error,
        }
    }

    fn exercise_completed<S: SessionStore>(store: S) {
        let mut controller = ChatSessionController::new(store);
        assert!(controller.session_id().is_none());
        let start = controller
            .begin_turn(
                text_message(Role::User, "hello"),
                model("model-a"),
                "turn-1",
            )
            .expect("first message should create the session");
        let assistant = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Reasoning {
                    reasoning: crate::llm::ReasoningContent {
                        display: "thinking".into(),
                        replay: None,
                    },
                },
                ContentBlock::Text {
                    text: "world".into(),
                    provider_metadata: Default::default(),
                },
            ],
            provider_metadata: Default::default(),
        };
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Completed,
            Some(assistant.clone()),
            usage(12),
            None,
        ));
        controller
            .finish_turn("turn-1", &terminal)
            .expect("completed turn should persist");
        assert!(controller.pending_turn_id().is_none());
        let state = controller
            .restore(&start.session_id)
            .expect("completed session should restore");
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].entry_id, start.user_entry_id);
        assert_eq!(state.messages[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(state.messages[0].model.as_ref(), Some(&model("model-a")));
        assert_eq!(state.messages[1].message, assistant);
        assert_eq!(state.messages[1].usage, usage(12));
        assert_eq!(state.turn_results.len(), 1);
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Completed);
        assert_eq!(state.turn_results[0].result.usage, usage(12));
        assert_eq!(
            state.latest_config.as_ref().map(|config| &config.model),
            Some(&model("model-a"))
        );
    }

    #[test]
    fn completed_contract_works_with_memory_store() {
        exercise_completed(InMemorySessionStore::new());
    }

    #[test]
    fn completed_contract_works_with_local_store() {
        let root = tempfile::tempdir().expect("tempdir");
        let store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("store should open");
        exercise_completed(store);
    }

    #[test]
    fn empty_draft_does_not_create_a_local_session() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut controller = ChatSessionController::new(
            LocalSessionStore::open(config.clone()).expect("store should open"),
        );
        let error = controller
            .begin_turn(
                text_message(Role::User, " \n\t"),
                model("model-a"),
                "turn-1",
            )
            .expect_err("blank message must stay an ephemeral draft");
        assert!(matches!(
            error,
            ChatSessionControllerError::EmptyUserMessage
        ));
        assert_eq!(controller.session_id(), None);
        let files = fs::read_dir(config.sessions_root())
            .expect("sessions root should exist")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert!(files.is_empty(), "blank draft wrote: {files:?}");
    }

    #[test]
    fn failed_generation_drops_partial_assistant_and_redacts_upstream_body() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut controller = ChatSessionController::new(
            LocalSessionStore::open(config.clone()).expect("store should open"),
        );
        let start = controller
            .begin_turn(
                text_message(Role::User, "persist me"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist before generation");
        let secret = "provider-secret-body";
        let error = GatewayError::provider("provider failed", Some("safe-code".into()))
            .with_upstream_body(secret);
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Failed,
            Some(text_message(Role::Assistant, "partial")),
            usage(9),
            Some(error),
        ));
        assert_eq!(terminal.status(), TurnStatus::Failed);
        controller
            .finish_turn("turn-1", &terminal)
            .expect("failed terminal should persist user and result");
        controller.flush().expect("flush should complete");
        let state = controller
            .restore(&start.session_id)
            .expect("failed session should restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(
            state.messages[0].message,
            text_message(Role::User, "persist me")
        );
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Failed);
        let session_file = fs::read_dir(config.sessions_root())
            .expect("sessions root")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .expect("session source");
        let persisted = fs::read_to_string(session_file).expect("read source");
        assert!(!persisted.contains(secret));
        assert!(persisted.contains("provider failed"));
        assert!(persisted.contains("safe-code"));
    }

    #[test]
    fn cancelled_generation_drops_partial_assistant() {
        let mut controller = ChatSessionController::new(InMemorySessionStore::new());
        let start = controller
            .begin_turn(
                text_message(Role::User, "cancel me"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Cancelled,
            Some(text_message(Role::Assistant, "partial")),
            usage(4),
            None,
        ));
        controller
            .finish_turn("turn-1", &terminal)
            .expect("cancelled terminal should persist");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Cancelled);
    }

    #[test]
    fn request_failure_and_invalid_terminal_states_are_explicit() {
        let mut controller = ChatSessionController::new(InMemorySessionStore::new());
        let start = controller
            .begin_turn(
                text_message(Role::User, "prepare"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        let error = GatewayError::configuration("missing provider");
        let terminal = ChatTurnTerminal::request_failed(&error);
        assert!(matches!(
            controller.finish_turn("other", &terminal),
            Err(ChatSessionControllerError::TurnIdMismatch { .. })
        ));
        assert!(matches!(
            controller.begin_turn(
                text_message(Role::User, "again"),
                model("model-a"),
                "turn-2"
            ),
            Err(ChatSessionControllerError::TurnInProgress { .. })
        ));
        controller
            .finish_turn("turn-1", &terminal)
            .expect("request failure should persist");
        assert!(matches!(
            controller.finish_turn("turn-1", &terminal),
            Err(ChatSessionControllerError::NoTurnInProgress)
        ));
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Failed);
    }

    fn exercise_terminal_without_assistant<S: SessionStore>(store: S, terminal: ChatTurnTerminal) {
        let expected_status = terminal.status;
        let mut controller = ChatSessionController::new(store);
        let start = controller
            .begin_turn(
                text_message(Role::User, "terminal without assistant"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        controller
            .finish_turn("turn-1", &terminal)
            .expect("terminal should persist");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].entry_id, start.user_entry_id);
        assert_eq!(state.turn_results.len(), 1);
        assert_eq!(state.turn_results[0].result.status, expected_status);
    }

    #[test]
    fn failed_generation_contract_works_with_memory_store() {
        exercise_terminal_without_assistant(
            InMemorySessionStore::new(),
            ChatTurnTerminal::request_failed(&GatewayError::configuration("missing provider")),
        );
    }

    #[test]
    fn failed_generation_contract_works_with_local_store() {
        let root = tempfile::tempdir().expect("tempdir");
        let store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("store should open");
        exercise_terminal_without_assistant(
            store,
            ChatTurnTerminal::request_failed(&GatewayError::configuration("missing provider")),
        );
    }

    #[test]
    fn cancelled_generation_contract_works_with_memory_store() {
        exercise_terminal_without_assistant(
            InMemorySessionStore::new(),
            ChatTurnTerminal::cancelled(),
        );
    }

    #[test]
    fn cancelled_generation_contract_works_with_local_store() {
        let root = tempfile::tempdir().expect("tempdir");
        let store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("store should open");
        exercise_terminal_without_assistant(store, ChatTurnTerminal::cancelled());
    }

    #[test]
    fn completed_without_assistant_snapshot_keeps_only_terminal_result() {
        let mut controller = ChatSessionController::new(InMemorySessionStore::new());
        let start = controller
            .begin_turn(
                text_message(Role::User, "no snapshot"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Completed,
            None,
            usage(7),
            None,
        ));
        controller
            .finish_turn("turn-1", &terminal)
            .expect("terminal should persist");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.turn_results.len(), 1);
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Completed);
    }

    #[test]
    fn a_completed_turn_id_cannot_be_reused() {
        let mut controller = ChatSessionController::new(InMemorySessionStore::new());
        controller
            .begin_turn(
                text_message(Role::User, "first"),
                model("model-a"),
                "turn-1",
            )
            .expect("first turn");
        controller
            .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
            .expect("first terminal");
        assert!(matches!(
            controller.begin_turn(
                text_message(Role::User, "reused"),
                model("model-a"),
                "turn-1",
            ),
            Err(ChatSessionControllerError::TurnIdAlreadyUsed { turn_id })
                if turn_id == "turn-1"
        ));
    }

    #[test]
    fn local_store_restarts_and_read_only_restore_keeps_timestamp() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let (session_id, before) = {
            let mut controller = ChatSessionController::new(
                LocalSessionStore::open(config.clone()).expect("store should open"),
            );
            let start = controller
                .begin_turn(
                    text_message(Role::User, "restart"),
                    model("model-a"),
                    "turn-1",
                )
                .expect("user should persist");
            let terminal = ChatTurnTerminal::from_generation(&generation(
                OutcomeStatus::Completed,
                Some(text_message(Role::Assistant, "restored")),
                usage(8),
                None,
            ));
            controller
                .finish_turn("turn-1", &terminal)
                .expect("turn should persist");
            controller.flush().expect("flush");
            let before = controller
                .store
                .get_summary(&start.session_id)
                .expect("summary")
                .expect("summary row");
            controller.shutdown().expect("shutdown");
            (start.session_id, before)
        };

        let mut restored = ChatSessionController::new(
            LocalSessionStore::open(config.clone()).expect("reopen store"),
        );
        let state = restored.restore(&session_id).expect("restart restore");
        let after = restored
            .store
            .get_summary(&session_id)
            .expect("summary")
            .expect("summary row");
        assert_eq!(state.messages.len(), 2);
        assert_eq!(before.created_at, after.created_at);
        assert_eq!(before.updated_at, after.updated_at);
    }

    #[test]
    fn completed_usage_is_not_counted_twice_in_catalog() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut controller =
            ChatSessionController::new(LocalSessionStore::open(config).expect("store should open"));
        let start = controller
            .begin_turn(
                text_message(Role::User, "tokens"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Completed,
            Some(text_message(Role::Assistant, "done")),
            usage(11),
            None,
        ));
        controller.finish_turn("turn-1", &terminal).expect("finish");
        controller.flush().expect("flush");
        let summary = controller
            .store
            .get_summary(&start.session_id)
            .expect("summary")
            .expect("summary row");
        assert_eq!(summary.total_tokens, 11);
    }

    #[test]
    fn changing_model_is_restored_as_the_latest_config() {
        let mut controller = ChatSessionController::new(InMemorySessionStore::new());
        let first = controller
            .begin_turn(
                text_message(Role::User, "first"),
                model("model-a"),
                "turn-1",
            )
            .expect("first turn");
        controller
            .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
            .expect("first terminal");
        controller
            .begin_turn(
                text_message(Role::User, "second"),
                model("model-b"),
                "turn-2",
            )
            .expect("second turn");
        controller
            .finish_turn("turn-2", &ChatTurnTerminal::cancelled())
            .expect("second terminal");

        let state = controller.restore(&first.session_id).expect("restore");
        assert_eq!(
            state.latest_config.as_ref().map(|config| &config.model),
            Some(&model("model-b"))
        );
        assert_eq!(state.messages[1].model, Some(model("model-b")));
    }

    struct FailOnceStore {
        inner: InMemorySessionStore,
        fail_next_append: bool,
    }

    impl SessionLifecycleStore for FailOnceStore {
        fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError> {
            self.inner.create_session(header)
        }

        fn append(
            &mut self,
            session_id: &SessionId,
            entries: Vec<SessionEntryKind>,
        ) -> Result<Vec<EntryId>, SessionError> {
            if self.fail_next_append {
                self.fail_next_append = false;
                return Err(SessionError::Io {
                    source: std::io::Error::other("injected append failure"),
                });
            }
            self.inner.append(session_id, entries)
        }

        fn load_session(
            &self,
            session_id: &SessionId,
            leaf: Option<&EntryId>,
        ) -> Result<ResolvedSessionState, SessionError> {
            self.inner.load_session(session_id, leaf)
        }
    }

    impl SessionTreeStore for FailOnceStore {
        fn set_leaf(
            &mut self,
            session_id: &SessionId,
            leaf: Option<&EntryId>,
        ) -> Result<(), SessionError> {
            self.inner.set_leaf(session_id, leaf)
        }

        fn load_session_tree(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionTreeSnapshot, SessionError> {
            self.inner.load_session_tree(session_id)
        }

        fn load_session_tree_for_leaf(
            &self,
            session_id: &SessionId,
            leaf: &EntryId,
        ) -> Result<SessionTreeSnapshot, SessionError> {
            self.inner.load_session_tree_for_leaf(session_id, leaf)
        }

        fn load_branch_preview(
            &self,
            session_id: &SessionId,
            branch_root: &EntryId,
        ) -> Result<SessionBranchPreview, SessionError> {
            self.inner.load_branch_preview(session_id, branch_root)
        }

        fn load_branch_tree(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionBranchTreeSnapshot, SessionError> {
            self.inner.load_branch_tree(session_id)
        }
    }

    impl SessionFlushStore for FailOnceStore {
        fn flush(&mut self) -> Result<(), SessionError> {
            self.inner.flush()
        }

        fn shutdown(&mut self) -> Result<(), SessionError> {
            self.inner.shutdown()
        }
    }

    #[test]
    fn terminal_append_failure_keeps_the_turn_retryable() {
        let mut controller = ChatSessionController::new(FailOnceStore {
            inner: InMemorySessionStore::new(),
            fail_next_append: false,
        });
        let start = controller
            .begin_turn(
                text_message(Role::User, "retry"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Completed,
            Some(text_message(Role::Assistant, "done")),
            usage(5),
            None,
        ));
        controller.store.fail_next_append = true;
        assert!(matches!(
            controller.finish_turn("turn-1", &terminal),
            Err(ChatSessionControllerError::Storage(SessionError::Io { .. }))
        ));
        assert_eq!(controller.pending_turn_id(), Some("turn-1"));

        controller
            .finish_turn("turn-1", &terminal)
            .expect("same terminal should retry");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.turn_results.len(), 1);
    }

    struct CommitThenErrorStore {
        inner: InMemorySessionStore,
        fail_create_after_commit: bool,
        fail_append_after_commit: bool,
    }

    impl SessionLifecycleStore for CommitThenErrorStore {
        fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError> {
            let id = self.inner.create_session(header)?;
            if self.fail_create_after_commit {
                self.fail_create_after_commit = false;
                return Err(SessionError::Io {
                    source: std::io::Error::other("injected post-commit create failure"),
                });
            }
            Ok(id)
        }

        fn append(
            &mut self,
            session_id: &SessionId,
            entries: Vec<SessionEntryKind>,
        ) -> Result<Vec<EntryId>, SessionError> {
            let ids = self.inner.append(session_id, entries)?;
            if self.fail_append_after_commit {
                self.fail_append_after_commit = false;
                return Err(SessionError::Io {
                    source: std::io::Error::other("injected post-commit append failure"),
                });
            }
            Ok(ids)
        }

        fn load_session(
            &self,
            session_id: &SessionId,
            leaf: Option<&EntryId>,
        ) -> Result<ResolvedSessionState, SessionError> {
            self.inner.load_session(session_id, leaf)
        }
    }

    impl SessionTreeStore for CommitThenErrorStore {
        fn set_leaf(
            &mut self,
            session_id: &SessionId,
            leaf: Option<&EntryId>,
        ) -> Result<(), SessionError> {
            self.inner.set_leaf(session_id, leaf)
        }

        fn load_session_tree(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionTreeSnapshot, SessionError> {
            self.inner.load_session_tree(session_id)
        }

        fn load_session_tree_for_leaf(
            &self,
            session_id: &SessionId,
            leaf: &EntryId,
        ) -> Result<SessionTreeSnapshot, SessionError> {
            self.inner.load_session_tree_for_leaf(session_id, leaf)
        }

        fn load_branch_preview(
            &self,
            session_id: &SessionId,
            branch_root: &EntryId,
        ) -> Result<SessionBranchPreview, SessionError> {
            self.inner.load_branch_preview(session_id, branch_root)
        }

        fn load_branch_tree(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionBranchTreeSnapshot, SessionError> {
            self.inner.load_branch_tree(session_id)
        }
    }

    impl SessionFlushStore for CommitThenErrorStore {
        fn flush(&mut self) -> Result<(), SessionError> {
            self.inner.flush()
        }

        fn shutdown(&mut self) -> Result<(), SessionError> {
            self.inner.shutdown()
        }
    }

    #[test]
    fn retry_after_post_commit_create_error_reuses_the_created_session() {
        let user = text_message(Role::User, "create retry");
        let mut controller = ChatSessionController::new(CommitThenErrorStore {
            inner: InMemorySessionStore::new(),
            fail_create_after_commit: true,
            fail_append_after_commit: false,
        });
        assert!(matches!(
            controller.begin_turn(user.clone(), model("model-a"), "turn-1"),
            Err(ChatSessionControllerError::Storage(SessionError::Io { .. }))
        ));
        let start = controller
            .begin_turn(user, model("model-a"), "turn-1")
            .expect("retry should discover the committed header");
        controller
            .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
            .expect("terminal should persist");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.turn_results.len(), 1);
    }

    #[test]
    fn retry_after_post_commit_user_error_does_not_duplicate_the_user_fact() {
        let user = text_message(Role::User, "user retry");
        let mut controller = ChatSessionController::new(CommitThenErrorStore {
            inner: InMemorySessionStore::new(),
            fail_create_after_commit: false,
            fail_append_after_commit: true,
        });
        assert!(matches!(
            controller.begin_turn(user.clone(), model("model-a"), "turn-1"),
            Err(ChatSessionControllerError::Storage(SessionError::Io { .. }))
        ));
        let start = controller
            .begin_turn(user, model("model-a"), "turn-1")
            .expect("retry should discover the committed user fact");
        controller
            .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
            .expect("terminal should persist");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.turn_results.len(), 1);
    }

    #[test]
    fn retry_after_post_commit_terminal_error_does_not_duplicate_terminal_facts() {
        let mut controller = ChatSessionController::new(CommitThenErrorStore {
            inner: InMemorySessionStore::new(),
            fail_create_after_commit: false,
            fail_append_after_commit: false,
        });
        let start = controller
            .begin_turn(
                text_message(Role::User, "terminal retry"),
                model("model-a"),
                "turn-1",
            )
            .expect("user should persist");
        let terminal = ChatTurnTerminal::from_generation(&generation(
            OutcomeStatus::Completed,
            Some(text_message(Role::Assistant, "done")),
            usage(5),
            None,
        ));
        controller.store.fail_append_after_commit = true;
        assert!(matches!(
            controller.finish_turn("turn-1", &terminal),
            Err(ChatSessionControllerError::Storage(SessionError::Io { .. }))
        ));
        controller
            .finish_turn("turn-1", &terminal)
            .expect("retry should discover the committed terminal");
        let state = controller.restore(&start.session_id).expect("restore");
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.turn_results.len(), 1);
    }
}
