use super::*;

impl ChatView {
    /// Transfer any unresolved durable turn to a detached terminal worker.
    ///
    /// Provider tasks are entity-owned and therefore cannot be the final owner
    /// of a terminal fact during application exit. The detached coordinator
    /// keeps the operation reservation until the exact terminal write settles,
    /// which makes `SessionStores::shutdown` wait without keeping the window
    /// alive.
    pub(crate) fn prepare_for_shutdown(&mut self, cx: &mut Context<Self>) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;

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
                Err(error) => {
                    crate::logging::error(
                        "chat.persistence",
                        format_args!("cannot retry Chat terminal during shutdown: {error}"),
                    );
                }
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
                    format_args!("cannot finalize Chat turn during shutdown: {error}"),
                ),
            }
        }

        // Once durability has an independent owner, dropping the provider task
        // is safe and avoids relying on a window-scoped cancellation callback.
        if let Some(reply) = self.reply_task.take() {
            reply.cancel();
        }
        self.pending = false;
        cx.notify();
    }

    /// Begin permanent deletion without performing storage I/O in the GPUI
    /// update. The controller mutex serializes this operation with any begin
    /// or terminal write already scheduled for the same conversation, so a
    /// first turn cannot publish an orphan after the user confirms deletion.
    pub(crate) fn request_delete(&mut self, cx: &mut Context<Self>) -> ChatDeleteRequest {
        if self.deletion_pending {
            return ChatDeleteRequest::Pending;
        }
        let Some(controller) = self.session_controller.clone() else {
            // A view without durable storage cannot have accepted a user turn.
            return ChatDeleteRequest::RemoveNow;
        };
        let operation_guard = match self
            .session_store
            .as_ref()
            .ok_or_else(|| "Chat session storage has not been initialized".to_string())
            .and_then(|store| store.reserve_operation().map_err(|error| error.to_string()))
        {
            Ok(guard) => guard,
            Err(error) => {
                crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot reserve permanent Chat deletion: {error}"),
                );
                self.notify_delete_failure(cx);
                return ChatDeleteRequest::Pending;
            }
        };

        self.deletion_requested = true;
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
                    format_args!("cannot finalize Chat turn before deletion: {error}"),
                ),
            }
        }
        if let Some(reply) = self.reply_task.take() {
            reply.cancel();
        }
        self.pending = false;
        self.deletion_pending = true;
        let background = cx.background_spawn(async move {
            let mut controller = controller
                .lock()
                .map_err(|_| "Chat session controller lock is poisoned".to_string())?;
            let authorized_store = operation_guard.authorized_store();
            controller.with_replaced_store(authorized_store, |controller| {
                controller
                    .delete_session()
                    .map_err(|error| error.to_string())
            })
        });
        let task = cx.spawn(async move |this, cx| {
            let result = background.await;
            let _ = this.update(cx, |this, cx| {
                this.deletion_pending = false;
                this._deletion_task = None;
                match result {
                    Ok(()) => cx.emit(ChatEvent::DeleteCompleted),
                    Err(error) => {
                        // A failed delete leaves the durable conversation in
                        // place. Re-enable normal interaction after the detached
                        // cancellation terminal has closed any active turn.
                        this.deletion_requested = false;
                        crate::logging::error(
                            "chat.persistence",
                            format_args!("failed to permanently delete Chat session: {error}"),
                        );
                        this.notify_delete_failure(cx);
                    }
                }
                cx.notify();
            });
        });
        self._deletion_task = Some(task);
        cx.notify();
        ChatDeleteRequest::Pending
    }

    fn notify_delete_failure(&self, cx: &mut Context<Self>) {
        let window_handle = self.window_handle;
        let message = t!("chat.error.persistence_delete_failed").to_string();
        cx.defer(move |cx| {
            let _ = window_handle.update(cx, |_, window, cx| {
                window.push_notification((NotificationType::Error, message), cx);
            });
        });
    }

    /// Submit a non-empty message when no reply or persistence operation is in
    /// flight. Durable begin runs on the background executor; provider work is
    /// started only after that commit succeeds.
    pub(in crate::chat) fn submit(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_selection_availability(cx);
        if self.pending
            || self.persistence_pending
            || self.deletion_requested
            || self.deletion_pending
            || self.shutdown_requested
            || text.is_empty()
            || !self.selection_available
            || (self.pending_turn_id.is_some() && !self.pending)
        {
            return false;
        }

        let Some(selection) = self.selection.clone() else {
            return false;
        };
        let Some(controller) = self.session_controller.clone() else {
            if let Some(reason) = &self.session_unavailable {
                crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot persist Chat turn: {reason}"),
                );
            }
            window.push_notification(
                (
                    NotificationType::Error,
                    t!("chat.error.persistence_start_failed").to_string(),
                ),
                cx,
            );
            return false;
        };
        let operation_guard = match self
            .session_store
            .as_ref()
            .ok_or_else(|| "Chat session storage has not been initialized".to_string())
            .and_then(|store| store.reserve_operation().map_err(|error| error.to_string()))
        {
            Ok(guard) => guard,
            Err(error) => {
                crate::logging::error(
                    "chat.persistence",
                    format_args!("cannot reserve Chat turn persistence: {error}"),
                );
                window.push_notification(
                    (
                        NotificationType::Error,
                        t!("chat.error.persistence_start_failed").to_string(),
                    ),
                    cx,
                );
                return false;
            }
        };

        let turn_id = format!("turn-{}", self.next_turn_id);
        let user_message = LlmMessage {
            role: crate::llm::Role::User,
            content: vec![ContentBlock::Text {
                text: text.clone(),
                provider_metadata: ProviderMetadata::default(),
            }],
            provider_metadata: ProviderMetadata::default(),
        };
        let request = PendingBeginRequest {
            text,
            user_message,
            selection,
            turn_id,
            composer_revision: self.composer_revision,
        };
        let pending_terminal = self.pending_terminal.clone();
        let (coordinator, begin) = TurnPersistenceCoordinator::start(
            controller,
            request.clone(),
            pending_terminal,
            operation_guard,
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
        cx.notify();
        true
    }

    fn finish_begin_persistence(
        &mut self,
        request: PendingBeginRequest,
        outcome: (bool, bool, Result<ChatTurnStart, BeginPersistenceError>),
        cx: &mut Context<Self>,
    ) {
        let (attempted_terminal_retry, terminal_committed, result) = outcome;
        self.persistence_pending = false;
        self._persistence_task = None;
        if terminal_committed {
            self.pending_terminal = None;
            self.pending_turn_id = None;
        }
        if self.shutdown_requested {
            // `prepare_for_shutdown` already handed this turn to the detached
            // coordinator. A late begin completion must never launch provider
            // work while the store is crossing its final mutation barrier.
            cx.notify();
            return;
        }
        if self.deletion_requested {
            // A committed begin may report success after deletion was already
            // accepted. Keep the turn out of the UI and, critically, do not
            // launch a provider request that the user has just cancelled.
            cx.notify();
            return;
        }
        let start = match result {
            Ok(start) if self.terminal_persistence.is_some() => start,
            Ok(_) => {
                // The view relinquished this turn (for example, deletion or
                // shutdown) while durable begin was running. The detached
                // worker owns cancellation persistence; provider work must not
                // start without its command capability.
                cx.notify();
                return;
            }
            Err(error) if error.is_deleted() => {
                // Permanent deletion can overtake a previously queued begin.
                // That is an expected cancellation; the deletion task owns
                // the user-visible outcome and removes the conversation.
                cx.notify();
                return;
            }
            Err(error) => {
                self.terminal_persistence = None;
                crate::logging::error(
                    "chat.persistence",
                    format_args!("failed to persist Chat turn begin: {error}"),
                );
                let message = if attempted_terminal_retry && !terminal_committed {
                    t!("chat.error.persistence_retry_failed").to_string()
                } else {
                    t!("chat.error.persistence_start_failed").to_string()
                };
                self.notify_begin_persistence_failure(message, cx);
                cx.notify();
                return;
            }
        };

        if self.messages.is_empty() {
            cx.emit(ChatEvent::TitleChanged(derive_title(&request.text)));
        }
        let old_len = self.messages.len();
        self.messages
            .push(Message::from_canonical(request.user_message, cx));
        self.messages.push(Message::empty(Role::Assistant));
        self.list_state.splice(old_len..old_len, 2);
        self.pending = true;
        self.pending_turn_id = Some(request.turn_id.clone());
        self.conversation_id = start.session_id.to_string();
        let history = self
            .messages
            .iter()
            .take(self.messages.len().saturating_sub(1))
            .map(Message::canonical)
            .filter(is_replayable)
            .collect();
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        #[cfg(test)]
        let reply_task = if let Some(dropped) = self.next_reply_drop_flag.take() {
            assistant::ReplyTask::pending_for_test(dropped, cx)
        } else {
            assistant::stream_reply(
                history,
                Some(request.selection),
                self.conversation_id.clone(),
                request.turn_id,
                cx,
            )
        };
        #[cfg(not(test))]
        let reply_task = assistant::stream_reply(
            history,
            Some(request.selection),
            self.conversation_id.clone(),
            request.turn_id,
            cx,
        );
        self.reply_task = Some(reply_task);
        if self.composer_revision == request.composer_revision {
            self.input_empty = true;
            let input = self.input.clone();
            let window_handle = self.window_handle;
            cx.defer(move |cx| {
                let _ = window_handle.update(cx, |_, window, cx| {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                });
            });
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Finish the turn in flight and attach its terminal error, if any. The
    /// error card's state is built here, outside render, and all terminal state
    /// changes are published with one notification.
    #[cfg(test)]
    pub fn finish_reply(
        &mut self,
        message: Option<IndexedMessage>,
        error: Option<GatewayError>,
        cx: &mut Context<Self>,
    ) {
        self.finish_reply_visual(message, error, cx);
        self.pending_turn_id = None;
        self.terminal_persistence = None;
        cx.notify();
    }

    pub(crate) fn finish_reply_request_failed(
        &mut self,
        error: GatewayError,
        cx: &mut Context<Self>,
    ) {
        self.finish_reply_with_terminal(
            None,
            ChatTurnTerminal::request_failed(&error),
            Some(error),
            cx,
        );
    }

    pub(crate) fn finish_reply_with_terminal(
        &mut self,
        message: Option<IndexedMessage>,
        terminal: ChatTurnTerminal,
        error: Option<GatewayError>,
        cx: &mut Context<Self>,
    ) {
        self.finish_reply_visual(message, error, cx);
        if self.deletion_requested {
            // The queued delete owns durability from this point. Persisting a
            // cancellation terminal would only add write traffic and another
            // race to a conversation that must be removed.
            self.pending_terminal = None;
            self.pending_turn_id = None;
            self.terminal_persistence = None;
            cx.notify();
            return;
        }
        if self.shutdown_requested {
            // Exit preparation already detached the cancellation terminal and
            // dropped the provider task. Ignore a terminal callback that was
            // queued immediately before that foreground transition.
            self.pending_terminal = None;
            self.pending_turn_id = None;
            self.terminal_persistence = None;
            cx.notify();
            return;
        }
        let Some(turn_id) = self.pending_turn_id.clone() else {
            self.pending_terminal = None;
            self.terminal_persistence = None;
            cx.notify();
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
                        format_args!("cannot reserve Chat terminal persistence: {error}"),
                    );
                    self.pending_terminal = Some((turn_id, terminal));
                    self.pending_turn_id = None;
                    self.notify_persistence_failure(cx);
                    cx.notify();
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
                        "failed to dispatch Chat terminal persistence; turn_id={retry_turn_id}: {error}"
                    ),
                );
                self.pending_terminal = Some((retry_turn_id, retry_terminal));
                self.pending_turn_id = None;
                self.notify_persistence_failure(cx);
                cx.notify();
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
                    Ok(()) => {
                        this.pending_turn_id = None;
                        this.pending_terminal = None;
                    }
                    Err(error) if error.is_deleted() => {
                        // Deletion owns the outcome when it overtakes a queued
                        // terminal write; no retry or failure UI remains valid.
                        this.pending_turn_id = None;
                        this.pending_terminal = None;
                    }
                    Err(error) => {
                        crate::logging::error(
                            "chat.persistence",
                            format_args!(
                                "failed to persist Chat terminal; turn_id={retry_turn_id}: {error}"
                            ),
                        );
                        // Retain the exact terminal DTO. The next submit first
                        // retries this fact on the same serialized controller
                        // before it attempts a new durable user turn.
                        this.pending_terminal =
                            Some((retry_turn_id.clone(), retry_terminal.clone()));
                        this.pending_turn_id = None;
                        this.notify_persistence_failure(cx);
                    }
                }
                cx.notify();
            });
        });
        self.persistence_pending = true;
        self._persistence_task = Some(task);
        cx.notify();
    }

    fn new_terminal_coordinator(
        &self,
        turn_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<TurnPersistenceCoordinator, String> {
        let controller = self
            .session_controller
            .clone()
            .ok_or_else(|| "Chat session storage has not been initialized".to_string())?;
        let operation_guard = self
            .session_store
            .as_ref()
            .ok_or_else(|| "Chat session storage has not been initialized".to_string())?
            .reserve_operation()
            .map_err(|error| error.to_string())?;
        Ok(TurnPersistenceCoordinator::for_terminal(
            controller,
            turn_id.to_string(),
            operation_guard,
            cx,
        ))
    }

    fn persist_terminal_detached(
        &self,
        coordinator: TurnPersistenceCoordinator,
        terminal: ChatTurnTerminal,
        turn_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let result = match coordinator.persist(terminal) {
            Ok(result) => result,
            Err(error) => {
                // A disconnected command receiver means the begin worker has
                // already failed or has already applied its own cancellation
                // fallback. There is no entity-owned retry left to wait on.
                crate::logging::error(
                    "chat.persistence",
                    format_args!(
                        "failed to dispatch detached Chat terminal; turn_id={}: {error}",
                        turn_id.as_deref().unwrap_or("pending-begin")
                    ),
                );
                return;
            }
        };
        cx.background_spawn(async move {
            match result.await {
                Ok(Ok(())) | Ok(Err(TerminalPersistenceError::Finish(ChatSessionControllerError::Deleted))) => {}
                Ok(Err(error)) => crate::logging::error(
                    "chat.persistence",
                    format_args!(
                        "detached Chat terminal persistence failed; turn_id={}: {error}",
                        turn_id.as_deref().unwrap_or("pending-begin")
                    ),
                ),
                // A begin coordinator legitimately has no terminal result when
                // deletion wins before the first durable user fact commits.
                // The operation guard is still released by that worker; avoid
                // turning this expected race into noisy diagnostics.
                Err(_) => {}
            }
        })
        .detach();
    }

    fn notify_begin_persistence_failure(&self, message: String, cx: &mut Context<Self>) {
        let window_handle = self.window_handle;
        cx.defer(move |cx| {
            let _ = window_handle.update(cx, |_, window, cx| {
                window.push_notification((NotificationType::Error, message), cx);
            });
        });
    }

    fn notify_persistence_failure(&self, cx: &mut Context<Self>) {
        let window_handle = self.window_handle;
        let message = t!("chat.error.persistence_finish_failed").to_string();
        cx.defer(move |cx| {
            let _ = window_handle.update(cx, |_, window, cx| {
                window.push_notification((NotificationType::Error, message), cx);
            });
        });
    }

    fn finish_reply_visual(
        &mut self,
        message: Option<IndexedMessage>,
        error: Option<GatewayError>,
        cx: &mut Context<Self>,
    ) {
        let turn_error = error.map(|error| TurnError::new(error, cx));
        if let Some(last) = self.messages.last_mut() {
            if let Some(message) = message {
                // Match pi's message lifecycle: deltas provide a responsive live
                // projection, then the complete message_end snapshot becomes
                // authoritative for both rendering and replay.
                last.replace_with_canonical(message, cx);
            }
            last.error = turn_error;
            // Terminal fallback for a stream that never delivered its explicit
            // `ReasoningFinished` boundary, including cancellation and failure.
            last.finish_reasoning(None);
        }
        self.pending = false;
        self.reply_task = None;
        self.remeasure_latest_message();
    }
}
