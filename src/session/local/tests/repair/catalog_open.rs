use super::super::*;
use crate::session::catalog::CATALOG_SCHEMA_VERSION;

#[test]
fn catalog_commits_write_ahead_repair_obligations_with_full_durability() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let store = LocalSessionStore::open(config).expect("open catalog");

    assert_eq!(
        store
            .catalog_synchronous_level_for_test()
            .expect("read catalog durability mode"),
        2,
        "an acknowledged projection intent must survive power loss before JSONL publication"
    );
}

#[test]
fn locked_catalog_is_not_replaced_as_corrupt() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let store = LocalSessionStore::open(config.clone()).expect("create catalog");
    drop(store);

    let index_path = config.index_path();
    let lock = rusqlite::Connection::open(&index_path).expect("open lock connection");
    lock.pragma_update(None, "journal_mode", "DELETE")
        .expect("switch journal mode");
    lock.execute_batch(
        "BEGIN EXCLUSIVE;
             INSERT INTO repair_state (key, value) VALUES ('test_lock', 'held');",
    )
    .expect("hold exclusive catalog lock");
    let original = fs::read(&index_path).expect("read catalog after lock");

    let error = match LocalSessionStore::open(config) {
        Ok(_) => panic!("locked catalog must not open successfully"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        LocalStoreError::Catalog(CatalogError::Sqlite(
            rusqlite::Error::SqliteFailure(code, _)
        )) if matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    ));
    assert!(!index_path.with_extension("sqlite.corrupt").exists());
    assert!(
        fs::read(&index_path).expect("read locked catalog") == original,
        "catalog bytes changed while SQLite reported a transient lock"
    );

    lock.execute_batch("ROLLBACK")
        .expect("release catalog lock");
}

#[cfg(unix)]
#[test]
fn unreadable_catalog_is_not_replaced_as_corrupt() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let store = LocalSessionStore::open(config.clone()).expect("create catalog");
    drop(store);

    let index_path = config.index_path();
    let original = fs::read(&index_path).expect("read catalog");
    fs::set_permissions(&index_path, fs::Permissions::from_mode(0o000))
        .expect("make catalog unreadable");

    let open_result = LocalSessionStore::open(config);
    fs::set_permissions(&index_path, fs::Permissions::from_mode(0o600))
        .expect("restore catalog permissions");
    let error = match open_result {
        Ok(_) => panic!("unreadable catalog must not be replaced"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        LocalStoreError::Catalog(CatalogError::Sqlite(_))
            | LocalStoreError::Catalog(CatalogError::Io(_))
    ));
    assert!(!index_path.with_extension("sqlite.corrupt").exists());
    assert!(
        fs::read(&index_path).expect("read restored catalog") == original,
        "catalog bytes changed after an environmental access failure"
    );
}

#[test]
fn partial_sqlite_schema_is_treated_as_a_rebuildable_projection() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&id)
        .expect("summary")
        .expect("row")
        .jsonl_path;
    store.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(config.index_path()).expect("index");
    connection
        .execute_batch(
            "DROP TABLE message_nodes;
                 CREATE TABLE message_nodes (session_id TEXT PRIMARY KEY NOT NULL);",
        )
        .expect("partial schema");
    drop(connection);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(source.exists());
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair if needed")
            .is_some()
    );
    assert_eq!(
        reopened
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions
            .len(),
        1
    );
}

#[test]
fn missing_catalog_index_is_treated_as_a_schema_mismatch() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("source survives index rebuild")])
        .expect("append");
    store.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(config.index_path()).expect("index");
    connection
        .execute_batch("DROP INDEX message_nodes_search;")
        .expect("remove required index");
    drop(connection);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair schema mismatch")
            .is_some()
    );
    let summary = reopened
        .get_summary(&id)
        .expect("summary")
        .expect("rebuilt row");
    assert_eq!(
        summary.preview.as_deref(),
        Some("source survives index rebuild")
    );
}

