use super::core::SharedStoreCore;
use super::domains::DomainStoreState;
use super::*;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc,
    thread,
    time::Duration,
};

use crate::session::{
    CatalogError, CatalogQuery, InMemorySessionStore, ProjectCatalogQuery, ProjectIdentity,
    ProjectSessionStore, ProjectSummary, SessionCatalogStore, SessionDomain, SessionError,
    SessionHeader, SessionId, SessionLifecycleStore, SessionReadStore,
};

fn assert_send_sync<T: Send + Sync>() {}

fn poison(core: &SharedStoreCore) {
    let core = core.clone();
    let _ = catch_unwind(AssertUnwindSafe(move || {
        let _guard = core.0.store.lock().expect("lock before poisoning");
        panic!("injected panic while holding session store lock");
    }));
}

#[test]
fn shared_capabilities_are_send_sync_and_poison_aware() {
    assert_send_sync::<SharedSessionStore>();
    assert_send_sync::<SharedSessionCatalog>();
    assert_send_sync::<SharedChatReferenceStore>();
    assert_send_sync::<SharedAgentProjectStore>();
    assert_send_sync::<SessionStores>();

    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let DomainStoreState::Ready(core) = &stores.chat else {
        panic!("Chat store should be ready");
    };
    poison(core);
    let missing = SessionId::new(SessionDomain::Chat);
    assert!(matches!(
        stores
            .chat()
            .expect("Chat lifecycle capability")
            .load_session(&missing, None),
        Err(SessionError::StorePoisoned)
    ));
    assert!(matches!(
        stores
            .chat_catalog()
            .expect("Chat catalog capability")
            .list_sessions(SessionDomain::Chat, CatalogQuery::first_page()),
        Err(CatalogError::StorePoisoned)
    ));
}

#[test]
fn unavailable_domain_does_not_hide_the_healthy_domain() {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    assert!(stores.chat().is_ok());
    assert!(stores.chat_catalog().is_ok());
    assert!(stores.chat_references().is_ok());
    assert!(matches!(
        stores.agent(),
        Err(SessionStoresError::DomainUnavailable {
            domain: SessionDomain::Agent,
            ..
        })
    ));
}

#[test]
fn chat_lifecycle_capability_rejects_agent_session_creation() {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let mut chat = stores.chat().expect("Chat lifecycle capability");
    let agent_header = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new(
            "/tmp/nostra-domain-boundary",
            "domain-boundary",
        )),
    );

    assert!(matches!(
        chat.create_session(agent_header),
        Err(SessionError::DomainMismatch {
            header: SessionDomain::Chat,
            id: SessionDomain::Agent,
        })
    ));
}

#[test]
fn chat_lifecycle_capability_rejects_agent_session_ids() {
    let mut backing = InMemorySessionStore::new();
    let header = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new(
            "/tmp/nostra-domain-read-boundary",
            "domain-read-boundary",
        )),
    );
    let session_id = header.session_id.clone();
    backing.create_session(header).expect("seed Agent session");
    let stores = SessionStores::with_chat_store(backing);
    let chat = stores.chat().expect("Chat lifecycle capability");

    assert!(matches!(
        chat.load_session(&session_id, None),
        Err(SessionError::DomainMismatch {
            header: SessionDomain::Chat,
            id: SessionDomain::Agent,
        })
    ));
}

#[test]
fn chat_lifecycle_capability_cannot_delete_an_agent_session() {
    let mut backing = InMemorySessionStore::new();
    let header = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new(
            "/tmp/nostra-domain-delete-boundary",
            "domain-delete-boundary",
        )),
    );
    let session_id = header.session_id.clone();
    backing.create_session(header).expect("seed Agent session");
    let stores = SessionStores::with_chat_store(backing);
    let mut chat = stores.chat().expect("Chat lifecycle capability");

    assert!(matches!(
        chat.delete_session(&session_id),
        Err(SessionError::DomainMismatch {
            header: SessionDomain::Chat,
            id: SessionDomain::Agent,
        })
    ));
}

