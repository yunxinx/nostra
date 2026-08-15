use super::*;

struct AtomicCreateStore {
    inner: InMemorySessionStore,
    atomic_create_calls: usize,
}

impl SessionLifecycleStore for AtomicCreateStore {
    fn create_session(&mut self, _header: SessionHeader) -> Result<SessionId, SessionError> {
        Err(SessionError::io(std::io::Error::other(
            "split session creation is forbidden",
        )))
    }

    fn create_session_with_entries(
        &mut self,
        header: SessionHeader,
        entries: Vec<SessionEntryKind>,
    ) -> Result<(SessionId, Vec<EntryId>), SessionError> {
        self.atomic_create_calls = self.atomic_create_calls.saturating_add(1);
        self.inner.create_session_with_entries(header, entries)
    }

    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError> {
        self.inner.append(session_id, entries)
    }

    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        self.inner.delete_session(session_id)
    }
}

impl SessionReadStore for AtomicCreateStore {
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError> {
        self.inner.load_session(session_id, leaf)
    }
}

impl SessionTreeStore for AtomicCreateStore {
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

impl SessionFlushStore for AtomicCreateStore {
    fn flush(&mut self) -> Result<(), SessionError> {
        self.inner.flush()
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        self.inner.shutdown()
    }
}

#[test]
fn first_turn_uses_the_atomic_session_creation_primitive() {
    let mut controller = ChatSessionController::new(AtomicCreateStore {
        inner: InMemorySessionStore::new(),
        atomic_create_calls: 0,
    });

    let start = controller
        .begin_turn(
            text_message(Role::User, "atomic first turn"),
            model("model-a"),
            "turn-1",
        )
        .expect("atomic begin");
    assert_eq!(controller.store.atomic_create_calls, 1);
    controller
        .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
        .expect("finish");
    let state = controller.restore(&start.session_id).expect("restore");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].entry_id, start.user_entry_id);
}

struct FailOnceStore {
    inner: InMemorySessionStore,
    fail_next_append: bool,
}

impl SessionLifecycleStore for FailOnceStore {
    fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError> {
        self.inner.create_session(header)
    }

    fn create_session_with_entries(
        &mut self,
        header: SessionHeader,
        entries: Vec<SessionEntryKind>,
    ) -> Result<(SessionId, Vec<EntryId>), SessionError> {
        self.inner.create_session_with_entries(header, entries)
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

    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        self.inner.delete_session(session_id)
    }
}

impl SessionReadStore for FailOnceStore {
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

    fn create_session_with_entries(
        &mut self,
        header: SessionHeader,
        entries: Vec<SessionEntryKind>,
    ) -> Result<(SessionId, Vec<EntryId>), SessionError> {
        let created = self.inner.create_session_with_entries(header, entries)?;
        if self.fail_create_after_commit {
            self.fail_create_after_commit = false;
            return Err(SessionError::Io {
                source: std::io::Error::other("injected post-commit create failure"),
            });
        }
        Ok(created)
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

    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        self.inner.delete_session(session_id)
    }
}

impl SessionReadStore for CommitThenErrorStore {
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
    let mut controller = ChatSessionController::new(CommitThenErrorStore {
        inner: InMemorySessionStore::new(),
        fail_create_after_commit: false,
        fail_append_after_commit: false,
    });
    controller
        .begin_turn(
            text_message(Role::User, "first turn"),
            model("model-a"),
            "turn-1",
        )
        .expect("first user");
    controller
        .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
        .expect("first terminal");

    controller.store.fail_append_after_commit = true;
    let user = text_message(Role::User, "user retry");
    assert!(matches!(
        controller.begin_turn(user.clone(), model("model-a"), "turn-2"),
        Err(ChatSessionControllerError::Storage(SessionError::Io { .. }))
    ));
    let start = controller
        .begin_turn(user, model("model-a"), "turn-2")
        .expect("retry should discover the committed user fact");
    controller
        .finish_turn("turn-2", &ChatTurnTerminal::cancelled())
        .expect("terminal should persist");
    let state = controller.restore(&start.session_id).expect("restore");
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.turn_results.len(), 2);
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
