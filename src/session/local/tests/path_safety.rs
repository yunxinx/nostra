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
fn opening_a_store_rejects_a_symlinked_sessions_root() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    fs::create_dir_all(config.storage_root()).expect("create domain root");
    symlink(outside.path(), config.sessions_root()).expect("link sessions root outside");

    let error = match LocalSessionStore::open(config) {
        Ok(_) => panic!("the configured sessions root must be a real directory"),
        Err(error) => error,
    };

    assert!(matches!(error, LocalStoreError::UnsafeSourcePath(_)));
    assert!(
        fs::read_dir(outside.path())
            .expect("outside directory")
            .next()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn opening_a_store_rejects_a_symlinked_catalog_file() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("create catalog");
    store.shutdown().expect("close catalog and recorders");
    drop(store);

    let index_path = config.index_path();
    let outside_index = outside.path().join("outside.sqlite");
    fs::rename(&index_path, &outside_index).expect("move catalog outside store boundary");
    symlink(&outside_index, &index_path).expect("replace catalog with symlink");
    let outside_before = fs::read(&outside_index).expect("read outside catalog");

    let error = match LocalSessionStore::open(config) {
        Ok(_) => panic!("catalog open must not follow a final symlink into an unrelated file"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        LocalStoreError::Catalog(CatalogError::UnsafePath(_))
    ));
    assert_eq!(
        fs::read(&outside_index).expect("read outside catalog after rejected open"),
        outside_before,
        "rejected catalog access changed the external database"
    );
}

#[cfg(unix)]
#[test]
fn deleting_rejects_a_sessions_root_reached_through_a_replaced_symlinked_ancestor() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("must remain inside Nostra")])
        .expect("append");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let file_name = source.file_name().expect("source file name").to_owned();
    store.shutdown().expect("close recorder");

    let detached_storage = root.path().join("detached-chat-storage");
    fs::rename(config.storage_root(), &detached_storage).expect("detach authorized storage root");
    let outside_sessions = outside.path().join("sessions");
    fs::create_dir(&outside_sessions).expect("outside sessions directory");
    let outside_source = outside_sessions.join(file_name);
    fs::copy(
        detached_storage
            .join("sessions")
            .join(outside_source.file_name().unwrap()),
        &outside_source,
    )
    .expect("copy source outside");
    symlink(outside.path(), config.storage_root()).expect("replace storage root with symlink");

    let error = LocalSessionStore::delete_session(&mut store, &session_id)
        .expect_err("the opened store must retain its original root authority");
    assert!(matches!(error, LocalStoreError::UnsafeSourcePath(_)));
    assert!(
        outside_source.exists(),
        "external source was permanently deleted"
    );
}

#[cfg(unix)]
#[test]
fn repair_rejects_a_sessions_root_replaced_by_a_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    fs::remove_dir(config.sessions_root()).expect("remove empty sessions root");
    symlink(outside.path(), config.sessions_root()).expect("replace sessions root with a symlink");

    let error = store
        .repair()
        .expect_err("repair must not enumerate a redirected sessions root");

    assert!(matches!(error, LocalStoreError::UnsafeSourcePath(_)));
}

#[cfg(unix)]
#[test]
fn creating_agent_session_rejects_a_symlinked_project_bucket() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let project = ProjectIdentity::new(root.path().join("project"), "Project");
    let bucket = config
        .sessions_root()
        .join(format!("--{}--", project.project_id));
    symlink(outside.path(), &bucket).expect("link project bucket outside the store");

    let header = SessionHeader::new(SessionDomain::Agent, Some(project));
    let session_id = header.session_id.clone();
    let error = store
        .create_session(header)
        .expect_err("a symlinked project bucket must not authorize session creation");

    assert_unsafe_source_error(&error);
    assert!(
        fs::read_dir(outside.path())
            .expect("outside directory")
            .next()
            .is_none(),
        "session facts escaped the configured sessions root"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn loading_a_session_rejects_a_symlinked_source_file() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
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
    store.shutdown().expect("close active handle");

    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move source outside the store");
    symlink(&outside_source, &source).expect("replace source with a symlink");

    let error = store
        .load_session(&session_id, None)
        .expect_err("a symlink must not redirect a session read");
    assert_unsafe_source_error(&error);
}