#[test]
fn chat_catalog_capability_rejects_agent_session_ids() {
    let mut backing = InMemorySessionStore::new();
    let header = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new(
            "/tmp/nostra-catalog-domain-boundary",
            "catalog-domain-boundary",
        )),
    );
    let session_id = header.session_id.clone();
    backing.create_session(header).expect("seed Agent session");
    let stores = SessionStores::with_chat_store(backing);
    let catalog = stores.chat_catalog().expect("Chat catalog capability");

    assert!(matches!(
        catalog.get_session_summary(&session_id),
        Err(CatalogError::DomainMismatch {
            expected: SessionDomain::Chat,
            actual: SessionDomain::Agent,
        })
    ));
    assert!(matches!(
        catalog.list_sessions(SessionDomain::Agent, CatalogQuery::first_page()),
        Err(CatalogError::DomainMismatch {
            expected: SessionDomain::Chat,
            actual: SessionDomain::Agent,
        })
    ));
}

#[test]
fn opening_domains_is_independent_and_bounded() {
    let (chat_started_tx, chat_started_rx) = mpsc::sync_channel(1);
    let (chat_release_tx, chat_release_rx) = mpsc::sync_channel(1);
    let (agent_started_tx, agent_started_rx) = mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let stores = SessionStores::open_with(
            move || {
                let _ = chat_started_tx.send(());
                let _ = chat_release_rx.recv();
                DomainStoreState::Ready(SharedStoreCore::new(InMemorySessionStore::new()))
            },
            move || {
                let _ = agent_started_tx.send(());
                DomainStoreState::Ready(SharedStoreCore::new(InMemorySessionStore::new()))
            },
        );
        let _ = finished_tx.send(stores);
    });

    chat_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat open started");
    agent_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Agent open must start independently of blocked Chat");
    let stores = finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("domain open must return after its deadline");
    assert!(stores.agent().is_ok(), "healthy Agent domain was discarded");
    assert!(matches!(
        stores.chat(),
        Err(SessionStoresError::DomainUnavailable {
            domain: SessionDomain::Chat,
            ..
        })
    ));
    let _ = chat_release_tx.send(());
}

#[test]
fn maintenance_attempts_both_ready_domains_and_aggregates_failures() {
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let DomainStoreState::Ready(chat) = &stores.chat else {
        panic!("Chat store should be ready");
    };
    let DomainStoreState::Ready(agent) = &stores.agent else {
        panic!("Agent store should be ready");
    };
    poison(chat);
    poison(agent);

    let error = stores.flush().expect_err("both poisoned stores fail");
    let rendered = error.to_string();
    assert!(rendered.contains("chat"));
    assert!(rendered.contains("agent"));
}

#[test]
fn flush_attempts_both_domains_and_bounds_a_blocked_store() {
    let (chat_started_tx, chat_started_rx) = mpsc::sync_channel(1);
    let (chat_release_tx, chat_release_rx) = mpsc::sync_channel(1);
    let mut chat = InMemorySessionStore::new();
    chat.observe_flush_for_test(chat_started_tx, Some(chat_release_rx));

    let (agent_started_tx, agent_started_rx) = mpsc::sync_channel(1);
    let mut agent = InMemorySessionStore::new();
    agent.observe_flush_for_test(agent_started_tx, None);

    let stores = SessionStores::with_stores(chat, agent);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = finished_tx.send(stores.flush());
    });

    chat_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat flush started");
    agent_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Agent flush must start independently of blocked Chat");
    let result = finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("flush must return a bounded diagnostic");
    assert!(result.is_err(), "blocked Chat flush cannot report success");
    let _ = chat_release_tx.send(());
}

