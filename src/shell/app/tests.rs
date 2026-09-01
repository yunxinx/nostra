use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use gpui::{Global, TestAppContext};
use gpui_component::WindowExt as _;
use reqwest_client::ReqwestClient;

use crate::llm::{
    GatewayGenerationService, GenerationService, HttpTransport, ProviderCatalogSnapshot,
};
use crate::runtime::{
    CHAT_WORKSPACE_ID, ComponentId, CompositionRoot, PROJECT_WORKSPACE_ID, RuntimeServices,
};
use crate::session::{
    ChatSessionController, ChatTurnTerminal, InMemorySessionStore, LocalSessionStore,
    LocalStoreConfig, ProjectCatalogQuery, ProjectIdentity, ProjectSessionStore,
    SessionCatalogStore, SessionDomain, SessionHeader, SessionLifecycleStore, SessionReadStore,
    SessionStores, TurnStatus,
};
use crate::shell::app::chat_workspace::ChatTarget;

use super::*;

type PreferenceSaver = Arc<dyn Fn(&Preferences) -> anyhow::Result<()> + Send + Sync>;

struct TestCompositionRoot(RefCell<Option<CompositionRoot>>);

impl Global for TestCompositionRoot {}

impl Drop for TestCompositionRoot {
    fn drop(&mut self) {
        if let Some(mut root) = self.0.get_mut().take() {
            let _ = futures::executor::block_on(root.close());
        }
    }
}

fn composed_services(
    cx: &mut TestAppContext,
    session_services: SessionStores,
    preference_handle: crate::preferences::PreferenceHandle,
) -> RuntimeServices {
    let generation_service = generation_service();
    let root = CompositionRoot::builder(session_services.clone())
        .with_preferences(preference_handle)
        .register_generation_provider(
            ComponentId::new("nostra.generation.test"),
            generation_service,
        )
        .select_generation_provider(ComponentId::new("nostra.generation.test"))
        .build_blocking()
        .expect("valid test composition");
    let services = root.services().expect("active test services");
    cx.update(|cx| cx.set_global(TestCompositionRoot(RefCell::new(Some(root)))));
    services
}

fn default_composed_services(
    cx: &mut TestAppContext,
    session_services: SessionStores,
    preference_handle: crate::preferences::PreferenceHandle,
) -> RuntimeServices {
    let root = CompositionRoot::builder(session_services)
        .with_preferences(preference_handle)
        .build_blocking()
        .expect("valid default composition");
    let services = root.services().expect("active default services");
    cx.update(|cx| cx.set_global(TestCompositionRoot(RefCell::new(Some(root)))));
    services
}

fn generation_service() -> Arc<dyn GenerationService> {
    Arc::new(GatewayGenerationService::new(
        ProviderCatalogSnapshot::new(Vec::new()),
        HttpTransport::new(Arc::new(ReqwestClient::new())),
        None,
    ))
}

fn seed_persisted_conversation(
    stores: &SessionStores,
    project: Option<ProjectIdentity>,
) -> SessionId {
    let store = if project.is_some() {
        stores.agent().expect("Agent lifecycle store")
    } else {
        stores.chat().expect("Chat lifecycle store")
    };
    let mut controller = match project {
        Some(project) => ChatSessionController::for_project(store, project),
        None => ChatSessionController::new(store),
    };
    let message = crate::llm::Message {
        role: crate::llm::Role::User,
        content: vec![crate::llm::ContentBlock::Text {
            text: "persisted fixture".into(),
            provider_metadata: crate::llm::ProviderMetadata::default(),
        }],
        provider_metadata: crate::llm::ProviderMetadata::default(),
    };
    let selection = crate::llm::ModelSelection {
        profile_id: "fixture-profile".into(),
        model_id: "fixture-model".into(),
    };
    let start = controller
        .begin_turn(message, selection, "fixture-turn")
        .expect("begin persisted fixture");
    controller
        .finish_turn("fixture-turn", &ChatTurnTerminal::cancelled())
        .expect("finish persisted fixture");
    start.session_id
}

fn add_app_window(cx: &mut TestAppContext) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    add_app_window_with_stores(cx, None)
}

fn add_app_window_with_stores(
    cx: &mut TestAppContext,
    stores: Option<SessionStores>,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    add_app_window_with_preferences_and_stores(cx, Preferences::default(), stores)
}

fn add_app_window_with_preferences(
    cx: &mut TestAppContext,
    prefs: Preferences,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    add_app_window_with_preferences_and_stores(cx, prefs, None)
}

fn add_app_window_with_preferences_and_stores(
    cx: &mut TestAppContext,
    prefs: Preferences,
    stores: Option<SessionStores>,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    let session_services = stores.clone().unwrap_or_default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        crate::preferences::init_global(prefs.clone(), cx);
    });
    let preference_handle = cx.update(|cx| crate::preferences::handle(cx));
    let services = composed_services(cx, session_services, preference_handle);
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let app = cx.new(|cx| ChatApp::new(prefs.clone(), services.clone(), window, cx));
        Root::new(app, window, cx)
    });
    let app = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ChatApp>()
            .expect("Root must contain the ChatApp")
    });
    (app, cx)
}

