use super::*;

use crate::llm::{ContentBlock, Message, ProviderMetadata, Role};
use crate::session::{
    ChatMessageUnavailable, EntryId, ReferencedMessage, SessionDomain, SessionId,
};

fn chat_reference() -> ChatMessageRef {
    ChatMessageRef {
        session_id: SessionId::new(SessionDomain::Chat),
        entry_id: EntryId::new(),
    }
}

fn preview(reference: ChatMessageRef, title: Option<&str>, timestamp: i64) -> ChatMessagePreview {
    ChatMessagePreview {
        reference,
        session_title: title.map(str::to_string),
        session_created_at: 1,
        timestamp,
        role: Role::User,
        preview: Some("hello world".to_string()),
    }
}

fn page(
    messages: Vec<ChatMessagePreview>,
    next_cursor: Option<ChatMessageSearchCursor>,
) -> ChatMessageSearchPage {
    ChatMessageSearchPage {
        messages,
        next_cursor,
    }
}

fn cursor_for(row: &ChatMessagePreview) -> ChatMessageSearchCursor {
    ChatMessageSearchCursor {
        timestamp: row.timestamp,
        session_id: row.reference.session_id.clone(),
        entry_id: row.reference.entry_id.clone(),
    }
}

// ---------------------------------------------------------------------------
// Trigger rule
// ---------------------------------------------------------------------------

#[test]
fn dollar_opens_picker_once_per_insertion() {
    assert!(dollar_newly_present(false, "look at $ this"));
    assert!(!dollar_newly_present(true, "another $"));
    assert!(!dollar_newly_present(false, "no trigger"));
    assert!(!dollar_newly_present(true, ""));
    // Removing every `$` re-arms the trigger.
    assert!(!dollar_newly_present(true, "gone"));
    assert!(dollar_newly_present(false, "back $"));
}

// ---------------------------------------------------------------------------
// Search snapshot
// ---------------------------------------------------------------------------

#[test]
fn blank_query_resets_without_a_request() {
    let mut search = ReferenceSearch::new();
    assert!(search.begin("  ").is_none());
    assert_eq!(search.status, ReferenceSearchStatus::Idle);
    assert!(search.results.is_empty());

    // A prior result set is cleared so stale rows cannot render.
    let generation = search.begin("needle").unwrap().0;
    assert!(search.apply_search(
        generation,
        page(vec![preview(chat_reference(), None, 5)], None)
    ));
    assert!(search.begin("   ").is_none());
    assert!(search.results.is_empty());
}

#[test]
fn search_applies_only_the_current_generation() {
    let mut search = ReferenceSearch::new();
    let stale = search.begin("old").unwrap().0;
    let fresh = search.begin("new").unwrap().0;
    assert_ne!(stale, fresh);

    assert!(!search.apply_search(stale, page(vec![preview(chat_reference(), None, 5)], None)));
    assert!(search.results.is_empty());
    assert_eq!(search.status, ReferenceSearchStatus::Searching);

    assert!(search.apply_search(fresh, page(vec![preview(chat_reference(), None, 6)], None)));
    assert_eq!(search.status, ReferenceSearchStatus::Ready);
    assert_eq!(search.results.len(), 1);
}

#[test]
fn failed_generation_is_guarded() {
    let mut search = ReferenceSearch::new();
    let stale = search.begin("old").unwrap().0;
    search.begin("new").unwrap();
    assert!(!search.fail(stale));
    assert_eq!(search.status, ReferenceSearchStatus::Searching);

    let fresh = search.generation;
    assert!(search.fail(fresh));
    assert_eq!(search.status, ReferenceSearchStatus::Failed);
}

#[test]
fn load_more_requires_ready_page_and_cursor() {
    let mut search = ReferenceSearch::new();
    assert!(search.begin_load_more().is_none());

    let generation = search.begin("needle").unwrap().0;
    assert!(search.begin_load_more().is_none(), "still searching");

    assert!(search.apply_search(
        generation,
        page(vec![preview(chat_reference(), None, 5)], None)
    ));
    assert!(!search.has_more());
    assert!(search.begin_load_more().is_none(), "no cursor");

    let row = preview(chat_reference(), None, 4);
    let cursor = cursor_for(&row);
    assert!(search.apply_search(generation, page(vec![row], Some(cursor))));
    assert!(search.has_more());

    let (load_generation, request) = search.begin_load_more().unwrap();
    assert_eq!(request.text, "needle");
    assert!(request.cursor.is_some());
    assert_eq!(search.status, ReferenceSearchStatus::Searching);
    assert!(!search.has_more(), "no re-entry while loading");
    let _ = load_generation;
}

