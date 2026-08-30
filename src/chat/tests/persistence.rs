use super::*;

#[gpui::test]
fn public_turn_flow_emits_binding_and_persists_a_completed_terminal(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let generation = Arc::new(ScriptedGenerationService::completed(
        scripted_completed_events(),
    ));
    let (chat, cx) = add_chat_window_with_generation_service(cx, stores, generation);
    let observed = Rc::new(RefCell::new(Vec::<ChatEvent>::new()));
    let _subscription = cx.update(|_, cx| {
        let observed = observed.clone();
        cx.subscribe(&chat, move |_, event: &ChatEvent, _| {
            observed.borrow_mut().push(event.clone());
        })
    });
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model".into(),
    };

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.select_model(selection.clone(), cx);
            assert!(this.submit("hello runtime".into(), window, cx));
        });
    });
    cx.run_until_parked();

    let events = observed.borrow();
    assert!(events.iter().any(|event| {
        matches!(event, ChatEvent::TitleChanged(title) if title == "hello runtime")
    }));
    let session_id = events.iter().find_map(|event| match event {
        ChatEvent::SessionBound(session_id) => Some(session_id.clone()),
        _ => None,
    });
    drop(events);
    let session_id = session_id.expect("durable begin emits a public session binding");
    let state = catalog
        .load_session(&session_id, None)
        .expect("load the completed Chat turn");
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(
        &state.messages[1].message.content[..],
        [ContentBlock::Text { text, .. }] if text == "scripted"
    ));
    assert!(matches!(
        state.turn_results.as_slice(),
        [result] if result.result.status == TurnStatus::Completed
    ));
}

#[gpui::test]
fn public_cancel_flow_persists_a_cancelled_terminal(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let generation = Arc::new(ScriptedGenerationService::pending());
    let (chat, cx) = add_chat_window_with_generation_service(cx, stores, generation);
    let observed = Rc::new(RefCell::new(Vec::<ChatEvent>::new()));
    let _subscription = cx.update(|_, cx| {
        let observed = observed.clone();
        cx.subscribe(&chat, move |_, event: &ChatEvent, _| {
            observed.borrow_mut().push(event.clone());
        })
    });
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model".into(),
    };

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.select_model(selection, cx);
            assert!(this.submit("cancel runtime".into(), window, cx));
        });
    });
    cx.run_until_parked();

    cx.update(|_, cx| chat.update(cx, |this, _| this.cancel_reply()));
    cx.run_until_parked();

    let session_id = observed.borrow().iter().find_map(|event| match event {
        ChatEvent::SessionBound(session_id) => Some(session_id.clone()),
        _ => None,
    });
    let session_id = session_id.expect("durable begin emits a public session binding");
    let state = catalog
        .load_session(&session_id, None)
        .expect("load the cancelled Chat turn");
    assert!(matches!(
        state.turn_results.as_slice(),
        [result] if result.result.status == TurnStatus::Cancelled
    ));
}

#[gpui::test]
fn deletion_queued_behind_the_first_turn_cannot_leave_an_orphan_session(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let controller = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .clone()
        })
    });
    let controller_guard = controller.lock().expect("hold controller");

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.selection = Some(ModelSelection {
                profile_id: "profile".into(),
                model_id: "model-a".into(),
            });
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            assert!(this.submit("delete while saving".into(), window, cx));
            assert_eq!(this.request_delete(cx), ChatDeleteRequest::Pending);
        });
    });
    drop(controller_guard);
    cx.run_until_parked();

    let page = catalog
        .list_sessions(SessionDomain::Chat, CatalogQuery::first_page())
        .expect("list Chat sessions after deletion");
    assert!(page.sessions.is_empty(), "delete left an orphan session");
    cx.update(|window, cx| {
        chat.read_with(cx, |this, _| assert!(!this.deletion_pending));
        let notifications = gpui_component::Root::read(window, cx)
            .notification
            .read(cx)
            .notifications();
        assert!(
            notifications.is_empty(),
            "a user-requested deletion is not a persistence failure"
        );
    });
}

