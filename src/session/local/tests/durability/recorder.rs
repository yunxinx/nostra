use super::super::*;

#[test]
fn permanent_append_error_does_not_poison_later_writes() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");

    let invalid = SessionEntryKind::Header(SessionHeader::new(SessionDomain::Chat, None));
    assert!(matches!(
        store.append(&id, vec![invalid]),
        Err(SessionError::InvalidEntryKind)
    ));

    let ids = store
        .append(&id, vec![message("valid after rejection")])
        .expect("valid append");
    store.flush().expect("flush");
    let resolved = store.load_session(&id, None).expect("load");
    assert_eq!(ids.len(), 1);
    assert_eq!(resolved.messages.len(), 1);
    assert_eq!(resolved.messages[0].entry_id, ids[0]);
}

#[test]
fn create_with_entries_does_not_publish_a_header_only_session() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    let invalid = SessionEntryKind::Header(SessionHeader::new(SessionDomain::Chat, None));

    assert!(matches!(
        store.create_session_with_entries(header, vec![invalid]),
        Err(SessionError::InvalidEntryKind)
    ));
    assert!(store.get_summary(&id).expect("summary").is_none());
    assert!(
        collect_jsonl_paths(&config.sessions_root())
            .expect("scan sessions")
            .is_empty()
    );
}

#[test]
fn ambiguous_append_retry_preserves_the_exact_durable_entry() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");

    store.fail_next_append_after_write_for_test(&id);
    assert!(
        store
            .append(&id, vec![message("one durable fact")])
            .is_err()
    );
    let committed = store.load_session(&id, None).expect("load committed fact");
    assert_eq!(committed.messages.len(), 1);
    let original_id = committed.messages[0].entry_id.clone();

    store.flush().expect("reconcile ambiguous result");
    let recovered = store.load_session(&id, None).expect("load recovered fact");
    assert_eq!(recovered.messages.len(), 1);
    assert_eq!(recovered.messages[0].entry_id, original_id);
}

#[test]
fn exact_retry_rejects_same_id_with_different_content() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");

    store.fail_next_append_after_write_for_test(&id);
    assert!(store.append(&id, vec![message("original")]).is_err());
    let summary = store
        .get_summary(&id)
        .expect("summary")
        .expect("catalog row");
    let mut entries = JsonlLoader::load(&summary.jsonl_path)
        .expect("load source")
        .entries;
    let exact_id = entries[1].id.clone();
    entries[1].kind = message("conflicting replacement");
    let mut source = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&summary.jsonl_path)
        .expect("rewrite source");
    for entry in &entries {
        serde_json::to_writer(&mut source, entry).expect("serialize entry");
        source.write_all(b"\n").expect("newline");
    }
    source.sync_all().expect("sync source");

    let error = store.flush().expect_err("conflicting retry must fail");
    assert!(
        matches!(
            error,
            SessionError::ExactBatchConflict(ref entry_id) if entry_id == &exact_id
        ),
        "unexpected retry error: {error:?}"
    );
    assert_eq!(
        JsonlLoader::load(&summary.jsonl_path)
            .expect("source remains readable")
            .entries
            .len(),
        2
    );
}

#[test]
fn ordinary_restore_and_append_reject_complete_corrupt_lines() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("trusted history")])
        .expect("append trusted message");
    let source = store
        .get_summary(&id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("shutdown writer");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .expect("open source")
        .write_all(b"{not-json}\n")
        .expect("append complete corrupt line");

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened.load_session(&id, None).is_err(),
        "ordinary restore must not silently omit a complete corrupt fact"
    );
    let before_append = std::fs::read(&source).expect("read corrupt source");
    assert!(
        reopened
            .append(&id, vec![message("must not cross corruption")])
            .is_err(),
        "mutation must refuse a source whose authoritative history is corrupt"
    );
    assert_eq!(
        std::fs::read(&source).expect("read source after rejected append"),
        before_append,
        "a rejected append must not write beyond the corrupt line"
    );
}

#[test]
fn flush_retries_pending_entries_and_is_idempotent() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&id);
    assert!(store.append(&id, vec![message("pending")]).is_err());
    store.flush().expect("retry pending");
    store.flush().expect("second flush");
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .preview
            .as_deref(),
        Some("pending")
    );
    store.shutdown().expect("shutdown");
    assert_eq!(store.open_handle_count(), 0);
}

