//! Structural tests for the materialization window and row view.

use gpui::{ListOffset, Pixels, TestAppContext, point, px};

use crate::chat::tests::{add_chat_window, init_app, redraw, redraw_settled};

#[gpui::test]
fn open_window_materializes_only_rows_near_the_tail(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 500, 50, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    let (materialized, total) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        (
            chat.view.materialized_row_count(),
            chat.view.projection.len(),
        )
    });
    assert!(
        materialized < total,
        "a {total}-row transcript must not materialize every row"
    );
    // The window holds roughly the viewport ± 3 screens; the fixture rows are
    // short, so a generous but still sub-linear bound catches regressions to
    // full materialization.
    assert!(
        materialized * 8 < total,
        "materialized {materialized} of {total} rows: windowing collapsed"
    );
}

#[gpui::test]
fn scrolling_release_keeps_renderers_out_of_the_retain_zone(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 300, 0, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    // Scroll to the top: rows near the tail must release.
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            chat.view.list_state.scroll_to(ListOffset::default());
        });
    });
    redraw(cx);
    redraw(cx);
    let at_top = cx.update(|_, cx| chat.read(cx).view.materialized_row_count());
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| chat.view.list_state.scroll_to_end());
    });
    redraw(cx);
    redraw(cx);
    let at_tail = cx.update(|_, cx| chat.read(cx).view.materialized_row_count());
    assert!(
        at_top < 300 && at_tail < 300,
        "both ends of the transcript must stay windowed (top={at_top}, tail={at_tail})"
    );
}

#[gpui::test]
fn streaming_appends_do_not_move_a_reader_scrolled_up(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 60, 0, cx);
        });
    });
    redraw(cx);
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            // Leave the tail: anchor the list one screen up.
            let anchor = chat.view.list_state.logical_scroll_top();
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
            chat.view.list_state.scroll_to(ListOffset {
                item_ix: anchor.item_ix.saturating_sub(4),
                offset_in_item: px(0.),
            });
        });
    });
    redraw(cx);
    let before = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::append_text(chat, 0, "late-text".into(), " more.", cx);
        });
    });
    redraw(cx);

    let after = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    assert_eq!(
        before.item_ix, after.item_ix,
        "appends must not move the anchor"
    );
    assert_eq!(
        before.offset_in_item, after.offset_in_item,
        "appends must not move the anchor offset"
    );
}

#[gpui::test]
fn a_prepend_keeps_the_anchor_row_content_stable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_paged_conversation(chat, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    // Land on the earliest loaded turn and capture it.
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            chat.view.list_state.scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(0.),
            });
        });
    });
    redraw_settled(cx);
    let (anchor_text, anchor_index_before) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let ix = chat.view.list_state.logical_scroll_top().item_ix;
        (chat.view.projection.row(ix).map(|row| row.debug_name()), ix)
    });

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::load_earlier_page(chat, cx);
        });
    });
    redraw_settled(cx);

    // The anchored row must survive the prepend with the same identity and
    // sit below the freshly inserted rows (its content did not move with the
    // reading position, AC3).
    let anchored = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let Some(anchor) = &anchor_text else {
            return false;
        };
        match chat
            .view
            .projection
            .rows()
            .iter()
            .position(|row| &row.debug_name() == anchor)
        {
            Some(ix) => ix >= anchor_index_before,
            None => false,
        }
    });
    assert!(anchored, "the anchor row must survive the prepend unmoved");
}