#[gpui::test]
fn deletion_overtaking_terminal_persistence_is_not_reported_as_a_failure(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model-a".into(),
    };
    let user = LlmMessage {
        role: crate::llm::Role::User,
        content: vec![ContentBlock::Text {
            text: "delete while finishing".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let controller = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .begin_turn(user.clone(), selection, "turn-1")
                .expect("begin");
            this.pending = true;
            this.pending_turn_id = Some("turn-1".into());
            this.messages.push(Message::from_canonical(user, cx));
            this.messages.push(Message::empty(Role::Assistant));
            this.session_controller
                .as_ref()
                .expect("controller")
                .clone()
        })
    });
    let controller_guard = controller.lock().expect("hold controller");

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply_with_terminal(None, ChatTurnTerminal::cancelled(), None, cx);
            assert!(this.persistence_pending);
            assert_eq!(this.request_delete(cx), ChatDeleteRequest::Pending);
        });
    });
    drop(controller_guard);
    cx.run_until_parked();

    let page = catalog
        .list_sessions(SessionDomain::Chat, CatalogQuery::first_page())
        .expect("list Chat sessions after deletion");
    assert!(page.sessions.is_empty(), "delete left an orphan session");
    cx.update(|window, cx| {
        let notifications = gpui_component::Root::read(window, cx)
            .notification
            .read(cx)
            .notifications();
        assert!(
            notifications.is_empty(),
            "a user-requested deletion is not a terminal persistence failure"
        );
    });
}

#[gpui::test]
fn deletion_after_durable_begin_does_not_start_provider_or_publish_the_turn(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let (committed_tx, committed_rx) = mpsc::sync_channel(1);
    let mut store = InMemorySessionStore::new();
    store.notify_create_after_commit_for_test(committed_tx);
    let stores = SessionStores::with_chat_store(store);
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let (chat, cx) = add_chat_window_with_stores(cx, stores);

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.selection = Some(ModelSelection {
                profile_id: "profile".into(),
                model_id: "model-a".into(),
            });
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            assert!(this.submit("delete after commit".into(), window, cx));
        });
    });
    while committed_rx.try_recv().is_err() {
        assert!(
            cx.executor().tick(),
            "durable begin parked before committing the first Chat facts"
        );
    }

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            assert_eq!(this.request_delete(cx), ChatDeleteRequest::Pending);
        });
    });
    cx.run_until_parked();

    let page = catalog
        .list_sessions(SessionDomain::Chat, CatalogQuery::first_page())
        .expect("list Chat sessions after deletion");
    assert!(page.sessions.is_empty(), "delete left an orphan session");
    cx.update(|window, cx| {
        chat.read_with(cx, |this, _| {
            assert!(
                this.messages.is_empty(),
                "a deleted durable begin must not publish a visible turn"
            );
            assert!(
                this.reply_task.is_none(),
                "a deleted durable begin must not start the provider"
            );
        });
        let notifications = gpui_component::Root::read(window, cx)
            .notification
            .read(cx)
            .notifications();
        assert!(
            notifications.is_empty(),
            "a deleted durable begin must not surface a persistence failure notification"
        );
    });
}

