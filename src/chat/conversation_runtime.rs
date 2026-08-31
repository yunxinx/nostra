//! Runtime ownership for one durable conversation.

use std::sync::{Arc, Mutex, MutexGuard};

use futures::channel::oneshot;
use gpui::{Context, EventEmitter, Task};

use crate::{
    llm::{GenerationService, Message as LlmMessage, ModelSelection},
    runtime::ConversationScopeHandle,
    session::{
        ChatSessionController, ChatTurnTerminal, ConversationContext, SessionId,
        SharedChatReferenceStore, SharedSessionStore,
    },
};

use super::assistant::ReplyTask;
use super::persistence::TurnPersistenceCoordinator;
use crate::llm::{
    GatewayError, GenerationOutcome, IndexedMessage, ReasoningContent, ReplayMetadata, ToolCall,
};

pub(super) type ChatSessionControllerHandle = Arc<Mutex<ChatSessionController<SharedSessionStore>>>;

#[derive(Clone, Default)]
pub(super) struct ConversationQuiescence {
    state: Arc<Mutex<ConversationQuiescenceState>>,
}

#[derive(Default)]
struct ConversationQuiescenceState {
    active_work: usize,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

pub(super) struct ConversationWorkLease {
    quiescence: ConversationQuiescence,
}

impl ConversationQuiescence {
    pub(super) fn begin_work(&self) -> ConversationWorkLease {
        let mut state = self.lock_state();
        state.active_work = state.active_work.saturating_add(1);
        ConversationWorkLease {
            quiescence: self.clone(),
        }
    }

    pub(super) async fn wait_until_idle(&self) {
        loop {
            let receiver = {
                let mut state = self.lock_state();
                if state.active_work == 0 {
                    return;
                }
                let (sender, receiver) = oneshot::channel();
                state.idle_waiters.push(sender);
                receiver
            };
            let _ = receiver.await;
        }
    }

