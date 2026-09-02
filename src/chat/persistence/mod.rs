mod lifecycle;
pub(crate) mod restore;

use super::conversation_runtime::{
    ChatSessionControllerHandle, ConversationQuiescence, ConversationRuntime, PendingBeginRequest,
};

use futures::channel::oneshot;
use gpui::{AppContext as _, Context};

use crate::session::{
    ChatSessionControllerError, ChatTurnStart, ChatTurnTerminal, SessionOperationGuard,
};

#[derive(Debug, thiserror::Error)]
enum BeginPersistenceError {
    #[error("Chat session controller lock is poisoned")]
    ControllerLockPoisoned,
    #[error("failed to retry Chat turn `{turn_id}`: {source}")]
    TerminalRetry {
        turn_id: String,
        #[source]
        source: ChatSessionControllerError,
    },
    #[error(transparent)]
    Begin(#[from] ChatSessionControllerError),
    #[error("Chat turn persistence worker disconnected")]
    WorkerDisconnected,
}

impl BeginPersistenceError {
    fn is_deleted(&self) -> bool {
        matches!(
            self,
            Self::Begin(ChatSessionControllerError::Deleted)
                | Self::TerminalRetry {
                    source: ChatSessionControllerError::Deleted,
                    ..
                }
        )
    }
}

#[derive(Debug, thiserror::Error)]
enum TerminalPersistenceError {
    #[error("Chat session controller lock is poisoned")]
    ControllerLockPoisoned,
    #[error(transparent)]
    Finish(#[from] ChatSessionControllerError),
    #[error("Chat terminal persistence worker disconnected")]
    WorkerDisconnected,
}

impl TerminalPersistenceError {
    fn is_deleted(&self) -> bool {
        matches!(self, Self::Finish(ChatSessionControllerError::Deleted))
    }
}

/// Owns the durable user-to-terminal gap independently of the Chat entity.
///
/// The detached worker keeps the shutdown reservation after a window closes.
/// Dropping the command sender means generation was cancelled by entity
/// release, so the worker records a cancelled terminal before releasing that
/// reservation. The foreground task only observes the result and may be
/// cancelled without cancelling the persistence itself.
pub(super) struct TurnPersistenceCoordinator {
    command: Option<oneshot::Sender<ChatTurnTerminal>>,
    result: Option<oneshot::Receiver<Result<(), TerminalPersistenceError>>>,
}

type BeginPersistenceOutcome = (bool, bool, Result<ChatTurnStart, BeginPersistenceError>);

impl TurnPersistenceCoordinator {
    fn start(
        controller: ChatSessionControllerHandle,
        request: PendingBeginRequest,
        pending_terminal: Option<(String, ChatTurnTerminal)>,
        operation_guard: SessionOperationGuard,
        quiescence: ConversationQuiescence,
        cx: &mut Context<ConversationRuntime>,
    ) -> (Self, oneshot::Receiver<BeginPersistenceOutcome>) {
        let attempted_terminal_retry = pending_terminal.is_some();
        let turn_id = request.turn_id.clone();
        let (begin_tx, begin_rx) = oneshot::channel();
        let (command_tx, command_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let work = quiescence.begin_work();
        cx.background_spawn(async move {
            let _work = work;
            let mut terminal_committed = false;
            let begin = (|| {
                let mut controller = controller
                    .lock()
                    .map_err(|_| BeginPersistenceError::ControllerLockPoisoned)?;
                let authorized_store = operation_guard.authorized_store();
                controller.with_replaced_store(authorized_store, |controller| {
                    if let Some((terminal_turn_id, terminal)) = pending_terminal {
                        controller
                            .finish_turn(&terminal_turn_id, &terminal)
                            .map_err(|source| BeginPersistenceError::TerminalRetry {
                                turn_id: terminal_turn_id,
                                source,
                            })?;
                        terminal_committed = true;
                    }
                    controller
                        .begin_turn(request.user_message, request.selection, request.turn_id)
                        .map_err(BeginPersistenceError::Begin)
                })
            })();
            let begin_succeeded = begin.is_ok();
            if let Err((_, _, begin)) =
                begin_tx.send((attempted_terminal_retry, terminal_committed, begin))
            {
                if begin.is_ok() {
                    let _ = persist_terminal_with_retry(
                        &controller,
                        &operation_guard,
                        &turn_id,
                        &ChatTurnTerminal::cancelled(),
                    );
                }
                return;
            }
            if !begin_succeeded {
                return;
            }

            let command = command_rx.await;
            let result = match command {
                Ok(terminal) => {
                    persist_terminal_with_retry(&controller, &operation_guard, &turn_id, &terminal)
                }
                Err(_) => {
                    // Window/entity release cancels provider generation. The
                    // durable user fact still needs one terminal outcome.
                    persist_terminal_with_retry(
                        &controller,
                        &operation_guard,
                        &turn_id,
                        &ChatTurnTerminal::cancelled(),
                    )
                }
            };
            let _ = result_tx.send(result);
            drop(operation_guard);
        })
        .detach();
        (
            Self {
                command: Some(command_tx),
                result: Some(result_rx),
            },
            begin_rx,
        )
    }

    fn for_terminal(
        controller: ChatSessionControllerHandle,
        turn_id: String,
        operation_guard: SessionOperationGuard,
        quiescence: ConversationQuiescence,
        cx: &mut Context<ConversationRuntime>,
    ) -> Self {
        let (command_tx, command_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let work = quiescence.begin_work();
        cx.background_spawn(async move {
            let _work = work;
            let result = match command_rx.await {
                Ok(terminal) => {
                    persist_terminal_with_retry(&controller, &operation_guard, &turn_id, &terminal)
                }
                Err(_) => persist_terminal_with_retry(
                    &controller,
                    &operation_guard,
                    &turn_id,
                    &ChatTurnTerminal::cancelled(),
                ),
            };
            let _ = result_tx.send(result);
            drop(operation_guard);
        })
        .detach();
        Self {
            command: Some(command_tx),
            result: Some(result_rx),
        }
    }

    fn persist(
        mut self,
        terminal: ChatTurnTerminal,
    ) -> Result<oneshot::Receiver<Result<(), TerminalPersistenceError>>, TerminalPersistenceError>
    {
        let command = self
            .command
            .take()
            .ok_or(TerminalPersistenceError::WorkerDisconnected)?;
        command
            .send(terminal)
            .map_err(|_| TerminalPersistenceError::WorkerDisconnected)?;
        self.result
            .take()
            .ok_or(TerminalPersistenceError::WorkerDisconnected)
    }
}

fn persist_terminal_with_retry(
    controller: &ChatSessionControllerHandle,
    operation_guard: &SessionOperationGuard,
    turn_id: &str,
    terminal: &ChatTurnTerminal,
) -> Result<(), TerminalPersistenceError> {
    match persist_terminal(controller, operation_guard, turn_id, terminal) {
        Ok(()) => Ok(()),
        Err(error) if error.is_deleted() => Err(error),
        Err(error) => {
            // The foreground observer may disappear immediately after dispatch
            // during window close. Keep one exact-DTO retry inside the worker so
            // terminal durability never depends on that entity staying alive.
            crate::logging::warn(
                "chat.persistence",
                format_args!(
                    "retrying Chat terminal after first failure; turn_id={turn_id}: {error}"
                ),
            );
            persist_terminal(controller, operation_guard, turn_id, terminal)
        }
    }
}

fn persist_terminal(
    controller: &ChatSessionControllerHandle,
    operation_guard: &SessionOperationGuard,
    turn_id: &str,
    terminal: &ChatTurnTerminal,
) -> Result<(), TerminalPersistenceError> {
    let mut controller = controller
        .lock()
        .map_err(|_| TerminalPersistenceError::ControllerLockPoisoned)?;
    let authorized_store = operation_guard.authorized_store();
    controller
        .with_replaced_store(authorized_store, |controller| {
            controller.finish_turn(turn_id, terminal)
        })
        .map_err(TerminalPersistenceError::Finish)
}
