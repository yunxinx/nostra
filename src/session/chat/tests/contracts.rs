use super::*;
use crate::session::{
    CatalogQuery, ProjectIdentity, ProjectSessionStore, SessionCatalogStore, SessionStores,
};

#[test]
fn completed_contract_works_with_memory_store() {
    exercise_completed(InMemorySessionStore::new());
}

#[test]
fn completed_contract_works_with_local_store() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
        .expect("store should open");
    exercise_completed(store);
}

#[test]
fn deleting_a_shared_chat_controller_removes_the_durable_session() {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let lifecycle = stores.chat().expect("Chat lifecycle capability");
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let mut controller = ChatSessionController::new(lifecycle.clone());
    let start = controller
        .begin_turn(
            text_message(Role::User, "delete me"),
            model("model-a"),
            "turn-1",
        )
        .expect("create durable Chat session");

    controller
        .delete_session()
        .expect("confirmed deletion should remove durable facts");

    assert_eq!(controller.session_id(), None);
    assert!(matches!(
        lifecycle.load_session(&start.session_id, None),
        Err(SessionError::SessionNotFound(id)) if id == start.session_id
    ));
    assert!(
        catalog
            .get_session_summary(&start.session_id)
            .expect("read catalog")
            .is_none()
    );
}

#[test]
fn project_controller_creates_and_restores_a_project_scoped_session() {
    let project = ProjectIdentity::new("/tmp/project-controller", "Project Controller");
    let stores = SessionStores::with_agent_store(InMemorySessionStore::new());
    let lifecycle = stores.agent().expect("Agent lifecycle capability");
    let catalog = stores
        .agent_projects()
        .expect("Agent project catalog capability");
    let mut controller = ChatSessionController::for_project(lifecycle, project.clone());

    let start = controller
        .begin_turn(
            text_message(Role::User, "project turn"),
            model("model-a"),
            "turn-1",
        )
        .expect("create project-scoped session");
    controller
        .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
        .expect("persist project terminal");

    assert_eq!(start.session_id.domain(), SessionDomain::Agent);
    let page = catalog
        .list_project_sessions(&project.project_id, CatalogQuery::first_page())
        .expect("list project sessions");
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].session_id, start.session_id);
    assert_eq!(page.sessions[0].project.as_ref(), Some(&project));

    let restored = controller
        .restore(&start.session_id)
        .expect("restore project session");
    assert_eq!(restored.messages.len(), 1);
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
    let store = LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
        .expect("store should open");
    exercise_terminal_without_assistant(
        store,
        ChatTurnTerminal::request_failed(&GatewayError::configuration("missing provider")),
    );
}

#[test]
fn cancelled_generation_contract_works_with_memory_store() {
    exercise_terminal_without_assistant(InMemorySessionStore::new(), ChatTurnTerminal::cancelled());
}

#[test]
fn cancelled_generation_contract_works_with_local_store() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
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

    let mut restored =
        ChatSessionController::new(LocalSessionStore::open(config.clone()).expect("reopen store"));
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