#[test]
fn load_more_appends_deduplicated_rows() {
    let mut search = ReferenceSearch::new();
    let generation = search.begin("needle").unwrap().0;
    let first = preview(chat_reference(), None, 5);
    let cursor = cursor_for(&first);
    assert!(search.apply_search(generation, page(vec![first.clone()], Some(cursor))));

    let (load_generation, _) = search.begin_load_more().unwrap();
    let second = preview(chat_reference(), None, 4);
    // The page repeats the boundary row and adds one new row.
    assert!(search.apply_load_more(load_generation, page(vec![first, second], None)));
    assert_eq!(search.results.len(), 2);
    assert_eq!(search.status, ReferenceSearchStatus::Ready);
    assert!(!search.has_more());
}

#[test]
fn load_more_failure_keeps_rows_for_retry() {
    let mut search = ReferenceSearch::new();
    let generation = search.begin("needle").unwrap().0;
    let row = preview(chat_reference(), None, 5);
    let cursor = cursor_for(&row);
    assert!(search.apply_search(generation, page(vec![row], Some(cursor))));

    let (load_generation, _) = search.begin_load_more().unwrap();
    assert!(search.fail_load_more(load_generation));
    assert_eq!(search.status, ReferenceSearchStatus::Ready);
    assert_eq!(search.results.len(), 1);
    assert!(search.has_more(), "cursor retained so scrolling retries");
}

#[test]
fn search_page_deduplicates_references() {
    let mut search = ReferenceSearch::new();
    let generation = search.begin("needle").unwrap().0;
    let row = preview(chat_reference(), None, 5);
    assert!(search.apply_search(generation, page(vec![row.clone(), row], None)));
    assert_eq!(search.results.len(), 1);
}

// ---------------------------------------------------------------------------
// Draft state
// ---------------------------------------------------------------------------

fn sample_read(reference: ChatMessageRef, title: Option<&str>) -> ChatMessageRead {
    ChatMessageRead {
        reference,
        session_title: title.map(str::to_string),
        session_created_at: 1,
        timestamp: 42,
        message: ReferencedMessage::from_message(&Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "first line\nsecond line".to_string(),
                provider_metadata: ProviderMetadata::default(),
            }],
            provider_metadata: ProviderMetadata::default(),
        }),
    }
}

#[test]
fn draft_keeps_reference_and_bounded_label_only() {
    let reference = chat_reference();
    let draft = ChatReferenceDraft::from_read(sample_read(reference.clone(), Some("Session")));

    assert_eq!(draft.reference, reference);
    assert_eq!(draft.session_title.as_deref(), Some("Session"));
    assert_eq!(draft.snippet.as_deref(), Some("first line\nsecond line"));
    assert_eq!(draft.timestamp, 42);
    assert_eq!(draft.chip_label().as_ref(), "first line…");

    // Untitled sessions fall back to the localized placeholder rather than a
    // stored default title; an empty body exercises that path.
    let mut empty_body = sample_read(chat_reference(), None);
    empty_body.message = ReferencedMessage {
        role: Role::User,
        content: Vec::new(),
    };
    let untitled = ChatReferenceDraft::from_read(empty_body);
    assert_eq!(untitled.snippet, None);
    assert_eq!(
        untitled.chip_label().as_ref(),
        t!("reference_picker.untitled_chat").to_string()
    );

    // An untitled session with a body still labels from the body.
    let untitled_with_body = ChatReferenceDraft::from_read(sample_read(chat_reference(), None));
    assert_eq!(untitled_with_body.chip_label().as_ref(), "first line…");
}

#[test]
fn chip_label_bounds_long_first_lines() {
    let draft = ChatReferenceDraft {
        reference: chat_reference(),
        session_title: Some("Session".to_string()),
        snippet: Some("x".repeat(200)),
        timestamp: 1,
    };
    let label = draft.chip_label();
    assert_eq!(label.chars().count(), 49, "48 glyphs plus ellipsis");
    assert!(label.ends_with('…'));
}

#[test]
fn draft_dedup_uses_reference_identity() {
    let reference = chat_reference();
    let first = ChatReferenceDraft::from_read(sample_read(reference.clone(), Some("One")));
    let second = ChatReferenceDraft::from_read(sample_read(reference.clone(), Some("Two")));

    let mut selected = HashSet::new();
    let mut drafts = vec![first.clone()];
    selected.insert(first.reference.clone());
    if selected.insert(second.reference.clone()) {
        drafts.push(second);
    }
    assert_eq!(drafts.len(), 1, "same reference cannot join twice");

    // A different reference is independent.
    let other = ChatReferenceDraft::from_read(sample_read(chat_reference(), None));
    assert!(selected.insert(other.reference.clone()));
}

