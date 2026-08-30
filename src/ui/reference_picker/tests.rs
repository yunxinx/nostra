use super::*;

use crate::llm::{ContentBlock, Message, ProviderMetadata, Role};
use crate::session::{
    ChatMessageUnavailable, ChatSessionRef, EntryId, ReferencedMessage, SessionDomain, SessionId,
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

// ---------------------------------------------------------------------------
// Token parsing
// ---------------------------------------------------------------------------

#[test]
fn token_activates_after_dollar_at_word_boundary() {
    let token = active_dollar_token("look at $needle", "look at $needle".len());
    assert_eq!(
        token,
        Some(ActiveToken {
            start: 8,
            end: 15,
            query: "needle".to_string(),
        })
    );

    // Bare `$` right after typing it: active with a blank query.
    let token = active_dollar_token("$", 1);
    assert_eq!(
        token,
        Some(ActiveToken {
            start: 0,
            end: 1,
            query: String::new(),
        })
    );

    // Line start and newline boundaries both count.
    assert!(active_dollar_token("$q", 2).is_some());
    assert!(active_dollar_token("one\ntwo $q", 9).is_some());
}

#[test]
fn token_rejects_mid_word_and_separated_dollars() {
    // `$` glued to a word is ordinary text (prices, shell hints).
    assert!(active_dollar_token("cost$5", 6).is_none());
    assert!(active_dollar_token("a$b", 3).is_none());
    // `$$` is not a token start.
    assert!(active_dollar_token("$$", 2).is_none());
    // Whitespace after `$` closes the token.
    assert!(active_dollar_token("$ needle", 8).is_none());
    // Cursor before the `$` sees nothing.
    assert!(active_dollar_token("$q", 0).is_none());
    // Absurdly long queries are treated as ordinary text.
    assert!(active_dollar_token(&format!("${}", "x".repeat(100)), 101).is_none());
}

#[test]
fn token_tracks_the_cursor_inside_the_query() {
    let text = "$needle and more";
    // Caret still inside the query: token ends at the caret.
    let token = active_dollar_token(text, 4).unwrap();
    assert_eq!(token.query, "nee");
    assert_eq!(token.end, 4);
    // Caret after a space: no active token even though a `$` exists.
    assert!(active_dollar_token(text, 9).is_none());
    // Multibyte characters before the token do not corrupt offsets.
    let text = "你好 $引用";
    let token = active_dollar_token(text, text.len()).unwrap();
    assert_eq!(token.query, "引用");
    assert_eq!(&text[token.start..token.end], "$引用");
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
    assert!(search.begin("").is_none());
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

    assert_eq!(draft.kind, ChatReferenceKind::Message(reference));
    assert_eq!(draft.session_title.as_deref(), Some("Session"));
    assert_eq!(draft.snippet.as_deref(), Some("first line\nsecond line"));
    assert_eq!(draft.timestamp, 42);
    assert_eq!(draft.chip_label().as_ref(), "first line…");

    // An empty body falls back to the localized placeholder title.
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
        kind: ChatReferenceKind::Message(chat_reference()),
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
    selected.insert(first.kind.clone());
    if selected.insert(second.kind.clone()) {
        drafts.push(second);
    }
    assert_eq!(drafts.len(), 1, "same reference cannot join twice");

    // A different reference is independent.
    let other = ChatReferenceDraft::from_read(sample_read(chat_reference(), None));
    assert!(selected.insert(other.kind.clone()));

    // A session-level chip in the same chat is not a duplicate of a message chip.
    let session = ChatSessionRef::new(reference.session_id.clone()).expect("session ref");
    let session_kind = ChatReferenceKind::Session(session);
    assert!(selected.insert(session_kind));
}

#[test]
fn search_results_group_by_session() {
    let session_a = SessionId::new(SessionDomain::Chat);
    let session_b = SessionId::new(SessionDomain::Chat);
    let a1 = preview(
        ChatMessageRef {
            session_id: session_a.clone(),
            entry_id: EntryId::new(),
        },
        Some("Alpha"),
        10,
    );
    let a2 = preview(
        ChatMessageRef {
            session_id: session_a.clone(),
            entry_id: EntryId::new(),
        },
        Some("Alpha"),
        11,
    );
    let b1 = preview(
        ChatMessageRef {
            session_id: session_b,
            entry_id: EntryId::new(),
        },
        Some("Beta"),
        12,
    );
    let groups = group_search_results(&[a1, b1, a2]);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].session_title.as_deref(), Some("Alpha"));
    assert_eq!(groups[0].messages.len(), 2);
    assert_eq!(groups[1].session_title.as_deref(), Some("Beta"));
    assert_eq!(groups[1].messages.len(), 1);
}

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
    let now = 1_750_000_000_000_i64;
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
        crate::appearance::fonts::init(Default::default(), cx);
        let mut memory = InMemorySessionStore::new();
        seed(&mut memory);
        cx.set_global(SessionStores::with_chat_store(memory));
    });
}

