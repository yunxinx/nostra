use super::*;

use gpui::{AppContext as _, Context, Window};
use gpui_component::{WindowExt as _, notification::NotificationType};
use rust_i18n::t;

use crate::{
    chat::{
        ChatDeleteRequest, ChatView,
        conversation_runtime::{
            BeginTurnAdmissionError, ConversationRequestGeneration, ConversationRuntime,
            ConversationRuntimeEvent, ConversationRuntimeFailure, ConversationRuntimeUpdate,
            PendingBeginRequest, StartedConversationTurn,
        },
    },
    llm::{ContentBlock, Message as LlmMessage, ModelSelection, ProviderMetadata},
    session::{ChatSessionControllerError, ChatTurnTerminal},
};

impl ConversationRuntime {
    /// Admit one durable user turn and publish it only after the user fact is
    /// committed. The runtime owns the persistence reservation and request
    /// generation before any provider work can begin.
    pub(super) fn begin_turn(
        &mut self,
        user_message: LlmMessage,
        selection: ModelSelection,
        composer_revision: u64,
        cx: &mut Context<Self>,
    ) -> Result<ConversationRequestGeneration, BeginTurnAdmissionError> {
        if self.generating
            || self.persistence_pending
            || self.deletion_requested
            || self.deletion_pending
            || self.shutdown_requested
            || self.pending_turn_id.is_some()
        {
            return Err(BeginTurnAdmissionError::NotAccepting);
        }
        let controller = self.session_controller.clone().ok_or_else(|| {
            BeginTurnAdmissionError::StorageUnavailable(
                self.session_unavailable
                    .clone()
                    .unwrap_or_else(|| "session storage is unavailable".into()),
            )
        })?;
        let operation_guard = self
            .session_store
            .as_ref()
            .ok_or_else(|| {
                BeginTurnAdmissionError::StorageUnavailable(
                    "session storage has not been initialized".into(),
                )
            })?
            .reserve_operation()
            .map_err(|error| BeginTurnAdmissionError::OperationReservation(error.to_string()))?;
        let generation = self
            .request_generation
            .next()
            .ok_or(BeginTurnAdmissionError::GenerationExhausted)?;
        self.request_generation = generation;
        let turn_id = format!("turn-{}", self.next_turn_id);
        let request = PendingBeginRequest {
            user_message,
            selection,
            turn_id,
            composer_revision,
            request_generation: generation,
        };
        let pending_terminal = self.pending_terminal.clone();
        let (coordinator, begin) = TurnPersistenceCoordinator::start(
            controller,
            request.clone(),
            pending_terminal,
            operation_guard,
            self.quiescence.clone(),
            cx,
        );
        self.terminal_persistence = Some(coordinator);
        let task = cx.spawn(async move |this, async_cx| {
            let outcome = begin.await.unwrap_or((
                false,
                false,
                Err(BeginPersistenceError::WorkerDisconnected),
            ));
            let _ = this.update(async_cx, |this, inner_cx| {
                this.finish_begin_persistence(request, outcome, inner_cx);
            });
        });
        self.persistence_pending = true;
        self._persistence_task = Some(task);
        self.publish_state(cx);
        Ok(generation)
    }

