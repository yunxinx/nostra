use super::*;
use crate::session::FavoriteChange;

#[test]
fn repair_rebuilds_a_deleted_catalog_and_delete_is_permanent() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let source = store
        .list(CatalogQuery::first_page())
        .expect("list")
        .sessions[0]
        .jsonl_path
        .clone();
    fs::remove_file(store.catalog_path()).expect("remove index");
    drop(store);
    let mut rebuilt = LocalSessionStore::open(config).expect("reopen");
    assert!(
        rebuilt
            .list(CatalogQuery::first_page())
            .expect("empty")
            .sessions
            .is_empty()
    );
    let report = rebuilt.repair().expect("repair");
    assert_eq!(report.rebuilt, 1);
    assert!(
        rebuilt
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions
            .len()
            == 1
    );
    rebuilt.delete_session(&id).expect("delete");
    assert!(!source.exists());
    assert!(
        rebuilt
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions
            .is_empty()
    );
    let index = rusqlite::Connection::open(rebuilt.catalog_path()).expect("index");
    let message_nodes: i64 = index
        .query_row(
            "SELECT COUNT(*) FROM message_nodes WHERE session_id = ?1",
            rusqlite::params![id.to_string()],
            |row| row.get(0),
        )
        .expect("message projection count");
    assert_eq!(message_nodes, 0);
}

#[test]
fn catalog_projects_metadata_and_filters_agent_projects() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
            .expect("open");
    let project = ProjectIdentity::new(root.path().join("project"), "Demo");
    let project_id = project.project_id.clone();
    let mut header = SessionHeader::new(SessionDomain::Agent, Some(project));
    header.created_at = 10;
    header.initial_model = Some(ModelSelection {
        profile_id: "profile".into(),
        model_id: "model".into(),
    });
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(
            &id,
            vec![message_with_metadata(
                "indexed discussion",
                Some(ModelSelection {
                    profile_id: "profile-2".into(),
                    model_id: "model-2".into(),
                }),
                7,
            )],
        )
        .expect("append");
    let summary = store.get_summary(&id).expect("summary").expect("row");
    assert_eq!(summary.title.as_deref(), Some("indexed discussion"));
    assert_eq!(summary.preview.as_deref(), Some("indexed discussion"));
    assert_eq!(summary.total_tokens, 7);
    assert_eq!(
        summary.model.as_ref().map(|model| model.model_id.as_str()),
        Some("model-2")
    );
    assert_eq!(
        store
            .list(CatalogQuery {
                project_id: Some(project_id),
                ..CatalogQuery::first_page()
            })
            .expect("filtered list")
            .sessions
            .len(),
        1
    );
}

#[test]
fn ordinary_append_updates_catalog_incrementally() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let (full_before, incremental_before) = store.projection_write_counts();
    for index in 0..500 {
        store
            .append(&id, vec![message(&format!("message-{index}"))])
            .expect("append");
    }
    let (full_after, incremental_after) = store.projection_write_counts();
    assert_eq!(full_after, full_before);
    assert_eq!(incremental_after.saturating_sub(incremental_before), 500);
    assert_eq!(
        store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .preview
            .as_deref(),
        Some("message-499")
    );
}

#[test]
fn catalog_metadata_uses_only_the_resolved_active_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let initial_model = ModelSelection {
        profile_id: "profile".into(),
        model_id: "initial".into(),
    };
    let active_model = ModelSelection {
        profile_id: "profile".into(),
        model_id: "active".into(),
    };
    let inactive_model = ModelSelection {
        profile_id: "profile".into(),
        model_id: "inactive".into(),
    };
    let mut header = SessionHeader::new(SessionDomain::Chat, None);
    header.initial_model = Some(initial_model);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let active_root = store
        .append(
            &id,
            vec![role_message_with_metadata(
                Role::Assistant,
                "active assistant root",
                active_model.clone(),
                3,
                "active-turn",
            )],
        )
        .expect("append active root")[0]
        .clone();
    store
        .append(
            &id,
            vec![role_message_with_metadata(
                Role::User,
                "inactive secret title",
                inactive_model,
                100,
                "inactive-turn",
            )],
        )
        .expect("append inactive branch");
    store
        .set_leaf(&id, Some(&active_root))
        .expect("rewind to active root");

    let summary = store.get_summary(&id).expect("summary").expect("row");
    assert_eq!(summary.title, None);
    assert_eq!(summary.preview.as_deref(), Some("active assistant root"));
    assert_eq!(summary.model, Some(active_model));
    assert_eq!(summary.total_tokens, 3);
}

