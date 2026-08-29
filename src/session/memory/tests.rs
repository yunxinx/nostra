use super::*;
use crate::{
    llm::{ContentBlock, Message, ModelSelection, Role, Usage},
    session::{
        Compaction, ConfigChange, MessageEntry, ProjectCatalogQuery, ProjectIdentity,
        ProjectSummary, SessionDomain,
    },
};

fn message(text: &str) -> SessionEntryKind {
    SessionEntryKind::Message(MessageEntry {
        message: Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.into(),
                provider_metadata: Default::default(),
            }],
            provider_metadata: Default::default(),
        },
        turn_id: None,
        model: None,
        usage: Usage::default(),
    })
}

#[test]
fn create_with_entries_rejects_the_whole_invalid_session() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    let invalid = SessionEntryKind::Header(SessionHeader::new(SessionDomain::Chat, None));

    assert!(matches!(
        store.create_session_with_entries(header, vec![invalid]),
        Err(SessionError::InvalidEntryKind)
    ));
    assert!(!store.contains(&id));
}

#[test]
fn creates_appends_loads_and_flushes() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let ids = store
        .append(&id, vec![message("hello"), message("world")])
        .expect("append");
    let state = store.load_session(&id, Some(&ids[1])).expect("load");
    assert_eq!(state.messages.len(), 2);
    store.flush().expect("flush");
    store.shutdown().expect("shutdown");
}

#[test]
fn explicit_leaf_switch_does_not_change_history_facts() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let first = store.append(&id, vec![message("first")]).expect("append")[0].clone();
    let _second = store.append(&id, vec![message("second")]).expect("append");
    let facts_before_switch = store.entries(&id).expect("entries").len();
    store.set_leaf(&id, Some(&first)).expect("switch leaf");
    let state = store.load_session(&id, None).expect("load");
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].entry_id, first);
    let facts = store.entries(&id).expect("entries");
    assert_eq!(facts.len(), facts_before_switch + 1);
    assert!(matches!(
        facts.last().map(|entry| &entry.kind),
        Some(SessionEntryKind::Leaf(_))
    ));
}

#[test]
fn compaction_admission_requires_an_active_message_target() {
    let mut store = InMemorySessionStore::new();
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let id = header.session_id.clone();
    store.create_session(header).expect("create");
    let active = store.append(&id, vec![message("active")]).expect("active")[0].clone();
    let inactive = store
        .append(&id, vec![message("inactive")])
        .expect("inactive")[0]
        .clone();
    store.set_leaf(&id, Some(&active)).expect("rewind");

    let facts_before = store.entries(&id).expect("facts").len();
    let off_path = SessionEntryKind::Compaction(Compaction {
        summary: "must not persist".into(),
        first_kept_entry_id: inactive.clone(),
        tokens_before: 10,
    });
    assert!(matches!(
        store.append(&id, vec![off_path]),
        Err(SessionError::InvalidCompactionTarget(target)) if target == inactive
    ));
    assert_eq!(store.entries(&id).expect("facts").len(), facts_before);

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
    let facts_before = store.entries(&id).expect("facts").len();
    let non_message = SessionEntryKind::Compaction(Compaction {
        summary: "must not persist".into(),
        first_kept_entry_id: config.clone(),
        tokens_before: 10,
    });
    assert!(matches!(
        store.append(&id, vec![non_message]),
        Err(SessionError::InvalidCompactionTarget(target)) if target == config
    ));
    assert_eq!(store.entries(&id).expect("facts").len(), facts_before);

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

fn agent_header(project: ProjectIdentity, created_at: i64) -> SessionHeader {
    let mut header = SessionHeader::new(SessionDomain::Agent, Some(project));
    header.created_at = created_at;
    header
}

#[test]
fn list_projects_aggregates_counts_and_orders_by_updated_at_desc() {
    let mut store = InMemorySessionStore::new();
    let project_a = ProjectIdentity::new(PathBuf::from("/tmp/a"), "Alpha");
    let project_b = ProjectIdentity::new(PathBuf::from("/tmp/b"), "Beta");
    let project_a_id = project_a.project_id.clone();
    let project_b_id = project_b.project_id.clone();

    let header_a1 = agent_header(project_a.clone(), 100);
    let a1_id = header_a1.session_id.clone();
    store.create_session(header_a1).expect("create a1");
    store
        .append(&a1_id, vec![message("a1")])
        .expect("append a1");

    let header_b1 = agent_header(project_b.clone(), 200);
    let b1_id = header_b1.session_id.clone();
    store.create_session(header_b1).expect("create b1");
    store
        .append(&b1_id, vec![message("b1")])
        .expect("append b1");

    let header_a2 = agent_header(project_a.clone(), 300);
    let a2_id = header_a2.session_id.clone();
    store.create_session(header_a2).expect("create a2");
    store
        .append(&a2_id, vec![message("a2")])
        .expect("append a2");

    let page = store
        .list_projects(ProjectCatalogQuery::first_page())
        .expect("list projects");
    assert_eq!(page.projects.len(), 2);
    let by_id: HashMap<String, ProjectSummary> = page
        .projects
        .iter()
        .cloned()
        .map(|summary| (summary.project_id.clone(), summary))
        .collect();
    assert_eq!(by_id[&project_a_id].session_count, 2);
    assert_eq!(by_id[&project_b_id].session_count, 1);
    assert!(page.projects[0].last_updated_at >= page.projects[1].last_updated_at);
    assert!(page.next_cursor.is_none());
}

#[test]
fn list_projects_keyset_pagination_does_not_duplicate_or_rewind() {
    let mut store = InMemorySessionStore::new();
    let mut project_ids = Vec::new();
    for index in 0..5 {
        let project = ProjectIdentity::new(
            PathBuf::from(format!("/tmp/project-{index}")),
            format!("Project {index}"),
        );
        project_ids.push(project.project_id.clone());
        let header = agent_header(project, 100 + index as i64);
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create");
        store
            .append(&session_id, vec![message("x")])
            .expect("append");
    }

    let first = store
        .list_projects(ProjectCatalogQuery::with_limit(2))
        .expect("first page");
    assert_eq!(first.projects.len(), 2);
    let cursor = first.next_cursor.expect("cursor");
    let second = store
        .list_projects(ProjectCatalogQuery {
            cursor: Some(cursor),
            limit: 2,
        })
        .expect("second page");
    assert_eq!(second.projects.len(), 2);
    let cursor = second.next_cursor.expect("cursor");
    let third = store
        .list_projects(ProjectCatalogQuery {
            cursor: Some(cursor),
            limit: 2,
        })
        .expect("third page");
    assert_eq!(third.projects.len(), 1);
    assert!(third.next_cursor.is_none());

    let mut all = first.projects;
    all.extend(second.projects);
    all.extend(third.projects);
    assert_eq!(all.len(), 5);
    let seen: HashSet<String> = all.iter().map(|p| p.project_id.clone()).collect();
    assert_eq!(seen.len(), 5);
}

#[test]
fn list_projects_ignores_chat_sessions() {
    let mut store = InMemorySessionStore::new();
    let chat_header = SessionHeader::new(SessionDomain::Chat, None);
    store.create_session(chat_header).expect("chat");
    let page = store
        .list_projects(ProjectCatalogQuery::first_page())
        .expect("list");
    assert!(page.projects.is_empty());
}