#[gpui::test]
fn restore_from_session_hydrates_messages_and_advances_turn_id(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let catalog = stores.chat_catalog().expect("Chat catalog capability");
    let (chat, cx) = add_chat_window_with_stores(cx, stores.clone());

    let session_id = cx.update(|_, cx| {
        chat.update(cx, |this, _cx| {
            let controller = this
                .session_controller
                .as_ref()
                .expect("controller")
                .clone();
            let mut guard = controller.lock().expect("lock");
            let user_message = LlmMessage {
                role: crate::llm::Role::User,
                content: vec![ContentBlock::Text {
                    text: "persisted turn-1".into(),
                    provider_metadata: ProviderMetadata::default(),
                }],
                provider_metadata: ProviderMetadata::default(),
            };
            let selection = ModelSelection {
                profile_id: "fixture-profile".into(),
                model_id: "fixture-model".into(),
            };
            let start = guard
                .begin_turn(user_message, selection, "turn-1")
                .expect("begin");
            guard
                .finish_turn("turn-1", &ChatTurnTerminal::cancelled())
                .expect("finish");
            start.session_id
        })
    });

    let state = catalog
        .load_session(&session_id, None)
        .expect("load resolved state");
    assert_eq!(state.messages.len(), 1);

    let (other, cx) = add_chat_window_with_stores(cx, stores);
    let restored = cx.update(|_, cx| {
        other.update(cx, |this, cx| {
            this.restore_from_session(&session_id, &state, cx)
        })
    });
    assert!(restored.is_ok(), "restore should succeed on an idle view");
    cx.update(|_, cx| {
        other.read_with(cx, |this, _| {
            assert_eq!(this.messages.len(), 1);
            assert_eq!(this.conversation_id, session_id.to_string());
            assert!(
                this.next_turn_id >= 2,
                "turn id must advance past the persisted turn-1"
            );
        });
    });
}

#[gpui::test]
fn restore_from_session_rejects_a_view_with_pending_generation(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let dropped = Rc::new(std::cell::Cell::new(false));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_pending_reply_for_test(dropped, cx);
        });
    });
    let session_id: SessionId = "chat-01923f5e-7f4a-7f4a-8f4a-0123456789ab"
        .parse()
        .expect("valid session id");
    let state = ResolvedSessionState {
        leaf_id: crate::session::EntryId::new(),
        path: Vec::new(),
        context: Vec::new(),
        messages: Vec::new(),
        transcript_replays: Vec::new(),
        turn_results: Vec::new(),
        latest_config: None,
        latest_compaction: None,
    };
    let result = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.restore_from_session(&session_id, &state, cx)
        })
    });
    assert!(result.is_err(), "restore must reject a streaming view");
}

#[gpui::test]
fn chat_view_persists_a_terminal_through_the_controller(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model-a".into(),
    };
    let user = LlmMessage {
        role: crate::llm::Role::User,
        content: vec![ContentBlock::Text {
            text: "hello".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let turn_id = "turn-1".to_string();
    let start = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let start = this
                .session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .begin_turn(user.clone(), selection.clone(), turn_id.clone())
                .expect("begin");
            this.conversation_id = start.session_id.to_string();
            this.pending = true;
            this.pending_turn_id = Some(turn_id.clone());
            this.messages.push(Message::from_canonical(user, cx));
            this.messages.push(Message::empty(Role::Assistant));
            start
        })
    });

    let controller = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .clone()
        })
    });
    let controller_guard = controller.lock().expect("hold controller");

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply_with_terminal(None, ChatTurnTerminal::cancelled(), None, cx);
            assert!(!this.pending);
            assert!(this.persistence_pending);
        });
    });
    drop(controller_guard);
    cx.run_until_parked();

    cx.update(|_, cx| {
        let state = chat.update(cx, |this, _cx| {
            assert!(!this.pending);
            assert!(this.pending_turn_id.is_none());
            this.session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .restore(&start.session_id)
                .expect("restore")
        });
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.turn_results.len(), 1);
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Cancelled);
    });
}

#[gpui::test]
fn queued_terminal_persistence_finishes_before_store_shutdown(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (chat, cx) = add_chat_window_with_stores(cx, stores.clone());
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model-a".into(),
    };
    let user = LlmMessage {
        role: crate::llm::Role::User,
        content: vec![ContentBlock::Text {
            text: "hello".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let controller = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .begin_turn(user.clone(), selection, "turn-1")
                .expect("begin");
            this.pending = true;
            this.pending_turn_id = Some("turn-1".into());
            this.messages.push(Message::from_canonical(user, cx));
            this.messages.push(Message::empty(Role::Assistant));
            this.session_controller
                .as_ref()
                .expect("controller")
                .clone()
        })
    });
    let controller_guard = controller.lock().expect("hold controller");

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply_with_terminal(None, ChatTurnTerminal::cancelled(), None, cx);
            assert!(this.persistence_pending);
        });
    });
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let _ = finished_tx.send(stores.shutdown());
    });
    assert!(
        finished_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "quit shutdown overtook a terminal write already queued by ChatView"
    );
    drop(controller_guard);
    cx.run_until_parked();
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown resumes after terminal persistence")
            .is_ok()
    );
    worker.join().expect("shutdown worker");
}