    fn finish_begin_persistence(
        &mut self,
        request: PendingBeginRequest,
        outcome: BeginPersistenceOutcome,
        cx: &mut Context<Self>,
    ) {
        let (attempted_terminal_retry, terminal_committed, result) = outcome;
        self.persistence_pending = false;
        self._persistence_task = None;
        if terminal_committed {
            self.pending_terminal = None;
            self.pending_turn_id = None;
        }
        if request.request_generation != self.request_generation {
            if let Ok(start) = &result {
                self.pending_terminal =
                    Some((request.turn_id.clone(), ChatTurnTerminal::cancelled()));
                self.pending_turn_id = None;
                self.session_id = Some(start.session_id.clone());
            }
            self.terminal_persistence = None;
            self.generating = false;
            self.publish_state(cx);
            return;
        }
        if self.shutdown_requested || self.deletion_requested {
            self.publish_state(cx);
            return;
        }
        let start = match result {
            Ok(start) if self.terminal_persistence.is_some() => start,
            Ok(_) => {
                self.publish_state(cx);
                return;
            }
            Err(error) if error.is_deleted() => {
                self.publish_state(cx);
                return;
            }
            Err(error) => {
                self.terminal_persistence = None;
                crate::logging::error(
                    "chat.persistence",
                    format_args!("failed to persist conversation turn begin: {error}"),
                );
                let failure = if attempted_terminal_retry && !terminal_committed {
                    ConversationRuntimeFailure::TerminalRetry
                } else {
                    ConversationRuntimeFailure::Begin
                };
                self.publish_state(cx);
                self.publish_event(ConversationRuntimeEvent::Failure(failure), cx);
                return;
            }
        };

        self.generating = true;
        self.pending_turn_id = Some(request.turn_id.clone());
        self.session_id = Some(start.session_id.clone());
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(request.user_message.clone(), cx);
        });
        let history = self.transcript.read(cx).replayable_history();
        let selection = request.selection.clone();
        let turn_id = request.turn_id.clone();
        let generation = request.request_generation;
        let session_id = start.session_id.clone();
        self.publish_state(cx);
        self.publish_event(
            ConversationRuntimeEvent::TurnStarted(Box::new(StartedConversationTurn {
                request,
                session_id: start.session_id,
            })),
            cx,
        );
        #[cfg(test)]
        if let Some(dropped) = self.next_reply_drop_flag.take() {
            self.reply_task = Some(super::super::assistant::ReplyTask::pending_for_test(
                dropped, cx,
            ));
            return;
        }
        self.start_generation(
            history,
            selection,
            session_id.to_string(),
            turn_id,
            generation,
            cx,
        );
    }

    pub(in crate::chat) fn finish_terminal(
        &mut self,
        generation: ConversationRequestGeneration,
        terminal: ChatTurnTerminal,
        cx: &mut Context<Self>,
    ) {
        if generation != self.request_generation {
            return;
        }
        self.generating = false;
        if self.deletion_requested || self.shutdown_requested {
            self.pending_terminal = None;
            self.pending_turn_id = None;
            self.terminal_persistence = None;
            self.publish_state(cx);
            return;
        }
        let Some(turn_id) = self.pending_turn_id.clone() else {
            self.pending_terminal = None;
            self.terminal_persistence = None;
            self.publish_state(cx);
            return;
        };
        let coordinator = if let Some(coordinator) = self.terminal_persistence.take() {
            coordinator
        } else {
            match self.new_terminal_coordinator(&turn_id, cx) {
                Ok(coordinator) => coordinator,
                Err(error) => {
                    crate::logging::error(
                        "chat.persistence",
                        format_args!("cannot reserve conversation terminal persistence: {error}"),
                    );
                    self.pending_terminal = Some((turn_id, terminal));
                    self.pending_turn_id = None;
                    self.publish_state(cx);
                    self.publish_event(
                        ConversationRuntimeEvent::Failure(ConversationRuntimeFailure::Terminal),
                        cx,
                    );
                    return;
                }
            }
        };

        let retry_terminal = terminal.clone();
        let retry_turn_id = turn_id.clone();
        let result = match coordinator.persist(terminal) {
            Ok(result) => result,
            Err(error) => {
                crate::logging::error(
                    "chat.persistence",
                    format_args!(
                        "failed to dispatch conversation terminal persistence; turn_id={retry_turn_id}: {error}"
                    ),
                );
                self.pending_terminal = Some((retry_turn_id, retry_terminal));
                self.pending_turn_id = None;
                self.publish_state(cx);
                self.publish_event(
                    ConversationRuntimeEvent::Failure(ConversationRuntimeFailure::Terminal),
                    cx,
                );
                return;
            }
        };
        let task = cx.spawn(async move |this, cx| {
            let result = result
                .await
                .unwrap_or(Err(TerminalPersistenceError::WorkerDisconnected));
            let _ = this.update(cx, |this, cx| {
                this.persistence_pending = false;
                this._persistence_task = None;
                match result {
                    Ok(())
                    | Err(TerminalPersistenceError::Finish(
                        ChatSessionControllerError::Deleted,
                    )) => {
                        this.pending_turn_id = None;
                        this.pending_terminal = None;
                    }
                    Err(error) => {
                        crate::logging::error(
                            "chat.persistence",
                            format_args!(
                                "failed to persist conversation terminal; turn_id={retry_turn_id}: {error}"
                            ),
                        );
                        this.pending_terminal =
                            Some((retry_turn_id.clone(), retry_terminal.clone()));
                        this.pending_turn_id = None;
                        this.publish_event(
                            ConversationRuntimeEvent::Failure(ConversationRuntimeFailure::Terminal),
                            cx,
                        );
                    }
                }
                this.publish_state(cx);
            });
        });
        self.persistence_pending = true;
        self._persistence_task = Some(task);
        self.publish_state(cx);
    }

    pub(in crate::chat) fn prepare_for_shutdown(&mut self, cx: &mut Context<Self>) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        self.advance_generation();

        if let Some(coordinator) = self.terminal_persistence.take() {
            self.persist_terminal_detached(
                coordinator,
                ChatTurnTerminal::cancelled(),
                self.pending_turn_id.clone(),
                cx,
            );
        } else if !self.persistence_pending
            && let Some((turn_id, terminal)) = self.pending_terminal.take()
        {
            match self.new_terminal_coordinator(&turn_id, cx) {
                Ok(coordinator) => {
                    self.persist_terminal_detached(coordinator, terminal, Some(turn_id), cx);
                }
                Err(error) => crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot retry conversation terminal during shutdown: {error}"),
                ),
            }
        } else if !self.persistence_pending
            && let Some(turn_id) = self.pending_turn_id.clone()
        {
            match self.new_terminal_coordinator(&turn_id, cx) {
                Ok(coordinator) => self.persist_terminal_detached(
                    coordinator,
                    ChatTurnTerminal::cancelled(),
                    Some(turn_id),
                    cx,
                ),
                Err(error) => crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot finalize conversation during shutdown: {error}"),
                ),
            }
        }
        self.cancel_and_release_generation();
        self.generating = false;
        self.publish_state(cx);
    }

    pub(super) fn request_delete(&mut self, cx: &mut Context<Self>) -> ChatDeleteRequest {
        if self.deletion_pending {
            return ChatDeleteRequest::Pending;
        }
        if self.shutdown_requested {
            return ChatDeleteRequest::Rejected;
        }
        let Some(controller) = self.session_controller.clone() else {
            return ChatDeleteRequest::RemoveNow;
        };
        let operation_guard = match self
            .session_store
            .as_ref()
            .ok_or_else(|| "conversation session storage has not been initialized".to_string())
            .and_then(|store| store.reserve_operation().map_err(|error| error.to_string()))
        {
            Ok(guard) => guard,
            Err(error) => {
                crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot reserve permanent conversation deletion: {error}"),
                );
                self.publish_event(
                    ConversationRuntimeEvent::Failure(ConversationRuntimeFailure::Delete),
                    cx,
                );
                return ChatDeleteRequest::Rejected;
            }
        };

        self.deletion_requested = true;
        self.advance_generation();
        let cancelled = ChatTurnTerminal::cancelled();
        if let Some(turn_id) = self.pending_turn_id.clone() {
            self.pending_terminal = Some((turn_id, cancelled.clone()));
        }
        if let Some(coordinator) = self.terminal_persistence.take() {
            self.persist_terminal_detached(
                coordinator,
                cancelled,
                self.pending_turn_id.clone(),
                cx,
            );
        } else if !self.persistence_pending
            && let Some((turn_id, terminal)) = self.pending_terminal.clone()
        {
            match self.new_terminal_coordinator(&turn_id, cx) {
                Ok(coordinator) => {
                    self.persist_terminal_detached(coordinator, terminal, Some(turn_id), cx);
                }
                Err(error) => crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot finalize conversation before deletion: {error}"),
                ),
            }
        }
        self.cancel_and_release_generation();
        self.generating = false;
        self.deletion_pending = true;
        let work = self.quiescence.begin_work();
        let background = cx.background_spawn(async move {
            let _work = work;
            let mut controller = controller
                .lock()
                .map_err(|_| "conversation session controller lock is poisoned".to_string())?;
            let authorized_store = operation_guard.authorized_store();
            controller.with_replaced_store(authorized_store, |controller| {
                controller
                    .delete_session()
                    .map_err(|error| error.to_string())
            })
        });
        cx.spawn(async move |this, cx| {
            let result = background.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.close_scope_after_delete(cx),
                    Err(error) => {
                        this.deletion_pending = false;
                        this.deletion_requested = false;
                        this.delete_completion_requested = false;
                        this.terminal_persistence = None;
                        this.generating = false;
                        crate::logging::error(
                            "chat.persistence",
                            format_args!("failed to permanently delete conversation: {error}"),
                        );
                        this.publish_event(
                            ConversationRuntimeEvent::Failure(ConversationRuntimeFailure::Delete),
                            cx,
                        );
                    }
                }
                this.publish_state(cx);
            });
        })
        .detach();
        self.publish_state(cx);
        ChatDeleteRequest::Pending
    }

    fn close_scope_after_delete(&mut self, cx: &mut Context<Self>) {
        self.delete_completion_requested = true;
        self.close_scope_after_quiescence(cx);
    }

    fn new_terminal_coordinator(
        &self,
        turn_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<TurnPersistenceCoordinator, String> {
        let controller = self
            .session_controller
            .clone()
            .ok_or_else(|| "conversation session storage has not been initialized".to_string())?;
        let operation_guard = self
            .session_store
            .as_ref()
            .ok_or_else(|| "conversation session storage has not been initialized".to_string())?
            .reserve_operation()
            .map_err(|error| error.to_string())?;
        Ok(TurnPersistenceCoordinator::for_terminal(
            controller,
            turn_id.to_string(),
            operation_guard,
            self.quiescence.clone(),
            cx,
        ))
    }

    fn persist_terminal_detached(
        &mut self,
        coordinator: TurnPersistenceCoordinator,
        terminal: ChatTurnTerminal,
        turn_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let retry_terminal = terminal.clone();
        let result = match coordinator.persist(terminal) {
            Ok(result) => result,
            Err(error) => {
                crate::logging::error(
                    "chat.persistence",
                    format_args!(
                        "failed to dispatch detached conversation terminal; turn_id={}: {error}",
                        turn_id.as_deref().unwrap_or("pending-begin")
                    ),
                );
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let result = result
                .await
                .unwrap_or(Err(TerminalPersistenceError::WorkerDisconnected));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(())
                    | Err(TerminalPersistenceError::Finish(
                        ChatSessionControllerError::Deleted,
                    )) => {
                        if this.pending_turn_id.as_deref() == turn_id.as_deref() {
                            this.pending_turn_id = None;
                        }
                        if this
                            .pending_terminal
                            .as_ref()
                            .is_some_and(|(pending_turn_id, _)| {
                                Some(pending_turn_id.as_str()) == turn_id.as_deref()
                            })
                        {
                            this.pending_terminal = None;
                        }
                    }
                    Err(error) => {
                        crate::logging::error(
                            "chat.persistence",
                            format_args!(
                                "detached conversation terminal persistence failed; turn_id={}: {error}",
                                turn_id.as_deref().unwrap_or("pending-begin")
                            ),
                        );
                        if let Some(turn_id) = turn_id.clone()
                            && this
                                .pending_turn_id
                                .as_deref()
                                .is_none_or(|pending_turn_id| pending_turn_id == turn_id)
                            && this
                                .pending_terminal
                                .as_ref()
                                .is_none_or(|(pending_turn_id, _)| pending_turn_id == &turn_id)
                        {
                            if this.pending_turn_id.as_deref() == Some(turn_id.as_str()) {
                                this.pending_turn_id = None;
                            }
                            this.pending_terminal = Some((turn_id, retry_terminal.clone()));
                        }
                    }
                }
                this.publish_state(cx);
            });
        })
        .detach();
    }
}