fn new_composer(cx: &mut gpui::VisualTestContext) -> gpui::Entity<ChatReferenceComposer> {
    let reference_store = cx.update(|_, cx| cx.global::<SessionStores>().chat_references().ok());
    cx.update(|window, cx| {
        cx.new(|cx| {
            ChatReferenceComposer::new_with_reference_store(reference_store.clone(), window, cx)
        })
    })
}

/// `set_value` suppresses change events, so drive the same path real typing
/// takes: `replace_all` emits `InputEvent::Change`.  Multi-line `replace_all`
/// rewinds the caret to 0; real typing leaves it after the inserted text, so
/// park it there.
fn set_input_value(
    cx: &mut gpui::VisualTestContext,
    input: &gpui::Entity<TextareaState>,
    value: &str,
) {
    cx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.replace_all(value, window, cx);
            state.set_selected_range(value.len()..value.len(), cx);
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
fn dollar_token_drives_inline_completion(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "the unique needle message");
        seed_chat_message(memory, "an unrelated conversation");
    });
    let cx = cx.add_empty_window();
    let composer = new_composer(cx);
    let input = cx.update(|_, cx| composer.read(cx).input.clone());

    set_input_value(cx, &input, "note the $ne");
    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert!(composer.is_completion_open());
        assert_eq!(composer.completion.search.results.len(), 1);
        assert_eq!(
            composer.completion.search.results[0].preview.as_deref(),
            Some("the unique needle message")
        );
    });

    // A space after the token closes the popup without a store request.
    set_input_value(cx, &input, "note the $ne and more");
    cx.update(|_, cx| {
        assert!(!composer.read(cx).is_completion_open());
    });

    // Escape dismissed state: retyping the token reopens it.
    set_input_value(cx, &input, "note the $ne");
    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| composer.dismiss_completion(cx));
        let _ = window;
    });
    cx.update(|_, cx| {
        assert!(!composer.read(cx).is_completion_open());
    });
    set_input_value(cx, &input, "note the $");
    cx.update(|_, cx| {
        // Bare `$` shows the hint state without querying the store.
        let composer = composer.read(cx);
        assert!(composer.is_completion_open());
        assert_eq!(
            composer.completion.search.status,
            ReferenceSearchStatus::Idle
        );
        assert!(composer.completion.search.results.is_empty());
    });
}

#[gpui::test]
fn confirming_completion_removes_token_and_adds_chip(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "the unique needle message");
    });
    let cx = cx.add_empty_window();
    let composer = new_composer(cx);
    let input = cx.update(|_, cx| composer.read(cx).input.clone());

    set_input_value(cx, &input, "note the $needle");

    // Confirm twice: the second pass deduplicates the reference.
    let confirm = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| composer.confirm_completion(window, cx));
        });
        cx.run_until_parked();
    };
    confirm(cx);
    confirm(cx);

    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert_eq!(composer.drafts.len(), 1);
        assert!(matches!(
            composer.drafts[0].kind,
            ChatReferenceKind::Session(_)
        ));
        assert!(composer.selected.contains(&composer.drafts[0].kind));
        // The `$needle` token was removed from the draft text.
        assert_eq!(input.read(cx).value().as_ref(), "note the ");
        // Completion closed with the token gone.
        assert!(!composer.is_completion_open());
    });

    // Removing the chip only touches draft state.
    let kind = cx.update(|_, cx| composer.read(cx).drafts[0].kind.clone());
    cx.update(|_, cx| composer.update(cx, |composer, cx| composer.remove_draft(kind.clone(), cx)));
    cx.update(|_, cx| {
        let composer = composer.read(cx);
        assert!(composer.drafts.is_empty());
        assert!(!composer.selected.contains(&kind));
    });
}

