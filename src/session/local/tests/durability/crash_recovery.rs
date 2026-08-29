use super::super::*;

use std::process::Command;

const CREATE_STAGE_CHILD_MARKER: &str = "NOSTRA_CREATE_STAGE_CRASH_CHILD";
const CREATE_STAGE_ROOT_ENV: &str = "NOSTRA_CREATE_STAGE_CRASH_ROOT";
const CREATE_STAGE_TEST_NAME: &str = "session::local::tests::durability::crash_recovery::abandoned_prepublication_stage_is_removed_on_next_open";
const CREATE_STAGE_PAYLOAD: &str = "unpublished staged conversation";

fn directory_contains_bytes(root: &Path, needle: &[u8]) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if file_type.is_file()
                && fs::read(entry.path())
                    .is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
            {
                return true;
            }
        }
    }
    false
}

fn leave_abandoned_create_stage(root: &Path) {
    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .arg("--exact")
        .arg(CREATE_STAGE_TEST_NAME)
        .arg("--nocapture")
        .env(CREATE_STAGE_CHILD_MARKER, "1")
        .env(CREATE_STAGE_ROOT_ENV, root)
        .output()
        .expect("run staged-create crash probe");
    assert_eq!(
        output.status.code(),
        Some(86),
        "crash probe did not stop at the staging boundary: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn abandoned_prepublication_stage_is_removed_on_next_open() {
    if std::env::var_os(CREATE_STAGE_CHILD_MARKER).is_some() {
        let root = PathBuf::from(std::env::var_os(CREATE_STAGE_ROOT_ENV).expect("child data root"));
        let config = LocalStoreConfig::new(root, SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config).expect("open child store");
        store.crash_after_create_stage_for_test();
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let _ = store.create_session_with_entries(header, vec![message(CREATE_STAGE_PAYLOAD)]);
        panic!("the injected process exit did not run");
    }

    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    leave_abandoned_create_stage(root.path());
    assert!(
        directory_contains_bytes(
            config.storage_root().as_path(),
            CREATE_STAGE_PAYLOAD.as_bytes()
        ),
        "the crash probe did not leave the prepublication plaintext artifact"
    );
    let unrelated = config.staging_root().join("user-note.txt");
    fs::write(&unrelated, b"must remain").expect("create unrelated staging-directory file");

    let store = LocalSessionStore::open(config.clone()).expect("reopen after staged-create crash");
    assert!(
        !directory_contains_bytes(
            config.storage_root().as_path(),
            CREATE_STAGE_PAYLOAD.as_bytes()
        ),
        "reopening left abandoned prepublication plaintext on disk"
    );
    assert_eq!(
        fs::read(unrelated).expect("unrelated file survives cleanup"),
        b"must remain",
        "cleanup removed a file outside Nostra's owned staging namespace"
    );
    assert!(
        store
            .list(CatalogQuery::first_page())
            .expect("empty catalog")
            .sessions
            .is_empty()
    );
}

#[test]
fn abandoned_stage_cleanup_does_not_depend_on_catalog_availability() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    leave_abandoned_create_stage(root.path());
    assert!(directory_contains_bytes(
        config.storage_root().as_path(),
        CREATE_STAGE_PAYLOAD.as_bytes()
    ));

    fs::remove_file(config.index_path()).expect("remove catalog file");
    fs::create_dir(config.index_path()).expect("replace catalog with invalid directory");
    assert!(
        LocalSessionStore::open(config.clone()).is_err(),
        "the invalid catalog path must still make the domain unavailable"
    );
    assert!(
        !directory_contains_bytes(
            config.storage_root().as_path(),
            CREATE_STAGE_PAYLOAD.as_bytes()
        ),
        "catalog failure prevented independent plaintext staging cleanup"
    );
}

#[test]
fn controller_delete_after_an_ambiguous_first_turn_failure_removes_the_published_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("store should open");
    store.fail_after_create_publish_for_test();
    let mut controller = ChatSessionController::new(store);

    controller
        .begin_turn(
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "possibly published".into(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            ModelSelection {
                profile_id: "profile-a".into(),
                model_id: "model-a".into(),
            },
            "turn-1",
        )
        .expect_err("injected failure follows source publication");
    controller
        .delete_session()
        .expect("delete should reconcile the pending session identity");

    let sources = fs::read_dir(config.sessions_root())
        .expect("sessions root")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert!(
        sources.is_empty(),
        "delete left sources behind: {sources:?}"
    );
}