#[gpui::test]
fn hovering_a_row_reveals_its_turn_actions_and_leaving_hides_them(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_completed_exchange(chat, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    // The assistant turn is the second turn; its actions row is the one
    // whose turn matches the prose row we will hover.
    let (actions_row, prose_turn) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let prose_row = chat
            .view
            .projection
            .rows()
            .iter()
            .find(|row| row.kind() == crate::chat::projection::RowKind::AssistantProse)
            .map(|row| row.id())
            .expect("assistant prose row");
        let actions = chat
            .view
            .projection
            .rows()
            .iter()
            .find(|row| {
                row.kind() == crate::chat::projection::RowKind::TurnActions
                    && row.id().turn == prose_row.turn
            })
            .map(|row| row.id())
            .expect("a completed assistant turn has an actions row");
        (actions, prose_row.turn)
    });
    let debug_name = actions_row.debug_name();

    // Selection alone must not pin hover: without a pointer over the turn
    // the row hover state stays off (AC4).
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        None,
        "selection must not reveal the actions row"
    );

    // Settle layout first: materialization shifts row heights, and the
    // hover hit test must run against the settled geometry.
    redraw(cx);
    redraw(cx);

    // The pointer enters a row of the turn: move it over the prose row and
    // let the row's own on_hover drive the state. Hover events dispatch
    // synchronously with the move, so no redraw is needed to observe them.
    let prose_bounds = cx
        .debug_bounds("row-prose-2-2")
        .expect("the prose row is visible");
    cx.simulate_mouse_move(prose_bounds.center(), None, gpui::Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        Some(prose_turn),
        "hovering the turn tracks the hovered turn"
    );
    assert!(
        cx.debug_bounds(Box::leak(debug_name.clone().into_boxed_str()))
            .is_some(),
        "the actions row is in the tree"
    );

    // Walking DOWN one row inside the same turn (prose -> actions row) must
    // keep the hover alive: gpui re-evaluates every hover element in one
    // move, and a naive leave-clears model would let the leave event of the
    // row above erase the enter event of the row below.
    let actions_bounds = cx
        .debug_bounds(Box::leak(debug_name.clone().into_boxed_str()))
        .expect("the actions row is visible under hover");
    cx.simulate_mouse_move(actions_bounds.center(), None, gpui::Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        Some(prose_turn),
        "hover survives a downward row-to-row move inside the turn"
    );

    // Walking DOWN across the turn boundary (user bubble -> assistant prose)
    // must switch the hovered turn, not clear it.
    let bubble_bounds = cx
        .debug_bounds("row-userbubble-1-1-bubble")
        .expect("the user bubble is visible");
    cx.simulate_mouse_move(bubble_bounds.center(), None, gpui::Modifiers::default());
    let user_turn = cx.update(|_, cx| {
        chat.read(cx)
            .view
            .projection
            .rows()
            .iter()
            .find(|row| row.kind() == crate::chat::projection::RowKind::UserBubble)
            .map(|row| row.id().turn)
    });
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        user_turn,
        "moving down into another turn switches the hovered turn"
    );

    // Back onto the assistant prose row, then up and out of every row.
    let prose_bounds = cx
        .debug_bounds("row-prose-2-2")
        .expect("the prose row is visible");
    cx.simulate_mouse_move(prose_bounds.center(), None, gpui::Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        Some(prose_turn),
        "re-entering the assistant turn re-arms its hover"
    );
    cx.simulate_mouse_move(point(px(320.), px(2.)), None, gpui::Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        None,
        "moving up out of every row clears the hover"
    );

    // The pointer leaves the turn's rows for real: moving it below the list
    // fires the row's hover(false), which clears the turn.
    cx.simulate_mouse_move(point(px(320.), px(470.)), None, gpui::Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.hovered_turn()),
        None,
        "leaving the rows hides the actions again"
    );
}

#[gpui::test]
fn cold_restore_rebuilds_with_the_saved_projection_and_anchor(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 200, 0, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    // The anchor row's identity and the live typography, captured before the
    // cold transition (the rebuild must measure under the same key for the
    // settled heights to count).
    let (projection, anchor, anchor_row, typography) = cx.update(|_, cx| {
        chat.update(cx, |chat, ctx| {
            let anchor = chat.view.list_state.logical_scroll_top();
            let row = chat.view.projection.row(anchor.item_ix).map(|row| row.id());
            let typography = chat.view.typography;
            let (projection, saved) = chat.cool_down(ctx);
            (projection, saved, row, typography)
        })
    });
    assert!(anchor.is_some(), "a scrolled transcript keeps an anchor");
    let anchor = anchor.expect("anchor");
    let anchor_row = anchor_row.expect("anchor row");
    let rows_before = projection.len();

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            let snapshot = chat.transcript.read(cx).snapshot();
            let restored = crate::chat::view::TranscriptView::new(
                &chat.transcript,
                &snapshot,
                typography,
                Some((projection, Some(anchor))),
                cx,
            );
            assert_eq!(restored.projection.len(), rows_before);

            // Restore hint: coverage-weighted blend of the settled mean and
            // the uniform estimate — a plain settled mean would under-size
            // the first-frame scrollbar when few rows carry measurements.
            let key = restored
                .projection
                .rows()
                .iter()
                .find_map(|row| row.measured_key())
                .expect("the cold projection carries settled measurements");
            let settled: Vec<f32> = restored
                .projection
                .rows()
                .iter()
                .filter_map(|row| row.settled_height(&key))
                .map(Pixels::as_f32)
                .collect();
            assert!(
                !settled.is_empty() && settled.len() < restored.projection.len(),
                "the fixture must mix settled and estimated rows"
            );
            let settled_mean = settled.iter().copied().sum::<f32>() / settled.len() as f32;
            let coverage = settled.len() as f32 / restored.projection.len() as f32;
            let expected_hint = settled_mean * coverage
                + crate::chat::MESSAGE_HEIGHT_HINT.as_f32() * (1. - coverage);
            assert_eq!(
                restored.restore_hint().as_f32(),
                expected_hint,
                "the cold hint must blend settled coverage with the uniform estimate"
            );

            let restored_top = restored.list_state.logical_scroll_top();
            assert_eq!(restored_top.item_ix, anchor.item_ix);
            assert_eq!(restored_top.offset_in_item, anchor.offset_in_item);
            // The anchor row keeps its content identity after the rebuild.
            assert_eq!(
                restored
                    .projection
                    .row(restored_top.item_ix)
                    .map(|row| row.id()),
                Some(anchor_row),
                "the restored reading position must land on the anchor row's content"
            );
        });
    });
}

