use super::*;

/// Streaming reasoning follows the same manual-scroll contract as the main
/// transcript: an upward gesture pauses following, and a later downward
/// gesture at the end re-arms it.
#[gpui::test]
fn streaming_reasoning_respects_manual_scroll_position(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..50 {
                test_support::append_reasoning(
                    this,
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the expanded reasoning body was drawn");
    let (bottom_offset, max_offset) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        chat.transcript
            .read(cx)
            .turns()
            .last()
            .map_or((px(0.), px(0.)), |_| {
                let trace = reasoning_part(chat).expect("reasoning trace");
                (trace.scroll_offset().y, trace.scroll_max().y)
            })
    });
    assert!(
        max_offset > px(80.),
        "the fixture must have scrollable reasoning"
    );
    assert_eq!(bottom_offset, -max_offset);

    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);

    let paused_offset = cx.update(|_, cx| {
        let turn = chat.read(cx);
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            !trace.is_following(),
            "upward scrolling must pause following"
        );
        trace.scroll_offset().y
    });
    assert!(paused_offset > bottom_offset, "the card should move upward");

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 50..100 {
                test_support::append_reasoning(
                    this,
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    redraw(cx);
    redraw(cx);

    let still_paused = cx.update(|_, cx| {
        let turn = chat.read(cx);
        reasoning_part(turn)
            .expect("reasoning trace")
            .scroll_offset()
            .y
    });
    assert_eq!(
        still_paused, paused_offset,
        "new reasoning must not force a user who scrolled up back to the end"
    );

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the reasoning body remains visible");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    redraw(cx);

    let rearmed = cx.update(|_, cx| {
        let turn = chat.read(cx);
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            trace.is_following(),
            "scrolling down at the end must re-arm following (offset={:?}, max={:?})",
            trace.scroll_offset(),
            trace.scroll_max()
        );
        trace.scroll_offset().y
    });
    assert!(
        cx.update(|_, cx| {
            let turn = chat.read(cx);
            let trace = reasoning_part(turn).expect("reasoning trace");
            trace.scroll_max().y + rearmed <= STICK_THRESHOLD
        }),
        "the downward gesture should reach the card's end"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "final line.\n\n", cx);
        });
    });
    redraw(cx);
    redraw(cx);

    assert!(cx.update(|_, cx| {
        let turn = chat.read(cx);
        let trace = reasoning_part(turn).expect("reasoning trace");
        trace.scroll_max().y + trace.scroll_offset().y <= STICK_THRESHOLD
    }));
}

#[gpui::test]
fn virtualized_reasoning_stops_following_after_manual_scroll(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..180 {
                test_support::append_reasoning(this,
                    0,
                    "reasoning-virtualized-follow".into(),
                    &format!(
                        "Reasoning line {line}: this fixture keeps enough prose to exercise the retained virtual list.\n\n"
                    ),
                    cx,
                );
            }
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the expanded reasoning body was drawn");
    let (bottom_offset, max_offset, virtualized) = cx.update(|_, cx| {
        let trace = reasoning_part(chat.read(cx)).expect("reasoning trace");
        (
            trace.scroll_offset().y,
            trace.scroll_max().y,
            trace.uses_virtualized_scroll(),
        )
    });
    assert!(
        virtualized,
        "long reasoning must use the retained virtual list"
    );
    assert!(
        max_offset > px(100.),
        "the fixture must have scrollable reasoning"
    );
    assert_eq!(bottom_offset, -max_offset);

    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);
    let paused_offset = cx.update(|_, cx| {
        let trace = reasoning_part(chat.read(cx)).expect("reasoning trace");
        assert!(
            !trace.is_following(),
            "manual scroll must disarm tail follow"
        );
        trace.scroll_offset().y
    });
    assert!(
        paused_offset > bottom_offset,
        "the card should move away from its tail"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(
                this,
                0,
                "reasoning-virtualized-follow".into(),
                "new tail content must not steal the reader's position.\n\n",
                cx,
            );
        });
    });
    redraw(cx);
    redraw(cx);
    let after_append = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("reasoning trace")
            .scroll_offset()
            .y
    });
    assert_eq!(
        after_append, paused_offset,
        "streaming into a virtualized card must preserve a manually chosen offset"
    );
}