// ---------------------------------------------------------------------------
// Confirm error mapping
// ---------------------------------------------------------------------------

#[test]
fn confirm_error_maps_typed_store_errors() {
    let reference = chat_reference();
    assert_eq!(
        ReferenceConfirmError::from_store(&ChatReferenceError::TooLarge { limit: 512 }),
        ReferenceConfirmError::TooLarge { limit: 512 }
    );
    assert_eq!(
        ReferenceConfirmError::from_store(&ChatReferenceError::Unavailable(
            ChatMessageUnavailable {
                reference: reference.clone(),
                reason: ChatMessageUnavailableReason::SessionDeleted,
            }
        )),
        ReferenceConfirmError::Unavailable(ChatMessageUnavailableReason::SessionDeleted)
    );
    // Transport-level failures collapse without leaking internals.
    assert_eq!(
        ReferenceConfirmError::from_store(&ChatReferenceError::Catalog(
            crate::session::CatalogError::DomainMismatch {
                expected: SessionDomain::Chat,
                actual: SessionDomain::Agent,
            }
        )),
        ReferenceConfirmError::Read
    );
}

// ---------------------------------------------------------------------------
// Time formatting
// ---------------------------------------------------------------------------

#[test]
fn reference_time_uses_time_for_now_and_date_for_old_years() {
    let now = 1_750_000_000_000_i64; // well inside the supported range
    assert_eq!(format_reference_time(now, now).len(), 5);
    assert!(format_reference_time(now, now).contains(':'));

    let old = 946_684_800_000_i64; // 2000-01-01T00:00:00Z
    let date_only = format_reference_time(now, old);
    assert_eq!(date_only.len(), 10, "YYYY-MM-DD");
    assert_eq!(&date_only[4..5], "-");
}

#[test]
fn reference_time_handles_unrepresentable_timestamps() {
    assert_eq!(format_reference_time(0, i64::MAX), "");
}

// ---------------------------------------------------------------------------
// Composer integration (GUI-level)
// ---------------------------------------------------------------------------

use gpui::TestAppContext;

use crate::session::{
    InMemorySessionStore, MessageEntry, SessionEntryKind, SessionHeader, SessionLifecycleStore,
    SessionStores, Usage,
};

fn seed_chat_message(store: &mut InMemorySessionStore, text: &str) -> crate::session::SessionId {
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    store
        .create_session(header)
        .expect("create seeded Chat session");
    store
        .append(
            &session_id,
            vec![SessionEntryKind::Message(MessageEntry {
                message: Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: text.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                turn_id: None,
                model: None,
                usage: Usage::default(),
            })],
        )
        .expect("append seeded Chat message");
    session_id
}

/// Install a global session-stores singleton seeded with Chat messages.
fn seed_global_stores(cx: &mut TestAppContext, seed: impl FnOnce(&mut InMemorySessionStore)) {
    cx.update(|cx| {
        gpui_component::init(cx);
        let mut memory = InMemorySessionStore::new();
        seed(&mut memory);
        cx.set_global(SessionStores::with_chat_store(memory));
    });
}

#[gpui::test]
fn dollar_trigger_opens_the_picker_once_per_insertion(cx: &mut TestAppContext) {
    seed_global_stores(cx, |_| {});
    let cx = cx.add_empty_window();
    let composer = cx.update(|window, cx| cx.new(|cx| ChatReferenceComposer::new(window, cx)));
    let input = cx.update(|_, cx| composer.read(cx).input.clone());

    // `set_value` suppresses change events, so drive the same path real
    // typing takes: `replace_all` emits `InputEvent::Change`.
    let set_value = |cx: &mut gpui::VisualTestContext, value: &str| {
        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.replace_all(value, window, cx));
        });
        cx.run_until_parked();
    };

    set_value(cx, "look at $");
    cx.update(|_, cx| assert!(composer.read(cx).open));

    // Dismiss (Escape / outside click path) while the `$` is still present.
    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| composer.set_open(false, window, cx));
    });
    cx.update(|_, cx| assert!(!composer.read(cx).open));

    // More `$` insertions do not reopen until every `$` is removed.
    set_value(cx, "another $");
    cx.update(|_, cx| assert!(!composer.read(cx).open));
    set_value(cx, "no trigger");
    cx.update(|_, cx| assert!(!composer.read(cx).open));
    set_value(cx, "back $");
    cx.update(|_, cx| assert!(composer.read(cx).open));
}