#[gpui::test]
fn provider_generation_keeps_shutdown_behind_terminal_persistence(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (chat, cx) = add_chat_window_with_stores(cx, stores.clone());
    let dropped = Rc::new(std::cell::Cell::new(false));

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.selection = Some(ModelSelection {
                profile_id: "profile".into(),
                model_id: "model-a".into(),
            });
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            this.next_reply_drop_flag = Some(Rc::clone(&dropped));
            assert!(this.submit("hold generation open".into(), window, cx));
        });
    });
    cx.run_until_parked();
    let session_id = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            assert!(this.pending, "provider generation should still be active");
            assert!(this.reply_task.is_some());
            this.conversation_id
                .parse::<crate::session::SessionId>()
                .expect("durable Chat session id")
        })
    });

    let shutdown_hold = stores
        .chat()
        .expect("Chat store")
        .reserve_operation()
        .expect("reserve shutdown test barrier");
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let _ = finished_tx.send(stores.shutdown());
    });
    assert!(
        finished_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "shutdown crossed the durable user/provider-to-terminal gap"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply_with_terminal(None, ChatTurnTerminal::cancelled(), None, cx);
        });
    });
    cx.run_until_parked();

    // A completed shutdown intentionally closes the shared store boundary.
    // Verify the terminal while the controller is still usable, then exercise
    // the final shutdown barrier independently.
    cx.update(|_, cx| {
        let state = chat.update(cx, |this, _| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .restore(&session_id)
                .expect("restore terminal")
        });
        assert_eq!(state.turn_results.len(), 1);
        assert_eq!(state.turn_results[0].result.status, TurnStatus::Cancelled);
    });

    drop(shutdown_hold);
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown resumes after terminal persistence")
            .is_ok()
    );
    worker.join().expect("shutdown worker");
}

#[gpui::test]
fn preparing_the_chat_view_for_shutdown_persists_a_cancelled_terminal(cx: &mut TestAppContext) {
    init_app(cx);
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let store = LocalSessionStore::open(config.clone()).expect("open local Chat store");
    let stores = SessionStores::with_chat_store(store);
    let (chat, cx) = add_chat_window_with_stores(cx, stores.clone());
    let dropped = Rc::new(std::cell::Cell::new(false));

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.selection = Some(ModelSelection {
                profile_id: "profile".into(),
                model_id: "model-a".into(),
            });
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            this.next_reply_drop_flag = Some(Rc::clone(&dropped));
            assert!(this.submit("close during generation".into(), window, cx));
        });
    });
    cx.run_until_parked();
    let session_id = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            assert!(this.pending);
            this.conversation_id
                .parse::<crate::session::SessionId>()
                .expect("durable Chat session id")
        })
    });

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| this.prepare_for_shutdown(cx));
        window.remove_window();
    });
    drop(chat);
    cx.run_until_parked();
    stores
        .shutdown()
        .expect("shutdown waits for release-triggered terminal persistence");
    drop(stores);

    let mut reopened = LocalSessionStore::open(config).expect("reopen local Chat store");
    let _ = reopened
        .repair_if_needed()
        .expect("settle any projection obligation");
    let restored = reopened
        .load_session(&session_id, None)
        .expect("restore released turn");
    assert_eq!(restored.turn_results.len(), 1);
    assert_eq!(
        restored.turn_results[0].result.status,
        TurnStatus::Cancelled
    );
}

