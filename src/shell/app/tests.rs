use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use gpui::TestAppContext;

use crate::session::{
    InMemorySessionStore, LocalSessionStore, LocalStoreConfig, SessionCatalogStore, SessionDomain,
    SessionReadStore, SessionStores, TurnStatus,
};

use super::*;

fn add_app_window(cx: &mut TestAppContext) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    add_app_window_with_stores(cx, None)
}

fn add_app_window_with_stores(
    cx: &mut TestAppContext,
    stores: Option<SessionStores>,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    let prefs = Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        crate::preferences::init_global(prefs.clone(), cx);
        if let Some(stores) = stores {
            cx.set_global(stores);
        }
    });
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let app = cx.new(|cx| ChatApp::new(prefs.clone(), window, cx));
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
    saver: super::PreferenceSaver,
) -> (Entity<ChatApp>, &mut gpui::VisualTestContext) {
    let prefs = Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        crate::preferences::init_global(prefs.clone(), cx);
        cx.set_global(stores);
    });
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let app = cx
            .new(|cx| ChatApp::new_with_preference_saver(prefs.clone(), window, cx, saver.clone()));
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
    let saver: super::PreferenceSaver = Arc::new(move |_| {
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
    let saver: super::PreferenceSaver = Arc::new(move |_| {
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
            assert!(this.conversations[0].view.update(cx, |chat, cx| {
                chat.start_durable_pending_reply_for_test(Rc::clone(&dropped), window, cx)
            }));
        });
    });
    cx.run_until_parked();
    let session_id = app.read_with(cx, |this, cx| {
        this.conversations[0]
            .view
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
    let (target, session_id) = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let conversation = &this.conversations[0];
            let session_id = conversation
                .view
                .update(cx, |chat, _| chat.persist_session_for_test());
            (conversation.view.entity_id(), session_id)
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
            this.conversations
                .iter()
                .all(|conversation| conversation.view.entity_id() != target)
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
            for _ in 1..20 {
                this.spawn_conversation(window, cx);
            }
            assert_eq!(this.conversations.len(), 20);

            let mut removed = Vec::new();
            while this.conversations.len() > 1 {
                let view = this.conversations[0].view.downgrade();
                let target = this.conversations[0].view.entity_id();
                removed.push(view);
                this.delete_conversation(target, window, cx);
            }
            assert_eq!(this.conversations.len(), 1);
            assert_eq!(this.active, 0);
            assert_eq!(
                this.conversations
                    .iter()
                    .filter(|conversation| {
                        let _ = &conversation._subscription;
                        true
                    })
                    .count(),
                1
            );
            removed
        })
    });
    cx.run_until_parked();
    assert!(first_removed.iter().all(|view| view.upgrade().is_none()));

    let second_removed = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            for _ in 1..20 {
                this.spawn_conversation(window, cx);
            }
            let mut removed = Vec::new();
            while this.conversations.len() > 1 {
                let view = this.conversations[0].view.downgrade();
                let target = this.conversations[0].view.entity_id();
                removed.push(view);
                this.delete_conversation(target, window, cx);
            }
            assert_eq!(this.conversations.len(), 1);
            assert_eq!(this.active, 0);
            removed
        })
    });
    cx.run_until_parked();
    assert!(second_removed.iter().all(|view| view.upgrade().is_none()));
}

#[gpui::test]
fn active_and_last_conversation_deletion_choose_deterministically(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_conversation(window, cx);
            this.spawn_conversation(window, cx);
            this.active = 1;
            let next = this.conversations[2].view.entity_id();
            let middle = this.conversations[1].view.entity_id();
            this.delete_conversation(middle, window, cx);
            assert_eq!(this.active, 1);
            assert_eq!(this.conversations[this.active].view.entity_id(), next);

            let before = this.conversations[this.active].view.entity_id();
            let non_active = this.conversations[0].view.entity_id();
            this.delete_conversation(non_active, window, cx);
            assert_eq!(this.active, 0);
            assert_eq!(this.conversations[0].view.entity_id(), before);

            let only = this.conversations[0].view.downgrade();
            let only_id = this.conversations[0].view.entity_id();
            this.delete_conversation(only_id, window, cx);
            assert_eq!(this.conversations.len(), 1);
            assert_ne!(this.conversations[0].view.entity_id(), only_id);
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
    let (target, weak) = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let conversation = &this.conversations[0];
            conversation.view.update(cx, |chat, cx| {
                chat.start_pending_reply_for_test(dropped.clone(), cx)
            });
            (conversation.view.entity_id(), conversation.view.downgrade())
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
    assert_eq!(app.read_with(cx, |this, _| this.conversations.len()), 1);
}

#[gpui::test]
fn delete_confirmation_keeps_the_original_target_after_switching(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    let (target, selected) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_conversation(window, cx);
            this.spawn_conversation(window, cx);
            this.active = 0;
            let target = this.conversations[0].view.entity_id();
            let title = this.conversations[0].title.clone();
            let selected = this.conversations[2].view.entity_id();
            this.request_delete_conversation(target, title, window, cx);
            this.select(2, window, cx);
            (target, selected)
        })
    });

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.dispatch_action(gpui_component::dialog::ConfirmDialog);
    cx.run_until_parked();

    app.read_with(cx, |this, _| {
        assert_eq!(this.conversations.len(), 2);
        assert!(
            this.conversations
                .iter()
                .all(|conversation| conversation.view.entity_id() != target)
        );
        assert_eq!(this.conversations[this.active].view.entity_id(), selected);
    });
}

#[gpui::test]
fn inline_confirm_target_survives_selection_switch(cx: &mut TestAppContext) {
    let (app, cx) = add_app_window(cx);
    let (target, selected) = cx.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.spawn_conversation(window, cx);
            this.spawn_conversation(window, cx);
            this.active = 0;
            let target = this.conversations[0].view.entity_id();
            let selected = this.conversations[2].view.entity_id();
            (target, selected)
        })
    });
    redraw(cx);

    let actions = Box::leak(format!("conversation-actions-{}", target.as_u64()).into_boxed_str());
    click(cx, actions);
    cx.simulate_keystrokes("down enter");
    redraw(cx);
    assert_eq!(app.read_with(cx, |this, _| this.confirming), Some(target));

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
        assert_eq!(this.conversations.len(), 2);
        assert!(
            this.conversations
                .iter()
                .all(|conversation| conversation.view.entity_id() != target)
        );
        assert_eq!(this.conversations[this.active].view.entity_id(), selected);
        assert_eq!(this.confirming, None);
    });
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