#[test]
fn handle_cache_is_bounded_and_delete_closes_active_handle() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat).with_max_open_handles(1);
    let mut store = LocalSessionStore::open(config).expect("open");
    let mut ids = Vec::new();
    for _ in 0..3 {
        let header = SessionHeader::new(SessionDomain::Chat, None);
        ids.push(header.session_id.clone());
        store.create_session(header).expect("create");
        assert!(store.open_handle_count() <= 1);
    }
    store.delete_session(&ids[2]).expect("delete");
    assert!(store.open_handle_count() <= 1);
    assert!(store.get_summary(&ids[2]).expect("summary").is_none());
}

#[test]
fn dirty_projection_survives_handle_pressure_and_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat).with_max_open_handles(1);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let first_header = SessionHeader::new(SessionDomain::Chat, None);
    let first_id = first_header.session_id.clone();
    store.create_session(first_header).expect("create first");

    let fault = rusqlite::Connection::open(store.catalog_path()).expect("fault connection");
    fault
        .execute_batch(&format!(
            "CREATE TRIGGER fail_first_projection
                 BEFORE UPDATE ON sessions
                 WHEN NEW.session_id = '{}'
                 BEGIN
                    SELECT RAISE(ABORT, 'injected projection failure');
                 END;",
            first_id
        ))
        .expect("install projection fault");
    assert!(
        store
            .append(&first_id, vec![message("durable despite catalog failure")])
            .is_err()
    );
    fault
        .execute_batch("DROP TRIGGER fail_first_projection;")
        .expect("remove projection fault");
    drop(fault);

    let second_header = SessionHeader::new(SessionDomain::Chat, None);
    let second_id = second_header.session_id.clone();
    store.create_session(second_header).expect("create second");
    store
        .append(&second_id, vec![message("exercise handle pressure")])
        .expect("append second");
    store.flush().expect("flush dirty projection");
    store.shutdown().expect("shutdown");

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    let _ = reopened
        .repair_if_needed()
        .expect("resume any durable repair intent");
    assert_eq!(
        reopened
            .get_summary(&first_id)
            .expect("first summary")
            .expect("first row")
            .preview
            .as_deref(),
        Some("durable despite catalog failure")
    );
}

#[test]
fn repair_keeps_a_projection_intent_until_its_session_is_reconciled() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("catalog baseline")])
        .expect("baseline append");
    let source = store
        .get_summary(&session_id)
        .expect("summary")
        .expect("catalog row")
        .jsonl_path;

    let fault = rusqlite::Connection::open(store.catalog_path()).expect("fault connection");
    fault
        .execute_batch(&format!(
            "CREATE TRIGGER reject_projection_update
                 BEFORE UPDATE ON sessions
                 WHEN NEW.session_id = '{session_id}'
                 BEGIN
                    SELECT RAISE(ABORT, 'injected projection failure');
                 END;"
        ))
        .expect("install projection fault");
    assert!(
        store
            .append(&session_id, vec![message("durable source update")])
            .is_err(),
        "the source append must outlive the failed catalog projection"
    );
    fault
        .execute_batch("DROP TRIGGER reject_projection_update;")
        .expect("remove projection fault");
    drop(fault);

    let valid_len = fs::metadata(&source).expect("source metadata").len();
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .expect("open source tail")
        .write_all(b"{\"interrupted\":")
        .expect("append interrupted tail");
    drop(store);

    let mut first_reopen = LocalSessionStore::open(config.clone()).expect("first reopen");
    assert!(
        first_reopen
            .repair_if_needed()
            .expect("first repair attempt")
            .is_some()
    );
    assert_eq!(
        first_reopen
            .get_summary(&session_id)
            .expect("summary after incomplete repair")
            .expect("baseline row is retained")
            .preview
            .as_deref(),
        Some("catalog baseline")
    );
    drop(first_reopen);

    fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .expect("open source for tail repair")
        .set_len(valid_len)
        .expect("remove interrupted tail");

    let mut second_reopen = LocalSessionStore::open(config).expect("second reopen");
    assert!(
        second_reopen
            .repair_if_needed()
            .expect("resume retained projection intent")
            .is_some(),
        "the unresolved session lost its durable repair obligation"
    );
    assert_eq!(
        second_reopen
            .get_summary(&session_id)
            .expect("summary after repair")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("durable source update")
    );
}