fn add_app_window_with_default_composition(
    cx: &mut TestAppContext,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    let prefs = Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        crate::preferences::init_global(prefs.clone(), cx);
    });
    let preference_handle = cx.update(|cx| crate::preferences::handle(cx));
    let services = default_composed_services(cx, SessionStores::default(), preference_handle);
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let app = cx.new(|cx| ChatApp::new(prefs.clone(), services.clone(), window, cx));
        Root::new(app, window, cx)
    });
    let app = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ChatApp>()
            .expect("Root must contain the ChatApp")
    });
    (app, cx)
}

fn add_app_window_with_replaceable_generation(
    cx: &mut TestAppContext,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    const REPLACEMENT_PROVIDER: ComponentId = ComponentId::new("nostra.generation.replacement");
    let prefs = Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        crate::preferences::init_global(prefs.clone(), cx);
    });
    let preference_handle = cx.update(|cx| crate::preferences::handle(cx));
    let root = CompositionRoot::builder(SessionStores::default())
        .with_preferences(preference_handle)
        .register_generation_provider(REPLACEMENT_PROVIDER, generation_service())
        .build_blocking()
        .expect("replaceable default composition");
    let services = root.services().expect("active replaceable services");
    cx.update(|cx| cx.set_global(TestCompositionRoot(RefCell::new(Some(root)))));
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let app = cx.new(|cx| ChatApp::new(prefs.clone(), services.clone(), window, cx));
        Root::new(app, window, cx)
    });
    let app = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ChatApp>()
            .expect("Root must contain the ChatApp")
    });
    (app, cx)
}

fn add_app_window_with_saver(
    cx: &mut TestAppContext,
    stores: SessionStores,
    saver: PreferenceSaver,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    let prefs = Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        crate::preferences::init_global(prefs.clone(), cx);
    });
    let preference_handle =
        crate::preferences::PreferenceHandle::with_saver(prefs.clone(), saver.clone());
    let services = composed_services(cx, stores.clone(), preference_handle);
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let app = cx.new(|cx| ChatApp::new(prefs.clone(), services.clone(), window, cx));
        Root::new(app, window, cx)
    });
    let app = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ChatApp>()
            .expect("Root must contain the ChatApp")
    });
    (app, cx)
}

#[gpui::test]
fn app_shutdown_hook_survives_release_of_the_main_view(cx: &mut TestAppContext) {
    let (chat_started_tx, chat_started_rx) = mpsc::sync_channel(1);
    let mut chat = InMemorySessionStore::new();
    chat.observe_shutdown_for_test(chat_started_tx, None);
    let (agent_started_tx, agent_started_rx) = mpsc::sync_channel(1);
    let mut agent = InMemorySessionStore::new();
    agent.observe_shutdown_for_test(agent_started_tx, None);
    let stores = SessionStores::with_stores(chat, agent);
    let preferences_saved = Arc::new(AtomicBool::new(false));
    let preferences_saved_for_test = Arc::clone(&preferences_saved);
    let saver: PreferenceSaver = Arc::new(move |_| {
        preferences_saved_for_test.store(true, Ordering::Release);
        Ok(())
    });
    let (app, cx) = add_app_window_with_saver(cx, stores, saver);
    drop(app);

    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    cx.cx.update(|cx| cx.shutdown());
    chat_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat shutdown must not depend on a weak ChatApp");
    agent_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Agent shutdown must not depend on a weak ChatApp");
    assert!(
        preferences_saved.load(Ordering::Acquire),
        "exit preferences must be attempted even when the window is already gone"
    );
}

#[gpui::test]
fn native_close_enters_the_pre_quit_durability_barrier(cx: &mut TestAppContext) {
    let (chat_started_tx, chat_started_rx) = mpsc::sync_channel(1);
    let mut chat = InMemorySessionStore::new();
    chat.observe_shutdown_for_test(chat_started_tx, None);
    let (agent_started_tx, agent_started_rx) = mpsc::sync_channel(1);
    let mut agent = InMemorySessionStore::new();
    agent.observe_shutdown_for_test(agent_started_tx, None);
    let preferences_saved = Arc::new(AtomicBool::new(false));
    let preferences_saved_for_test = Arc::clone(&preferences_saved);
    let saver: PreferenceSaver = Arc::new(move |_| {
        preferences_saved_for_test.store(true, Ordering::Release);
        Ok(())
    });
    let (_app, cx) = add_app_window_with_saver(cx, SessionStores::with_stores(chat, agent), saver);

    assert!(
        !cx.simulate_close(),
        "native close must wait for the asynchronous pre-quit barrier"
    );
    cx.run_until_parked();

    chat_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat shutdown must be attempted by native close");
    agent_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Agent shutdown must be attempted by native close");
    assert!(
        preferences_saved.load(Ordering::Acquire),
        "native close must save preferences alongside session shutdown"
    );
}