#[gpui::test]
fn popup_cursor_moves_within_results(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "needle alpha");
        seed_chat_message(memory, "needle beta");
    });
    let cx = cx.add_empty_window();
    let composer = new_composer(cx);
    let input = cx.update(|_, cx| composer.read(cx).input.clone());
    set_input_value(cx, &input, "$needle");

    cx.update(|_, cx| {
        assert_eq!(composer.read(cx).visible_completion_rows().len(), 2);
    });
    cx.update(|_, cx| {
        composer.update(cx, |composer, cx| composer.move_completion_cursor(1, cx));
    });
    cx.update(|_, cx| {
        assert_eq!(composer.read(cx).completion.cursor, 1);
    });
    // Clamped at both ends.
    cx.update(|_, cx| {
        composer.update(cx, |composer, cx| composer.move_completion_cursor(1, cx));
        composer.update(cx, |composer, cx| composer.move_completion_cursor(1, cx));
    });
    cx.update(|_, cx| {
        assert_eq!(composer.read(cx).completion.cursor, 1);
    });
    cx.update(|_, cx| {
        composer.update(cx, |composer, cx| composer.move_completion_cursor(-5, cx));
    });
    cx.update(|_, cx| {
        assert_eq!(composer.read(cx).completion.cursor, 0);
    });
}

#[gpui::test]
fn confirm_reports_typed_unavailability(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "soon deleted needle");
    });
    let cx = cx.add_empty_window();
    let composer = new_composer(cx);
    let input = cx.update(|_, cx| composer.read(cx).input.clone());
    set_input_value(cx, &input, "$soon");

    cx.update(|_, cx| {
        composer.update(cx, |composer, cx| composer.expand_highlighted_session(cx));
        composer.update(cx, |composer, cx| composer.move_completion_cursor(1, cx));
    });

    // Delete the source session between search and confirm.
    cx.update(|_, cx| {
        let mut lifecycle = cx
            .global::<SessionStores>()
            .clone()
            .chat()
            .expect("Chat lifecycle capability");
        let session_id = composer.read(cx).completion.search.results[0]
            .reference
            .session_id
            .clone();
        lifecycle
            .delete_session(&session_id)
            .expect("delete source");
    });

    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| composer.confirm_completion(window, cx));
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
    let composer = new_composer(cx);
    let input = cx.update(|_, cx| composer.read(cx).input.clone());
    set_input_value(cx, &input, "$giant");

    cx.update(|_, cx| {
        composer.update(cx, |composer, cx| composer.expand_highlighted_session(cx));
        composer.update(cx, |composer, cx| composer.move_completion_cursor(1, cx));
    });

    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| composer.confirm_completion(window, cx));
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

/// Renders the composer as a window root so keystroke dispatch reaches the
/// real listener tree (wrapper `on_key_down`, input keymap context).
struct ComposerHost(gpui::Entity<ChatReferenceComposer>);

