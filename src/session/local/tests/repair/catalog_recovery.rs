use super::super::*;

#[test]
fn corrupt_catalog_is_rebuilt_without_losing_jsonl_sources() {
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
    fs::write(config.index_path(), b"not a sqlite database").expect("corrupt index");
    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions
            .is_empty()
    );
    assert!(source.exists());
    assert!(reopened.repair().expect("repair").rebuilt == 1);
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
fn catalog_repair_requirement_survives_reopen_until_repair_completes() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&id, vec![message("recover me after another restart")])
        .expect("append");
    store.shutdown().expect("shutdown");

    fs::write(config.index_path(), b"not a sqlite database").expect("corrupt index");
    let replaced = LocalSessionStore::open(config.clone()).expect("replace projection");
    assert!(
        replaced
            .list(CatalogQuery::first_page())
            .expect("empty replacement")
            .sessions
            .is_empty()
    );
    drop(replaced);

    let mut reopened = LocalSessionStore::open(config).expect("reopen replacement");
    assert!(
        reopened
            .repair_if_needed()
            .expect("resume durable repair")
            .is_some()
    );
    assert_eq!(
        reopened
            .get_summary(&id)
            .expect("summary")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("recover me after another restart")
    );
    drop(reopened);
}

#[test]
fn a_missing_catalog_requires_automatic_source_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("recover missing catalog")])
        .expect("create durable source");
    store.shutdown().expect("shutdown");
    drop(store);

    let index = config.index_path();
    let _ = fs::remove_file(index.with_extension("sqlite-wal"));
    let _ = fs::remove_file(index.with_extension("sqlite-shm"));
    fs::remove_file(&index).expect("remove disposable catalog");

    let mut reopened = LocalSessionStore::open(config).expect("reopen missing catalog");
    assert!(
        reopened
            .repair_if_needed()
            .expect("rebuild from authoritative sources")
            .is_some(),
        "a newly created empty index must not hide existing JSONL sessions"
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("projection restored")
            .preview
            .as_deref(),
        Some("recover missing catalog")
    );
}

#[cfg(unix)]
#[test]
fn catalog_replacement_during_a_live_store_never_hides_later_source_facts() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut original = LocalSessionStore::open(config.clone()).expect("open original store");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    original.create_session(header).expect("create");
    original
        .append(&session_id, vec![message("before replacement")])
        .expect("append baseline");

    let index = config.index_path();
    for path in [
        index.clone(),
        index.with_extension("sqlite-wal"),
        index.with_extension("sqlite-shm"),
    ] {
        if path.exists() {
            let backup = path.with_extension(format!(
                "{}.stale",
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("sqlite")
            ));
            fs::rename(&path, backup).expect("move live catalog inode aside");
        }
    }

    let mut replacement = LocalSessionStore::open(config.clone()).expect("open replacement");
    replacement
        .repair_if_needed()
        .expect("repair replacement catalog")
        .expect("missing catalog requires repair");
    drop(replacement);

    let _ = original.append(&session_id, vec![message("after replacement")]);
    drop(original);

    let mut reopened = LocalSessionStore::open(config).expect("reopen current catalog");
    let _ = reopened
        .repair_if_needed()
        .expect("check retained recovery obligation");
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("session remains discoverable")
            .preview
            .as_deref(),
        Some("after replacement"),
        "a live stale SQLite connection must not acknowledge a newer JSONL fact only in an unlinked catalog"
    );
    for query in ["before replacement", "after replacement"] {
        assert_eq!(
            reopened
                .search_chat_messages(ChatMessageSearchQuery::new(query))
                .expect("search rebuilt message projection")
                .messages
                .len(),
            1,
            "catalog replacement recovery must rebuild the complete active message projection"
        );
    }
}

#[test]
fn interrupted_catalog_replacement_still_requires_source_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("recover after replacement crash")])
        .expect("create durable source");
    store.shutdown().expect("shutdown");
    drop(store);

    fs::write(config.index_path(), b"not a sqlite database").expect("corrupt index");
    Catalog::interrupt_replacement_after_backup_for_test(config.index_path());
    assert!(
        LocalSessionStore::open(config.clone()).is_err(),
        "fault must stop replacement before its SQLite repair marker"
    );

    let mut reopened = LocalSessionStore::open(config).expect("reopen empty replacement");
    assert!(
        reopened
            .repair_if_needed()
            .expect("resume catalog reconstruction")
            .is_some(),
        "a missing index beside a replacement backup must retain the repair obligation"
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("source projection restored")
            .preview
            .as_deref(),
        Some("recover after replacement crash")
    );
}

#[test]
fn initialized_empty_catalog_keeps_the_external_repair_marker_across_a_crash() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.shutdown().expect("shutdown");

    fs::write(config.index_path(), []).expect("truncate catalog to zero bytes");
    Catalog::interrupt_initialize_after_open_for_test(config.index_path());
    assert!(
        LocalSessionStore::open(config.clone()).is_err(),
        "the injected crash must abort before the in-database marker is written"
    );

    let mut reopened = LocalSessionStore::open(config).expect("reopen after crash");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair marker check")
            .is_some(),
        "the external marker must force source reconciliation"
    );
    assert!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .is_some(),
        "the intact JSONL source must be rebuilt after the crash"
    );
}

