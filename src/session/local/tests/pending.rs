use super::*;

#[test]
fn dropping_a_store_after_failed_append_keeps_the_projection_repair_obligation() {
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

    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("persisted during recorder drop")])
            .is_err()
    );
    drop(store);

    assert_eq!(
        JsonlLoader::load(&source)
            .expect("recorder drop persists its exact pending batch")
            .entries
            .len(),
        2
    );

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair pending projection")
            .is_some(),
        "recorder Drop advanced JSONL after the catalog repair intent was cleared"
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary after repair")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("persisted during recorder drop")
    );
}

#[test]
fn ambiguous_leaf_write_keeps_the_projection_repair_obligation() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");
    let first = store
        .append(&session_id, vec![message("first branch")])
        .expect("append first")[0]
        .clone();
    store
        .append(&session_id, vec![message("second branch")])
        .expect("append second");

    store.fail_next_set_leaf_after_write_for_test(&session_id);
    assert!(
        store.set_leaf(&session_id, Some(&first)).is_err(),
        "the injected result loss must be observable by the caller"
    );
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    let state = reopened
        .load_session(&session_id, None)
        .expect("JSONL contains the durable leaf selection");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].entry_id, first);
    assert!(
        reopened
            .repair_if_needed()
            .expect("repair leaf projection")
            .is_some(),
        "a durable Leaf fact lost its catalog repair obligation"
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary after repair")
            .expect("repaired row")
            .preview
            .as_deref(),
        Some("first branch")
    );
}

#[test]
fn append_reconciles_a_durable_result_loss_before_preparing_the_next_batch() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");

    store.fail_next_append_after_write_for_test(&session_id);
    assert!(
        store
            .append(&session_id, vec![message("durable first batch")])
            .is_err()
    );
    let first_id = store
        .load_session(&session_id, None)
        .expect("the first batch reached the fact log")
        .messages[0]
        .entry_id
        .clone();

    let returned = store
        .append(&session_id, vec![message("second batch")])
        .expect("ordinary append reconciles the ambiguous prior result");

    assert_eq!(returned.len(), 1);
    let restored = store.load_session(&session_id, None).expect("restore");
    assert_eq!(restored.messages.len(), 2);
    assert_eq!(restored.messages[0].entry_id, first_id);
    assert_eq!(restored.messages[1].entry_id, returned[0]);
}

#[test]
fn explicit_repair_flushes_active_recorders_before_clearing_their_obligations() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store.create_session(header).expect("create");

    store.fail_next_append_for_test(&session_id);
    assert!(
        store
            .append(
                &session_id,
                vec![message("persisted after explicit repair")]
            )
            .is_err()
    );
    store.repair().expect("repair including the pending writer");
    assert_eq!(
        store
            .get_summary(&session_id)
            .expect("summary after repair")
            .expect("projection after repair")
            .preview
            .as_deref(),
        Some("persisted after explicit repair")
    );
    drop(store);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    assert!(
        reopened
            .repair_if_needed()
            .expect("check repair state")
            .is_none(),
        "the explicit repair already reconciled the exact pending batch"
    );
    assert_eq!(
        reopened
            .get_summary(&session_id)
            .expect("summary")
            .expect("repaired projection")
            .preview
            .as_deref(),
        Some("persisted after explicit repair")
    );
}