#[gpui::test]
fn measured_rows_cache_heights_for_the_cold_first_frame(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_completed_exchange(chat, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    let settled = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let key = chat.view.current_key();
        chat.view
            .projection
            .rows()
            .iter()
            .filter(|row| row.settled_height(&key).is_some())
            .count()
    });
    assert!(
        settled > 0,
        "rows painted from settled content must cache their measured height"
    );
}

/// The collapsed group header's toggle disappears while the group is
/// expanded; the first member row must carry the affordance that folds it
/// back, round-tripping the one stable group id.
#[gpui::test]
fn an_expanded_tool_group_collapses_through_its_leader_row(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::push_empty(chat, crate::chat::Role::Assistant, cx);
            for index in 0..3 {
                crate::chat::test_support::start_tool_call(
                    chat,
                    index,
                    index,
                    format!("call-{index}"),
                    "lookup".into(),
                    cx,
                );
            }
        });
    });
    redraw_settled(cx);

    let group_id = cx
        .update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .find(|row| row.kind() == crate::chat::projection::RowKind::ToolActivityGroup)
                .map(|row| row.id())
        })
        .expect("collapsed group row");
    let expand_selector: &'static str =
        Box::leak(format!("{}-expand", group_id.debug_name()).into_boxed_str());
    let expand_bounds = cx
        .debug_bounds(expand_selector)
        .expect("the collapsed header offers the expand control");

    cx.simulate_click(expand_bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);

    // Expanded: exactly one member leads, and it carries the group id the
    // collapse control sends.
    let leader = cx
        .update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .find(|row| row.group().is_some() && row.leads_group())
                .map(|row| (row.id(), row.group()))
        })
        .expect("leader member row after expansion");
    assert_eq!(
        leader.1,
        Some(group_id),
        "expand/collapse must round-trip one stable group id"
    );
    let collapse_selector: &'static str =
        Box::leak(format!("{}-collapse", leader.0.debug_name()).into_boxed_str());
    let collapse_bounds = cx
        .debug_bounds(collapse_selector)
        .expect("the leader row renders the collapse control");

    cx.simulate_click(collapse_bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);

    let (group_back, no_members) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        (
            chat.view
                .projection
                .rows()
                .iter()
                .any(|row| row.id() == group_id),
            chat.view
                .projection
                .rows()
                .iter()
                .all(|row| row.group().is_none()),
        )
    });
    assert!(group_back, "the group row returns with its original id");
    assert!(no_members, "collapsing clears every member declaration");
    assert!(
        cx.debug_bounds(expand_selector).is_some(),
        "the header is back"
    );
    assert!(
        cx.debug_bounds(collapse_selector).is_none(),
        "the collapse control is gone with the expansion"
    );
}