#[gpui::test]
fn released_chat_retries_its_exact_cancelled_terminal_once(cx: &mut TestAppContext) {
    init_app(cx);
    let (append_tx, append_rx) = mpsc::sync_channel(1);
    let mut store = InMemorySessionStore::new();
    // Atomic first-turn creation does not call `append`; the first append is
    // therefore the release-triggered terminal that this regression fails.
    store.fail_append_at_for_test(1);
    store.observe_append_success_for_test(append_tx);
    let stores = SessionStores::with_chat_store(store);
    let read_store = stores.chat().expect("Chat lifecycle capability");
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let dropped = Rc::new(std::cell::Cell::new(false));

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.selection = Some(ModelSelection {
                profile_id: "profile".into(),
                model_id: "model-a".into(),
            });
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            this.next_reply_drop_flag = Some(Rc::clone(&dropped));
            assert!(this.submit("close through one transient failure".into(), window, cx));
        });
    });
    cx.run_until_parked();
    let session_id = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            this.conversation_id
                .parse::<crate::session::SessionId>()
                .expect("durable Chat session id")
        })
    });

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| this.prepare_for_shutdown(cx));
        window.remove_window();
    });
    drop(chat);
    cx.run_until_parked();

    assert_eq!(
        append_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached terminal should retry after one transient failure"),
        2
    );
    let restored = read_store
        .load_session(&session_id, None)
        .expect("restore release-triggered terminal");
    assert_eq!(restored.turn_results.len(), 1);
    assert_eq!(
        restored.turn_results[0].result.status,
        TurnStatus::Cancelled
    );
}

#[gpui::test]
fn shutdown_during_inflight_terminal_retries_the_exact_terminal(cx: &mut TestAppContext) {
    init_app(cx);
    let (append_tx, append_rx) = mpsc::sync_channel(1);
    let mut store = InMemorySessionStore::new();
    store.fail_append_at_for_test(1);
    store.observe_append_success_for_test(append_tx);
    let stores = SessionStores::with_chat_store(store);
    let read_store = stores.chat().expect("Chat lifecycle capability");
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let user = LlmMessage {
        role: crate::llm::Role::User,
        content: vec![ContentBlock::Text {
            text: "close after terminal dispatch".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let session_id = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let start = this
                .session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .begin_turn(
                    user.clone(),
                    ModelSelection {
                        profile_id: "profile".into(),
                        model_id: "model-a".into(),
                    },
                    "turn-1",
                )
                .expect("begin");
            this.pending = true;
            this.pending_turn_id = Some("turn-1".into());
            this.messages.push(Message::from_canonical(user, cx));
            this.messages.push(Message::empty(Role::Assistant));
            start.session_id
        })
    });
    let controller = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .clone()
        })
    });
    let controller_guard = controller.lock().expect("hold controller");

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply_with_terminal(None, ChatTurnTerminal::cancelled(), None, cx);
            assert!(this.persistence_pending);
            assert!(this.terminal_persistence.is_none());
        });
    });
    cx.update(|window, cx| {
        chat.update(cx, |this, cx| this.prepare_for_shutdown(cx));
        window.remove_window();
    });
    drop(chat);
    drop(controller_guard);
    cx.run_until_parked();

    assert_eq!(
        append_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("an in-flight terminal must retain its bounded retry after entity release"),
        2
    );
    let restored = read_store
        .load_session(&session_id, None)
        .expect("restore terminal after entity release");
    assert_eq!(restored.turn_results.len(), 1);
    assert_eq!(
        restored.turn_results[0].result.status,
        TurnStatus::Cancelled
    );
}

