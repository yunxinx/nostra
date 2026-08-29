use super::super::*;

#[test]
fn repair_reindexes_external_append_and_removes_missing_sources() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let path = store
        .get_summary(&id)
        .expect("summary")
        .expect("row")
        .jsonl_path;
    JsonlWriter::open(&path)
        .expect("writer")
        .append(message("external append"))
        .expect("append");
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .preview,
        None
    );
    let report = store.repair().expect("repair");
    assert_eq!(report.rebuilt, 1);
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .preview
            .as_deref(),
        Some("external append")
    );
    fs::remove_file(path).expect("remove source");
    let report = store.repair().expect("repair stale");
    assert_eq!(report.removed, 1);
    assert!(store.get_summary(&id).expect("summary").is_none());
}

#[test]
fn repair_does_not_treat_an_untrusted_outside_path_as_missing_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("authoritative in-root source")])
        .expect("append");
    store.shutdown().expect("shutdown handles");

    let outside_missing = root.path().join("outside-missing.jsonl");
    let catalog = rusqlite::Connection::open(store.catalog_path()).expect("catalog");
    catalog
        .execute(
            "UPDATE sessions SET jsonl_path = ?1 WHERE session_id = ?2",
            rusqlite::params![outside_missing.to_string_lossy(), id.to_string()],
        )
        .expect("tamper source path");
    drop(catalog);

    let report = store.repair().expect("repair");
    assert!(!report.issues.is_empty());
    assert_eq!(report.removed, 0);
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("valid source is reindexed")
            .preview
            .as_deref(),
        Some("authoritative in-root source")
    );
}

#[test]
fn repair_does_not_clear_a_missing_source_before_its_parent_is_durable() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("close handle");
    fs::remove_file(&source).expect("simulate an interrupted source unlink");

    store.fail_next_directory_sync_for_test(source.parent().expect("source parent").to_path_buf());
    assert!(
        store.repair().is_err(),
        "repair must not clear the row until the missing directory entry is durable"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after failed repair")
            .is_some(),
        "a failed directory barrier must preserve the catalog recovery obligation"
    );

    let report = store.repair().expect("retry repair after directory sync");
    assert_eq!(report.removed, 1);
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after successful repair")
            .is_none()
    );
}

#[test]
fn repair_apply_failure_rolls_back_every_catalog_change() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let mut sessions = Vec::new();
    for label in ["first", "second"] {
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        store
            .append(&id, vec![message(&format!("old {label}"))])
            .expect("append old projection");
        let path = store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .jsonl_path;
        sessions.push((id, path, format!("old {label}")));
    }
    store.shutdown().expect("shutdown handles");
    for (_, path, old_preview) in &sessions {
        JsonlWriter::open(path)
            .expect("external writer")
            .append(message(&format!("new after {old_preview}")))
            .expect("external append");
    }

    let fault = rusqlite::Connection::open(store.catalog_path()).expect("fault connection");
    fault
        .execute_batch(
            "CREATE TABLE repair_fault_counter (attempts INTEGER NOT NULL);
                 INSERT INTO repair_fault_counter VALUES (0);
                 CREATE TRIGGER fail_second_repair_projection
                 BEFORE UPDATE ON sessions
                 BEGIN
                    UPDATE repair_fault_counter SET attempts = attempts + 1;
                    SELECT CASE WHEN (SELECT attempts FROM repair_fault_counter) = 2
                        THEN RAISE(ABORT, 'injected repair failure') END;
                 END;",
        )
        .expect("install repair fault");
    drop(fault);

    assert!(store.repair().is_err());
    for (id, _, old_preview) in sessions {
        assert_eq!(
            store
                .get_summary(&id)
                .expect("summary after failed repair")
                .expect("existing row is preserved")
                .preview
                .as_deref(),
            Some(old_preview.as_str())
        );
    }
}

#[test]
fn repair_reports_corrupt_jsonl_without_blocking_other_sessions() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let first = SessionHeader::new(SessionDomain::Chat, None);
    let first_id = first.session_id.clone();
    store.create_session(first).expect("first create");
    let second = SessionHeader::new(SessionDomain::Chat, None);
    let second_id = second.session_id.clone();
    store.create_session(second).expect("second create");
    let first_path = store
        .get_summary(&first_id)
        .expect("summary")
        .expect("first row")
        .jsonl_path;
    fs::OpenOptions::new()
        .append(true)
        .open(first_path)
        .expect("open source")
        .write_all(b"{not-json}\n")
        .expect("append corrupt line");

    let report = store.repair().expect("repair");
    assert_eq!(report.rebuilt, 1);
    assert!(!report.issues.is_empty());
    assert!(
        store
            .get_summary(&first_id)
            .expect("first summary")
            .is_some()
    );
    assert!(
        store
            .get_summary(&second_id)
            .expect("second summary")
            .is_some()
    );
}

#[test]
fn repair_preserves_catalog_row_for_an_existing_corrupt_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("last trusted preview")])
        .expect("append trusted message");
    let path = store
        .get_summary(&id)
        .expect("summary")
        .expect("row")
        .jsonl_path;
    store.shutdown().expect("shutdown handle");

    JsonlWriter::open(&path)
        .expect("external writer")
        .append(message("untrusted partial repair preview"))
        .expect("append valid prefix");
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open source")
        .write_all(b"{not-json}\n")
        .expect("append corrupt fact");

    let report = store.repair().expect("repair with issue");
    assert!(!report.issues.is_empty());
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary after repair")
            .expect("existing row is retained")
            .preview
            .as_deref(),
        Some("last trusted preview")
    );
}

#[test]
fn repair_retries_a_canonical_source_whose_header_is_temporarily_unreadable() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("shutdown writer");
    drop(store);

    std::fs::write(&source, b"{not-json}\n").expect("replace source with unreadable header");
    let catalog = rusqlite::Connection::open(config.index_path()).expect("catalog");
    catalog
        .execute(
            "INSERT INTO repair_state (key, value) VALUES ('repair_required', 'test')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .expect("arm generic repair marker");
    drop(catalog);

    let mut first = LocalSessionStore::open(config.clone()).expect("first reopen");
    let report = first
        .repair_if_needed()
        .expect("first repair")
        .expect("generic repair marker");
    assert!(!report.issues.is_empty());
    drop(first);

    let mut second = LocalSessionStore::open(config).expect("second reopen");
    assert!(
        second
            .repair_if_needed()
            .expect("second repair check")
            .is_some(),
        "the canonical source identity must retain a precise retry obligation"
    );
}

#[cfg(unix)]
#[test]
fn repair_does_not_follow_symlinked_session_sources() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let external_path = root.path().join("outside-session.jsonl");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let external_id = header.session_id.clone();
    JsonlWriter::create(&external_path, header)
        .expect("external source")
        .append(message("must remain outside the catalog"))
        .expect("external message");
    let linked_path = config.sessions_root().join("linked.jsonl");
    symlink(&external_path, &linked_path).expect("link external source");

    let report = store.repair().expect("repair");
    assert_eq!(report.scanned, 0);
    assert!(store.get_summary(&external_id).expect("summary").is_none());
    assert!(external_path.exists());
}
