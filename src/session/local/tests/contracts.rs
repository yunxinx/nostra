use super::*;

#[test]
fn memory_and_local_agent_stores_share_tree_and_replay_contracts() {
    let project = ProjectIdentity::new("/tmp/agent-project", "Agent Project");
    let mut memory = InMemorySessionStore::new();
    exercise_agent_tree_contract(&mut memory, project.clone());

    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
    let mut local = LocalSessionStore::open(config.clone()).expect("open local agent");
    let session_id = exercise_agent_tree_contract(&mut local, project.clone());
    local.shutdown().expect("shutdown local agent");

    let reopened = LocalSessionStore::open(config).expect("reopen local agent");
    assert_eq!(
        reopened
            .load_branch_tree(&session_id)
            .expect("reloaded branch tree")
            .nodes
            .len(),
        3
    );
}

#[test]
fn project_scoped_restore_rejects_sessions_from_another_project() {
    let mut memory = InMemorySessionStore::new();
    assert_project_scoped_restore_isolation(&mut memory);

    let root = tempfile::tempdir().expect("tempdir");
    let mut local =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
            .expect("open local agent");
    assert_project_scoped_restore_isolation(&mut local);
}

#[test]
fn local_chat_store_round_trips_and_lists_without_read_timestamp_mutation() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("hello")])
        .expect("append");
    let before = store
        .list(CatalogQuery::first_page())
        .expect("list")
        .sessions;
    let state = store.load_session(&session_id, None).expect("load");
    let after = store
        .list(CatalogQuery::first_page())
        .expect("list")
        .sessions;
    assert_eq!(state.messages.len(), 1);
    assert_eq!(before, after);
    assert_eq!(before[0].preview.as_deref(), Some("hello"));
    assert_eq!(before[0].title.as_deref(), Some("hello"));
    store.shutdown().expect("shutdown");

    let mut reopened =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("reopen");
    assert_eq!(
        reopened
            .load_session(&session_id, None)
            .expect("reload")
            .messages
            .len(),
        1
    );
    reopened.shutdown().expect("shutdown");
}

#[test]
fn complete_blank_jsonl_lines_are_rejected_outside_explicit_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("authoritative message")])
        .expect("append");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("close recorder");

    std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .expect("open source")
        .write_all(b"\n")
        .expect("append complete blank line");

    let reopened = LocalSessionStore::open(config.clone()).expect("reopen for load");
    assert!(matches!(
        reopened.load_session(&session_id, None),
        Err(SessionError::CorruptLine { .. })
    ));
    drop(reopened);

    let mut reopened = LocalSessionStore::open(config).expect("reopen for append");
    assert!(matches!(
        reopened.append(&session_id, vec![message("must not cross corruption")]),
        Err(SessionError::CorruptLine { .. })
    ));

    let report = reopened.repair().expect("repair reports source issue");
    assert!(!report.issues.is_empty());
    assert!(
        reopened
            .get_summary(&session_id)
            .expect("summary after repair")
            .is_some(),
        "repair must preserve the last trusted row for an existing corrupt source"
    );
}

#[test]
fn a_complete_malformed_header_is_reported_as_a_corrupt_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("last trusted message")])
        .expect("append");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("close recorder");

    std::fs::write(&source, b"{not-json}\n").expect("replace source with malformed header");

    let reopened = LocalSessionStore::open(config.clone()).expect("reopen for load");
    assert!(matches!(
        reopened.load_session(&session_id, None),
        Err(SessionError::CorruptLine { line: 1, .. })
    ));
    drop(reopened);

    let mut reopened = LocalSessionStore::open(config).expect("reopen for append");
    assert!(matches!(
        reopened.append(&session_id, vec![message("must not cross corruption")]),
        Err(SessionError::CorruptLine { line: 1, .. })
    ));

    let report = reopened.repair().expect("repair reports source issue");
    assert!(!report.issues.is_empty());
    assert!(
        reopened
            .get_summary(&session_id)
            .expect("summary after repair")
            .is_some(),
        "repair must preserve the last trusted row when the source header is corrupt"
    );
}

#[test]
fn chat_and_agent_roots_and_catalogs_are_isolated() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut chat = LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
        .expect("chat");
    let mut agent =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
            .expect("agent");
    let chat_header = SessionHeader::new(SessionDomain::Chat, None);
    let chat_id = chat_header.session_id.clone();
    chat.create_session(chat_header).expect("chat create");
    let agent_header = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    );
    let agent_id = agent_header.session_id.clone();
    agent.create_session(agent_header).expect("agent create");
    assert_ne!(chat.catalog_path(), agent.catalog_path());
    let chat_index = rusqlite::Connection::open(chat.catalog_path()).expect("chat index");
    let chat_projects: i64 = chat_index
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
            [],
            |row| row.get(0),
        )
        .expect("chat schema");
    let agent_index = rusqlite::Connection::open(agent.catalog_path()).expect("agent index");
    let agent_projects: i64 = agent_index
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
            [],
            |row| row.get(0),
        )
        .expect("agent schema");
    assert_eq!(chat_projects, 0);
    assert_eq!(agent_projects, 1);
    assert_eq!(
        chat.list(CatalogQuery::first_page())
            .expect("list")
            .sessions
            .len(),
        1
    );
    assert_eq!(
        agent
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions
            .len(),
        1
    );
    assert!(chat.load_session(&agent_id, None).is_err());
    assert!(agent.load_session(&chat_id, None).is_err());
}

#[test]
fn pagination_uses_creation_cursor_without_duplicates() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    for index in 0..35 {
        let mut header = SessionHeader::new(SessionDomain::Chat, None);
        header.created_at = index;
        store.create_session(header).expect("create");
    }
    let first = store
        .list(CatalogQuery::with_limit(30))
        .expect("first page");
    let second = store
        .list(CatalogQuery {
            cursor: first.next_cursor.clone(),
            ..CatalogQuery::with_limit(30)
        })
        .expect("second page");
    assert_eq!(first.sessions.len(), 30);
    assert_eq!(second.sessions.len(), 5);
    let first_ids = first
        .sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect::<std::collections::HashSet<_>>();
    assert!(
        second
            .sessions
            .iter()
            .all(|session| !first_ids.contains(&session.session_id))
    );
}