/// AC1 across a full reading round trip: after every settled stop, the
/// materialized set stays inside the retain zone computed for wherever the
/// list actually is.
#[gpui::test]
fn materialized_rows_stay_in_the_retain_zone_through_a_round_trip(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 500, 50, cx);
        });
    });
    redraw_settled(cx);

    let total = cx.update(|_, cx| chat.read(cx).view.projection.len());
    assert!(total > 100, "the fixture must be long enough to window");

    fn assert_windowed(
        label: &str,
        chat: &gpui::Entity<crate::chat::ChatView>,
        cx: &mut gpui::VisualTestContext,
    ) {
        use super::window::compute_zones;

        let inside = cx.update(|_, cx| {
            let chat = chat.read(cx);
            let view = &chat.view;
            let materialized = view.materialized_row_indices();
            assert!(!materialized.is_empty(), "{label}: nothing materialized");
            let scroll_top = view.list_state.logical_scroll_top();
            let viewport = view.list_state.viewport_bounds().size.height;
            let key = view.current_key();
            let (_, retain) = compute_zones(view.projection.rows(), scroll_top, viewport, &key);
            // This fixture streams nothing, so no turn is exempt from the
            // zone: every materialized row must be retained.
            materialized.iter().all(|ix| retain.contains(ix))
        });
        assert!(inside, "{label}: materialized rows escaped the retain zone");
    }

    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
        });
    });
    for stop in [total / 2, total / 4, 0, total / 4, total / 2] {
        cx.update(|_, cx| {
            chat.update(cx, |chat, _| {
                chat.view.list_state.scroll_to(ListOffset {
                    item_ix: stop,
                    offset_in_item: px(0.),
                });
            });
        });
        redraw_settled(cx);
        assert_windowed("mid-stop", &chat, cx);
    }
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| chat.view.list_state.scroll_to_end());
    });
    redraw_settled(cx);
    assert_windowed("tail", &chat, cx);
}

/// The jump affordance is a pure function of the scroll state: hidden while
/// following the tail (and on an empty transcript), visible once the reader
/// scrolls off the tail.
#[gpui::test]
fn the_jump_button_appears_only_when_the_tail_is_out_of_view(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    redraw_settled(cx);
    assert!(
        !cx.update(|_, cx| chat.read(cx).view.show_jump_button()),
        "an empty transcript has nothing to jump to"
    );

    // A short exchange first: the list's first layout stamps the content
    // width that the rest of the seed grows within, mirroring a live
    // conversation growing after its view opened.
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_completed_exchange(chat, cx);
        });
    });
    redraw_settled(cx);
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 300, 0, cx);
        });
    });
    redraw_settled(cx);
    assert!(
        !cx.update(|_, cx| chat.read(cx).view.show_jump_button()),
        "following the tail keeps the affordance hidden"
    );

    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
            chat.view.list_state.scroll_to(ListOffset::default());
        });
    });
    redraw_settled(cx);
    assert!(
        cx.update(|_, cx| chat.read(cx).view.show_jump_button()),
        "leaving the tail reveals the affordance"
    );
}

/// Clicking the real button restores tail following, lands on the tail, and
/// parks hover: rows that slid under the stationary pointer do not claim it.
#[gpui::test]
fn jumping_to_latest_follows_the_tail_and_parks_hover(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    // Warm the list layout at the final width, then grow the conversation —
    // the live-session shape in which the affordance exists.
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_completed_exchange(chat, cx);
        });
    });
    redraw_settled(cx);
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_large_conversation(chat, 200, 0, cx);
        });
    });
    redraw_settled(cx);
    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
            chat.view.list_state.scroll_to(ListOffset::default());
        });
    });
    redraw_settled(cx);
    assert!(cx.update(|_, cx| chat.read(cx).view.show_jump_button()));

    let button = cx
        .debug_bounds("jump-to-latest")
        .expect("the jump button is in the tree");
    // Rest the pointer on the button first: its hover claims whatever turn
    // sits beneath it *before* the jump.
    cx.simulate_mouse_move(button.center(), None, gpui::Modifiers::default());
    redraw(cx);
    let hovered_before = cx.update(|_, cx| chat.read(cx).view.hovered_turn());
    assert!(hovered_before.is_some(), "the button overlays a row's turn");
    let tail_turn = cx.update(|_, cx| {
        chat.read(cx)
            .view
            .projection
            .rows()
            .last()
            .map(|row| row.id().turn)
    });

    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);

    let (following, visible, at_end) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        (
            chat.view.list_state.is_following_tail(),
            chat.view.show_jump_button(),
            chat.view.list_state.is_scrolled_to_end(),
        )
    });
    assert!(following, "the jump restores tail following");
    assert!(!visible, "the jump hides the button");
    assert_eq!(at_end, Some(true), "the jump lands on the tail");

    // The jump slides tail rows under the stationary pointer; the parked
    // pointer keeps its pre-jump hover and does not adopt what slid beneath.
    let hovered_after = cx.update(|_, cx| chat.read(cx).view.hovered_turn());
    assert_eq!(
        hovered_after, hovered_before,
        "a parked pointer must keep its hover until it moves"
    );
    assert_ne!(
        hovered_after, tail_turn,
        "the rows that slid beneath must not take over the hover"
    );
}