#[gpui::test]
fn native_main_window_close_persists_the_active_turn_terminal(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let store = LocalSessionStore::open(config.clone()).expect("open local Chat store");
    let stores = SessionStores::with_chat_store(store);
    let (app, cx) = add_app_window_with_stores(cx, Some(stores));
    let dropped = Rc::new(Cell::new(false));

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            let view = this.chat_workspace().read(cx).conversations.conversations()[0]
                .view
                .clone();
            assert!(view.update(cx, |chat, cx| {
                chat.start_durable_pending_reply_for_test(Rc::clone(&dropped), window, cx)
            }));
        });
    });
    cx.run_until_parked();
    let session_id = app.read_with(cx, |this, cx| {
        this.chat_snapshot().conversations()[0]
            .view()
            .read_with(cx, |chat, _| chat.durable_session_id_for_test())
            .expect("durable Chat session id")
    });

    drop(app);
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    cx.cx.update(|cx| cx.shutdown());

    let mut reopened = LocalSessionStore::open(config).expect("reopen local Chat store");
    let _ = reopened
        .repair_if_needed()
        .expect("settle any projection obligation");
    let restored = reopened
        .load_session(&session_id, None)
        .expect("restore closed-window turn");
    assert_eq!(restored.turn_results.len(), 1);
    assert_eq!(
        restored.turn_results[0].result.status,
        TurnStatus::Cancelled
    );
}

#[gpui::test]
fn confirmed_delete_removes_the_persisted_session_before_dropping_the_view(
    cx: &mut TestAppContext,
) {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let (app, cx) = add_app_window_with_stores(cx, Some(stores));
    let (target, session_id) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            let view = this.chat_workspace().read(cx).conversations.conversations()[0]
                .view
                .clone();
            let session_id = view.update(cx, |chat, cx| chat.persist_session_for_test(cx));
            (view.entity_id(), session_id)
        })
    });
    assert!(
        catalog
            .get_session_summary(&session_id)
            .expect("read persisted summary")
            .is_some()
    );

    cx.update(|window, cx| {
        app.update(cx, |this, cx| this.delete_conversation(target, window, cx));
    });
    cx.run_until_parked();

    assert!(
        catalog
            .get_session_summary(&session_id)
            .expect("read summary after deletion")
            .is_none(),
        "confirmed deletion left the durable session behind"
    );
    app.read_with(cx, |this, _| {
        assert!(
            this.chat_snapshot()
                .conversations()
                .iter()
                .all(|conversation| conversation.view().entity_id() != target)
        );
    });
}

fn redraw(cx: &mut gpui::VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect("element should be drawn");
    cx.simulate_click(bounds.center(), Default::default());
    redraw(cx);
}

#[gpui::test]
fn deleting_conversations_releases_views_and_owned_subscriptions(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);

    let first_removed = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            for _ in 1..20 {
                this.spawn_draft(window, cx);
            }
            assert_eq!(this.chat_snapshot().conversations().len(), 20);

            let mut removed = Vec::new();
            while this.chat_snapshot().conversations().len() > 1 {
                let view = this.chat_snapshot().conversations()[0].view();
                let view = view.downgrade();
                let target = this.chat_snapshot().conversations()[0].target();
                removed.push(view);
                this.delete_conversation(target, window, cx);
            }
            assert_eq!(this.chat_snapshot().conversations().len(), 1);
            assert!(this.chat_snapshot().active().is_some());
            assert_eq!(
                this.chat_snapshot()
                    .conversations()
                    .iter()
                    .filter(|_| true)
                    .count(),
                1
            );
            removed
        })
    });
    cx.run_until_parked();
    assert!(first_removed.iter().all(|view| view.upgrade().is_none()));
    cx.update(|_, cx| {
        app.read_with(cx, |this, cx| {
            assert_eq!(
                this.chat_workspace()
                    .read(cx)
                    .runtime_services
                    .scope_count(),
                3
            )
        });
    });

    let second_removed = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            for _ in 1..20 {
                this.spawn_draft(window, cx);
            }
            let mut removed = Vec::new();
            while this.chat_snapshot().conversations().len() > 1 {
                let view = this.chat_snapshot().conversations()[0].view().downgrade();
                let target = this.chat_snapshot().conversations()[0].target();
                removed.push(view);
                this.delete_conversation(target, window, cx);
            }
            assert_eq!(this.chat_snapshot().conversations().len(), 1);
            assert!(this.chat_snapshot().active().is_some());
            removed
        })
    });
    cx.run_until_parked();
    assert!(second_removed.iter().all(|view| view.upgrade().is_none()));
    cx.update(|_, cx| {
        app.read_with(cx, |this, cx| {
            assert_eq!(
                this.chat_workspace()
                    .read(cx)
                    .runtime_services
                    .scope_count(),
                3
            )
        });
    });

    let third_removed = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            for _ in 1..20 {
                this.spawn_draft(window, cx);
            }
            let mut removed = Vec::new();
            while this.chat_snapshot().conversations().len() > 1 {
                let view = this.chat_snapshot().conversations()[0].view().downgrade();
                let target = this.chat_snapshot().conversations()[0].target();
                removed.push(view);
                this.delete_conversation(target, window, cx);
            }
            assert_eq!(this.chat_snapshot().conversations().len(), 1);
            assert!(this.chat_snapshot().active().is_some());
            removed
        })
    });
    cx.run_until_parked();
    assert!(third_removed.iter().all(|view| view.upgrade().is_none()));
    cx.update(|_, cx| {
        app.read_with(cx, |this, cx| {
            assert_eq!(
                this.chat_workspace()
                    .read(cx)
                    .runtime_services
                    .scope_count(),
                3
            )
        });
    });
}