#[test]
fn catalog_message_projection_does_not_duplicate_full_message_bodies() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
        .expect("open");
    let catalog = rusqlite::Connection::open(store.catalog_path()).expect("catalog");
    let mut columns = catalog
        .prepare("PRAGMA table_info(message_nodes)")
        .expect("message node schema");
    let names = columns
        .query_map([], |row| row.get::<_, String>(1))
        .expect("message node columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("column names");

    assert!(names.iter().any(|name| name == "preview"));
    assert!(names.iter().any(|name| name == "searchable_folded"));
    assert!(
        !names.iter().any(|name| name == "message_json"),
        "the disposable search projection must not duplicate full message bodies"
    );
}

#[test]
fn rejected_compaction_is_not_written_to_the_source_log() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let active = store.append(&id, vec![message("active")]).expect("active")[0].clone();
    let inactive = store
        .append(&id, vec![message("inactive")])
        .expect("inactive")[0]
        .clone();
    store.set_leaf(&id, Some(&active)).expect("rewind");
    let source = store
        .get_summary(&id)
        .expect("summary")
        .expect("row")
        .jsonl_path;

    let facts_before = JsonlLoader::load(&source).expect("source").entries.len();
    let off_path = SessionEntryKind::Compaction(Compaction {
        summary: "must not persist".into(),
        first_kept_entry_id: inactive.clone(),
        tokens_before: 10,
    });
    assert!(matches!(
        store.append(&id, vec![off_path]),
        Err(SessionError::InvalidCompactionTarget(target)) if target == inactive
    ));
    assert_eq!(
        JsonlLoader::load(&source).expect("source").entries.len(),
        facts_before
    );

    let config = store
        .append(
            &id,
            vec![SessionEntryKind::ConfigChange(ConfigChange {
                model: ModelSelection {
                    profile_id: "profile".into(),
                    model_id: "model".into(),
                },
                system_prompt: None,
            })],
        )
        .expect("config")[0]
        .clone();
    let facts_before = JsonlLoader::load(&source).expect("source").entries.len();
    let non_message = SessionEntryKind::Compaction(Compaction {
        summary: "must not persist".into(),
        first_kept_entry_id: config.clone(),
        tokens_before: 10,
    });
    assert!(matches!(
        store.append(&id, vec![non_message]),
        Err(SessionError::InvalidCompactionTarget(target)) if target == config
    ));
    assert_eq!(
        JsonlLoader::load(&source).expect("source").entries.len(),
        facts_before
    );

    let continuation = store
        .append(&id, vec![message("continuation")])
        .expect("append after rejection")[0]
        .clone();
    store
        .append(
            &id,
            vec![SessionEntryKind::Compaction(Compaction {
                summary: "valid summary".into(),
                first_kept_entry_id: continuation.clone(),
                tokens_before: 10,
            })],
        )
        .expect("active message compaction");
    let restored = store.load_session(&id, None).expect("restored session");
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].entry_id, continuation);
}

