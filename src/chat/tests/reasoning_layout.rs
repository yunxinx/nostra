use super::*;

/// The design claim behind rendering the card inline instead of floating it:
/// once a reasoning stream saturates the height budget, the card stops
/// growing, so everything laid out below it holds still no matter how many
/// tokens still arrive. Asserted against the transcript's own content height,
/// which is what a reflow would move.
#[gpui::test]
fn a_saturated_card_stops_moving_the_content_below_it(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
    };
    let transcript_content_height = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y)
    };

    // Well past a seven-line budget, so the cap is already engaged.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..40 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    let saturated = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .expect("trace")
            .scroll_max_offset()
    });
    assert!(
        saturated > px(0.),
        "the card must be hiding content behind its own scroll, not growing to fit it"
    );
    let before = transcript_content_height(cx);

    // Another 40 paragraphs of reasoning: all of it lands inside the card.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 40..80 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    assert_eq!(
        transcript_content_height(cx),
        before,
        "a capped card must not change the transcript's layout as it streams"
    );
}

/// A terminal reasoning block is historical content, so opening it starts at
/// the beginning even though live reasoning follows the tail while streaming.
#[gpui::test]
fn completed_long_reasoning_opens_at_the_top(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    let source = (0..240)
        .map(|line| format!("Completed reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            chat.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source,
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed completed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("completed reasoning trace");
        assert!(
            trace.uses_virtualized_scroll(),
            "the fixture must exercise the long-document path"
        );
        assert_eq!(
            trace.scroll_offset(),
            point(px(0.), px(0.)),
            "opening historical reasoning must not jump to its tail"
        );
    });
}

/// The large-document path must not turn every expanded disclosure into a
/// fixed-height viewport. Short reasoning still grows only to its content.
#[gpui::test]
fn short_reasoning_keeps_its_natural_height(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    let source = "Check the relevant constraints.\n\nChoose the smallest valid change.";
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            chat.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source.into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed short reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("expanded short reasoning body");
    let height_budget = cx.update(|window, _| window.line_height() * 7.);
    assert!(
        body.size.height < height_budget,
        "short reasoning was stretched to the {:?} height budget",
        height_budget
    );
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("short reasoning trace");
        assert!(!trace.uses_virtualized_scroll());
        assert_eq!(trace.scroll_max_offset(), px(0.));
    });
}

/// A virtualized reasoning card's scrollbar host must use the card's full
/// width. Padding belongs inside the scrollable TextView; otherwise the
/// scrollbar is inset into the content column once the long-document path is
/// selected, while the short native path remains flush with the card edge.
#[gpui::test]
fn virtualized_reasoning_scrollbar_host_reaches_card_edge(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    let source = (0..240)
        .map(|line| format!("Long reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            chat.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source,
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed long reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("expanded reasoning body");
    let viewport = cx
        .debug_bounds("reasoning-viewport-0")
        .expect("virtualized reasoning viewport");
    assert_eq!(
        viewport.right(),
        body.right(),
        "the virtualized scrollbar host must reach the card's right edge"
    );
}

/// Crossing the large-document threshold must not replace an actively used
/// native scroll handle. Returning to the tail must transition immediately:
/// a completed stream may never append another delta to trigger migration.
#[gpui::test]
fn streaming_reasoning_defers_virtualization_while_the_reader_is_scrolled_up(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    let initial = (0..90)
        .map(|line| format!("Reasoning line {line} has compact source.\n\n"))
        .collect::<String>();
    assert!(
        initial.len() < VIRTUALIZED_SOURCE_BYTES,
        "the first stream segment must remain below the virtualization gate"
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &initial, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the expanded reasoning body was drawn");
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(!trace.uses_virtualized_scroll());
        assert!(trace.scroll_max().y > px(80.));
    });

    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);

    let paused_offset = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(!trace.is_following());
        trace.scroll_offset()
    });

    let threshold_crossing = "Additional retained paragraph content.\n\n".repeat(80);
    assert!(
        initial.len() + threshold_crossing.len() >= VIRTUALIZED_SOURCE_BYTES,
        "the second segment must cross the virtualization gate"
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &threshold_crossing, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            !trace.uses_virtualized_scroll(),
            "virtualization must wait while the reader is away from the tail"
        );
        assert_eq!(
            trace.scroll_offset(),
            paused_offset,
            "crossing the threshold must preserve the exact native offset"
        );
    });

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the reasoning body remains visible");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    redraw(cx);
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            trace.is_following(),
            "returning to the tail must re-arm follow"
        );
        assert!(
            trace.uses_virtualized_scroll(),
            "returning to the tail must migrate even when no later delta arrives"
        );
        assert!(
            trace.scroll_max().y + trace.scroll_offset().y <= STICK_THRESHOLD,
            "the new virtualized viewport must remain anchored to the tail"
        );
    });
}

