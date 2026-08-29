use super::*;

fn assert_unsafe_source_error(error: &SessionError) {
    let SessionError::Io { source } = error else {
        panic!("expected an I/O boundary error, got {error:?}");
    };
    assert!(
        matches!(
            source
                .get_ref()
                .and_then(|error| error.downcast_ref::<LocalStoreError>()),
            Some(LocalStoreError::UnsafeSourcePath(_))
        ),
        "expected the typed unsafe-source error to survive the store trait boundary: {error:?}"
    );
}

#[cfg(unix)]
#[test]
fn repair_never_retries_a_pending_batch_through_a_replaced_source_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("pending secret")])
            .is_err()
    );
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move source outside boundary");
    symlink(&outside_source, &source).expect("replace source with symlink");
    let outside_before = fs::read(&outside_source).expect("read outside source");

    assert!(
        store.repair().is_err(),
        "repair must refuse an exact retry after source authority is lost"
    );
    assert_eq!(
        fs::read(&outside_source).expect("read outside source after repair"),
        outside_before,
        "the pending batch escaped through a source symlink"
    );
}

#[cfg(unix)]
#[test]
fn dropping_a_store_never_retries_pending_data_after_source_authority_is_lost() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("pending secret")])
            .is_err()
    );
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move source outside boundary");
    symlink(&outside_source, &source).expect("replace source with symlink");
    let outside_before = fs::read(&outside_source).expect("read outside source");

    drop(store);

    assert_eq!(
        fs::read(&outside_source).expect("read outside source after store drop"),
        outside_before,
        "recorder Drop followed a replacement symlink after the store lost authority"
    );
}

#[test]
fn dropping_a_store_never_retries_pending_data_into_a_replaced_regular_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let detached = tempfile::tempdir().expect("detached tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("pending secret")])
            .is_err()
    );
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let detached_source = detached.path().join("session.jsonl");
    fs::rename(&source, &detached_source).expect("detach original source inode");
    fs::copy(&detached_source, &source).expect("install replacement regular source");
    let replacement_before = fs::read(&source).expect("read replacement source");

    drop(store);

    assert_eq!(
        fs::read(&source).expect("read replacement source after store drop"),
        replacement_before,
        "recorder Drop retried an old pending batch into a replacement inode"
    );
}

#[test]
fn repair_never_retries_pending_data_into_a_replaced_regular_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let detached = tempfile::tempdir().expect("detached tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("pending secret")])
            .is_err()
    );
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let detached_source = detached.path().join("session.jsonl");
    fs::rename(&source, &detached_source).expect("detach original source inode");
    fs::copy(&detached_source, &source).expect("install replacement regular source");
    let replacement_before = fs::read(&source).expect("read replacement source");

    assert!(
        store.repair().is_err(),
        "repair must reject a pending retry after the source inode is replaced"
    );
    assert_eq!(
        fs::read(&source).expect("read replacement source after repair"),
        replacement_before,
        "repair retried an old pending batch into a replacement inode"
    );
}

#[test]
fn append_never_retries_pending_data_into_a_replaced_regular_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let detached = tempfile::tempdir().expect("detached tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("pending secret")])
            .is_err()
    );
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let detached_source = detached.path().join("session.jsonl");
    fs::rename(&source, &detached_source).expect("detach original source inode");
    fs::copy(&detached_source, &source).expect("install replacement regular source");
    let replacement_before = fs::read(&source).expect("read replacement source");

    let error = store
        .append(&session_id, vec![message("must not follow replacement")])
        .expect_err("append must reject a retained handle whose source inode changed");
    assert_unsafe_source_error(&error);
    assert_eq!(
        fs::read(&source).expect("read replacement source after rejected append"),
        replacement_before,
        "append retried an old pending batch into a replacement inode"
    );
}