#[gpui::test]
fn active_and_last_conversation_deletion_choose_deterministically(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            let middle = this.chat_snapshot().conversations()[1].target();
            let next = this.chat_snapshot().conversations()[2].target();
            this.chat_workspace()
                .update(cx, |workspace, cx| workspace.select_target(middle, cx));
            this.delete_conversation(middle, window, cx);
            assert_eq!(this.chat_snapshot().active(), Some(next));
            assert_eq!(
                this.chat_snapshot()
                    .active_view()
                    .expect("active view")
                    .entity_id(),
                next
            );

            let before = this
                .chat_snapshot()
                .active_view()
                .expect("active view")
                .entity_id();
            let non_active = this.chat_snapshot().conversations()[0].target();
            this.delete_conversation(non_active, window, cx);
            assert_eq!(this.chat_snapshot().active(), Some(before));
            assert_eq!(
                this.chat_snapshot()
                    .active_view()
                    .expect("active view")
                    .entity_id(),
                before
            );

            let only = this
                .chat_snapshot()
                .active_view()
                .expect("active view")
                .downgrade();
            let only_id = this
                .chat_snapshot()
                .active_view()
                .expect("active view")
                .entity_id();
            this.delete_conversation(only_id, window, cx);
            assert!(this.chat_snapshot().conversations().is_empty());
            assert!(this.chat_snapshot().active().is_none());
            drop(only);
        });
    });
}

#[gpui::test]
fn deleting_a_streaming_conversation_cancels_its_task_without_resurrection(
    cx: &mut TestAppContext,
) {
    let (app, cx) = add_app_window(cx);
    let dropped = Rc::new(Cell::new(false));
    let (target, weak) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            let view = this.chat_workspace().read(cx).conversations.conversations()[0]
                .view
                .clone();
            view.update(cx, |chat, cx| {
                chat.start_pending_reply_for_test(dropped.clone(), cx)
            });
            (view.entity_id(), view.downgrade())
        })
    });
    cx.run_until_parked();
    assert!(!dropped.get());

    cx.update(|window, cx| {
        app.update(cx, |this, cx| this.delete_conversation(target, window, cx));
    });
    cx.run_until_parked();

    assert!(dropped.get());
    assert!(weak.upgrade().is_none());
    assert_eq!(
        app.read_with(cx, |this, _| this.chat_snapshot().conversations().len()),
        0
    );
}

#[gpui::test]
fn delete_confirmation_keeps_the_original_target_after_switching(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    let (target, selected) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            let target = this.chat_snapshot().conversations()[0].target();
            let selected = this.chat_snapshot().conversations()[2].target();
            this.chat_workspace().update(cx, |workspace, cx| {
                workspace.select_target(target, cx);
                workspace.begin_delete_confirmation(ChatTarget::View(target), window, cx);
            });
            this.select(2, window, cx);
            (target, selected)
        })
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            let target = this
                .chat_snapshot()
                .confirming()
                .cloned()
                .expect("delete target");
            this.chat_workspace().update(cx, |workspace, cx| {
                workspace.confirm_delete_target(target, window, cx)
            });
        });
    });
    cx.run_until_parked();

    app.read_with(cx, |this, _| {
        assert_eq!(this.chat_snapshot().conversations().len(), 2);
        assert!(
            this.chat_snapshot()
                .conversations()
                .iter()
                .all(|conversation| conversation.view().entity_id() != target)
        );
        assert_eq!(this.chat_snapshot().active(), Some(selected));
    });
}

#[gpui::test]
fn inline_confirm_target_survives_selection_switch(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    let (target, selected) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            let target = this.chat_snapshot().conversations()[0].target();
            let selected = this.chat_snapshot().conversations()[2].target();
            this.chat_workspace()
                .update(cx, |workspace, cx| workspace.select_target(target, cx));
            (target, selected)
        })
    });
    redraw(cx);
    let actions = Box::leak(format!("conversation-actions-{}", target.as_u64()).into_boxed_str());
    click(cx, actions);
    cx.simulate_keystrokes("down enter");
    redraw(cx);
    assert_eq!(
        app.read_with(cx, |this, _| { this.chat_snapshot().confirming().cloned() }),
        Some(ChatTarget::View(target))
    );

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.select(2, window, cx);
        });
    });
    redraw(cx);

    let confirm = Box::leak(
        format!("conversation-delete-confirm-{}-confirm", target.as_u64()).into_boxed_str(),
    );
    click(cx, confirm);

    app.read_with(cx, |this, _| {
        assert_eq!(this.chat_snapshot().conversations().len(), 2);
        assert!(
            this.chat_snapshot()
                .conversations()
                .iter()
                .all(|conversation| conversation.view().entity_id() != target)
        );
        assert_eq!(this.chat_snapshot().active(), Some(selected));
        assert_eq!(this.chat_snapshot().confirming(), None);
    });
}