#[test]
fn agent_project_location_updates_keep_sessions_in_the_same_stable_bucket() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
            .expect("open");
    let first_project = ProjectIdentity::new(root.path().join("first"), "First");
    let project_id = first_project.project_id.clone();
    let first_header = SessionHeader::new(SessionDomain::Agent, Some(first_project));
    store
        .create_session(first_header)
        .expect("first agent session");

    let moved_project = ProjectIdentity::from_parts(
        project_id.clone(),
        root.path().join("moved"),
        "Moved project",
    )
    .expect("moved project");
    let second_header = SessionHeader::new(SessionDomain::Agent, Some(moved_project.clone()));
    store
        .create_session(second_header)
        .expect("second agent session");
    let page = store
        .list(CatalogQuery {
            project_id: Some(project_id.clone()),
            ..CatalogQuery::first_page()
        })
        .expect("project page");
    assert_eq!(page.sessions.len(), 2);
    assert!(page.sessions.iter().all(|summary| {
        summary.project.as_ref().map(|project| &project.project_id) == Some(&project_id)
    }));

    assert_eq!(
        store
            .get_project_identity(&project_id)
            .expect("project registry row"),
        Some(moved_project)
    );
}

#[test]
fn project_registry_is_order_independent_and_promotes_after_delete() {
    fn identities(root: &Path) -> (String, SessionHeader, SessionHeader) {
        let original = ProjectIdentity::new(root.join("original"), "Original");
        let project_id = original.project_id.clone();
        let moved = ProjectIdentity::from_parts(project_id.clone(), root.join("moved"), "Moved")
            .expect("moved identity");
        let mut original_header = SessionHeader::new(SessionDomain::Agent, Some(original));
        let mut moved_header = SessionHeader::new(SessionDomain::Agent, Some(moved));
        original_header.created_at = 100;
        moved_header.created_at = 100;
        (project_id, original_header, moved_header)
    }

    let first_root = tempfile::tempdir().expect("first root");
    let second_root = tempfile::tempdir().expect("second root");
    let (project_id, original_header, moved_header) = identities(first_root.path());

    let mut first = LocalSessionStore::open(LocalStoreConfig::new(
        first_root.path(),
        SessionDomain::Agent,
    ))
    .expect("first store");
    first
        .create_session(original_header.clone())
        .expect("original first");
    first
        .create_session(moved_header.clone())
        .expect("moved second");

    let mut second = LocalSessionStore::open(LocalStoreConfig::new(
        second_root.path(),
        SessionDomain::Agent,
    ))
    .expect("second store");
    second
        .create_session(moved_header.clone())
        .expect("moved first");
    second
        .create_session(original_header.clone())
        .expect("original second");

    let expected_winner = if original_header.session_id > moved_header.session_id {
        original_header.project.clone().expect("original identity")
    } else {
        moved_header.project.clone().expect("moved identity")
    };
    assert_eq!(
        first
            .get_project_identity(&project_id)
            .expect("first registry"),
        Some(expected_winner.clone())
    );
    assert_eq!(
        second
            .get_project_identity(&project_id)
            .expect("second registry"),
        Some(expected_winner.clone())
    );

    let winner_id = if original_header.session_id > moved_header.session_id {
        original_header.session_id.clone()
    } else {
        moved_header.session_id.clone()
    };
    let expected_fallback = if winner_id == original_header.session_id {
        moved_header.project.clone().expect("moved fallback")
    } else {
        original_header.project.clone().expect("original fallback")
    };
    first.delete_session(&winner_id).expect("delete winner");
    assert_eq!(
        first
            .get_project_identity(&project_id)
            .expect("promoted registry"),
        Some(expected_fallback)
    );
}

