use super::*;

/// Streaming reasoning follows the same manual-scroll contract as the main
/// transcript: an upward gesture pauses following, and a later downward
/// gesture at the end re-arms it (AC1).
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
        .expect("the streaming reasoning preview was drawn");
    let (bottom_offset, max_offset) = cx.update(|_, cx| {
        let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
        (renderer.scroll_offset().y, renderer.scroll_max().y)
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
        let renderer = reasoning_part(turn).expect("reasoning renderer");
        assert!(
            !renderer.is_following(),
            "upward scrolling must pause following"
        );
        renderer.scroll_offset().y
    });
    assert!(
        paused_offset > bottom_offset,
        "the preview should move upward"
    );

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
            .expect("reasoning renderer")
            .scroll_offset()
            .y
    });
    assert_eq!(
        still_paused, paused_offset,
        "new reasoning must not force a user who scrolled up back to the end"
    );

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the reasoning preview remains visible");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    redraw(cx);

    let rearmed = cx.update(|_, cx| {
        let turn = chat.read(cx);
        let renderer = reasoning_part(turn).expect("reasoning renderer");
        assert!(
            renderer.is_following(),
            "scrolling down at the end must re-arm following (offset={:?}, max={:?})",
            renderer.scroll_offset(),
            renderer.scroll_max()
        );
        renderer.scroll_offset().y
    });
    assert!(
        cx.update(|_, cx| {
            let turn = chat.read(cx);
            let renderer = reasoning_part(turn).expect("reasoning renderer");
            renderer.scroll_max().y + rearmed <= STICK_THRESHOLD
        }),
        "the downward gesture should reach the preview's end"
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
        let renderer = reasoning_part(turn).expect("reasoning renderer");
        renderer.scroll_max().y + renderer.scroll_offset().y <= STICK_THRESHOLD
    }));
}

/// The reasoning viewport owns every vertical wheel gesture inside its bounds.
/// This remains true regardless of whether transcript wheel smoothing is on
/// (AC3: nested boundaries never leak into the transcript, and the eased
/// replay keeps its anchor-then-replay shape).
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
            cx.update(|_, cx| chat.read(cx).view.list_state.max_offset_for_scrollbar().y > px(0.)),
            "the transcript fixture must be scrollable"
        );
        let body = cx
            .debug_bounds("reasoning-body-0")
            .expect("the latest reasoning preview must be visible");
        let transcript_before =
            cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
        let card_before = cx.update(|_, cx| {
            reasoning_part(chat.read(cx))
                .expect("reasoning renderer")
                .scroll_offset()
                .y
        });

        // A previously queued transcript animation must be abandoned as soon
        // as the pointer enters the nested viewport.
        if smooth_scrolling {
            cx.simulate_event(ScrollWheelEvent {
                position: point(px(320.), px(40.)),
                delta: ScrollDelta::Lines(point(0., -3.)),
                ..Default::default()
            });
            assert!(
                cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining != px(0.)),
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
                let renderer = reasoning_part(chat).expect("reasoning renderer");
                (
                    chat.view.list_state.logical_scroll_top(),
                    renderer.scroll_offset().y,
                    chat.view.smooth_scroll.remaining,
                    renderer.smooth_scroll_remaining(),
                )
            });
        assert_eq!(transcript_after.item_ix, transcript_before.item_ix);
        assert_eq!(
            transcript_after.offset_in_item, transcript_before.offset_in_item,
            "reasoning wheel input leaked into the transcript (smooth={smooth_scrolling})"
        );
        assert!(
            card_offset < px(0.),
            "the reasoning preview itself must consume the wheel input"
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
                "reasoning wheel input must queue eased viewport motion"
            );
            assert!(
                cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
                "reasoning wheel input must schedule an animation frame"
            );
            redraw(cx);
            let eased_card_offset = cx.update(|_, cx| {
                reasoning_part(chat.read(cx))
                    .expect("reasoning renderer")
                    .scroll_offset()
                    .y
            });
            assert!(
                eased_card_offset > card_before,
                "the animation frame must advance the reasoning viewport"
            );

            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(20.))),
                ..Default::default()
            });
            let (precise_offset, pending_after_precise) = cx.update(|_, cx| {
                let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
                (
                    renderer.scroll_offset().y,
                    renderer.smooth_scroll_remaining(),
                )
            });
            assert!(
                precise_offset > eased_card_offset,
                "precise touchpad input must keep the viewport's native immediate path"
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
        // they must not fall through just because the viewport cannot move
        // further, and the transcript anchor plus any queued easing must be
        // untouched (AC3).
        for delta in [
            ScrollDelta::Lines(point(0., 1_000.)),
            ScrollDelta::Lines(point(0., 1_000.)),
            ScrollDelta::Lines(point(0., -1_000.)),
            ScrollDelta::Lines(point(0., -1_000.)),
        ] {
            let transcript_before_boundary =
                cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
            let queued_before = cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining);
            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta,
                ..Default::default()
            });
            let transcript_after_boundary =
                cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
            assert_eq!(
                transcript_after_boundary.item_ix, transcript_before_boundary.item_ix,
                "boundary wheel input leaked into the transcript (smooth={smooth_scrolling})"
            );
            assert_eq!(
                transcript_after_boundary.offset_in_item, transcript_before_boundary.offset_in_item,
                "boundary wheel input changed the transcript offset (smooth={smooth_scrolling})"
            );
            assert_eq!(
                cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining),
                queued_before,
                "boundary wheel input must not queue transcript easing (smooth={smooth_scrolling})"
            );
        }
    }
}

