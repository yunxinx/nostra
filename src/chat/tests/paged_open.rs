//! Paged-open acceptance tests for the P5 storage cursor (AC1-AC4, R5,
//! anchors): a thousand-turn disk session opens by deserializing only the
//! tail page, backward paging reaches the earliest content, appends during
//! generation interleave cleanly, a lost index falls back and repairs, and
//! the request-side history stays independent of the loaded range.

use std::sync::atomic::Ordering;

use gpui::{ListOffset, px};

use crate::llm::{ContentBlock, Message as LlmMessage, ModelSelection, ProviderMetadata, Role};
use crate::session::{
    ChatSessionCatalogController, ChatSessionController, EntryId, LocalSessionStore,
    LocalStoreConfig, MessageEntry, ResolvedSessionState, SessionDomain, SessionEntryKind,
    SessionFlushStore, SessionHeader, SessionId, SessionLifecycleStore, SessionReadStore,
    SessionStores, SessionTreeStore, Usage,
};

use super::{add_chat_window_with_stores, init_app};
use crate::chat::persistence::{JsonlOffsetSource, open_tail_page, restore::TAIL_PAGE_TURNS};
use crate::chat::transcript::{ResolvedStateSource, TranscriptCursor, TranscriptSource as _};

fn selection() -> ModelSelection {
    ModelSelection {
        profile_id: "profile".into(),
        model_id: "fixture-model".into(),
    }
}

fn message_entry(text: &str, turn: usize) -> SessionEntryKind {
    SessionEntryKind::Message(MessageEntry {
        message: LlmMessage {
            role: if turn % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                provider_metadata: ProviderMetadata::default(),
            }],
            provider_metadata: ProviderMetadata::default(),
        },
        turn_id: Some(format!("turn-{}", turn / 2 + 1)),
        model: Some(selection()),
        usage: Usage::default(),
    })
}

/// Seed `turns` durable turns (two message entries each) in a local Chat
/// store, restart the store, and hand it to the shared service layer.
fn seeded_stores(turns: usize) -> (SessionStores, SessionId, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    let header = SessionHeader::new(SessionDomain::Chat, None);
    let session_id = header.session_id.clone();
    let entries: Vec<SessionEntryKind> = (0..turns * 2)
        .map(|index| message_entry(&format!("message {index}"), index))
        .collect();
    {
        let mut store = LocalSessionStore::open(config.clone()).expect("open seed store");
        store
            .create_session_with_entries(header, entries)
            .expect("seed session");
        store.shutdown().expect("shutdown seed writer");
    }
    let stores =
        SessionStores::with_chat_store(LocalSessionStore::open(config).expect("reopen store"));
    (stores, session_id, root)
}

fn select_session(
    stores: &SessionStores,
    session_id: &SessionId,
) -> crate::session::SelectedChatSession {
    let mut controller =
        ChatSessionCatalogController::new(stores.chat_catalog().expect("catalog capability"));
    controller
        .load_initial()
        .and_then(|_| controller.select(session_id))
        .expect("select session")
}