/// AC6 update side: a declared typography change invalidates the old
/// measurements, drops the effective height back to the refreshed estimate,
/// and asks the list to re-measure exactly the affected rows.
#[gpui::test]
fn applying_pending_typography_invalidates_and_remeasures(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_completed_exchange(chat, cx);
        });
    });
    redraw_settled(cx);

    let (key, text_rows) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let key = chat.view.current_key();
        let text_rows: Vec<crate::chat::projection::RowId> = chat
            .view
            .projection
            .rows()
            .iter()
            // Text rows' estimates scale with the line height, so a
            // typography change must remeasure them; chrome rows may keep
            // a constant estimate.
            .filter(|row| {
                matches!(
                    row.kind(),
                    crate::chat::projection::RowKind::AssistantProse
                        | crate::chat::projection::RowKind::UserBubble
                ) && row.measured_key() == Some(key)
            })
            .map(|row| row.id())
            .collect();
        (key, text_rows)
    });
    assert!(
        !text_rows.is_empty(),
        "the fixture must have measured text rows"
    );

    cx.update(|_, cx| {
        chat.update(cx, |chat, _| {
            let mut next = chat.view.typography;
            next.line_height = px(28.);
            next.font_size = px(16.);
            next.typography_revision += 1;
            chat.view.pending_typography = Some(next);
        });
    });
    let changed =
        cx.update(|_, cx| chat.update(cx, |chat, _| chat.view.apply_pending_typography()));
    assert!(changed);

    cx.update(|_, cx| {
        let chat = chat.read(cx);
        let view = &chat.view;
        assert!(
            view.pending_typography.is_none(),
            "the frame declaration is consumed by the apply"
        );
        let new_key = view.current_key();
        assert_ne!(new_key.typography_revision, key.typography_revision);
        for id in &text_rows {
            let ix = view.projection.row_index(*id).expect("row survives");
            let row = &view.projection.rows()[ix];
            assert_eq!(
                row.measured_key(),
                None,
                "the measurement taken under the old typography must be gone"
            );
            assert_eq!(
                row.effective_height(&new_key),
                row.estimated_height(),
                "the effective height falls back to the refreshed estimate"
            );
        }
        let request = view
            .last_remeasure_request
            .as_ref()
            .expect("the apply asked the list to re-measure");
        for id in &text_rows {
            let ix = view.projection.row_index(*id).expect("row survives");
            assert!(
                request.contains(&ix),
                "the remeasure request must include the invalidated row {ix}"
            );
        }
    });
}

/// AC6 render-detection path, covered through the theme revision (font-size
/// switching is not controllable in the test window): bumping the theme
/// revision makes the next render declare the change, and the following
/// update-phase sync invalidates and re-measures.
#[gpui::test]
fn a_theme_change_flows_through_the_frame_observation(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::tests::fixtures::seed_completed_exchange(chat, cx);
        });
    });
    redraw_settled(cx);

    let key = cx.update(|_, cx| chat.read(cx).view.current_key());
    assert!(
        cx.update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .any(|row| row.measured_key() == Some(key))
        }),
        "the fixture must have measured rows"
    );

    cx.update(|_, _| crate::chat::projection::note_theme_changed());
    redraw_settled(cx);

    cx.update(|_, cx| {
        let chat = chat.read(cx);
        let view = &chat.view;
        assert!(
            view.pending_typography.is_none(),
            "the frame declaration must be consumed by the window sync"
        );
        let new_key = view.current_key();
        assert_ne!(new_key.theme_revision, key.theme_revision);
        assert!(
            view.projection
                .rows()
                .iter()
                .all(|row| row.measured_key() != Some(key)),
            "measurements taken under the old theme must be invalidated"
        );
        assert!(
            view.projection
                .rows()
                .iter()
                .any(|row| row.measured_key() == Some(new_key)),
            "repainted rows re-measure under the new theme"
        );
    });
}