#[gpui::test]
fn agent_draft_uses_the_shared_inline_delete_interaction(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().expect("project folder");
    let project = ProjectIdentity::new(folder.path(), "Draft project");
    let prefs = Preferences {
        agent_projects: vec![crate::preferences::AgentProjectRecord {
            project_id: project.project_id.clone(),
            canonical_path: project.canonical_path.clone(),
            display_name: project.display_name.clone(),
        }],
        ..Preferences::default()
    };
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let (app, cx) = add_app_window_with_preferences_and_stores(cx, prefs, Some(stores));
    let target = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.switch_workspace(PROJECT_WORKSPACE_ID, window, cx);
            this.project_workspace().update(cx, |workspace, cx| {
                workspace.open_draft(project.project_id.clone(), window, cx)
            });
            this.project_workspace()
                .read(cx)
                .snapshot()
                .active()
                .expect("active Agent draft")
        })
    });
    redraw(cx);

    let actions =
        Box::leak(format!("agent-conversation-actions-{}", target.as_u64()).into_boxed_str());
    click(cx, actions);
    cx.simulate_keystrokes("down enter");
    redraw(cx);
    assert_eq!(
        app.read_with(cx, |this, _| {
            this.project_snapshot().confirming().cloned()
        }),
        Some(ProjectTarget::View(target))
    );

    let confirm = Box::leak(
        format!(
            "agent-conversation-delete-confirm-{}-confirm",
            target.as_u64()
        )
        .into_boxed_str(),
    );
    click(cx, confirm);

    app.read_with(cx, |this, _| {
        assert!(this.project_snapshot().conversations().is_empty());
        assert!(this.project_snapshot().active().is_none());
        assert_eq!(this.project_snapshot().confirming(), None);
    });
}

#[gpui::test]
fn deleting_the_active_agent_reveals_its_inline_confirmation_anchor(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().expect("project folder");
    let project = ProjectIdentity::new(folder.path(), "Draft project");
    let prefs = Preferences {
        agent_projects: vec![crate::preferences::AgentProjectRecord {
            project_id: project.project_id.clone(),
            canonical_path: project.canonical_path.clone(),
            display_name: project.display_name.clone(),
        }],
        ..Preferences::default()
    };
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let (app, cx) = add_app_window_with_preferences_and_stores(cx, prefs, Some(stores));
    let target = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.switch_workspace(PROJECT_WORKSPACE_ID, window, cx);
            this.project_workspace().update(cx, |workspace, cx| {
                workspace.open_draft(project.project_id.clone(), window, cx);
                workspace.toggle_project(project.project_id.clone(), cx);
            });
            this.collapsed = true;
            this.request_delete_active(window, cx);
            this.project_workspace()
                .read(cx)
                .snapshot()
                .active()
                .expect("active Agent draft")
        })
    });
    redraw(cx);

    app.read_with(cx, |this, _| {
        assert!(!this.collapsed);
        assert!(
            this.project_snapshot()
                .catalog()
                .is_project_expanded(&project.project_id)
        );
        assert_eq!(
            this.project_snapshot().confirming(),
            Some(&ProjectTarget::View(target))
        );
    });
    let confirm = Box::leak(
        format!(
            "agent-conversation-delete-confirm-{}-confirm",
            target.as_u64()
        )
        .into_boxed_str(),
    );
    assert!(cx.debug_bounds(confirm).is_some());
}

#[test]
fn delete_chat_labels_resolve_in_every_locale() {
    for locale in ["en", "zh-CN"] {
        for key in [
            "sidebar.delete_chat",
            "sidebar.delete_chat_title",
            "sidebar.delete_chat_confirm",
            "sidebar.delete_chat_cancel",
            "sidebar.more_actions",
            "menu.delete_chat",
            "chat.error.runtime_unavailable",
            "chat.error.persistence_delete_failed",
        ] {
            assert_ne!(t!(key, locale = locale).to_string(), key);
        }
        assert!(
            t!(
                "sidebar.delete_chat_description",
                locale = locale,
                title = "fixture"
            )
            .contains("fixture")
        );
    }
}

#[gpui::test]
fn unavailable_conversation_runtime_reports_a_notification_without_creating_a_draft(
    cx: &mut TestAppContext,
) {
    let (app, cx) = add_app_window(cx);
    cx.update(|_, cx| {
        let mut root = cx
            .global::<TestCompositionRoot>()
            .0
            .borrow_mut()
            .take()
            .expect("test composition root");
        futures::executor::block_on(root.close()).expect("close test composition");
    });

    cx.update(|window, cx| {
        let notification_count = window.notifications(cx).len();
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            assert!(this.chat_snapshot().conversations().is_empty());
        });
        assert_eq!(window.notifications(cx).len(), notification_count + 1);
    });
}

#[gpui::test]
fn runtime_snapshot_updates_are_applied_to_app_owned_state(cx: &mut TestAppContext) {
    const DEFAULT_PROVIDER: ComponentId = ComponentId::new("nostra.generation.gateway");
    const REPLACEMENT_PROVIDER: ComponentId = ComponentId::new("nostra.generation.replacement");
    let (app, cx) = add_app_window_with_replaceable_generation(cx);
    let initial_revision = app.read_with(cx, |this, _| {
        assert!(
            this.runtime_snapshot_for_test()
                .snapshot()
                .components()
                .iter()
                .any(|component| component.component() == DEFAULT_PROVIDER)
        );
        this.runtime_snapshot_for_test().revision()
    });

    cx.update(|_, cx| {
        let root = cx.global::<TestCompositionRoot>();
        let mut root = root.0.borrow_mut();
        let root = root.as_mut().expect("test composition root");
        assert!(
            futures::executor::block_on(root.replace_generation_provider(REPLACEMENT_PROVIDER))
                .expect("replace generation Provider")
        );
    });
    cx.run_until_parked();

    app.read_with(cx, |this, _| {
        let update = this.runtime_snapshot_for_test();
        assert!(update.revision() > initial_revision);
        assert!(
            update
                .snapshot()
                .components()
                .iter()
                .any(|component| component.component() == REPLACEMENT_PROVIDER)
        );
        assert!(
            update
                .snapshot()
                .components()
                .iter()
                .all(|component| component.component() != DEFAULT_PROVIDER)
        );
    });
}