#[test]
fn list_projects_pages_agent_projects_with_counts_and_stable_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
            .expect("open");

    let mut project_ids = Vec::new();
    for index in 0..3 {
        let project = ProjectIdentity::new(
            root.path().join(format!("project-{index}")),
            format!("Project {index}"),
        );
        project_ids.push(project.project_id.clone());
        let header = SessionHeader::new(SessionDomain::Agent, Some(project));
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create");
        store
            .append(&session_id, vec![message("x")])
            .expect("append");
    }

    // Add a second session to project 0 so session_count distinguishes it.
    let project_0 = ProjectIdentity::from_parts(
        project_ids[0].clone(),
        root.path().join("project-0-moved"),
        "Project 0 Moved",
    )
    .expect("moved identity");
    let second_header = SessionHeader::new(SessionDomain::Agent, Some(project_0));
    store
        .create_session(second_header)
        .expect("second session in project 0");

    let first = store
        .list_projects(ProjectCatalogQuery::with_limit(2))
        .expect("first page");
    assert_eq!(first.projects.len(), 2);
    let cursor = first.next_cursor.expect("has more");
    let second = store
        .list_projects(ProjectCatalogQuery {
            cursor: Some(cursor),
            limit: 2,
        })
        .expect("second page");
    assert_eq!(second.projects.len(), 1);
    assert!(second.next_cursor.is_none());

    let mut all = first.projects;
    all.extend(second.projects);
    assert_eq!(all.len(), 3);
    let seen: HashSet<String> = all.iter().map(|p| p.project_id.clone()).collect();
    assert_eq!(seen.len(), 3);
    for summary in &all {
        if summary.project_id == project_ids[0] {
            assert_eq!(summary.session_count, 2);
        } else {
            assert_eq!(summary.session_count, 1);
        }
    }
    // Ordering is stable by (updated_at DESC, project_id DESC).
    assert!(all[0].last_updated_at >= all[1].last_updated_at);
    assert!(all[1].last_updated_at >= all[2].last_updated_at);
}

#[test]
fn list_projects_rejects_chat_domain() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
        .expect("open");
    assert!(matches!(
        store.list_projects(ProjectCatalogQuery::first_page()),
        Err(CatalogError::DomainMismatch {
            expected: SessionDomain::Agent,
            actual: SessionDomain::Chat,
        })
    ));
}

#[test]
fn favorite_change_projects_and_filters_the_catalog() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut store =
        LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
            .expect("open");
    let starred = SessionHeader::new(SessionDomain::Chat, None);
    let starred_id = starred.session_id.clone();
    store.create_session(starred).expect("create starred");
    let plain = SessionHeader::new(SessionDomain::Chat, None);
    let plain_id = plain.session_id.clone();
    store.create_session(plain).expect("create plain");
    store
        .append(
            &starred_id,
            vec![SessionEntryKind::FavoriteChange(FavoriteChange {
                favorited: true,
            })],
        )
        .expect("star");

    let favorites = store
        .list(CatalogQuery::favorites())
        .expect("favorites")
        .sessions;
    assert_eq!(favorites.len(), 1);
    assert_eq!(favorites[0].session_id, starred_id);
    assert!(favorites[0].favorited);

    let timeline = store
        .list(CatalogQuery::timeline_first_page())
        .expect("timeline")
        .sessions;
    assert_eq!(timeline.len(), 1);
    assert_eq!(timeline[0].session_id, plain_id);
    assert!(!timeline[0].favorited);

    store
        .append(
            &starred_id,
            vec![SessionEntryKind::FavoriteChange(FavoriteChange {
                favorited: false,
            })],
        )
        .expect("unstar");
    assert!(
        store
            .list(CatalogQuery::favorites())
            .expect("empty favorites")
            .sessions
            .is_empty()
    );
    assert_eq!(
        store
            .list(CatalogQuery::timeline_first_page())
            .expect("full timeline")
            .sessions
            .len(),
        2
    );
}

#[test]
fn favorite_survives_catalog_rebuild_from_jsonl() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let mut store = LocalSessionStore::open(config.clone()).expect("open");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    store
        .append(
            &id,
            vec![SessionEntryKind::FavoriteChange(FavoriteChange {
                favorited: true,
            })],
        )
        .expect("star");
    fs::remove_file(store.catalog_path()).expect("remove index");
    drop(store);

    let mut rebuilt = LocalSessionStore::open(config).expect("reopen");
    rebuilt.repair().expect("repair");
    let favorites = rebuilt
        .list(CatalogQuery::favorites())
        .expect("favorites")
        .sessions;
    assert_eq!(favorites.len(), 1);
    assert_eq!(favorites[0].session_id, id);
    assert!(favorites[0].favorited);
}