#[test]
fn a_timed_out_flush_closes_only_the_blocked_domain_boundary() {
    let (chat_started_tx, chat_started_rx) = mpsc::sync_channel(1);
    let (chat_release_tx, chat_release_rx) = mpsc::sync_channel(1);
    let mut chat = InMemorySessionStore::new();
    chat.observe_flush_for_test(chat_started_tx, Some(chat_release_rx));

    let agent = InMemorySessionStore::new();
    let agent_header = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new(
            "/tmp/nostra-service-test",
            "service-test",
        )),
    );
    let stores = SessionStores::with_stores(chat, agent);
    let flush_stores = stores.clone();
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let flush_worker = thread::spawn(move || {
        let _ = finished_tx.send(flush_stores.flush());
    });

    chat_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat flush started");
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("timed-out flush result")
            .is_err()
    );

    let chat_capability = stores.chat().expect("Chat capability remains addressable");
    let (mutation_tx, mutation_rx) = mpsc::sync_channel(1);
    let mutation_worker = thread::spawn(move || {
        let mut chat = chat_capability;
        let _ =
            mutation_tx.send(chat.create_session(SessionHeader::new(SessionDomain::Chat, None)));
    });
    let mutation_result = mutation_rx
        .recv_timeout(Duration::from_millis(50))
        .expect("a timed-out domain must reject new work immediately");
    assert!(matches!(
        mutation_result,
        Err(SessionError::StoreShuttingDown)
    ));

    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let (catalog_tx, catalog_rx) = mpsc::sync_channel(1);
    let catalog_worker = thread::spawn(move || {
        let _ =
            catalog_tx.send(catalog.list_sessions(SessionDomain::Chat, CatalogQuery::first_page()));
    });
    let catalog_result = catalog_rx
        .recv_timeout(Duration::from_millis(50))
        .expect("a timed-out catalog must reject reads immediately");
    assert!(matches!(
        catalog_result,
        Err(CatalogError::StoreShuttingDown)
    ));
    catalog_worker.join().expect("catalog worker");

    let _ = chat_release_tx.send(());
    mutation_worker.join().expect("mutation worker");
    flush_worker.join().expect("flush worker");

    let agent = stores.agent().expect("healthy Agent capability");
    let mut agent = agent;
    agent
        .create_session(agent_header)
        .expect("healthy Agent domain remains usable");
}

#[test]
fn shutdown_waits_for_a_reserved_persistence_operation() {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let permit = stores
        .chat()
        .expect("Chat lifecycle capability")
        .reserve_operation()
        .expect("reserve persistence operation");
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let shutdown = stores.clone();
    let worker = thread::spawn(move || {
        let _ = finished_tx.send(shutdown.shutdown());
    });

    assert!(
        finished_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "shutdown reached the store while an already-scheduled write was reserved"
    );
    drop(permit);
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown should resume after the reservation is released")
            .is_ok()
    );
    worker.join().expect("shutdown worker");
}

#[test]
fn concurrent_shutdown_is_rejected_without_waiting_for_the_active_shutdown() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let mut store = InMemorySessionStore::new();
    store.observe_shutdown_for_test(started_tx, Some(release_rx));
    let stores = SessionStores::with_chat_store(store);

    let first_stores = stores.clone();
    let (first_tx, first_rx) = mpsc::sync_channel(1);
    let first_worker = thread::spawn(move || {
        let _ = first_tx.send(first_stores.shutdown());
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first shutdown reached the backing store");

    let second_stores = stores.clone();
    let (second_tx, second_rx) = mpsc::sync_channel(1);
    let second_worker = thread::spawn(move || {
        let _ = second_tx.send(second_stores.shutdown());
    });
    let second_result = second_rx
        .recv_timeout(Duration::from_millis(50))
        .expect("a second shutdown must be rejected before the active shutdown completes");
    assert!(second_result.is_err());

    release_tx.send(()).expect("release first shutdown");
    assert!(
        first_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first shutdown completes")
            .is_ok()
    );
    first_worker.join().expect("first shutdown worker");
    second_worker.join().expect("second shutdown worker");
}

#[test]
fn shutdown_attempts_both_domains_and_bounds_a_blocked_store() {
    let (chat_started_tx, chat_started_rx) = mpsc::sync_channel(1);
    let (chat_release_tx, chat_release_rx) = mpsc::sync_channel(1);
    let mut chat = InMemorySessionStore::new();
    chat.observe_shutdown_for_test(chat_started_tx, Some(chat_release_rx));

    let (agent_started_tx, agent_started_rx) = mpsc::sync_channel(1);
    let mut agent = InMemorySessionStore::new();
    agent.observe_shutdown_for_test(agent_started_tx, None);

    let stores = SessionStores::with_stores(chat, agent);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = finished_tx.send(stores.shutdown());
    });

    chat_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat shutdown started");
    agent_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Agent shutdown must start independently of blocked Chat");
    let result = finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown must return a bounded diagnostic");
    assert!(
        result.is_err(),
        "blocked Chat shutdown cannot report success"
    );
    let _ = chat_release_tx.send(());
}

