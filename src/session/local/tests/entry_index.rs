use super::*;

/// A v8-era catalog file (the previous schema version) must be treated as a
/// disposable index: opening rebuilds it from JSONL and the rebuilt v9 file
/// carries the full entry-offset index.
#[test]
fn a_v8_catalog_is_rebuilt_with_the_entry_index() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("hello")])
        .expect("create");
    store.shutdown().expect("shutdown");
    {
        let connection = rusqlite::Connection::open(config.index_path()).expect("open index");
        connection
            .pragma_update(None, "user_version", 8_i64)
            .expect("downgrade version");
    }
    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    // The downgraded index was rebuilt as an empty disposable catalog; the
    // explicit repair pass is the seam that restores rows from JSONL.
    reopened.repair().expect("repair");
    let records = reopened.load_entry_index(&id, None).expect("entry index");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].kind, crate::session::EntryKindTag::Header);
    assert_eq!(records[1].kind, crate::session::EntryKindTag::Message);
    assert!(records.iter().all(|record| record.byte_len.is_some()));
}

/// Incremental appends keep the offset index aligned with the durable source
/// bytes; read_entries then returns exactly the requested facts.
#[test]
fn appended_entries_are_locatable_by_offset() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("first")])
        .expect("create");
    let appended = store
        .append(&id, vec![message("second"), message("third")])
        .expect("append");

    let records = store.load_entry_index(&id, None).expect("entry index");
    assert_eq!(records.len(), 4);
    let loaded = store.read_entries(&id, &appended).expect("read entries");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].id, appended[0]);
    assert_eq!(loaded[1].id, appended[1]);

    // The stored bytes must actually decode the same facts the source holds.
    let state = store.load_session(&id, None).expect("load session");
    assert_eq!(state.messages.len(), 3);
    let tail_messages = &state.messages[1..];
    let matches = tail_messages
        .iter()
        .zip(&loaded)
        .all(|(resolved, entry)| resolved.entry_id == entry.id);
    assert!(matches);

    let unknown = crate::session::EntryId::new();
    assert!(store.read_entries(&id, &[unknown]).is_err());
    store.shutdown().expect("shutdown");
}

/// An interrupted trailing line truncates the source on writer reopen; the
/// entry index must be rebuilt so offsets match the repaired file exactly.
#[test]
fn truncated_tail_rebuild_offsets_after_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("first")])
        .expect("create");
    store.shutdown().expect("shutdown");

    let summary = store.get_summary(&id).expect("summary").expect("row");
    let source = summary.jsonl_path;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .expect("open source");
    use std::io::Write as _;
    file.write_all(br#"{"id":"interrupted""#)
        .expect("partial tail");
    drop(file);

    let mut reopened = LocalSessionStore::open(config).expect("reopen");
    reopened.repair().expect("repair");
    // A trailing partial line keeps the prior trusted rows; the valid prefix
    // offsets are unchanged because the interrupted bytes were never a fact.
    let records = reopened.load_entry_index(&id, None).expect("entry index");
    assert_eq!(records.len(), 2);
    let entries = reopened
        .read_entries(
            &id,
            records
                .iter()
                .map(|record| record.entry_id.clone())
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .expect("read all entries after repair");
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| records.iter().any(|record| record.entry_id == entry.id))
    );
}

/// A leaf change rebuilds the whole index, so an active path after a rewind
/// resolves from the rebuilt rows without message-body deserialization.
#[test]
fn leaf_change_rebuilds_the_entry_index_for_the_new_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store
        .create_session_with_entries(header, vec![message("root")])
        .expect("create");
    store.append(&id, vec![message("branch")]).expect("branch");
    let records = store.load_entry_index(&id, None).expect("index");
    let root_id = records[1].entry_id.clone();
    store.set_leaf(&id, Some(&root_id)).expect("set leaf");
    let records = store.load_entry_index(&id, None).expect("index after leaf");
    // The resolved path after the rewind is header -> root: the off-path
    // branch entry and the Leaf fact itself are not path rows.
    assert_eq!(records.len(), 2);
    let messages: Vec<_> = records
        .iter()
        .filter(|record| record.kind == crate::session::EntryKindTag::Message)
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].entry_id, root_id);
    store.shutdown().expect("shutdown");
}