    fn release_work(&self) {
        let waiters = {
            let mut state = self.lock_state();
            state.active_work = state.active_work.saturating_sub(1);
            (state.active_work == 0).then(|| std::mem::take(&mut state.idle_waiters))
        };
        if let Some(waiters) = waiters {
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ConversationQuiescenceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for ConversationWorkLease {
    fn drop(&mut self) {
        self.quiescence.release_work();
    }
}

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

#[derive(Clone)]
pub(super) struct FinishedConversationTurn {
    pub generation: ConversationRequestGeneration,
    pub message: Option<IndexedMessage>,
    pub error: Option<GatewayError>,
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
    StreamBatch {
        generation: ConversationRequestGeneration,
        events: Vec<ConversationStreamEvent>,
    },
    GenerationRequestFailed {
        generation: ConversationRequestGeneration,
        error: GatewayError,
    },
    GenerationFinished(Box<FinishedConversationTurn>),
    Failure(ConversationRuntimeFailure),
    DeleteCompleted,
}

#[derive(Clone)]
pub(super) enum ConversationStreamEvent {
    TextStarted {
        content_index: usize,
        id: String,
    },
    TextDelta {
        content_index: usize,
        id: String,
        delta: String,
    },
    TextFinished {
        content_index: usize,
        id: String,
        replay: Option<ReplayMetadata>,
    },
    ReasoningStarted {
        content_index: usize,
        id: String,
    },
    ReasoningDelta {
        content_index: usize,
        id: String,
        delta: String,
    },
    ReasoningFinished {
        content_index: usize,
        id: String,
        replay: Option<ReplayMetadata>,
    },
    ReasoningSnapshotUpdated {
        content_index: usize,
        id: String,
        reasoning: ReasoningContent,
    },
    ToolCallStarted {
        content_index: usize,
        index: usize,
        id: String,
        name: String,
    },
    ToolCallFinished {
        content_index: usize,
        index: usize,
        tool_call: Box<ToolCall>,
    },
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
    pub(super) delete_completion_requested: bool,
    pub(super) scope_close_requested: bool,
    pub(super) shutdown_requested: bool,
    pub(super) pending_turn_id: Option<String>,
    pub(super) terminal_persistence: Option<TurnPersistenceCoordinator>,
    pub(super) pending_terminal: Option<(String, ChatTurnTerminal)>,
    pub(super) session_id: Option<SessionId>,
    pub(super) next_turn_id: u64,
    pub(super) request_generation: ConversationRequestGeneration,
    pub(super) generating: bool,
    pub(super) reply_task: Option<ReplyTask>,
    pub(super) quiescence: ConversationQuiescence,
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
            delete_completion_requested: false,
            scope_close_requested: false,
            shutdown_requested: false,
            pending_turn_id: None,
            terminal_persistence: None,
            pending_terminal: None,
            session_id: None,
            next_turn_id: 1,
            request_generation: ConversationRequestGeneration::none(),
            generating: false,
            reply_task: None,
            quiescence: ConversationQuiescence::default(),
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

    pub(super) fn close_scope(&mut self, cx: &mut Context<Self>) {
        self.prepare_for_shutdown(cx);
        self.close_scope_after_quiescence(cx);
    }

    pub(super) fn close_scope_after_quiescence(&mut self, cx: &mut Context<Self>) {
        if self.scope_close_requested {
            return;
        }
        self.scope_close_requested = true;
        let scope = self.scope.clone();
        let quiescence = self.quiescence.clone();
        cx.spawn(async move |this, cx| {
            let result = close_scope_when_quiescent(quiescence, scope).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.scope_close_requested = false;
                    if this.delete_completion_requested {
                        this.delete_completion_requested = false;
                        this.deletion_pending = false;
                        this.publish_event(ConversationRuntimeEvent::DeleteCompleted, cx);
                    }
                    this.publish_state(cx);
                }
                Err(error) => {
                    this.scope_close_requested = false;
                    if this.delete_completion_requested {
                        this.delete_completion_requested = false;
                        this.deletion_pending = false;
                        this.deletion_requested = false;
                        this.publish_event(
                            ConversationRuntimeEvent::Failure(ConversationRuntimeFailure::Delete),
                            cx,
                        );
                    }
                    crate::logging::error(
                        "runtime.scope",
                        format_args!("conversation scope close failed: {error}"),
                    );
                    this.publish_state(cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn start_generation(
        &mut self,
        history: Vec<LlmMessage>,
        selection: ModelSelection,
        session_id: String,
        turn_id: String,
        generation: ConversationRequestGeneration,
        cx: &mut Context<Self>,
    ) {
        if generation != self.request_generation || !self.generating {
            return;
        }
        match super::assistant::stream_reply(
            super::assistant::ReplyRequest {
                history,
                selection,
                generation_service: Arc::clone(&self.generation_service),
                conversation_id: session_id,
                turn_id,
                request_generation: generation,
            },
            cx,
        ) {
            Ok(reply_task) => self.reply_task = Some(reply_task),
            Err(error) => self.finish_generation_request_failed(generation, error, cx),
        }
    }

    pub(super) fn request_stop(&mut self) {
        if let Some(reply_task) = &self.reply_task {
            reply_task.cancel();
        }
    }

    pub(super) fn cancel_and_release_generation(&mut self) {
        if let Some(reply_task) = self.reply_task.take() {
            reply_task.cancel();
        }
    }

    pub(super) fn finish_generation_request_failed(
        &mut self,
        generation: ConversationRequestGeneration,
        error: GatewayError,
        cx: &mut Context<Self>,
    ) {
        if generation != self.request_generation {
            return;
        }
        let terminal = ChatTurnTerminal::request_failed(&error);
        self.publish_event(
            ConversationRuntimeEvent::GenerationRequestFailed { generation, error },
            cx,
        );
        self.reply_task = None;
        self.finish_terminal(generation, terminal, cx);
    }

    pub(super) fn finish_generation(
        &mut self,
        generation: ConversationRequestGeneration,
        outcome: GenerationOutcome,
        cx: &mut Context<Self>,
    ) {
        if generation != self.request_generation {
            return;
        }
        let terminal = ChatTurnTerminal::from_generation(&outcome);
        let error = terminal_failure(
            outcome.status,
            outcome.error.clone(),
            outcome.request_id.clone(),
        );
        self.publish_event(
            ConversationRuntimeEvent::GenerationFinished(Box::new(FinishedConversationTurn {
                generation,
                message: outcome.message.clone(),
                error: error.clone(),
            })),
            cx,
        );
        self.reply_task = None;
        self.finish_terminal(generation, terminal, cx);
    }
}

async fn close_scope_when_quiescent(
    quiescence: ConversationQuiescence,
    scope: ConversationScopeHandle,
) -> Result<(), crate::runtime::ScopeError> {
    quiescence.wait_until_idle().await;
    scope.close().await
}

fn terminal_failure(
    status: crate::llm::OutcomeStatus,
    error: Option<GatewayError>,
    request_id: String,
) -> Option<GatewayError> {
    (status == crate::llm::OutcomeStatus::Failed).then(|| {
        let mut error =
            error.unwrap_or_else(|| GatewayError::provider("provider request failed", None));
        error.request_id.get_or_insert(request_id);
        error
    })
}

impl EventEmitter<ConversationRuntimeEvent> for ConversationRuntime {}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::{
        ConversationQuiescence, ConversationRequestGeneration, close_scope_when_quiescent,
    };

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

    #[gpui::test]
    fn scope_close_waits_for_active_durable_work(cx: &mut TestAppContext) {
        let scope = crate::runtime::ConversationScopeHandle::for_test();
        let quiescence = ConversationQuiescence::default();
        let work = quiescence.begin_work();
        let close_scope = scope.clone();

        cx.foreground_executor()
            .spawn(async move {
                close_scope_when_quiescent(quiescence, close_scope)
                    .await
                    .expect("test scope close succeeds");
            })
            .detach();
        cx.run_until_parked();

        assert!(scope.is_open(), "active durable work keeps the scope open");

        drop(work);
        cx.run_until_parked();

        assert!(!scope.is_open(), "scope closes after durable work settles");
    }
}