#[test]
fn flush_attempts_every_pending_session_after_a_projection_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let mut session_ids = Vec::new();
    for text in ["first pending", "second pending"] {
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create");
        store.fail_next_append_for_test(&session_id);
        assert!(store.append(&session_id, vec![message(text)]).is_err());
        session_ids.push(session_id);
    }

    let fault = rusqlite::Connection::open(store.catalog_path()).expect("fault connection");
    fault
        .execute_batch(
            "CREATE TRIGGER reject_flush_projection
                 BEFORE UPDATE ON sessions
                 BEGIN
                    SELECT RAISE(ABORT, 'injected projection failure');
                 END;",
        )
        .expect("install projection fault");

    let error = store
        .flush()
        .expect_err("flush should report projection failures");
    let rendered = error.to_string();
    for session_id in session_ids {
        assert!(
            rendered.contains(&session_id.to_string()),
            "aggregate error omitted failed session `{session_id}`: {rendered}"
        );
        assert_eq!(
            store
                .load_session(&session_id, None)
                .expect("pending source was attempted")
                .messages
                .len(),
            1
        );
    }
}

#[test]
fn append_after_failure_persists_pending_and_current_batches_but_returns_current_ids() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&id);
    assert!(store.append(&id, vec![message("pending")]).is_err());

    let returned = store
        .append(&id, vec![message("current")])
        .expect("retry pending before current");
    assert_eq!(returned.len(), 1);
    let resolved = store.load_session(&id, None).expect("load");
    assert_eq!(resolved.messages.len(), 2);
    assert_eq!(returned[0], resolved.messages[1].entry_id);
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .preview
            .as_deref(),
        Some("current")
    );
}

#[test]
fn append_failure_reconciles_a_pending_batch_committed_before_the_current_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");

    store.fail_next_append_for_test(&id);
    assert!(store.append(&id, vec![message("first")]).is_err());
    store.fail_next_append_for_test(&id);
    assert!(store.append(&id, vec![message("second")]).is_err());

    let returned = store
        .append(&id, vec![message("third")])
        .expect("retry second before third");
    let resolved = store.load_session(&id, None).expect("load");
    assert_eq!(resolved.messages.len(), 3);
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0], resolved.messages[2].entry_id);
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .preview
            .as_deref(),
        Some("third")
    );
}

#[test]
fn incremental_projection_deduplicates_usage_across_batches() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message_with_metadata("first", None, 7)])
        .expect("message usage");
    store
        .append(
            &id,
            vec![SessionEntryKind::TurnResult(TurnResult {
                turn_id: Some("turn-1".into()),
                status: TurnStatus::Completed,
                finish_reason: None,
                error: None,
                usage: Usage {
                    total_tokens: 11,
                    ..Usage::default()
                },
            })],
        )
        .expect("terminal usage");
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .total_tokens,
        11
    );
}

fn exercise_store_contract(store: &mut dyn SessionStore) {
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let ids = store
        .append(&id, vec![message("first"), message("second")])
        .expect("append");
    assert_eq!(
        store.load_session(&id, None).expect("load").messages.len(),
        2
    );
    store.set_leaf(&id, Some(&ids[0])).expect("set leaf");
    assert_eq!(
        store.load_session(&id, None).expect("load").messages.len(),
        1
    );
    store.flush().expect("flush");
    store.shutdown().expect("shutdown");
}

#[test]
fn memory_and_local_stores_share_the_lifecycle_contract() {
    let mut memory = InMemorySessionStore::new();
    exercise_store_contract(&mut memory);

    let root = tempfile::tempdir().expect("tempdir");
    let mut local =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    exercise_store_contract(&mut local);
}

#[test]
fn concurrent_flushes_keep_the_catalog_and_source_readable() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut seed = LocalSessionStore::open(config.clone()).expect("seed open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    seed.create_session(header).expect("create");
    seed.append(&id, vec![message("concurrent")])
        .expect("append");
    seed.shutdown().expect("seed shutdown");

    let mut left = LocalSessionStore::open(config.clone()).expect("left open");
    let mut right = LocalSessionStore::open(config.clone()).expect("right open");
    left.ensure_handle(&id).expect("left handle");
    right.ensure_handle(&id).expect("right handle");
    std::thread::scope(|scope| {
        let left_flush = scope.spawn(|| left.flush());
        let right_flush = scope.spawn(|| right.flush());
        assert!(left_flush.join().expect("left thread").is_ok());
        assert!(right_flush.join().expect("right thread").is_ok());
    });

    let reopened = LocalSessionStore::open(config).expect("reopen");
    assert_eq!(
        reopened
            .load_session(&id, None)
            .expect("load")
            .messages
            .len(),
        1
    );
}