#[gpui::test]
fn finished_scrolled_reasoning_reopens_on_the_virtualized_path(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    let initial = (0..90)
        .map(|line| format!("Reasoning line {line} has compact source.\n\n"))
        .collect::<String>();
    let threshold_crossing = "Additional retained paragraph content.\n\n".repeat(80);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &initial, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the expanded reasoning body was drawn");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);
    let paused_offset = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        reasoning_part(turn)
            .expect("reasoning trace")
            .scroll_offset()
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &threshold_crossing, cx);
        });
    });
    redraw(cx);
    redraw(cx);
    assert!(cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        !trace.is_following() && !trace.uses_virtualized_scroll()
    }));

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.finish_reply(None, None, cx));
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("finished reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    cx.update(|window, cx| {
        assert!(window.simulate_next_frame(cx) > 0);
    });
    redraw(cx);
    cx.update(|window, cx| {
        assert!(window.simulate_next_frame(cx) > 0);
    });
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            trace.uses_virtualized_scroll(),
            "reopening a finished long stream must not return to full natural Markdown layout"
        );
        assert!(
            trace.scroll_offset().y > -trace.scroll_max().y + px(1.),
            "migration on disclosure must preserve a non-tail reader position"
        );
        assert!(
            trace.scroll_offset().y < px(-1.),
            "migration on disclosure must not jump the reader to the top"
        );
        assert!(
            paused_offset.y < px(-1.),
            "the natural path must have captured a non-zero reader position"
        );
    });
}

#[gpui::test]
fn authoritative_short_reasoning_returns_to_natural_height(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    let long_source = (0..180)
        .map(|line| format!("Long reasoning line {line} keeps the retained list active."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let short_source = "The authoritative summary is short.";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &long_source, cx);
        });
    });
    redraw(cx);
    redraw(cx);
    assert!(cx.update(|_, cx| {
        reasoning_part(chat.read(cx).messages.last().expect("assistant turn"))
            .expect("reasoning trace")
            .uses_virtualized_scroll()
    }));

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: short_source.into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("short terminal reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("short terminal reasoning body");
    let height_budget = cx.update(|window, _| window.line_height() * 7.);
    assert!(body.size.height < height_budget);
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            !trace.uses_virtualized_scroll(),
            "a short authoritative replacement must leave the fixed-height virtual path"
        );
        assert_eq!(trace.scroll_max_offset(), px(0.));
    });
}

#[gpui::test]
fn authoritative_long_reasoning_promotes_even_when_the_reader_is_not_following(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    let initial = (0..70)
        .map(|line| format!("Initial reasoning line {line}.\n\n"))
        .collect::<String>();
    let authoritative = (0..220)
        .map(|line| format!("Authoritative reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &initial, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("initial reasoning body");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            let trace = reasoning_part_mut(this.messages.last_mut().expect("assistant turn"))
                .expect("reasoning trace");
            assert!(!trace.is_following());
            // Preserve expansion across the terminal boundary without changing
            // the final disclosure state.
            trace.toggle();
            trace.toggle();
        });
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: authoritative,
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(trace.is_expanded());
        assert!(
            trace.uses_virtualized_scroll(),
            "a long authoritative replacement must not leave an expanded full-layout path"
        );
    });
}