/// A view rebuilt while a stream is in flight (cold cycle, first layout)
/// re-reads the accumulated content: the live empty-seed rule only applies
/// to the row of a part a `PartInserted` just created, whose delta the
/// following `Append` event replays.
#[gpui::test]
fn a_view_created_mid_stream_seeds_the_accumulated_stream(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::push_empty(chat, crate::chat::Role::Assistant, cx);
            crate::chat::test_support::start_text(chat, 0, "mid-stream".into(), cx);
            crate::chat::test_support::append_text(chat, 0, "mid-stream".into(), "Hello worl", cx);
        });
    });
    redraw_settled(cx);

    // Cold cycle: hand the projection back and rebuild the view mid-stream.
    let restore = cx.update(|_, cx| {
        chat.update(cx, |chat, ctx| {
            let (projection, anchor) = chat.cool_down(ctx);
            (projection, anchor)
        })
    });
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            let typography = chat.view.typography;
            let snapshot = chat.transcript.read(cx).snapshot();
            chat.view = crate::chat::view::TranscriptView::new(
                &chat.transcript,
                &snapshot,
                typography,
                Some(restore),
                cx,
            );
        });
    });
    redraw_settled(cx);

    fn streaming_prose_text(
        chat: &gpui::Entity<crate::chat::ChatView>,
        cx: &mut gpui::App,
    ) -> Option<String> {
        use crate::chat::projection::RowKind;
        use crate::chat::transcript::PartId;
        let chat = chat.read(cx);
        chat.view
            .projection
            .rows()
            .iter()
            .enumerate()
            .find(|(_, row)| row.kind() == RowKind::AssistantProse && row.id().part != PartId::NONE)
            .and_then(|(ix, _)| {
                chat.view.slots[ix]
                    .renderer
                    .as_any()
                    .downcast_ref::<crate::chat::rows::ProseRenderer>()
            })
            .map(|renderer| renderer.text_for_test().to_string())
    }

    let seeded = cx.update(|_, cx| streaming_prose_text(&chat, cx));
    assert_eq!(
        seeded.as_deref(),
        Some("Hello worl"),
        "the rebuilt streaming row must carry everything streamed so far"
    );

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::append_text(chat, 0, "mid-stream".into(), "d", cx);
        });
    });
    redraw_settled(cx);
    let appended = cx.update(|_, cx| streaming_prose_text(&chat, cx));
    assert_eq!(
        appended.as_deref(),
        Some("Hello world"),
        "the next delta continues the accumulated content instead of restarting it"
    );
}