impl ChatView {
    pub(crate) fn close_scope(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.runtime.update(cx, |runtime, cx| {
            runtime.close_scope(cx);
            runtime.snapshot()
        });
        self.apply_runtime_snapshot(snapshot);
    }

    pub(in crate::chat) fn handle_runtime_update(
        &mut self,
        update: &ConversationRuntimeUpdate,
        cx: &mut Context<Self>,
    ) {
        let runtime_snapshot = update.snapshot().clone();
        if !self.apply_runtime_snapshot(runtime_snapshot.clone()) {
            return;
        }
        match update.event() {
            ConversationRuntimeEvent::StateChanged => {
                cx.notify();
            }
            ConversationRuntimeEvent::TurnStarted(turn) => {
                if turn.request.request_generation != runtime_snapshot.request_generation() {
                    return;
                }
                if self.composer_revision == turn.request.composer_revision {
                    self.input_empty = true;
                    self.input_blank = true;
                    let composer = self.composer.clone();
                    let window_handle = self.window_handle;
                    cx.defer(move |cx| {
                        let _ = window_handle.update(cx, |_, window, cx| {
                            composer
                                .update(cx, |composer, cx| composer.clear_after_submit(window, cx));
                        });
                    });
                }
                self.scroll_to_bottom();
                cx.notify();
            }
            ConversationRuntimeEvent::StreamBatch { generation, events } => {
                if *generation == runtime_snapshot.request_generation() {
                    if !events.is_empty() {
                        self.follow_stream();
                        self.remeasure_latest_message();
                    }
                    cx.notify();
                }
            }
            ConversationRuntimeEvent::GenerationRequestFailed { generation } => {
                if *generation == runtime_snapshot.request_generation() {
                    self.follow_stream();
                    self.remeasure_latest_message();
                    cx.notify();
                }
            }
            ConversationRuntimeEvent::GenerationFinished(generation) => {
                if *generation == runtime_snapshot.request_generation() {
                    self.follow_stream();
                    self.remeasure_latest_message();
                    cx.notify();
                }
            }
            ConversationRuntimeEvent::Failure(failure) => {
                self.notify_runtime_failure(*failure, cx);
            }
            ConversationRuntimeEvent::DeleteCompleted => {}
        }
    }