#[gpui::test]
fn startup_shows_empty_workspace_without_creating_a_draft(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    app.read_with(cx, |this, _| {
        assert!(
            this.chat_snapshot().conversations().is_empty(),
            "no draft on startup"
        );
        assert!(
            this.chat_snapshot().active().is_none(),
            "no active target on startup"
        );
        assert!(this.chat_snapshot().opened_session_index().is_empty());
    });
}

#[gpui::test]
fn workspace_host_registers_and_owns_the_builtin_workspaces(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window_with_default_composition(cx);

    app.read_with(cx, |this, cx| {
        let definitions = this.workspace_host.registry_snapshot().definitions();
        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0].id(), CHAT_WORKSPACE_ID);
        assert_eq!(definitions[0].order(), 10);
        assert_eq!(definitions[1].id(), PROJECT_WORKSPACE_ID);
        assert_eq!(definitions[1].order(), 20);
        assert_eq!(
            this.workspace_host.registry_snapshot().scope(),
            this.chat_workspace()
                .read(cx)
                .runtime_services
                .window_scope()
        );

        let chat_workspace = this.chat_workspace();
        let project_workspace = this.project_workspace();
        assert_eq!(
            chat_workspace.entity_id(),
            this.workspace_host.chat_workspace().entity_id()
        );
        assert_eq!(
            project_workspace.entity_id(),
            this.workspace_host.project_workspace().entity_id()
        );
        assert_ne!(chat_workspace.entity_id(), project_workspace.entity_id());
        assert!(
            chat_workspace
                .read(cx)
                .snapshot()
                .conversations()
                .is_empty()
        );
        assert!(
            project_workspace
                .read(cx)
                .snapshot()
                .conversations()
                .is_empty()
        );
    });

    let expected_markdown_revision = app.read_with(cx, |this, cx| {
        this.chat_workspace()
            .read(cx)
            .runtime_services
            .markdown_extensions()
            .revision()
    });
    let active_view = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            this.chat_snapshot()
                .active_view()
                .expect("default composition creates an active Chat view")
        })
    });
    assert_eq!(
        active_view.read_with(cx, |view, _| view.markdown_extension_revision()),
        expected_markdown_revision
    );
}

#[gpui::test]
fn startup_restores_the_last_workspace_when_enabled(cx: &mut TestAppContext) {
    let prefs = Preferences {
        restore_last_workspace_on_start: true,
        last_workspace_id: PROJECT_WORKSPACE_ID,
        ..Preferences::default()
    };
    let (app, _) = add_app_window_with_preferences(cx, prefs);

    app.read_with(cx, |this, _| {
        assert_eq!(this.workspace_id, PROJECT_WORKSPACE_ID);
    });
}

#[gpui::test]
fn startup_uses_chat_when_workspace_restore_is_disabled(cx: &mut TestAppContext) {
    let prefs = Preferences {
        restore_last_workspace_on_start: false,
        last_workspace_id: PROJECT_WORKSPACE_ID,
        ..Preferences::default()
    };
    let (app, _) = add_app_window_with_preferences(cx, prefs);

    app.read_with(cx, |this, _| {
        assert_eq!(this.workspace_id, CHAT_WORKSPACE_ID);
    });
}

#[gpui::test]
fn disabled_workspace_restore_still_records_an_explicit_mode(cx: &mut TestAppContext) {
    let prefs = Preferences {
        restore_last_workspace_on_start: false,
        ..Preferences::default()
    };
    let (app, cx) = add_app_window_with_preferences(cx, prefs);

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.switch_workspace(PROJECT_WORKSPACE_ID, window, cx);
        });
    });

    app.read_with(cx, |this, _| {
        assert_eq!(this.workspace_id, PROJECT_WORKSPACE_ID);
    });
    cx.update(|_, cx| {
        let prefs = crate::preferences::get(cx);
        assert!(!prefs.restore_last_workspace_on_start);
        assert_eq!(prefs.last_workspace_id, PROJECT_WORKSPACE_ID);
    });
}

#[gpui::test]
fn deleting_an_agent_draft_discards_only_the_unpersisted_view(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().expect("project folder");
    let project = ProjectIdentity::new(folder.path(), "Draft project");
    let prefs = Preferences {
        agent_projects: vec![crate::preferences::AgentProjectRecord {
            project_id: project.project_id.clone(),
            canonical_path: project.canonical_path.clone(),
            display_name: project.display_name.clone(),
        }],
        ..Preferences::default()
    };
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let project_store = stores.agent_projects().expect("Agent project store");
    let (app, cx) = add_app_window_with_preferences_and_stores(cx, prefs, Some(stores));

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.switch_workspace(PROJECT_WORKSPACE_ID, window, cx);
            this.project_workspace().update(cx, |workspace, cx| {
                workspace.open_draft(project.project_id.clone(), window, cx);
                let target = workspace.snapshot().active().expect("active Agent draft");
                workspace.confirm_delete_target(ProjectTarget::View(target), window, cx);
            });
        });
    });

    app.read_with(cx, |this, cx| {
        let snapshot = this.project_workspace().read(cx).snapshot();
        assert!(snapshot.conversations().is_empty());
        assert!(snapshot.active().is_none());
        assert!(snapshot.catalog().open().is_none());
    });
    let page = project_store
        .list_project_sessions(
            &project.project_id,
            crate::session::CatalogQuery::first_page(),
        )
        .expect("list project sessions");
    assert!(page.sessions.is_empty());
    assert!(folder.path().exists());
}