#[test]
fn repair_if_needed_returns_an_aggregate_issue_report() {
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
    store.shutdown().expect("shutdown");
    fs::OpenOptions::new()
        .append(true)
        .open(source)
        .expect("open source")
        .write_all(b"{not-json}\n")
        .expect("append corrupt record");
    fs::write(config.index_path(), b"not a sqlite database").expect("replace catalog");

    let mut reopened = LocalSessionStore::open(config).expect("open replacement catalog");
    let report = reopened
        .repair_if_needed()
        .expect("repair attempt")
        .expect("replacement catalog requires repair");

    assert_eq!(report.scanned, 1);
    assert_eq!(report.rebuilt, 0);
    assert!(!report.issues.is_empty());
}

#[test]
fn repair_does_not_delete_a_session_it_rebuilt_from_the_authoritative_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let created_at = header.created_at;
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("authoritative source")])
        .expect("append");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("close handle");

    let tampered_created_at = created_at.saturating_add(1);
    let tampered_path = config
        .sessions_root()
        .join(format!("{tampered_created_at}_{session_id}.jsonl"));
    let catalog = rusqlite::Connection::open(store.catalog_path()).expect("catalog");
    catalog
        .execute(
            "UPDATE sessions SET created_at = ?1, jsonl_path = ?2 WHERE session_id = ?3",
            rusqlite::params![
                tampered_created_at,
                tampered_path.to_string_lossy(),
                session_id.to_string()
            ],
        )
        .expect("tamper disposable projection identity fields");
    drop(catalog);

    let report = store.repair().expect("repair");

    assert_eq!(report.rebuilt, 1);
    assert_eq!(report.removed, 0);
    let summary = store
        .get_summary(&session_id)
        .expect("summary after repair")
        .expect("rebuilt row must remain visible");
    assert_eq!(summary.created_at, created_at);
    assert_eq!(summary.jsonl_path, source);
    assert_eq!(
        store
            .load_session(&session_id, None)
            .expect("restore repaired source")
            .messages
            .len(),
        1
    );
}

#[test]
fn repair_preserves_and_retries_a_corrupt_authoritative_source_despite_catalog_identity_drift() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let created_at = header.created_at;
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("authoritative message")])
        .expect("append");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("shutdown");
    let valid_len = fs::metadata(&source).expect("source metadata").len();
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .expect("open source")
        .write_all(b"{not-json}\n")
        .expect("append corrupt record");

    let fake_created_at = created_at.saturating_add(1);
    let fake_path = config
        .sessions_root()
        .join(format!("{fake_created_at}_{session_id}.jsonl"));
    let catalog = rusqlite::Connection::open(store.catalog_path()).expect("catalog");
    catalog
        .execute(
            "UPDATE sessions SET created_at = ?1, jsonl_path = ?2 WHERE session_id = ?3",
            rusqlite::params![
                fake_created_at,
                fake_path.to_string_lossy(),
                session_id.to_string()
            ],
        )
        .expect("tamper disposable identity fields");
    drop(catalog);

    let report = store.repair().expect("repair valid prefix");
    assert_eq!(report.removed, 0);
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after incomplete repair")
            .is_some(),
        "an observed authoritative source must preserve its last trusted row"
    );

    fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .expect("open source for repair")
        .set_len(valid_len)
        .expect("remove corrupt suffix");
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("retry repaired source")
            .is_some(),
        "an unresolved canonical source needs a per-session repair obligation"
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("recovered projection")
            .preview
            .as_deref(),
        Some("authoritative message")
    );
}

#[test]
fn repair_does_not_choose_between_duplicate_canonical_sources() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let original_created_at = header.created_at;
    store
        .create_session_with_entries(header, vec![message("trusted catalog row")])
        .expect("create original source");
    store.shutdown().expect("close original recorder");

    let mut duplicate_header = SessionHeader::new(SessionDomain::Chat, None);
    duplicate_header.session_id = session_id.clone();
    duplicate_header.created_at = duplicate_header
        .created_at
        .max(original_created_at.saturating_add(1));
    let duplicate = config.sessions_root().join(format!(
        "{}_{}.jsonl",
        duplicate_header.created_at, session_id
    ));
    JsonlWriter::create(&duplicate, duplicate_header)
        .expect("create second canonical source")
        .append(message("must not win by scan order"))
        .expect("append duplicate message");

    let report = store.repair().expect("repair reports ambiguity");

    assert_eq!(report.rebuilt, 0);
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("multiple canonical source files"))
    );
    assert_eq!(
        store
            .get_summary(&session_id)
            .expect("summary")
            .expect("last trusted row is preserved")
            .preview
            .as_deref(),
        Some("trusted catalog row")
    );
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("retry unresolved ambiguity")
            .is_some(),
        "an ambiguous identity must retain a precise repair obligation"
    );
}