#[gpui::test]
fn picker_searches_confirms_and_removes_references(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "the unique needle message");
        seed_chat_message(memory, "an unrelated conversation");
    });
    let cx = cx.add_empty_window();
    let composer = cx.update(|window, cx| cx.new(|cx| ChatReferenceComposer::new(window, cx)));

    // Open the picker via the `$` trigger, then search the catalog.
    let input = cx.update(|_, cx| composer.read(cx).input.clone());
    cx.update(|window, cx| {
        input.update(cx, |state, cx| state.replace_all("note $", window, cx));
    });
    cx.run_until_parked();
    let list = cx.update(|_, cx| composer.read(cx).list.clone());
    let search = |cx: &mut gpui::VisualTestContext, query: &str| {
        cx.update(|window, cx| {
            let task = list.update(cx, |list, cx| {
                list.delegate_mut().perform_search(query, window, cx)
            });
            task.detach();
        });
        cx.run_until_parked();
    };
    search(cx, "unique needle");

    cx.update(|_, cx| {
        let delegate = list.read(cx).delegate();
        assert_eq!(delegate.search.results.len(), 1);
        assert_eq!(
            delegate.search.results[0].preview.as_deref(),
            Some("the unique needle message")
        );
    });

    // Confirm the row twice: the second confirm is deduplicated.
    let confirm_first = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            list.update(cx, |list, cx| {
                list.delegate_mut()
                    .set_selected_index(Some(IndexPath::default()), window, cx);
                list.delegate_mut().confirm(false, window, cx);
            });
        });
        cx.run_until_parked();
    };
    confirm_first(cx);
    confirm_first(cx);

    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert_eq!(composer.drafts.len(), 1);
        assert!(
            composer.drafts[0]
                .chip_label()
                .as_ref()
                .contains("the unique needle message")
        );
        assert!(
            composer
                .selected
                .borrow()
                .contains(&composer.drafts[0].reference)
        );
    });

    // Removing the chip only touches draft state; the source stays readable.
    let reference = cx.update(|_, cx| composer.read(cx).drafts[0].reference.clone());
    cx.update(|_, cx| {
        composer.update(cx, |composer, cx| {
            composer.remove_draft(reference.clone(), cx)
        });
    });
    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert!(composer.drafts.is_empty());
        assert!(!composer.selected.borrow().contains(&reference));
    });
}

#[gpui::test]
fn confirm_reports_typed_unavailability(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "soon deleted needle");
    });
    let cx = cx.add_empty_window();
    let composer = cx.update(|window, cx| cx.new(|cx| ChatReferenceComposer::new(window, cx)));

    let list = cx.update(|_, cx| composer.read(cx).list.clone());
    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| composer.set_open(true, window, cx));
    });
    cx.update(|window, cx| {
        let task = list.update(cx, |list, cx| {
            list.delegate_mut()
                .perform_search("soon deleted", window, cx)
        });
        task.detach();
    });
    cx.run_until_parked();

    // Delete the source session between search and confirm.
    cx.update(|_, cx| {
        let mut lifecycle = cx
            .global::<SessionStores>()
            .clone()
            .chat()
            .expect("Chat lifecycle capability");
        let session_id = list.read(cx).delegate().search.results[0]
            .reference
            .session_id
            .clone();
        lifecycle
            .delete_session(&session_id)
            .expect("delete source");
    });

    cx.update(|window, cx| {
        list.update(cx, |list, cx| {
            list.delegate_mut()
                .set_selected_index(Some(IndexPath::default()), window, cx);
            list.delegate_mut().confirm(false, window, cx);
        });
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert_eq!(
            composer.confirm_error,
            Some(ReferenceConfirmError::Unavailable(
                ChatMessageUnavailableReason::SessionDeleted
            ))
        );
        assert!(composer.drafts.is_empty());
    });
}

#[gpui::test]
fn confirm_reports_oversized_messages(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        let oversized = format!("giant needle {}", "x".repeat(60_000));
        seed_chat_message(memory, &oversized);
    });
    let cx = cx.add_empty_window();
    let composer = cx.update(|window, cx| cx.new(|cx| ChatReferenceComposer::new(window, cx)));

    let list = cx.update(|_, cx| composer.read(cx).list.clone());
    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| composer.set_open(true, window, cx));
    });
    cx.update(|window, cx| {
        let task = list.update(cx, |list, cx| {
            list.delegate_mut()
                .perform_search("giant needle", window, cx)
        });
        task.detach();
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
        list.update(cx, |list, cx| {
            list.delegate_mut()
                .set_selected_index(Some(IndexPath::default()), window, cx);
            list.delegate_mut().confirm(false, window, cx);
        });
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert_eq!(
            composer.confirm_error,
            Some(ReferenceConfirmError::TooLarge {
                limit: crate::session::MAX_REFERENCE_MESSAGE_BYTES
            })
        );
        assert!(composer.drafts.is_empty());
    });
}