#[gpui::test]
fn deleting_an_agent_project_removes_sessions_and_preferences_but_keeps_the_folder(
    cx: &mut TestAppContext,
) {
    let folder = tempfile::tempdir().expect("project folder");
    let project = ProjectIdentity::new(folder.path(), "Persistent project");
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let mut lifecycle = stores.agent().expect("Agent lifecycle store");
    let session_header = SessionHeader::new(SessionDomain::Agent, Some(project.clone()));
    let session_id = session_header.session_id.clone();
    lifecycle
        .create_session(session_header)
        .expect("create Agent session");
    let project_store = stores.agent_projects().expect("Agent project store");
    let summary = project_store
        .list_projects(ProjectCatalogQuery::first_page())
        .expect("list projects")
        .projects
        .into_iter()
        .next()
        .expect("project summary");
    let prefs = Preferences {
        agent_projects: vec![crate::preferences::AgentProjectRecord {
            project_id: project.project_id.clone(),
            canonical_path: project.canonical_path.clone(),
            display_name: project.display_name.clone(),
        }],
        ..Preferences::default()
    };
    let (app, cx) = add_app_window_with_preferences_and_stores(cx, prefs, Some(stores));

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.switch_workspace(PROJECT_WORKSPACE_ID, window, cx);
            this.project_workspace().update(cx, |workspace, cx| {
                workspace.open_session(project.project_id.clone(), session_id.clone(), window, cx);
            });
        });
    });
    cx.run_until_parked();
    app.read_with(cx, |this, cx| {
        let workspace = this.project_workspace().read(cx);
        assert_eq!(workspace.snapshot().conversations().len(), 1);
        assert_eq!(workspace.runtime_services.scope_count(), 3);
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.project_workspace().update(cx, |workspace, cx| {
                workspace.confirm_delete_target(
                    ProjectTarget::Project(summary.project_id.clone()),
                    window,
                    cx,
                )
            });
        });
    });
    cx.run_until_parked();

    let page = project_store
        .list_project_sessions(
            &project.project_id,
            crate::session::CatalogQuery::first_page(),
        )
        .expect("list sessions after delete");
    assert!(page.sessions.is_empty());
    cx.update(|_, cx| {
        assert!(crate::preferences::get(cx).agent_projects.is_empty());
    });
    assert!(folder.path().exists());
    app.read_with(cx, |this, cx| {
        assert!(
            !this
                .project_snapshot()
                .is_deleting_project(&project.project_id)
        );
        let workspace = this.project_workspace().read(cx);
        assert!(workspace.snapshot().conversations().is_empty());
        assert_eq!(workspace.runtime_services.scope_count(), 2);
    });
}

fn assert_sidebar_content_alignment(cx: &mut gpui::VisualTestContext) {
    redraw(cx);
    let top = cx
        .debug_bounds("sidebar-top-reserved")
        .expect("sidebar top reservation");
    let list = cx
        .debug_bounds("sidebar-list-surface")
        .expect("sidebar list surface");
    let account = cx
        .debug_bounds("sidebar-account-boundary")
        .expect("sidebar account boundary");
    let search = cx
        .debug_bounds("sidebar-search-boundary")
        .expect("sidebar search boundary");

    assert_eq!(list.left(), account.left());
    assert_eq!(list.right(), search.right());
    assert_eq!(list.top() - top.bottom(), SIDEBAR_CONTENT_INSET);
}

#[gpui::test]
fn chat_sidebar_uses_the_shared_content_inset(cx: &mut TestAppContext) {
    let (_, cx) = add_app_window(cx);
    assert_sidebar_content_alignment(cx);
}

#[gpui::test]
fn project_sidebar_uses_the_shared_content_inset(cx: &mut TestAppContext) {
    let prefs = Preferences {
        last_workspace_id: PROJECT_WORKSPACE_ID,
        ..Preferences::default()
    };
    let (_, cx) = add_app_window_with_preferences(cx, prefs);
    assert_sidebar_content_alignment(cx);
}

#[gpui::test]
fn account_work_mode_submenu_switches_and_records_the_selected_workspace(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    redraw(cx);
    let account = cx
        .debug_bounds("sidebar-account-boundary")
        .expect("account menu trigger");
    cx.simulate_click(account.center(), Default::default());
    redraw(cx);

    // Settings is initially selected; move to Work mode, enter its submenu,
    // then select Project.
    cx.simulate_keystrokes("down down right down enter");
    redraw(cx);

    app.read_with(cx, |this, _| {
        assert_eq!(this.workspace_id, PROJECT_WORKSPACE_ID);
    });
    cx.update(|_, cx| {
        assert_eq!(
            crate::preferences::get(cx).last_workspace_id,
            PROJECT_WORKSPACE_ID
        );
    });
}