    pub(crate) fn prepare_for_shutdown(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.runtime.update(cx, |runtime, cx| {
            runtime.prepare_for_shutdown(cx);
            runtime.snapshot()
        });
        self.apply_runtime_snapshot(snapshot);
    }

    pub(crate) fn request_delete(&mut self, cx: &mut Context<Self>) -> ChatDeleteRequest {
        let (request, snapshot) = self.runtime.update(cx, |runtime, cx| {
            let request = runtime.request_delete(cx);
            (request, runtime.snapshot())
        });
        self.apply_runtime_snapshot(snapshot);
        request
    }

    pub(in crate::chat) fn submit(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if text.is_empty() || !self.selection_available {
            return false;
        }
        let Some(selection) = self.selection.clone() else {
            return false;
        };
        let user_message = LlmMessage {
            role: crate::llm::Role::User,
            content: vec![ContentBlock::Text {
                text: text.clone(),
                provider_metadata: ProviderMetadata::default(),
            }],
            provider_metadata: ProviderMetadata::default(),
        };
        let composer_revision = self.composer_revision;
        let (result, snapshot) = self.runtime.update(cx, |runtime, cx| {
            let result = runtime.begin_turn(user_message, selection, composer_revision, cx);
            (result, runtime.snapshot())
        });
        self.apply_runtime_snapshot(snapshot);
        match result {
            Ok(_) => true,
            Err(BeginTurnAdmissionError::NotAccepting) => false,
            Err(error) => {
                crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot begin conversation turn: {error}"),
                );
                window.push_notification(
                    (
                        NotificationType::Error,
                        t!("chat.error.persistence_start_failed").to_string(),
                    ),
                    cx,
                );
                false
            }
        }
    }

    fn notify_runtime_failure(&self, failure: ConversationRuntimeFailure, cx: &mut Context<Self>) {
        let message = match failure {
            ConversationRuntimeFailure::Begin => t!("chat.error.persistence_start_failed"),
            ConversationRuntimeFailure::TerminalRetry => {
                t!("chat.error.persistence_retry_failed")
            }
            ConversationRuntimeFailure::Terminal => t!("chat.error.persistence_finish_failed"),
            ConversationRuntimeFailure::Delete => t!("chat.error.persistence_delete_failed"),
        }
        .to_string();
        let window_handle = self.window_handle;
        cx.defer(move |cx| {
            let _ = window_handle.update(cx, |_, window, cx| {
                window.push_notification((NotificationType::Error, message), cx);
            });
        });
    }
}