/// AC5 through the real interaction path: a run of activities renders as one
/// step-stack header; expanding splits it into individually expandable rows;
/// each member's fold state survives a warm rebuild and a collapse/re-expand
/// round trip of the stack itself.
#[gpui::test]
fn a_step_stack_splits_into_individually_expandable_rows(cx: &mut TestAppContext) {
    use crate::chat::projection::{ActivityDisclosure, RowKind};

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::push_empty(chat, crate::chat::Role::Assistant, cx);
            for index in 0..3 {
                crate::chat::test_support::start_tool_call(
                    chat,
                    index,
                    index,
                    format!("call-{index}"),
                    format!("tool-{index}"),
                    cx,
                );
            }
        });
    });
    redraw_settled(cx);

    let group_id = cx
        .update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .find(|row| row.kind() == RowKind::ToolActivityGroup)
                .map(|row| row.id())
        })
        .expect("the run collapses into one step-stack row");
    let expand_selector: &'static str =
        Box::leak(format!("{}-expand", group_id.debug_name()).into_boxed_str());
    let expand_bounds = cx
        .debug_bounds(expand_selector)
        .expect("the step-stack header is drawn");
    cx.simulate_click(expand_bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);

    let member_ids = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let members: Vec<_> = chat
            .view
            .projection
            .rows()
            .iter()
            .filter(|row| row.kind() == RowKind::ToolActivity)
            .map(|row| row.id())
            .collect();
        (
            members,
            chat.view
                .projection
                .rows()
                .iter()
                .filter(|row| row.leads_group())
                .count(),
        )
    });
    assert_eq!(
        member_ids.0.len(),
        3,
        "expanding splits the stack into individual activity rows"
    );
    assert_eq!(member_ids.1, 1, "exactly one member leads the expansion");

    // The first member expands through its own header.
    let first = member_ids.0[0];
    let header_selector: &'static str =
        Box::leak(format!("{}-header", first.debug_name()).into_boxed_str());
    let header_bounds = cx
        .debug_bounds(header_selector)
        .expect("the member's clickable header is drawn");
    cx.simulate_click(header_bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let open_state = cx.update(|_, cx| chat.read(cx).view.projection.disclosure(first).activity);
    assert_eq!(
        open_state,
        ActivityDisclosure::Open {
            arguments_open: true
        },
        "the member row folds independently"
    );

    // A warm rebuild (a new turn) keeps the member's state.
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::push_empty(chat, crate::chat::Role::User, cx);
        });
    });
    redraw_settled(cx);
    let after_rebuild = cx.update(|_, cx| chat.read(cx).view.projection.disclosure(first).activity);
    assert_eq!(
        after_rebuild, open_state,
        "the member's fold state survives a rebuild"
    );

    // Collapse the stack through its leader and re-expand: the member state
    // comes back with it.
    let leader = cx
        .update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .find(|row| row.leads_group())
                .map(|row| row.id())
        })
        .expect("leader member row");
    let collapse_selector: &'static str =
        Box::leak(format!("{}-collapse", leader.debug_name()).into_boxed_str());
    let collapse_bounds = cx
        .debug_bounds(collapse_selector)
        .expect("the leader row carries the stack's collapse control");
    cx.simulate_click(collapse_bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);
    assert!(
        cx.update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .any(|row| row.id() == group_id)
        }),
        "the stack header returns"
    );

    let expand_bounds = cx
        .debug_bounds(expand_selector)
        .expect("header drawn again");
    cx.simulate_click(expand_bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);
    let after_round_trip =
        cx.update(|_, cx| chat.read(cx).view.projection.disclosure(first).activity);
    assert_eq!(
        after_round_trip, open_state,
        "the member's fold state survives the split round trip"
    );
}

/// Regression (P3 dual-axis review): a tool call's arguments arrive with the
/// call's `Finished` event while the tool is still running. The projection
/// must route that change to the activity row, so a user who expanded the row
/// mid-run reads the arguments instead of an empty body until turn end.
#[gpui::test]
fn a_running_tool_call_reveals_its_arguments_once_the_call_finishes(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(480.)));

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::push_empty(chat, crate::chat::Role::Assistant, cx);
            crate::chat::test_support::start_tool_call(
                chat,
                0,
                0,
                "call-0".into(),
                "lookup".into(),
                cx,
            );
        });
    });
    redraw_settled(cx);

    let row_id = cx
        .update(|_, cx| {
            chat.read(cx)
                .view
                .projection
                .rows()
                .iter()
                .find(|row| row.kind() == crate::chat::projection::RowKind::ToolActivity)
                .map(|row| row.id())
        })
        .expect("the activity row of the live call");
    let header_selector: &'static str =
        Box::leak(format!("{}-header", row_id.debug_name()).into_boxed_str());
    let header = cx
        .debug_bounds(header_selector)
        .expect("the activity header paints");
    cx.simulate_click(header.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    redraw(cx);

    let arguments_selector: &'static str =
        Box::leak(format!("{}-arguments", row_id.debug_name()).into_boxed_str());
    assert!(
        cx.debug_bounds(arguments_selector).is_none(),
        "no arguments exist while the call is still streaming"
    );

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            crate::chat::test_support::apply_stream(
                chat,
                &[
                    crate::chat::conversation_runtime::ConversationStreamEvent::ToolCallFinished {
                        content_index: 0,
                        index: 0,
                        tool_call: Box::new(crate::llm::ToolCall {
                            id: "call-0".into(),
                            name: "lookup".into(),
                            arguments: serde_json::json!({ "q": "nostra" }),
                            raw_arguments: r#"{"q":"nostra"}"#.into(),
                            provider_metadata: Default::default(),
                        }),
                    },
                ],
                cx,
            );
        });
    });
    redraw_settled(cx);

    assert!(
        cx.debug_bounds(arguments_selector).is_some(),
        "the finished call's arguments appear while the tool still runs"
    );
}