impl gpui::Render for ComposerHost {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

/// Dispatch a keystroke as an action-only key event.  `simulate_keystrokes`
/// routes through `dispatch_keystroke`, whose simulated-IME tail inserts the
/// parsed `key_char` (e.g. "\n" for "enter") after the action propagates;
/// real platform events never take that tail.
fn press_key(cx: &mut gpui::VisualTestContext, key: &str, control: bool) {
    let keystroke = gpui::Keystroke {
        key: key.to_string(),
        key_char: None,
        modifiers: gpui::Modifiers {
            control,
            ..Default::default()
        },
    };
    cx.simulate_event(gpui::KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    cx.simulate_event(gpui::KeyUpEvent { keystroke });
}

#[gpui::test]
fn completion_keys_route_through_real_keystrokes(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "needle alpha");
        seed_chat_message(memory, "needle beta");
    });
    let composer_cell: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<ChatReferenceComposer>>>,
    > = std::rc::Rc::new(std::cell::RefCell::new(None));
    let reference_store = cx.update(|cx| cx.global::<SessionStores>().chat_references().ok());
    let cell = composer_cell.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let composer = cx.new(|cx| {
            ChatReferenceComposer::new_with_reference_store(reference_store.clone(), window, cx)
        });
        composer.update(cx, |composer, cx| composer.focus_input(window, cx));
        *cell.borrow_mut() = Some(composer.clone());
        let host = cx.new(|_| ComposerHost(composer));
        gpui_component::Root::new(host, window, cx)
    });
    let composer = composer_cell
        .borrow()
        .clone()
        .expect("composer built with the window root");

    let input = cx.update(|_, cx| composer.read(cx).input.clone());
    set_input_value(cx, &input, "$needle");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });

    // ctrl-n / ctrl-p move the popup cursor through the wrapper listener.
    press_key(cx, "n", true);
    cx.update(|_, cx| assert_eq!(composer.read(cx).completion.cursor, 1));
    press_key(cx, "p", true);
    cx.update(|_, cx| assert_eq!(composer.read(cx).completion.cursor, 0));

    // Enter arrives as PressEnter and confirms the selected row.
    press_key(cx, "enter", false);
    cx.run_until_parked();
    cx.update(|_, cx| {
        let state = composer.read(cx);
        assert_eq!(state.drafts.len(), 1);
        assert!(matches!(
            state.drafts[0].kind,
            ChatReferenceKind::Session(_)
        ));
        assert!(!state.is_completion_open());
        assert_eq!(input.read(cx).value().as_ref(), "");
    });

    // Escape closes a reopened popup through the wrapper listener.
    set_input_value(cx, &input, "$nee");
    cx.update(|_, cx| assert!(composer.read(cx).is_completion_open()));
    press_key(cx, "escape", false);
    cx.update(|_, cx| {
        assert!(!composer.read(cx).is_completion_open());
    });
}

#[gpui::test]
fn tab_expands_session_and_enter_confirms_message(cx: &mut TestAppContext) {
    seed_global_stores(cx, |memory| {
        seed_chat_message(memory, "the unique needle message");
    });
    let composer_cell: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<ChatReferenceComposer>>>,
    > = std::rc::Rc::new(std::cell::RefCell::new(None));
    let reference_store = cx.update(|cx| cx.global::<SessionStores>().chat_references().ok());
    let cell = composer_cell.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let composer = cx.new(|cx| {
            ChatReferenceComposer::new_with_reference_store(reference_store.clone(), window, cx)
        });
        composer.update(cx, |composer, cx| composer.focus_input(window, cx));
        *cell.borrow_mut() = Some(composer.clone());
        let host = cx.new(|_| ComposerHost(composer));
        gpui_component::Root::new(host, window, cx)
    });
    let composer = composer_cell
        .borrow()
        .clone()
        .expect("composer built with the window root");

    let input = cx.update(|_, cx| composer.read(cx).input.clone());
    set_input_value(cx, &input, "$needle");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });

    press_key(cx, "tab", false);
    cx.update(|_, cx| {
        let state = composer.read(cx);
        assert!(state.completion.expanded_session.is_some());
        assert_eq!(state.visible_completion_rows().len(), 2);
    });

    press_key(cx, "n", true);
    press_key(cx, "enter", false);
    cx.run_until_parked();
    cx.update(|_, cx| {
        let state = composer.read(cx);
        assert_eq!(state.drafts.len(), 1);
        assert!(matches!(
            state.drafts[0].kind,
            ChatReferenceKind::Message(_)
        ));
        assert!(
            state.drafts[0]
                .chip_label()
                .as_ref()
                .contains("the unique needle message")
        );
        assert!(!state.is_completion_open());
        assert_eq!(input.read(cx).value().as_ref(), "");
    });
}

#[test]
fn reference_picker_labels_resolve_in_every_locale() {
    for locale in ["en", "zh-CN"] {
        for key in [
            "reference_picker.composer_placeholder",
            "reference_picker.hint",
            "reference_picker.empty",
            "reference_picker.empty_turns",
            "reference_picker.untitled_chat",
        ] {
            let resolved = t!(key, locale = locale).to_string();
            assert_ne!(resolved, key, "{key} unresolved for {locale}");
            assert!(!resolved.is_empty(), "{key} empty for {locale}");
        }
    }
}