fn prose_of(turn: &crate::chat::transcript::Turn) -> String {
    turn.parts
        .iter()
        .find_map(|part| match &part.source {
            crate::chat::transcript::PartSource::Prose { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Walk `load_before` from a tail page until the path start and return the
/// texts in order.
fn walk_to_earliest(
    source: &JsonlOffsetSource,
    tail: &crate::chat::transcript::TranscriptPage,
) -> Vec<String> {
    let mut collected: Vec<String> = tail.turns.iter().map(prose_of).collect();
    let mut cursor = match tail.cursor_before.clone() {
        Some(cursor) => cursor,
        None => return collected,
    };
    loop {
        let page = source.load_before(&cursor, TAIL_PAGE_TURNS);
        if page.turns.is_empty() {
            break;
        }
        let mut texts: Vec<String> = page.turns.iter().map(prose_of).collect();
        texts.extend(collected);
        collected = texts;
        let Some(next) = page.cursor_before else {
            break;
        };
        cursor = next;
    }
    collected
}

fn full_state(stores: &SessionStores, session_id: &SessionId) -> ResolvedSessionState {
    stores
        .chat()
        .expect("chat capability")
        .load_session(session_id, None)
        .expect("full load")
}

/// AC1: opening a thousand-turn session deserializes only the tail page —
/// bounded targeted reads, never the whole transcript.
#[test]
fn opening_a_thousand_turn_session_deserializes_only_the_tail_page() {
    let (stores, session_id, _root) = seeded_stores(1_000);
    let selected = select_session(&stores, &session_id);
    let store = stores.chat().expect("chat capability");

    crate::session::READ_ENTRIES_PROBE.store(0, Ordering::SeqCst);
    let opened = open_tail_page(&store, &session_id, &selected).expect("open tail page");
    let reads = crate::session::READ_ENTRIES_PROBE.load(Ordering::SeqCst);

    // 2,000 durable message facts exist; the paged open deserialized only
    // the tail page (50) plus a bounded handful of one-entry reads (model
    // resolution, turn seed), never the whole transcript.
    assert!(
        opened.restore.source.is_some(),
        "the paged source must be used"
    );
    assert_eq!(opened.restore.page.turns.len(), TAIL_PAGE_TURNS);
    assert!(opened.restore.page.cursor_before.is_some());
    assert!(
        reads <= TAIL_PAGE_TURNS + 4 && reads > TAIL_PAGE_TURNS,
        "open deserialized {reads} facts for a 2,000-fact session"
    );
    assert_eq!(opened.restore.next_turn_seed, 1_001);

    // AC1: paging backwards reaches the earliest content, equal entry for
    // entry to the full load.
    let source = opened.restore.source.expect("paged source");
    let collected = walk_to_earliest(&source, &opened.restore.page);
    let state = full_state(&stores, &session_id);
    assert_eq!(collected.len(), state.messages.len());
    for (text, resolved) in collected.iter().zip(&state.messages) {
        assert_eq!(
            text,
            &resolved
                .message
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        );
    }
}

/// AC4: the three `TranscriptSource` methods agree between the in-memory
/// source (full resolved state) and the disk source (entry index + offsets)
/// on the same session.
#[test]
fn memory_and_disk_sources_agree_on_all_three_methods() {
    let turns = 12;
    let (stores, session_id, _root) = seeded_stores(turns);
    let state = full_state(&stores, &session_id);
    let memory = ResolvedStateSource::new(state);

    let store = stores.chat().expect("chat capability");
    let handle = store.tree_read_handle();
    let disk = JsonlOffsetSource::new(
        session_id.clone(),
        handle.clone(),
        handle
            .load_entry_index(&session_id, None)
            .expect("entry index"),
    );

    assert_eq!(memory.total_hint(), disk.total_hint());

    let memory_tail = memory.load_tail(5);
    let disk_tail = disk.load_tail(5);
    assert_eq!(memory_tail.turns.len(), disk_tail.turns.len());
    for (left, right) in memory_tail.turns.iter().zip(&disk_tail.turns) {
        assert_eq!(prose_of(left), prose_of(right));
    }
    assert_eq!(
        memory_tail.cursor_before.is_some(),
        disk_tail.cursor_before.is_some()
    );

    let memory_cursor = memory_tail.cursor_before.clone().expect("memory cursor");
    let disk_cursor = disk_tail.cursor_before.clone().expect("disk cursor");
    let memory_before = memory.load_before(&memory_cursor, 5);
    let disk_before = disk.load_before(&disk_cursor, 5);
    assert_eq!(memory_before.turns.len(), disk_before.turns.len());
    for (left, right) in memory_before.turns.iter().zip(&disk_before.turns) {
        assert_eq!(prose_of(left), prose_of(right));
    }
    assert_eq!(
        memory_before.cursor_before,
        disk_before.cursor_before.map(|cursor| TranscriptCursor {
            entry_id: cursor.entry_id
        })
    );

    // A stale cursor keeps itself and returns an empty page on both sides.
    let stale = TranscriptCursor {
        entry_id: EntryId::new(),
    };
    assert!(memory.load_before(&stale, 2).turns.is_empty());
    assert!(disk.load_before(&stale, 2).turns.is_empty());
}

/// AC2: a turn appended while the conversation is live (the streaming tail
/// path) never disturbs the paged source's immutable prefix: prepend pages
/// still read the original entries and the combined sequence has neither
/// duplicates nor gaps.
#[test]
fn appended_turns_do_not_disturb_pending_prepend_pages() {
    let turns = 60;
    let (stores, session_id, _root) = seeded_stores(turns);
    let selected = select_session(&stores, &session_id);
    let store = stores.chat().expect("chat capability");
    let opened = open_tail_page(&store, &session_id, &selected).expect("open");
    let source = opened.restore.source.expect("paged source");

    // The generation-side append: a new turn persisted while the tail page
    // is already displayed. The transcript write path owns the model's tail.
    let mut lifecycle = store.clone();
    lifecycle
        .append(
            &session_id,
            vec![
                message_entry("appended user", turns * 2),
                message_entry("appended assistant", turns * 2 + 1),
            ],
        )
        .expect("append during generation");

    // The prepend pages still serve the original prefix exactly: the append
    // cannot change the immutable prefix of the path snapshot.
    let collected = walk_to_earliest(&source, &opened.restore.page);
    let mut all: Vec<String> = collected;
    all.push("appended user".into());
    all.push("appended assistant".into());

    let state = full_state(&stores, &session_id);
    let durable: Vec<String> = state
        .messages
        .iter()
        .map(|resolved| {
            resolved
                .message
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .collect();
    // No duplicate or missing turn: every durable message appears once.
    assert_eq!(all, durable);
}

/// AC3: a deleted catalog file rebuilds through the service startup repair
/// seam, and a session-level index loss falls back to the full load first.
#[test]
fn a_lost_index_falls_back_to_a_full_load_and_then_rebuilds() {
    let (stores, session_id, root) = seeded_stores(4);
    let store = stores.chat().expect("chat capability");
    // Session-level index loss: the entries rows disappear while the session
    // row stays, so the paged open must fail closed and fall back.
    {
        let connection = rusqlite::Connection::open(
            LocalStoreConfig::new(root.path(), SessionDomain::Chat).index_path(),
        )
        .expect("open index");
        connection
            .execute("DELETE FROM entries", [])
            .expect("drop entry rows");
    }
    let selected = select_session(&stores, &session_id);
    let opened = open_tail_page(&store, &session_id, &selected).expect("fallback open");
    assert!(
        opened.restore.source.is_none(),
        "an unusable index must fall back to the full load"
    );
    assert_eq!(opened.restore.page.turns.len(), 8);
    assert!(opened.restore.page.cursor_before.is_none());

    // The fallback filed a repair intent: the next mutation rebuilds the
    // index, after which the paged source serves the same session again.
    let mut lifecycle = store.clone();
    lifecycle
        .append(
            &session_id,
            vec![
                message_entry("rebuild marker", 8),
                message_entry("rebuild reply", 9),
            ],
        )
        .expect("mutation rebuilds the projection");
    let selected = select_session(&stores, &session_id);
    let reopened = open_tail_page(&store, &session_id, &selected).expect("paged open");
    assert!(
        reopened.restore.source.is_some(),
        "the index must be rebuilt"
    );
    assert_eq!(reopened.restore.page.turns.len(), TAIL_PAGE_TURNS.min(10));

    // File-level loss (the whole catalog file deleted): the production
    // service opens through `repair_if_needed`, which rebuilds the catalog
    // from JSONL (AC3's second tier).
    let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
    std::fs::remove_file(config.index_path()).expect("delete catalog file");
    let mut restarted = LocalSessionStore::open(config.clone()).expect("reopen after deletion");
    let report = restarted.repair_if_needed().expect("startup repair");
    assert!(report.is_some_and(|report| report.rebuilt >= 1));
    let records = restarted
        .load_entry_index(&session_id, None)
        .expect("entry index after rebuild");
    assert!(records.len() >= 10);
    assert!(records.iter().all(|record| record.byte_len.is_some()));
}

/// R5: after a paged restore that holds only the tail, the next send's
/// request history contains every earlier turn and its turn id does not
/// collide with the durable ones.
#[test]
fn a_paged_restore_keeps_full_request_history_and_unique_turn_ids() {
    let turns = 30;
    let (stores, session_id, _root) = seeded_stores(turns);
    let selected = select_session(&stores, &session_id);
    let store = stores.chat().expect("chat capability");
    let opened = open_tail_page(&store, &session_id, &selected).expect("open");

    // The paged restore sees only the tail page; the open path's turn seed
    // still lands past the durable maximum (design §5.3: the last message
    // entry names it through one targeted read).
    assert!(opened.restore.page.turns.len() < turns * 2);
    assert_eq!(opened.restore.next_turn_seed, turns as u64 + 1);

    // A fresh controller binds exactly like the runtime does and sends the
    // next turn: no `TurnIdAlreadyUsed`, full history including the turns
    // that were never paged into the view.
    let mut controller = ChatSessionController::new(store.clone());
    controller
        .restore_binding(&session_id, opened.selection.clone())
        .expect("restore binding");
    let next_turn = format!("turn-{}", turns + 1);
    let user = LlmMessage {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "next send after a paged restore".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let start = controller
        .begin_turn(user.clone(), selection(), next_turn.clone())
        .expect("turn id must not collide");
    let history = controller.request_history().expect("request history");
    assert_eq!(
        history.len(),
        turns * 2 + 1,
        "history includes every earlier turn plus the new user message"
    );
    assert_eq!(history.last(), Some(&user));
    let state = stores
        .chat()
        .expect("chat capability")
        .load_session(&start.session_id, None)
        .expect("load after begin");
    assert_eq!(state.messages.len(), turns * 2 + 1);
}

/// The anchor contract through the real source path (P2's assertion pattern
/// over the production loader): a prepend page read from disk lands without
/// moving the anchored row.
#[gpui::test]
fn a_prepend_page_from_the_real_source_keeps_the_anchor_stable(cx: &mut gpui::TestAppContext) {
    init_app(cx);
    // Three pages of content: the open settles at the tail page, one
    // auto-triggered page lands while the reader sits at the top, and a
    // final page remains for the explicit production load below.
    let turns = 75;
    let total_model_turns = turns * 2;
    let (stores, session_id, _root) = seeded_stores(turns);
    let selected = select_session(&stores, &session_id);
    let store = stores.chat().expect("chat capability");
    let opened = open_tail_page(&store, &session_id, &selected).expect("open");
    assert!(
        opened.restore.page.cursor_before.is_some(),
        "the session must have an earlier page"
    );

    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.restore_session(&opened.restore, cx)
                .expect("paged restore");
        });
    });
    // The deferred Reset lands, the list resets at the top, and the
    // production scroll trigger loads one earlier page automatically.
    cx.run_until_parked();
    super::redraw_settled(cx);
    cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            assert_eq!(this.transcript_snapshot.turn_count(), TAIL_PAGE_TURNS * 2);
            assert!(this.transcript_snapshot.has_earlier());
        });
    });

    // Anchor the reader near the top of the loaded range.
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
            chat.view.list_state.scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(0.),
            });
        });
    });
    super::redraw_settled(cx);
    let (anchor_text, anchor_index_before) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let ix = chat.view.list_state.logical_scroll_top().item_ix;
        (chat.view.projection.row(ix).map(|row| row.debug_name()), ix)
    });

    // The production scroll trigger drives the real source: anchored at the
    // top, the loader reads one more disk page in the background.
    cx.run_until_parked();
    super::redraw_settled(cx);
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            assert!(
                chat.load_before(cx) || !chat.transcript_snapshot.has_earlier(),
                "either a page is already in flight or everything is loaded"
            );
        });
    });
    cx.run_until_parked();
    super::redraw_settled(cx);

    cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            assert_eq!(
                this.transcript_snapshot.turn_count(),
                total_model_turns,
                "the earlier page must land in full"
            );
            assert!(!this.transcript_snapshot.has_earlier());
        });
    });
    let anchored = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let Some(anchor) = &anchor_text else {
            return false;
        };
        chat.view
            .projection
            .rows()
            .iter()
            .position(|row| &row.debug_name() == anchor)
            .is_some_and(|ix| ix >= anchor_index_before)
    });
    assert!(anchored, "the anchor row must survive the prepend unmoved");
}