/// Seed a finished assistant turn whose single reasoning part is far past the
/// expanded height cap, then expand it into the clamped viewport.
fn expanded_clamped_reasoning(chat: &gpui::Entity<ChatView>, cx: &mut gpui::VisualTestContext) {
    let source = (0..240)
        .map(|line| format!("Faded reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source,
                            replay: None,
                            duration_ms: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger");
    cx.simulate_click(trigger.center(), Modifiers::default());
    redraw(cx);
    redraw(cx);
    cx.debug_bounds("reasoning-body-0")
        .expect("the clamped reasoning body");
}

/// The clamped viewport fades whichever of its edges still hides content:
/// opening at the top shows only the bottom fade, the middle shows both, the
/// bottom shows only the top. Both wheel paths must keep the fades in step —
/// the precise one-shot path through the body's layout observers, and the
/// eased discrete-wheel path through its per-frame view notifications.
#[gpui::test]
fn clamped_reasoning_fades_track_the_internal_scroll_position(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    expanded_clamped_reasoning(&chat, cx);

    let fades = |cx: &mut gpui::VisualTestContext| {
        (
            cx.debug_bounds("reasoning-fade-top-0").is_some(),
            cx.debug_bounds("reasoning-fade-bottom-0").is_some(),
        )
    };
    let wheel = |cx: &mut gpui::VisualTestContext, dy: gpui::Pixels| {
        let body = cx
            .debug_bounds("reasoning-body-0")
            .expect("the clamped reasoning body");
        cx.simulate_event(ScrollWheelEvent {
            position: body.center(),
            delta: ScrollDelta::Pixels(point(px(0.), dy)),
            ..Default::default()
        });
        redraw(cx);
    };

    // A completed trace opens parked at the top: only the bottom edge hides
    // content, and the fixture must be genuinely clamped for the rest of the
    // test to mean anything.
    let max_offset = cx.update(|_, cx| {
        let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
        assert_eq!(
            renderer.scroll_offset().y,
            px(0.),
            "a completed trace opens at the top of its clamped body"
        );
        renderer.scroll_max().y
    });
    assert!(
        max_offset > px(400.),
        "the fixture must overflow the cap by a wide margin"
    );
    assert_eq!(
        fades(cx),
        (false, true),
        "parked at the top: only the bottom fade"
    );

    // A precise gesture into the middle leaves both edges hiding content.
    wheel(cx, px(-300.));
    let mid = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("reasoning renderer")
            .scroll_offset()
            .y
    });
    assert!(
        mid < px(-2.) && mid > -max_offset + px(2.),
        "the gesture must park inside the body (offset {mid:?}, max {max_offset:?})"
    );
    assert_eq!(fades(cx), (true, true), "in the middle: both fades");

    // Driving past the bottom leaves only the top edge.
    wheel(cx, px(-100_000.));
    assert_eq!(fades(cx), (true, false), "at the bottom: only the top fade");

    // And driving back up restores the opening state.
    wheel(cx, px(100_000.));
    assert_eq!(
        fades(cx),
        (false, true),
        "back at the top: only the bottom fade"
    );

    // The eased discrete-wheel path converges on the same state: every
    // animation frame notifies the view, so the fades follow the motion.
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
    });
    redraw(cx);
    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the clamped reasoning body");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Lines(point(0., -1_000.)),
        ..Default::default()
    });
    // Step the eased motion to its clamp. The internal scrollbar's
    // decorative fade keeps requesting animation frames under the simulated
    // clock, so the loop keys on the card's own remaining distance instead
    // of a fully idle frame.
    for _ in 0..64 {
        cx.run_until_parked();
        let remaining = cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            reasoning_part(chat.read(cx))
                .expect("reasoning renderer")
                .smooth_scroll_remaining()
        });
        if remaining == px(0.) {
            break;
        }
    }
    redraw(cx);
    assert_eq!(
        fades(cx),
        (true, false),
        "the eased path must reach the bottom fade state"
    );
}