/// The reasoning viewport owns every vertical wheel gesture inside its bounds.
/// This remains true regardless of whether transcript wheel smoothing is on.
#[gpui::test]
fn reasoning_wheel_events_never_scroll_the_transcript(cx: &mut TestAppContext) {
    init_app(cx);

    for smooth_scrolling in [false, true] {
        let (chat, cx) = add_chat_window(cx);
        cx.simulate_resize(gpui::size(px(640.), px(420.)));
        cx.update(|_, cx| {
            preferences::update_in_memory(cx, |prefs| {
                prefs.smooth_chat_scrolling = smooth_scrolling;
            });
            chat.update(cx, |this, cx| {
                for index in 0..12 {
                    test_support::push_canonical(
                        this,
                        LlmMessage {
                            role: crate::llm::Role::Assistant,
                            content: vec![ContentBlock::Text {
                                text: format!("earlier message {index}\n\n{}", "body ".repeat(24)),
                                provider_metadata: ProviderMetadata::default(),
                            }],
                            provider_metadata: ProviderMetadata::default(),
                        },
                        cx,
                    );
                }
                for role in [Role::User, Role::Assistant] {
                    test_support::push_empty(this, role, cx);
                }
                for line in 0..60 {
                    test_support::append_reasoning(
                        this,
                        0,
                        "reasoning-0".into(),
                        &format!("Reasoning line {line}.\n\n"),
                        cx,
                    );
                }
            });
        });
        redraw(cx);
        redraw(cx);

        assert!(
            cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y > px(0.)),
            "the transcript fixture must be scrollable"
        );
        let body = cx
            .debug_bounds("reasoning-body-0")
            .expect("the latest reasoning card must be visible");
        let transcript_before = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
        let card_before = cx.update(|_, cx| {
            reasoning_part(chat.read(cx))
                .expect("reasoning trace")
                .scroll_offset()
                .y
        });

        // A previously queued transcript animation must be abandoned as soon
        // as the pointer enters the nested card.
        if smooth_scrolling {
            cx.simulate_event(ScrollWheelEvent {
                position: point(px(320.), px(40.)),
                delta: ScrollDelta::Lines(point(0., -3.)),
                ..Default::default()
            });
            assert!(
                cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining != px(0.)),
                "the transcript fixture must have a queued smooth motion"
            );
        }

        cx.simulate_event(ScrollWheelEvent {
            position: body.center(),
            delta: ScrollDelta::Lines(point(0., 3.)),
            ..Default::default()
        });

        let (transcript_after, card_offset, pending_transcript_scroll, pending_card_scroll) = cx
            .update(|_, cx| {
                let chat = chat.read(cx);
                let trace = reasoning_part(chat).expect("reasoning trace");
                (
                    chat.list_state.logical_scroll_top(),
                    trace.scroll_offset().y,
                    chat.smooth_scroll.remaining,
                    trace.smooth_scroll_remaining(),
                )
            });
        assert_eq!(transcript_after.item_ix, transcript_before.item_ix);
        assert_eq!(
            transcript_after.offset_in_item, transcript_before.offset_in_item,
            "reasoning wheel input leaked into the transcript (smooth={smooth_scrolling})"
        );
        assert!(
            card_offset < px(0.),
            "the reasoning card itself must consume the wheel input"
        );
        assert_eq!(
            pending_transcript_scroll,
            px(0.),
            "reasoning input must not queue transcript motion"
        );
        if smooth_scrolling {
            assert_eq!(
                card_offset, card_before,
                "smooth reasoning scrolling must restore the native wheel jump"
            );
            assert!(
                pending_card_scroll != px(0.),
                "reasoning wheel input must queue eased card motion"
            );
            assert!(
                cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
                "reasoning wheel input must schedule an animation frame"
            );
            redraw(cx);
            let eased_card_offset = cx.update(|_, cx| {
                reasoning_part(chat.read(cx))
                    .expect("reasoning trace")
                    .scroll_offset()
                    .y
            });
            assert!(
                eased_card_offset > card_before,
                "the animation frame must advance the reasoning card"
            );

            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(20.))),
                ..Default::default()
            });
            let (precise_offset, pending_after_precise) = cx.update(|_, cx| {
                let trace = reasoning_part(chat.read(cx)).expect("reasoning trace");
                (trace.scroll_offset().y, trace.smooth_scroll_remaining())
            });
            assert!(
                precise_offset > eased_card_offset,
                "precise touchpad input must keep the card's native immediate path"
            );
            assert_eq!(
                pending_after_precise,
                px(0.),
                "precise input must cancel queued discrete-wheel motion"
            );
        } else {
            assert!(
                card_offset > card_before,
                "native reasoning scrolling must remain immediate when smoothing is off"
            );
            assert_eq!(pending_card_scroll, px(0.));
        }

        // Repeated wheel ticks at either nested boundary are still contained;
        // they must not fall through just because the card cannot move further.
        for delta in [
            ScrollDelta::Lines(point(0., 1_000.)),
            ScrollDelta::Lines(point(0., 1_000.)),
            ScrollDelta::Lines(point(0., -1_000.)),
            ScrollDelta::Lines(point(0., -1_000.)),
        ] {
            let transcript_before_boundary =
                cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta,
                ..Default::default()
            });
            let transcript_after_boundary =
                cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
            assert_eq!(
                transcript_after_boundary.item_ix, transcript_before_boundary.item_ix,
                "boundary wheel input leaked into the transcript (smooth={smooth_scrolling})"
            );
            assert_eq!(
                transcript_after_boundary.offset_in_item, transcript_before_boundary.offset_in_item,
                "boundary wheel input changed the transcript offset (smooth={smooth_scrolling})"
            );
        }
    }
}

/// Reasoning code blocks resolve the active palette in their custom renderer,
/// so a theme change must not churn the streaming markdown entity.
#[gpui::test]
fn theme_switch_preserves_the_reasoning_body(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(
                this,
                0,
                "reasoning-0".into(),
                "```json\n{\"a\":1}\n```",
                cx,
            );
        });
    });
    let before = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("trace")
            .body_entity_id()
    });

    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let reasoning = reasoning_part(turn).expect("the trace survives a theme switch");
        assert_eq!(
            reasoning.body_entity_id(),
            before,
            "theme changes must not replace the streaming markdown state"
        );
        assert!(
            reasoning_states(turn, cx)[0].0.contains("json"),
            "re-parsing must not lose what already streamed"
        );
    });

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}