#[test]
fn lifecycle_mutations_are_rejected_after_shutdown_returns() {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let mut chat = stores.chat().expect("Chat lifecycle capability");
    stores.shutdown().expect("shutdown");

    assert!(matches!(
        chat.create_session(SessionHeader::new(SessionDomain::Chat, None)),
        Err(SessionError::StoreShuttingDown)
    ));
}

#[test]
fn lifecycle_mutations_are_rejected_once_shutdown_begins() {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let DomainStoreState::Ready(core) = &stores.chat else {
        panic!("Chat store should be ready");
    };
    let _operation = core
        .reserve_operation(SessionDomain::Chat)
        .expect("reserve an in-flight operation");
    core.begin_shutdown().expect("begin shutdown");

    let mut chat = stores.chat().expect("Chat lifecycle capability");
    let header = SessionHeader::new(SessionDomain::Chat, None);
    assert!(matches!(
        chat.create_session(header),
        Err(SessionError::StoreShuttingDown)
    ));
}

#[test]
fn shared_agent_project_capability_keeps_project_restore_scoped() {
    let project_a = ProjectIdentity::new("/tmp/project-a", "Project A");
    let project_b = ProjectIdentity::new("/tmp/project-b", "Project B");
    let project_a_id = project_a.project_id.clone();
    let project_b_id = project_b.project_id.clone();

    let header = SessionHeader::new(SessionDomain::Agent, Some(project_b));
    let session_id = header.session_id.clone();
    let mut backing = InMemorySessionStore::new();
    backing
        .create_session(header)
        .expect("create agent session");
    let stores = SessionStores::with_agent_store(backing);
    let projects = stores.agent_projects().expect("Agent project capability");

    assert!(matches!(
        projects.load_project_session(&project_a_id, &session_id, None),
        Err(SessionError::ProjectMismatch {
            expected,
            actual,
            ..
        }) if expected == project_a_id && actual == project_b_id
    ));
}

#[test]
fn shared_agent_project_capability_lists_projects_through_the_backing_store() {
    let project_a = ProjectIdentity::new("/tmp/nostra-service-list-a", "Service Alpha");
    let project_b = ProjectIdentity::new("/tmp/nostra-service-list-b", "Service Beta");
    let project_a_id = project_a.project_id.clone();
    let project_b_id = project_b.project_id.clone();

    let mut backing = InMemorySessionStore::new();
    backing
        .create_session(SessionHeader::new(
            SessionDomain::Agent,
            Some(project_a.clone()),
        ))
        .expect("create a1");
    backing
        .create_session(SessionHeader::new(
            SessionDomain::Agent,
            Some(project_b.clone()),
        ))
        .expect("create b1");
    backing
        .create_session(SessionHeader::new(SessionDomain::Agent, Some(project_a)))
        .expect("create a2");

    let stores = SessionStores::with_agent_store(backing);
    let projects = stores.agent_projects().expect("Agent project capability");

    let page = projects
        .list_projects(ProjectCatalogQuery::first_page())
        .expect("list projects through shared capability");
    assert_eq!(page.projects.len(), 2);

    let by_id: std::collections::HashMap<String, ProjectSummary> = page
        .projects
        .iter()
        .cloned()
        .map(|summary| (summary.project_id.clone(), summary))
        .collect();
    assert_eq!(by_id[&project_a_id].session_count, 2);
    assert_eq!(by_id[&project_b_id].session_count, 1);
    assert!(page.next_cursor.is_none());
}

#[test]
fn shared_agent_project_capability_list_projects_survives_a_poisoned_backing_store() {
    let stores = SessionStores::with_agent_store(InMemorySessionStore::new());
    let DomainStoreState::Ready(core) = &stores.agent else {
        panic!("Agent store should be ready");
    };
    poison(core);
    let projects = stores.agent_projects().expect("Agent project capability");
    assert!(matches!(
        projects.list_projects(ProjectCatalogQuery::first_page()),
        Err(CatalogError::StorePoisoned)
    ));
}