#[test]
fn failed_source_publication_does_not_leave_a_permanent_projection_intent() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let source = config.sessions_root().join(format!(
        "{}_{}.jsonl",
        header.created_at.max(0),
        header.session_id
    ));
    fs::write(&source, b"occupied").expect("block no-clobber publication");

    assert!(
        store.create_session(header).is_err(),
        "publishing over an existing source must fail"
    );
    fs::remove_file(source).expect("remove blocking file");
    drop(store);

    let mut first_reopen = LocalSessionStore::open(config.clone()).expect("first reopen");
    let _ = first_reopen
        .repair_if_needed()
        .expect("settle any conservative repair marker");
    drop(first_reopen);

    let mut second_reopen = LocalSessionStore::open(config).expect("second reopen");
    assert!(
        second_reopen
            .repair_if_needed()
            .expect("check repair state")
            .is_none(),
        "a failed create with no published source left an endless repair loop"
    );
}

#[test]
fn abandoned_prepublication_create_intent_is_cleared_by_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.fail_after_create_intent_for_test();

    assert!(
        store.create_session(header).is_err(),
        "the injected interruption must leave only the durable intent"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_none()
    );
    assert!(
        collect_jsonl_paths(&config.sessions_root())
            .expect("enumerate sources")
            .is_empty(),
        "a prepublication interruption exposed a final session source"
    );
    drop(store);

    let mut first_reopen = LocalSessionStore::open(config.clone()).expect("first reopen");
    assert!(
        first_reopen
            .repair_if_needed()
            .expect("repair abandoned create")
            .is_some()
    );
    drop(first_reopen);

    let mut second_reopen = LocalSessionStore::open(config).expect("second reopen");
    assert!(
        second_reopen
            .repair_if_needed()
            .expect("check settled repair state")
            .is_none(),
        "an intent with no catalog row or source candidate caused an endless repair loop"
    );
}

#[test]
fn interrupted_append_is_discovered_and_repaired_on_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("catalog baseline")])
        .expect("baseline append");

    store.fail_after_append_commit_for_test();
    assert!(
        store
            .append(&session_id, vec![message("durable source update")])
            .is_err()
    );
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("resume interrupted projection")
            .is_some()
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("durable source update")
    );
}

#[test]
fn interrupted_leaf_selection_is_discovered_and_repaired_on_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let first = store
        .append(&session_id, vec![message("first")])
        .expect("first")[0]
        .clone();
    let second = store
        .append(&session_id, vec![message("second")])
        .expect("second")[0]
        .clone();

    store.fail_after_leaf_commit_for_test();
    assert!(store.set_leaf(&session_id, Some(&first)).is_err());
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("resume interrupted leaf projection")
            .is_some()
    );
    let state = reopened
        .load_session(&session_id, None)
        .expect("load repaired session");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].entry_id, first);
    assert_ne!(state.messages[0].entry_id, second);
}

#[test]
fn interrupted_create_is_discovered_and_repaired_on_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();

    store.fail_after_create_publish_for_test();
    assert!(
        store
            .create_session_with_entries(header, vec![message("published source")])
            .is_err()
    );
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("resume interrupted creation")
            .is_some()
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("published source")
    );
}

#[test]
fn repair_keeps_create_intent_until_the_published_source_directory_is_durable() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();

    store.fail_next_directory_sync_for_test(config.sessions_root());
    assert!(
        store
            .create_session_with_entries(header, vec![message("published source")])
            .is_err(),
        "the first directory durability barrier must fail after source publication"
    );
    assert!(
        store
            .get_summary(&session_id)
            .expect("catalog remains readable")
            .is_none(),
        "a failed create must not claim that its projection was committed"
    );

    store.fail_next_directory_sync_for_test(config.sessions_root());
    assert!(
        store.repair_if_needed().is_err(),
        "repair must not clear the projection intent before the source namespace is durable"
    );

    assert!(
        store
            .repair_if_needed()
            .expect("retry repair after directory sync")
            .is_some(),
        "the failed repair must retain its durable recovery obligation"
    );
    assert_eq!(
        store
            .get_summary(&session_id)
            .expect("summary")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("published source")
    );
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("check settled repair state")
            .is_none(),
        "a durable repaired source left a stale projection intent"
    );
    assert_eq!(
        reopened
            .load_session(&session_id, None)
            .expect("load repaired session")
            .messages
            .len(),
        1
    );
}

#[test]
fn interrupted_delete_is_discovered_and_repaired_on_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(&session_id, vec![message("delete me")])
        .expect("append");

    store.fail_after_delete_commit_for_test();
    assert!(store.delete_session(&session_id).is_err());
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("resume interrupted deletion")
            .is_some()
    );
    assert!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .is_none()
    );
}