#[cfg(unix)]
#[test]
fn appending_a_session_rejects_a_symlinked_source_file() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
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
    store.shutdown().expect("close active handle");

    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move source outside the store");
    symlink(&outside_source, &source).expect("replace source with a symlink");
    let before = fs::read(&outside_source).expect("outside source before append");

    let error = store
        .append(&session_id, vec![message("must not escape")])
        .expect_err("a symlink must not redirect a session append");

    assert_unsafe_source_error(&error);
    assert_eq!(
        fs::read(&outside_source).expect("outside source after append"),
        before,
        "the rejected append changed a source outside Nostra's store"
    );
}

#[cfg(unix)]
#[test]
fn an_open_handle_cannot_append_after_its_source_path_becomes_a_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
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

    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move the open source outside the store");
    symlink(&outside_source, &source).expect("replace source with a symlink");
    let before = fs::read(&outside_source).expect("outside source before append");

    let error = store
        .append(
            &session_id,
            vec![message("must not escape through an open handle")],
        )
        .expect_err("an open file descriptor must not bypass path authorization");

    assert_unsafe_source_error(&error);
    assert_eq!(
        fs::read(&outside_source).expect("outside source after append"),
        before,
        "the retained recorder wrote through a path that no longer belongs to the store"
    );
}

#[cfg(unix)]
#[test]
fn an_open_handle_cannot_mask_a_symlinked_source_during_load() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
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

    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move the open source outside the store");
    symlink(&outside_source, &source).expect("replace source with a symlink");

    let error = store
        .load_session(&session_id, None)
        .expect_err("a cached snapshot must not hide an unauthorized source path");
    assert_unsafe_source_error(&error);
}

#[cfg(unix)]
#[test]
fn deleting_a_missing_source_rejects_a_symlinked_project_bucket() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
    let mut store = LocalSessionStore::open(config).expect("open");
    let project = ProjectIdentity::new(root.path().join("project"), "Project");
    let header = SessionHeader::new(SessionDomain::Agent, Some(project));
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let bucket = source.parent().expect("project bucket").to_path_buf();
    store.shutdown().expect("close active handle");

    fs::remove_file(&source).expect("remove source");
    fs::remove_dir(&bucket).expect("remove empty project bucket");
    symlink(outside.path(), &bucket).expect("replace bucket with an outside symlink");

    let error = LocalSessionStore::delete_session(&mut store, &session_id)
        .expect_err("a missing filename does not make its symlinked parent safe");

    assert!(matches!(error, LocalStoreError::UnsafeSourcePath(_)));
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_some(),
        "an unauthorized deletion removed the catalog row"
    );
}

#[cfg(unix)]
#[test]
fn repair_does_not_remove_a_missing_source_below_a_symlinked_project_bucket() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
    let mut store = LocalSessionStore::open(config).expect("open");
    let project = ProjectIdentity::new(root.path().join("project"), "Project");
    let header = SessionHeader::new(SessionDomain::Agent, Some(project));
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let bucket = source.parent().expect("project bucket").to_path_buf();
    store.shutdown().expect("close active handle");

    fs::remove_file(&source).expect("remove source");
    fs::remove_dir(&bucket).expect("remove empty project bucket");
    symlink(outside.path(), &bucket).expect("replace bucket with an outside symlink");

    let report = store.repair().expect("repair completes with an issue");

    assert_eq!(report.removed, 0);
    assert!(!report.issues.is_empty());
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_some(),
        "repair treated an unauthorized parent as proof that the source was deleted"
    );
}

#[test]
fn deleting_a_missing_source_below_a_real_or_absent_parent_is_idempotent() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
    let mut store = LocalSessionStore::open(config).expect("open");
    let project = ProjectIdentity::new(root.path().join("project"), "Project");
    let header = SessionHeader::new(SessionDomain::Agent, Some(project));
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let bucket = source.parent().expect("project bucket").to_path_buf();
    store.shutdown().expect("close active handle");

    fs::remove_file(&source).expect("remove source");
    fs::remove_dir(bucket).expect("remove empty project bucket");
    LocalSessionStore::delete_session(&mut store, &session_id)
        .expect("a confirmed missing in-root source remains an idempotent delete");

    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_none()
    );
}

#[test]
fn repair_rejects_a_valid_session_at_a_noncanonical_source_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let stray = config.sessions_root().join("stray.jsonl");
    JsonlWriter::create(&stray, header)
        .expect("create stray source")
        .append(message("must not enter the catalog"))
        .expect("append stray message");

    let report = store.repair().expect("repair completes with an issue");

    assert_eq!(report.rebuilt, 0);
    assert!(!report.issues.is_empty());
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_none(),
        "repair published a row whose canonical source cannot be opened"
    );
}

