use super::super::*;

#[test]
fn store_open_requires_the_initial_directory_chain_to_be_durable() {
    let root = tempfile::tempdir().expect("tempdir");
    let data_root = root.path().join("fresh").join("nostra");
    let config = LocalStoreConfig::new(&data_root, SessionDomain::Chat);

    super::super::source::fail_next_directory_sync_for_test(data_root.clone());
    assert!(
        LocalSessionStore::open(config.clone()).is_err(),
        "opening must not succeed before the new domain directory is durable"
    );
    assert!(
        !config.sessions_root().exists(),
        "directory creation continued after its parent durability barrier failed"
    );
    assert!(
        !config.index_path().exists(),
        "the disposable catalog was created below an undurable directory chain"
    );

    LocalSessionStore::open(config).expect("retry completes the directory barriers");
}

#[test]
fn agent_create_does_not_publish_before_the_project_bucket_is_durable() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let project = ProjectIdentity::new(root.path().join("project"), "Project");
    let header = SessionHeader::new(SessionDomain::Agent, Some(project));
    let session_id = header.session_id.clone();

    store.fail_next_directory_sync_for_test(config.sessions_root());
    assert!(
        store
            .create_session_with_entries(header, vec![message("must not publish")])
            .is_err(),
        "bucket durability failure must be visible to the caller"
    );
    assert!(store.get_summary(&session_id).expect("summary").is_none());
    assert!(
        collect_jsonl_paths(&config.sessions_root())
            .expect("scan sessions")
            .is_empty(),
        "no fact source may be published below an unsynced project bucket"
    );
}

#[test]
fn delete_keeps_the_catalog_obligation_when_parent_sync_fails() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let parent = source.parent().expect("source parent").to_path_buf();

    store.fail_next_directory_sync_for_test(parent);
    assert!(
        store.delete_session(&session_id).is_err(),
        "an unlink without a durable directory barrier is not a successful delete"
    );
    assert!(!source.exists());
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after failed delete")
            .is_some(),
        "catalog state must retain the recovery obligation"
    );

    store.fail_next_directory_sync_for_test(
        source
            .parent()
            .expect("source parent remains available")
            .to_path_buf(),
    );
    assert!(
        store.delete_session(&session_id).is_err(),
        "a retry must re-establish the directory durability barrier even when the file is already absent"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after retry barrier failure")
            .is_some(),
        "the catalog row cannot be cleared before a retry durably confirms the prior unlink"
    );
}

#[test]
fn delete_retry_syncs_a_confirmed_missing_source_before_clearing_the_catalog() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let parent = source.parent().expect("source parent").to_path_buf();

    store.fail_next_directory_sync_for_test(parent.clone());
    assert!(store.delete_session(&session_id).is_err());
    assert!(!source.exists());

    store.fail_next_directory_sync_for_test(parent);
    assert!(
        store.delete_session(&session_id).is_err(),
        "a retry must durably confirm the already-missing namespace entry"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after failed retry")
            .is_some()
    );

    store
        .delete_session(&session_id)
        .expect("final retry completes deletion");
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after delete")
            .is_none()
    );
}

#[test]
fn delete_rejects_catalog_paths_outside_the_session_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.shutdown().expect("close active handle");

    let external_path = root.path().join("unrelated-user-file.txt");
    fs::write(&external_path, b"must survive").expect("external file");
    let catalog = rusqlite::Connection::open(store.catalog_path()).expect("catalog");
    catalog
        .execute(
            "UPDATE sessions SET jsonl_path = ?1 WHERE session_id = ?2",
            rusqlite::params![external_path.to_string_lossy(), id.to_string()],
        )
        .expect("tamper catalog path");
    drop(catalog);

    assert!(store.delete_session(&id).is_err());
    assert_eq!(
        fs::read(&external_path).expect("external file survives"),
        b"must survive"
    );
    assert!(store.get_summary(&id).expect("summary").is_some());
}

#[test]
fn read_and_append_ignore_untrusted_catalog_source_paths() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let entry_id = store
        .append(&id, vec![message("authorized source")])
        .expect("append")
        .into_iter()
        .next()
        .expect("message entry");
    let authorized_path = store
        .get_summary(&id)
        .expect("summary")
        .expect("row")
        .jsonl_path;
    store.shutdown().expect("close active handle");

    let outside_path = root.path().join("outside-session.jsonl");
    fs::copy(&authorized_path, &outside_path).expect("copy source outside domain root");
    fs::remove_file(&authorized_path).expect("remove authorized source");
    let before = fs::read(&outside_path).expect("outside source before access");
    let catalog = rusqlite::Connection::open(store.catalog_path()).expect("catalog");
    catalog
        .execute(
            "UPDATE sessions SET jsonl_path = ?1 WHERE session_id = ?2",
            rusqlite::params![outside_path.to_string_lossy(), id.to_string()],
        )
        .expect("tamper catalog path");
    drop(catalog);

    assert!(store.load_session(&id, None).is_err());
    assert!(store.append(&id, vec![message("must not escape")]).is_err());
    let reference = ChatMessageRef::new(id, entry_id).expect("Chat reference");
    assert!(matches!(
        store.read_chat_message(&reference),
        Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
            reason: ChatMessageUnavailableReason::SessionDeleted,
            ..
        }))
    ));
    assert_eq!(
        fs::read(&outside_path).expect("outside source after rejected access"),
        before
    );
}
