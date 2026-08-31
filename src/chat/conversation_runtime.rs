//! Runtime ownership for one durable conversation.

use std::sync::{Arc, Mutex};

use gpui::{Context, EventEmitter, Task};

use crate::{
    llm::{GenerationService, Message as LlmMessage, ModelSelection},
    runtime::ConversationScopeHandle,
    session::{
        ChatSessionController, ChatTurnTerminal, ConversationContext, SessionId,
        SharedChatReferenceStore, SharedSessionStore,
    },
};

use super::persistence::TurnPersistenceCoordinator;

pub(super) type ChatSessionControllerHandle = Arc<Mutex<ChatSessionController<SharedSessionStore>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ConversationRequestGeneration(u64);

impl ConversationRequestGeneration {
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    pub(super) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum BeginTurnAdmissionError {
    #[error("conversation runtime is not accepting another turn")]
    NotAccepting,
    #[error("conversation session storage is unavailable: {0}")]
    StorageUnavailable(String),
    #[error("conversation session operation could not be reserved: {0}")]
    OperationReservation(String),
    #[error("conversation request generations are exhausted")]
    GenerationExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConversationRuntimeSnapshot {
    scope: crate::runtime::ScopeId,
    session_id: Option<SessionId>,
    request_generation: ConversationRequestGeneration,
    generating: bool,
    persistence_pending: bool,
    deletion_requested: bool,
    deletion_pending: bool,
    shutdown_requested: bool,
    pending_turn: bool,
    terminal_retry_pending: bool,
}

impl ConversationRuntimeSnapshot {
    #[cfg(test)]
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    #[must_use]
    pub const fn request_generation(&self) -> ConversationRequestGeneration {
        self.request_generation
    }

    #[must_use]
    pub const fn is_generating(&self) -> bool {
        self.generating
    }

    #[cfg(test)]
    #[must_use]
    pub const fn has_pending_turn(&self) -> bool {
        self.pending_turn
    }

    #[cfg(test)]
    #[must_use]
    pub const fn has_terminal_retry_pending(&self) -> bool {
        self.terminal_retry_pending
    }

    #[must_use]
    pub const fn persistence_pending(&self) -> bool {
        self.persistence_pending
    }

    #[must_use]
    pub const fn deletion_pending(&self) -> bool {
        self.deletion_pending
    }

    #[must_use]
    pub const fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    #[must_use]
    pub const fn has_in_flight_work(&self) -> bool {
        self.generating
            || self.persistence_pending
            || self.deletion_requested
            || self.deletion_pending
            || self.pending_turn
            || self.terminal_retry_pending
    }
}

#[derive(Clone)]
pub(super) struct PendingBeginRequest {
    pub text: String,
    pub user_message: LlmMessage,
    pub selection: ModelSelection,
    pub turn_id: String,
    pub composer_revision: u64,
    pub request_generation: ConversationRequestGeneration,
}

#[derive(Clone)]
pub(super) struct StartedConversationTurn {
    pub request: PendingBeginRequest,
    pub session_id: SessionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConversationRuntimeFailure {
    Begin,
    TerminalRetry,
    Terminal,
    Delete,
}

#[derive(Clone)]
pub(super) enum ConversationRuntimeEvent {
    StateChanged(ConversationRuntimeSnapshot),
    TurnStarted(Box<StartedConversationTurn>),
    Failure(ConversationRuntimeFailure),
    DeleteCompleted,
}

pub(crate) struct ConversationRuntime {
    pub(super) scope: ConversationScopeHandle,
    pub(super) conversation: ConversationContext,
    pub(super) generation_service: Arc<dyn GenerationService>,
    pub(super) session_controller: Option<ChatSessionControllerHandle>,
    pub(super) session_store: Option<SharedSessionStore>,
    pub(super) session_unavailable: Option<String>,
    pub(super) persistence_pending: bool,
    pub(super) _persistence_task: Option<Task<()>>,
    pub(super) deletion_requested: bool,
    pub(super) deletion_pending: bool,
    pub(super) _deletion_task: Option<Task<()>>,
    pub(super) _scope_close_task: Option<Task<()>>,
    pub(super) shutdown_requested: bool,
    pub(super) pending_turn_id: Option<String>,
    pub(super) terminal_persistence: Option<TurnPersistenceCoordinator>,
    pub(super) pending_terminal: Option<(String, ChatTurnTerminal)>,
    pub(super) session_id: Option<SessionId>,
    pub(super) next_turn_id: u64,
    pub(super) request_generation: ConversationRequestGeneration,
    pub(super) generating: bool,
}

impl ConversationRuntime {
    pub(crate) fn new(
        scope: ConversationScopeHandle,
        conversation: ConversationContext,
        generation_service: Arc<dyn GenerationService>,
    ) -> Self {
        let (session_controller, session_store, session_unavailable) =
            match conversation.lifecycle() {
                Ok(store) => {
                    let controller = ChatSessionController::with_descriptor(
                        store.clone(),
                        conversation.descriptor().clone(),
                    );
                    (Some(Arc::new(Mutex::new(controller))), Some(store), None)
                }
                Err(error) => (None, None, Some(error.to_string())),
            };
        Self {
            scope,
            conversation,
            generation_service,
            session_controller,
            session_store,
            session_unavailable,
            persistence_pending: false,
            _persistence_task: None,
            deletion_requested: false,
            deletion_pending: false,
            _deletion_task: None,
            _scope_close_task: None,
            shutdown_requested: false,
            pending_turn_id: None,
            terminal_persistence: None,
            pending_terminal: None,
            session_id: None,
            next_turn_id: 1,
            request_generation: ConversationRequestGeneration::none(),
            generating: false,
        }
    }

    #[must_use]
    pub(super) fn snapshot(&self) -> ConversationRuntimeSnapshot {
        ConversationRuntimeSnapshot {
            scope: self.scope.scope(),
            session_id: self.session_id.clone(),
            request_generation: self.request_generation,
            generating: self.generating,
            persistence_pending: self.persistence_pending,
            deletion_requested: self.deletion_requested,
            deletion_pending: self.deletion_pending,
            shutdown_requested: self.shutdown_requested,
            pending_turn: self.pending_turn_id.is_some() || self.terminal_persistence.is_some(),
            terminal_retry_pending: self.pending_terminal.is_some(),
        }
    }

    #[must_use]
    pub fn references(&self) -> Option<SharedChatReferenceStore> {
        self.conversation.references()
    }

    #[must_use]
    pub(super) fn supports_references(&self) -> bool {
        self.conversation.descriptor().supports_references()
    }

    #[must_use]
    pub fn generation_service(&self) -> Arc<dyn GenerationService> {
        Arc::clone(&self.generation_service)
    }

    #[cfg(test)]
    #[must_use]
    pub(super) const fn current_generation(&self) -> ConversationRequestGeneration {
        self.request_generation
    }

    #[cfg(test)]
    pub(super) fn session_controller_for_test(&self) -> ChatSessionControllerHandle {
        self.session_controller
            .clone()
            .expect("test conversation should have session storage")
    }

    #[cfg(test)]
    pub(super) fn mark_turn_pending_for_test(
        &mut self,
        session_id: SessionId,
        turn_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.session_id = Some(session_id);
        self.pending_turn_id = Some(turn_id.into());
        self.generating = true;
        self.publish_state(cx);
    }

    #[cfg(test)]
    pub(super) fn mark_generating_for_test(&mut self, cx: &mut Context<Self>) {
        self.generating = true;
        self.publish_state(cx);
    }

    pub(super) fn advance_generation(&mut self) -> bool {
        let Some(next) = self.request_generation.next() else {
            return false;
        };
        self.request_generation = next;
        true
    }

    pub(super) fn publish_state(&self, cx: &mut Context<Self>) {
        self.publish_event(ConversationRuntimeEvent::StateChanged(self.snapshot()), cx);
    }

    pub(super) fn publish_event(&self, event: ConversationRuntimeEvent, cx: &mut Context<Self>) {
        let runtime = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = runtime.update(cx, |_, cx| cx.emit(event));
        });
    }

    pub(super) fn close_scope_detached(&self, cx: &mut Context<Self>) {
        let scope = self.scope.clone();
        cx.spawn(async move |_this, _cx| {
            if let Err(error) = scope.close().await {
                crate::logging::error(
                    "runtime.scope",
                    format_args!("conversation scope close failed: {error}"),
                );
            }
        })
        .detach();
    }
}

impl EventEmitter<ConversationRuntimeEvent> for ConversationRuntime {}

#[cfg(test)]
mod tests {
    use super::ConversationRequestGeneration;

    #[test]
    fn request_generations_advance_monotonically() {
        let first = ConversationRequestGeneration::none();
        let second = first.next().expect("second request generation");
        let third = second.next().expect("third request generation");

        assert!(first < second);
        assert!(second < third);
        assert!(third.next().is_some());
    }

    #[test]
    fn request_generation_exhaustion_does_not_wrap() {
        let maximum = ConversationRequestGeneration(u64::MAX);

        assert!(maximum.next().is_none());
    }
}