/// The loader stops after a failed page instead of spinning on an
/// unreachable cursor, and a new restore re-arms it.
#[gpui::test]
fn a_failed_prepend_page_stalls_the_loader_until_the_next_restore(cx: &mut gpui::TestAppContext) {
    init_app(cx);
    let turns = 60;
    let (stores, session_id, root) = seeded_stores(turns);
    let selected = select_session(&stores, &session_id);
    let store = stores.chat().expect("chat capability");
    let opened = open_tail_page(&store, &session_id, &selected).expect("open");
    // Corrupt the offset rows after the source snapshot was taken: reads
    // fail with a typed index error and every page folds into an empty
    // result.
    {
        let connection = rusqlite::Connection::open(
            LocalStoreConfig::new(root.path(), SessionDomain::Chat).index_path(),
        )
        .expect("open index");
        connection
            .execute("UPDATE entries SET byte_offset = byte_offset + 1", [])
            .expect("skew offsets");
    }
    assert!(opened.restore.page.cursor_before.is_some());

    let (chat, cx) = add_chat_window_with_stores(cx, stores);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.restore_session(&opened.restore, cx)
                .expect("paged restore");
        });
    });
    // The deferred Reset lands and the scroll trigger issues the load; the
    // corrupted offsets make it fail closed, which the view must absorb as
    // a stall rather than a crash or a retry loop.
    cx.run_until_parked();
    super::redraw_settled(cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            assert_eq!(this.transcript_snapshot.turn_count(), TAIL_PAGE_TURNS);
            // The failed page kept the cursor but stopped the loader.
            assert!(this.transcript_snapshot.has_earlier());
        });
        chat.update(cx, |chat, cx| {
            assert!(!chat.load_before(cx), "a stalled loader must not retry");
        });
    });
}