/// The fades are pure paint: they carry no hitbox, so input aimed through
/// them still reaches the nested viewport and its scrollbar.
#[gpui::test]
fn reasoning_fades_never_intercept_pointer_input(cx: &mut TestAppContext) {
    init_app(cx);
    // The internal scrollbar must be present and interactive for the gutter
    // click below, whatever the app-level hover policy is.
    cx.update(|cx| {
        let theme = gpui_base::Theme::global_mut(cx);
        theme.scrollbar = theme
            .scrollbar
            .clone()
            .with_mode(gpui_base::ScrollbarMode::Always);
    });
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    expanded_clamped_reasoning(&chat, cx);

    // Park in the middle so both fades are painted.
    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the clamped reasoning body");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-300.))),
        ..Default::default()
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("reasoning-fade-top-0").is_some()
            && cx.debug_bounds("reasoning-fade-bottom-0").is_some(),
        "the fixture must park with both fades painted"
    );

    // A wheel gesture landing inside the top fade strip still reaches the
    // nested viewport: the card consumes it, the transcript neither moves
    // nor queues motion.
    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the clamped reasoning body");
    let fade_point = point(body.center().x, body.origin.y + px(3.));
    let (transcript_before, card_before) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        (
            chat.view.list_state.logical_scroll_top(),
            reasoning_part(chat)
                .expect("reasoning renderer")
                .scroll_offset()
                .y,
        )
    });
    cx.simulate_event(ScrollWheelEvent {
        position: fade_point,
        delta: ScrollDelta::Pixels(point(px(0.), px(60.))),
        ..Default::default()
    });
    redraw(cx);
    let (transcript_after, card_after, queued) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        (
            chat.view.list_state.logical_scroll_top(),
            reasoning_part(chat)
                .expect("reasoning renderer")
                .scroll_offset()
                .y,
            chat.view.smooth_scroll.remaining,
        )
    });
    assert_eq!(transcript_after.item_ix, transcript_before.item_ix);
    assert_eq!(
        transcript_after.offset_in_item,
        transcript_before.offset_in_item
    );
    assert!(
        card_after > card_before,
        "a wheel gesture through the fade must still move the nested viewport"
    );
    assert_eq!(
        queued,
        px(0.),
        "input through the fade must not queue transcript easing"
    );

    // A click in the scrollbar gutter underneath the top fade still pages
    // the body: the fade cannot block the track.
    let viewport = cx
        .debug_bounds("reasoning-viewport-0")
        .expect("the clamped reasoning viewport");
    let gutter = point(viewport.right() - px(4.), viewport.origin.y + px(8.));
    let before = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("reasoning renderer")
            .scroll_offset()
            .y
    });
    cx.simulate_click(gutter, Modifiers::default());
    redraw(cx);
    let after = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("reasoning renderer")
            .scroll_offset()
            .y
    });
    assert!(
        after > before,
        "a gutter click underneath the fade must still page the body \
         (before {before:?}, after {after:?})"
    );
}

/// The streaming preview keeps its single top fade: it tail-follows, so only
/// content scrolled above the viewport can be hidden, and the bottom edge
/// never fades.
#[gpui::test]
fn the_streaming_preview_keeps_only_its_top_fade(cx: &mut TestAppContext) {
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

    let (offset, max_offset) = cx.update(|_, cx| {
        let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
        (renderer.scroll_offset().y, renderer.scroll_max().y)
    });
    assert!(
        max_offset > px(80.),
        "the fixture must have scrollable reasoning"
    );
    assert_eq!(offset, -max_offset, "the preview tail-follows the stream");
    assert!(
        cx.debug_bounds("reasoning-fade-top-0").is_some(),
        "content above the tail viewport keeps the top fade"
    );
    assert!(
        cx.debug_bounds("reasoning-fade-bottom-0").is_none(),
        "the tail-following preview never fades its bottom edge"
    );
}