#[cfg(unix)]
#[test]
fn append_reopens_a_regular_source_replaced_with_matching_size_and_mtime() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("baseline")])
        .expect("append baseline");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    let source_bytes = fs::read(&source).expect("read source");
    let original_metadata = fs::metadata(&source).expect("source metadata");
    let original_modified = original_metadata.modified().expect("source mtime");

    let detached = root.path().join("detached-original.jsonl");
    fs::rename(&source, &detached).expect("detach the recorder's open inode");
    fs::write(&source, &source_bytes).expect("install same-sized replacement");
    fs::File::options()
        .write(true)
        .open(&source)
        .expect("open replacement")
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .expect("restore matching mtime");
    let replacement_metadata = fs::metadata(&source).expect("replacement metadata");
    assert_eq!(replacement_metadata.len(), original_metadata.len());
    assert_eq!(
        replacement_metadata.modified().expect("replacement mtime"),
        original_modified
    );

    store
        .append(&session_id, vec![message("must survive replacement")])
        .expect("append after replacement");
    store.shutdown().expect("shutdown");

    let reopened = LocalSessionStore::open(config).expect("reopen");
    let restored = reopened
        .load_session(&session_id, None)
        .expect("restore canonical source");
    assert_eq!(restored.messages.len(), 2);
    assert!(matches!(
        restored.messages[1].message.content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "must survive replacement"
    ));
}

#[test]
fn delete_uses_the_validated_source_header_instead_of_catalog_identity_fields() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let created_at = header.created_at;
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("delete this source")])
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
        .expect("tamper disposable catalog identity fields");
    drop(catalog);

    store
        .delete_session(&session_id)
        .expect("permanent delete resolves the authoritative source");

    assert!(!source.exists(), "delete left the real JSONL source behind");
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog after delete")
            .is_none()
    );
    assert_eq!(store.repair().expect("repair after delete").rebuilt, 0);
}

#[test]
fn delete_removes_a_unique_cataloged_source_even_when_its_contents_are_corrupt() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("delete corrupt source")])
        .expect("create");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("close source recorder");
    drop(store);

    fs::write(&source, b"{\"corrupt\":true}\n").expect("corrupt source");
    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .get_summary(&session_id)
            .expect("catalog before delete")
            .is_some()
    );

    reopened
        .delete_session(&session_id)
        .expect("permanent delete must not require a readable transcript");

    assert!(!source.exists(), "corrupt source survived permanent delete");
    assert!(
        reopened
            .get_summary(&session_id)
            .expect("catalog after delete")
            .is_none()
    );
    assert_eq!(reopened.repair().expect("repair after delete").rebuilt, 0);
}

#[test]
fn delete_rejects_duplicate_sources_before_removing_an_active_session() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let original_created_at = header.created_at;
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("authoritative source")])
        .expect("append");
    let original = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;

    let mut duplicate_header = SessionHeader::new(SessionDomain::Chat, None);
    duplicate_header.session_id = session_id.clone();
    duplicate_header.created_at = duplicate_header
        .created_at
        .max(original_created_at.saturating_add(1));
    let duplicate = config.sessions_root().join(format!(
        "{}_{}.jsonl",
        duplicate_header.created_at, session_id
    ));
    JsonlWriter::create(&duplicate, duplicate_header).expect("create duplicate source");

    assert!(matches!(
        store.delete_session(&session_id),
        Err(LocalStoreError::AmbiguousSessionSource(found)) if found == session_id
    ));
    assert!(
        original.exists(),
        "failed delete must preserve the active source"
    );
    assert!(
        duplicate.exists(),
        "failed delete must preserve the duplicate source"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("summary after rejected delete")
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn exact_reference_read_does_not_follow_a_symlinked_source() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let entry_id = store
        .append(
            &session_id,
            vec![message("outside content must not be returned")],
        )
        .expect("append")[0]
        .clone();
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;
    store.shutdown().expect("close active handle");

    let outside_source = outside.path().join("session.jsonl");
    fs::rename(&source, &outside_source).expect("move source outside the store");
    symlink(&outside_source, &source).expect("replace source with a symlink");
    let reference = ChatMessageRef::new(session_id, entry_id).expect("reference");

    assert!(matches!(
        store.read_chat_message(&reference),
        Err(ChatReferenceError::Unavailable(ChatMessageUnavailable {
            reason: ChatMessageUnavailableReason::SourceCorrupt,
            ..
        }))
    ));
}