#[gpui::test]
fn new_chat_creates_a_draft_without_a_session_id(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.new_chat(window, cx);
        });
    });
    app.read_with(cx, |this, _| {
        assert_eq!(this.chat_snapshot().conversations().len(), 1);
        assert!(
            this.chat_snapshot().conversations()[0]
                .session_id()
                .is_none(),
            "draft has no session id"
        );
        assert!(this.chat_snapshot().active().is_some());
        assert!(this.chat_snapshot().opened_session_index().is_empty());
    });
}

#[gpui::test]
fn switching_active_target_does_not_cancel_other_streams(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    let dropped = Rc::new(Cell::new(false));
    let (first, _second) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            this.spawn_draft(window, cx);
            let first = this.chat_snapshot().conversations()[0].view();
            let second = this.chat_snapshot().conversations()[1].view();
            first.update(cx, |chat, cx| {
                chat.start_pending_reply_for_test(Rc::clone(&dropped), cx)
            });
            this.chat_workspace().update(cx, |workspace, cx| {
                workspace.select_target(second.entity_id(), cx)
            });
            (first.entity_id(), second.entity_id())
        })
    });
    cx.run_until_parked();
    assert!(!dropped.get(), "first stream must still be running");

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.select_target(first, window, cx);
        });
    });
    cx.run_until_parked();
    assert!(
        !dropped.get(),
        "switching back must not cancel the first stream"
    );

    app.read_with(cx, |this, _| {
        assert_eq!(this.chat_snapshot().active(), Some(first));
        assert_eq!(this.chat_snapshot().conversations().len(), 2);
    });
}

#[gpui::test]
fn deleting_the_last_conversation_returnss_to_empty_workspace(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            let target = this.chat_snapshot().conversations()[0].target();
            this.delete_conversation(target, window, cx);
        });
    });
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        assert!(this.chat_snapshot().conversations().is_empty());
        assert!(this.chat_snapshot().active().is_none());
    });
}

#[gpui::test]
fn select_session_reuses_an_already_opened_view(cx: &mut TestAppContext) {
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (app, cx) = add_app_window_with_stores(cx, Some(stores));
    let session_id = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            {
                let view = this.chat_snapshot().conversations()[0].view();
                view.update(cx, |chat, cx| chat.persist_session_for_test(cx))
            }
        })
    });
    cx.run_until_parked();
    let opened_target = app.read_with(cx, |this, _| {
        assert_eq!(this.chat_snapshot().opened_session_index().len(), 1);
        *this
            .chat_snapshot()
            .opened_session_index()
            .get(&session_id)
            .expect("session bound")
    });

    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_draft(window, cx);
            this.select_session(session_id, window, cx);
        });
    });
    cx.run_until_parked();
    app.read_with(cx, |this, _| {
        assert_eq!(
            this.chat_snapshot().active(),
            Some(opened_target),
            "reuse without spawning a new view"
        );
        assert_eq!(this.chat_snapshot().conversations().len(), 2);
    });
}

#[gpui::test]
fn selecting_a_chat_draft_invalidates_an_older_session_restore(cx: &mut TestAppContext) {
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let session_id = seed_persisted_conversation(&stores, None);
    let (app, cx) = add_app_window_with_stores(cx, Some(stores));

    let draft_target = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.select_session(session_id.clone(), window, cx);
            this.spawn_draft(window, cx);
            this.chat_snapshot().active().expect("new draft is active")
        })
    });
    cx.run_until_parked();

    app.read_with(cx, |this, _| {
        assert_eq!(this.chat_snapshot().active(), Some(draft_target));
        assert_eq!(this.chat_snapshot().conversations().len(), 1);
        assert!(
            !this
                .chat_snapshot()
                .opened_session_index()
                .contains_key(&session_id)
        );
    });
}

#[gpui::test]
fn selecting_a_project_draft_invalidates_an_older_session_restore(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().expect("project folder");
    let project = ProjectIdentity::new(folder.path(), "Project fixture");
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let session_id = seed_persisted_conversation(&stores, Some(project.clone()));
    let prefs = Preferences {
        agent_projects: vec![crate::preferences::AgentProjectRecord {
            project_id: project.project_id.clone(),
            canonical_path: project.canonical_path.clone(),
            display_name: project.display_name.clone(),
        }],
        ..Preferences::default()
    };
    let (app, cx) = add_app_window_with_preferences_and_stores(cx, prefs, Some(stores));

    let draft_target = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.switch_workspace(PROJECT_WORKSPACE_ID, window, cx);
            this.project_workspace().update(cx, |workspace, cx| {
                workspace.open_session(project.project_id.clone(), session_id.clone(), window, cx);
                workspace.open_draft(project.project_id.clone(), window, cx);
            });
            this.project_workspace()
                .read(cx)
                .snapshot()
                .active()
                .expect("new project draft is active")
        })
    });
    cx.run_until_parked();

    app.read_with(cx, |this, cx| {
        let snapshot = this.project_workspace().read(cx).snapshot();
        assert_eq!(snapshot.active(), Some(draft_target));
        assert_eq!(snapshot.conversations().len(), 1);
        assert!(snapshot.conversations()[0].session_id().is_none());
    });
}