#[test]
fn unique_catalog_index_is_treated_as_a_schema_mismatch() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("source survives malformed index")])
        .expect("append");
    store.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(config.index_path()).expect("index");
    connection
        .execute_batch(
            "DROP INDEX message_nodes_search;
             CREATE UNIQUE INDEX message_nodes_search
             ON message_nodes(session_id, searchable_folded);",
        )
        .expect("replace index with an incompatible unique constraint");
    drop(connection);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair incompatible index")
            .is_some(),
        "a current-version catalog must validate index uniqueness, not only its columns"
    );
    let summary = reopened
        .get_summary(&id)
        .expect("summary")
        .expect("rebuilt row");
    assert_eq!(
        summary.preview.as_deref(),
        Some("source survives malformed index")
    );
}

#[test]
fn partial_catalog_index_is_treated_as_a_schema_mismatch() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("source survives partial index")])
        .expect("append");
    store.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(config.index_path()).expect("index");
    connection
        .execute_batch(
            "DROP INDEX message_nodes_search;
             CREATE INDEX message_nodes_search
             ON message_nodes(session_id, searchable_folded)
             WHERE searchable_folded <> '';",
        )
        .expect("replace index with an incompatible partial predicate");
    drop(connection);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair incompatible index")
            .is_some(),
        "a current-version catalog must reject partial indexes that omit required rows"
    );
    let summary = reopened
        .get_summary(&id)
        .expect("summary")
        .expect("rebuilt row");
    assert_eq!(
        summary.preview.as_deref(),
        Some("source survives partial index")
    );
}

#[test]
fn unexpected_catalog_trigger_is_treated_as_a_schema_mismatch() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("source survives an unexpected trigger")])
        .expect("append");
    store.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(config.index_path()).expect("index");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_session_projection
             BEFORE INSERT ON sessions
             BEGIN
                 SELECT RAISE(ABORT, 'unexpected trigger ran');
             END;",
        )
        .expect("install incompatible trigger");
    drop(connection);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair incompatible trigger")
            .is_some(),
        "a current-version catalog must reject executable schema objects it did not create"
    );
    let summary = reopened
        .get_summary(&id)
        .expect("summary")
        .expect("rebuilt row");
    assert_eq!(
        summary.preview.as_deref(),
        Some("source survives an unexpected trigger")
    );
}

#[test]
fn unexpected_catalog_table_constraint_is_treated_as_a_schema_mismatch() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    store.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(config.index_path()).expect("index");
    connection
        .execute_batch(
            "ALTER TABLE repair_state RENAME TO repair_state_old;
             CREATE TABLE repair_state (
                 key TEXT PRIMARY KEY NOT NULL,
                 value TEXT NOT NULL,
                 UNIQUE(value)
             );
             INSERT INTO repair_state (key, value)
                 SELECT key, value FROM repair_state_old;
             DROP TABLE repair_state_old;",
        )
        .expect("install incompatible table constraint");
    drop(connection);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair incompatible table constraint")
            .is_some(),
        "a current-version catalog must reject hidden UNIQUE/CHECK constraints, not only column metadata"
    );
}

#[test]
fn replacement_initialization_reports_its_actual_environmental_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let store = LocalSessionStore::open(config.clone()).expect("create catalog");
    drop(store);

    let index_path = config.index_path();
    let connection = rusqlite::Connection::open(&index_path).expect("open catalog");
    connection
        .pragma_update(None, "user_version", CATALOG_SCHEMA_VERSION + 1)
        .expect("make schema unsupported");
    drop(connection);
    Catalog::fail_replacement_initialization_for_test(index_path.clone());

    let error = match LocalSessionStore::open(config) {
        Ok(_) => panic!("replacement initialization fault must fail open"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        LocalStoreError::Catalog(CatalogError::Io(ref source))
            if source.to_string().contains("injected replacement catalog initialization failure")
    ));
    assert!(
        index_path.with_extension("sqlite.repair-required").exists(),
        "replacement failure must retain the durable repair obligation"
    );
}