#[gpui::test]
fn terminal_persistence_failure_unblocks_chat_and_notifies_the_user(cx: &mut TestAppContext) {
    init_app(cx);
    let mut store = InMemorySessionStore::new();
    // The first turn uses atomic create-with-user; the first ordinary
    // append is therefore the terminal batch. Both bounded attempts must
    // fail to exercise the user-visible retry state.
    store.fail_append_at_for_test(1);
    store.fail_append_at_for_test(2);
    let stores = SessionStores::with_chat_store(store);

    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model-a".into(),
    };
    let user = LlmMessage {
        role: crate::llm::Role::User,
        content: vec![ContentBlock::Text {
            text: "hello".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let turn_id = "turn-1".to_string();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let start = this
                .session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .begin_turn(user.clone(), selection, turn_id.clone())
                .expect("begin");
            this.conversation_id = start.session_id.to_string();
            this.pending = true;
            this.pending_turn_id = Some(turn_id);
            this.messages.push(Message::from_canonical(user, cx));
            this.messages.push(Message::empty(Role::Assistant));
            this.finish_reply_with_terminal(None, ChatTurnTerminal::cancelled(), None, cx);
        });
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
        chat.read_with(cx, |this, _| {
            assert!(!this.pending);
            assert!(this.pending_turn_id.is_none());
            assert!(this.pending_terminal.is_some());
        });
        let notifications = gpui_component::Root::read(window, cx)
            .notification
            .read(cx)
            .notifications();
        assert_eq!(notifications.len(), 1);
    });
}

#[gpui::test]
fn durable_begin_runs_off_foreground_and_preserves_a_newer_draft(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model-a".into(),
    };
    let controller = cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            this.session_controller
                .as_ref()
                .expect("controller")
                .clone()
        })
    });
    let controller_guard = controller.lock().expect("hold controller");

    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            this.selection = Some(selection);
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            this.composer_revision = 1;
            this.input.update(cx, |input, cx| {
                input.set_value("original draft", window, cx)
            });
            assert!(this.submit("original draft".into(), window, cx));
            assert!(this.persistence_pending);
            assert!(this.messages.is_empty());
            assert!(this.reply_task.is_none());
            assert_eq!(this.input.read(cx).value(), "original draft");

            // A foreground edit made while the durable begin is queued must
            // survive its later success callback.
            this.composer_revision = 2;
            this.input
                .update(cx, |input, cx| input.set_value("newer draft", window, cx));
        });
    });
    drop(controller_guard);
    cx.run_until_parked();

    cx.update(|_, cx| {
        chat.read_with(cx, |this, cx| {
            assert!(!this.persistence_pending);
            assert_eq!(this.input.read(cx).value(), "newer draft");
            assert!(!this.messages.is_empty());
        });
    });
}

#[gpui::test]
fn durable_begin_failure_keeps_the_composer_and_notifies(cx: &mut TestAppContext) {
    init_app(cx);
    let stores = SessionStores::with_chat_store(InMemorySessionStore::new());
    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    let selection = ModelSelection {
        profile_id: "profile".into(),
        model_id: "model-a".into(),
    };
    cx.update(|window, cx| {
        chat.update(cx, |this, cx| {
            let existing = LlmMessage {
                role: crate::llm::Role::User,
                content: vec![ContentBlock::Text {
                    text: "existing pending turn".into(),
                    provider_metadata: ProviderMetadata::default(),
                }],
                provider_metadata: ProviderMetadata::default(),
            };
            this.session_controller
                .as_ref()
                .expect("controller")
                .lock()
                .expect("controller lock")
                .begin_turn(existing, selection.clone(), "turn-1")
                .expect("seed pending controller turn");
            this.selection = Some(selection);
            this.selection_available = true;
            this.provider_catalog_revision = crate::providers::catalog_revision();
            this.composer_revision = 1;
            this.input
                .update(cx, |input, cx| input.set_value("new draft", window, cx));
            assert!(this.submit("new draft".into(), window, cx));
        });
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
        chat.read_with(cx, |this, cx| {
            assert!(!this.persistence_pending);
            assert!(this.messages.is_empty());
            assert_eq!(this.input.read(cx).value(), "new draft");
        });
        let notifications = gpui_component::Root::read(window, cx)
            .notification
            .read(cx)
            .notifications();
        assert_eq!(notifications.len(), 1);
    });
}
