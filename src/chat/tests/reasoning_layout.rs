use super::*;

/// The design claim behind the fixed streaming preview: once reasoning
/// saturates the preview's line budget, the row stops growing, so everything
/// laid out below it holds still no matter how many tokens still arrive
/// (AC1). Asserted against the transcript's own content height, which is what
/// a reflow would move.
#[gpui::test]
fn a_saturated_preview_stops_moving_the_content_below_it(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
    };
    let transcript_content_height = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| chat.read(cx).view.list_state.max_offset_for_scrollbar().y)
    };

    // Well past the six-line preview budget, so the cap is already engaged.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..40 {
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
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    let saturated = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("trace")
            .scroll_max_offset()
    });
    assert!(
        saturated > px(0.),
        "the preview must be hiding content behind its own scroll, not growing to fit it"
    );
    let before = transcript_content_height(cx);

    // Another 40 paragraphs of reasoning: all of it lands inside the preview.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 40..80 {
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
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    assert_eq!(
        transcript_content_height(cx),
        before,
        "a saturated preview must not change the transcript's layout as it streams"
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
        .expect("collapsed completed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let renderer = reasoning_part(turn).expect("completed reasoning renderer");
        assert!(
            renderer.is_scrollable(),
            "the clamped viewport must use the retained scrollable TextView"
        );
        assert_eq!(
            renderer.scroll_offset(),
            point(px(0.), px(0.)),
            "opening historical reasoning must not jump to its tail"
        );
    });
}

/// AC1 / AC2: the expanded body is content-adaptive up to a cap. Short
/// content renders at its own natural height (no inner scrollbar, no blank
/// space); long content clamps to exactly
/// `max(12 lines, viewport × 45%)` and scrolls internally; and the
/// collapse/expand round trip returns to the same form.
#[gpui::test]
fn expanded_height_fits_short_content_and_clamps_long_content(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    // --- Short trace: four lines of body text. ---
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "one\ntwo\nthree\nfour".into(),
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
        .expect("collapsed short reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("natural-height reasoning body");
    let line_height = cx.update(|window, _| window.line_height());
    assert!(
        (body.size.height - line_height * 4.).abs() <= line_height,
        "a four-line trace must expand to its own height (± one line), got {:?} \
         against {line_height:?} per line",
        body.size.height
    );
    assert!(
        cx.debug_bounds("reasoning-viewport-0").is_none(),
        "short content must not render the inner scroll viewport"
    );
    assert!(
        cx.update(|_, cx| reasoning_part(chat.read(cx))
            .expect("renderer")
            .scroll_max_offset())
            == px(0.),
        "short content must not hide anything behind a cap"
    );

    // --- Long trace: 240 paragraphs, far over the cap at both heights. ---
    let source = (0..240)
        .map(|line| format!("Budgeted reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    // A text preamble puts this trace at content_index 1, so
                    // its selectors (`reasoning-*-1`) stay distinct from the
                    // short trace's `reasoning-*-0` above.
                    content: vec![
                        ContentBlock::Text {
                            text: "Preamble.".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: source,
                                replay: None,
                                duration_ms: None,
                            },
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);

    for window_height in [700., 400.] {
        cx.simulate_resize(gpui::size(px(900.), px(window_height)));
        // First height expands through the trigger; the second height keeps
        // the body open and must follow the resized window.
        if window_height == 700. {
            let trigger = cx
                .debug_bounds("reasoning-trigger-1")
                .expect("collapsed long reasoning trigger");
            cx.simulate_click(trigger.center(), gpui::Modifiers::default());
            redraw(cx);
            redraw(cx);
        } else {
            redraw(cx);
            redraw(cx);
        }

        let body = cx
            .debug_bounds("reasoning-body-1")
            .expect("clamped reasoning body");
        let (expected, line_height) = cx.update(|window, cx| {
            let viewport = chat.read(cx).view.viewport_height;
            (
                crate::chat::rows::typography::reasoning_cap(window.line_height(), viewport),
                window.line_height(),
            )
        });
        assert!(
            (body.size.height - expected).abs() < px(1.),
            "at window height {window_height} the cap was {:?}, expected {:?}",
            body.size.height,
            expected
        );
        // The line-count floor must actually bind at the shorter window: the
        // 12-line minimum exceeds 45% of a 400px viewport.
        if window_height == 400. {
            assert!(
                expected <= line_height * 12. + px(1.),
                "the short window must exercise the line-count floor, got {expected:?}"
            );
        }
        assert!(
            cx.update(|_, cx| reasoning_parts(chat.read(cx))
                .last()
                .expect("long renderer")
                .scroll_max_offset())
                > px(0.),
            "long content must scroll inside the cap"
        );

        // Round trip: collapse back to the trigger line, then re-expand to
        // the same clamped form. The smaller window can leave the trigger
        // above the viewport, so bring it back into the window first.
        chat.update(cx, |this, _| {
            this.view.list_state.scroll_to(ListOffset::default());
        });
        redraw(cx);
        let trigger = cx
            .debug_bounds("reasoning-trigger-1")
            .expect("expanded reasoning trigger");
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
        redraw(cx);
        redraw(cx);
        assert!(
            cx.debug_bounds("reasoning-body-1").is_none(),
            "collapsing must fold the body away"
        );
        let trigger = cx
            .debug_bounds("reasoning-trigger-1")
            .expect("collapsed reasoning trigger after the round trip");
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
        redraw(cx);
        redraw(cx);
        let back = cx
            .debug_bounds("reasoning-body-1")
            .expect("re-expanded reasoning body");
        assert!(
            (back.size.height - expected).abs() < px(1.),
            "re-expanding must return to the clamped height"
        );
    }
}

/// AC2 (R2): expanding at the tail must freeze the viewport instead of
/// re-anchoring the list to its end: the panel grows below the fold, the
/// frozen item/offset stay put, and the `Tail` mode survives so a later
/// return to the bottom re-engages following without any explicit mode
/// change. Collapsing at the tail behaves the same.
#[gpui::test]
fn expanding_at_the_tail_freezes_the_viewport_and_keeps_the_tail_mode(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    // Enough earlier turns that the tail is genuinely off-screen content,
    // then a finished reasoning trace plus its answer at the end.
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            for index in 0..12 {
                test_support::push_canonical(
                    chat,
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
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: (0..240)
                                    .map(|line| format!("Tail reasoning paragraph {line}."))
                                    .collect::<Vec<_>>()
                                    .join("\n\n"),
                                replay: None,
                                duration_ms: None,
                            },
                        },
                        ContentBlock::Text {
                            text: "The answer.".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);
    redraw(cx);

    assert!(
        cx.update(|_, cx| chat.read(cx).view.list_state.is_following_tail()),
        "the fixture must start following the tail"
    );
    let before = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    assert!(
        cx.debug_bounds("reasoning-body-0").is_none(),
        "the trace starts collapsed"
    );

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("reasoning trigger at the tail");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    // The viewport froze on the same row anchor: the content above the
    // fold did not move, and the panel extended below the fold rather than
    // pushing everything up to pin the panel's bottom edge.
    let after = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    assert_eq!(
        after.item_ix, before.item_ix,
        "an expand at the tail must not move the scroll-top row"
    );
    assert_eq!(
        after.offset_in_item, before.offset_in_item,
        "an expand at the tail must not move the row offset"
    );
    assert!(
        !cx.update(|_, cx| chat.read(cx).view.list_state.is_following_tail()),
        "following pauses while the row grows (Tail mode retained, paused)"
    );
    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("expanded reasoning body");
    let fold = cx.update(|_, cx| chat.read(cx).view.list_state.viewport_bounds().bottom());
    assert!(
        body.bottom() > fold,
        "the panel must grow below the fold ({:?} vs fold {fold:?})",
        body.bottom()
    );

    // Returning to the bottom re-engages the retained Tail mode on its own.
    cx.update(|_, cx| {
        chat.update(cx, |this, _| this.view.list_state.scroll_to_end());
    });
    redraw(cx);
    assert!(
        cx.update(|_, cx| chat.read(cx).view.list_state.is_following_tail()),
        "scrolling back to the bottom must re-engage tail following without \
         an explicit mode change — the mode was paused, not downgraded"
    );

    // Collapsing at the tail is the same contract: the row shrinks without
    // the viewport bouncing, and the fold returns to the bottom.
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("expanded reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);
    assert!(
        cx.debug_bounds("reasoning-body-0").is_none(),
        "collapsing folds the body away"
    );
    assert!(
        cx.update(|_, cx| chat.read(cx).view.list_state.is_following_tail()),
        "back at the bottom after the collapse, following has re-engaged"
    );
}

/// The painted-height feedback loop: a trace too long for the cap but short
/// enough that the source-length pre-check stays quiet renders its first
/// frame at natural height; the painted report above the cap then flips the
/// form to the clamped viewport. The stored report is the proof the natural
/// form actually painted — a pre-clamped trace never reports a body height.
#[gpui::test]
fn a_medium_trace_clamps_after_the_first_natural_frame(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    // Eighteen single-line rows: painted height lands above the cap (45% of
    // the viewport), while the characters-per-line estimate stays under
    // twice the cap, so nothing pre-clamps the first frame.
    let source = (0..18)
        .map(|_| "x".repeat(70))
        .collect::<Vec<_>>()
        .join("\n");
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
        .expect("collapsed medium reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    let (cap, line_height) = cx.update(|window, cx| {
        let viewport = chat.read(cx).view.viewport_height;
        (
            crate::chat::rows::typography::reasoning_cap(window.line_height(), viewport),
            window.line_height(),
        )
    });
    let clamped = cx
        .debug_bounds("reasoning-body-0")
        .expect("the clamped reasoning body");
    assert!(
        (clamped.size.height - cap).abs() < px(1.),
        "the flipped form must render at the cap ({:?} vs {cap:?})",
        clamped.size.height
    );
    assert!(
        cx.debug_bounds("reasoning-viewport-0").is_some(),
        "the clamped form must render the inner scroll viewport"
    );
    assert!(
        cx.update(|_, cx| reasoning_part(chat.read(cx))
            .expect("renderer")
            .scroll_max_offset())
            > px(0.),
        "the cap must hide content behind the internal scroll"
    );
    let painted = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("renderer")
            .natural_height_for_test()
            .expect("the natural form must have painted before clamping")
    });
    assert!(
        painted > cap && painted <= line_height * 18. + px(20.),
        "the stored report must be the medium trace's own height plus the \
         card's body padding ({painted:?} vs cap {cap:?}) — a pre-clamped \
         trace would never report one"
    );
}

/// R9: the clamped viewport's scrollbar host must span the card's interior —
/// the horizontal padding lives inside the scrollable TextView, so its
/// absolutely positioned scrollbar is measured against the full card width
/// and lands in the right-hand gutter instead of over the text's last
/// column. Bounds alone cannot see padding moved onto the TextView's own
/// wrapper (the wrapper would still span the card while the TextView and
/// its scrollbar shrink), so the gutter is also asserted functionally: a
/// click at the card's right edge must page the body through the scrollbar
/// track, and dead padding space would swallow it.
#[gpui::test]
fn clamped_reasoning_scrollbar_keeps_its_gutter_at_the_card_edge(cx: &mut TestAppContext) {
    init_app(cx);
    // The internal track must be present and interactive for the click
    // assertion below, whatever the app-level hover policy is.
    cx.update(|cx| {
        let theme = gpui_base::Theme::global_mut(cx);
        theme.scrollbar = theme
            .scrollbar
            .clone()
            .with_mode(gpui_base::ScrollbarMode::Always);
    });
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    let source = (0..240)
        .map(|line| format!("Long reasoning paragraph {line}."))
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
        .expect("collapsed long reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    let card = cx
        .debug_bounds("reasoning-card-0")
        .expect("the expanded body renders its outlined card");
    let viewport = cx
        .debug_bounds("reasoning-viewport-0")
        .expect("clamped reasoning viewport");
    assert!(
        viewport.right() >= card.right() - px(2.),
        "the scrollbar host must span the card interior up to its border \
         ({:?} vs card right {:?})",
        viewport.right(),
        card.right()
    );
    assert!(
        viewport.left() <= card.left() + px(2.),
        "the host must start at the card's left border as well"
    );

    // The functional half of the gutter contract: the track sits at the
    // card's right edge, over the TextView's own padding area. Padding on
    // any wrapper would move the track left, and this click would land on
    // dead space instead of paging the body.
    let before = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("the clamped trace")
            .scroll_offset()
    });
    assert_eq!(
        before.y,
        px(0.),
        "a completed trace opens its clamped body at the top"
    );
    cx.simulate_click(
        gpui::point(viewport.right() - px(4.), viewport.center().y),
        gpui::Modifiers::default(),
    );
    redraw(cx);
    let after = cx.update(|_, cx| {
        reasoning_part(chat.read(cx))
            .expect("the clamped trace")
            .scroll_offset()
    });
    assert!(
        after.y < before.y,
        "a click in the card-edge gutter must reach the scrollbar track and \
         page the body (before {before:?}, after {after:?})"
    );
}

/// A source past the windowed thresholds must never lay out at natural
/// height when expanded: the windowed block layout cannot combine with the
/// internal-scroll viewport, so the clamped viewport is what keeps the
/// per-frame layout work bounded. Asserted on the settled frame after the
/// expand — a natural-height regression would materialize every code line
/// at once.
#[gpui::test]
fn an_oversized_expanded_reasoning_stays_layout_bounded(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(560.)));

    let code = (0..640)
        .map(|line| {
            format!(
                "let value_{line} = compute_really_long_identifier_{line}({line}, \"payload-{line}\");"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prose = (0..480)
        .map(|line| format!("Reasoning paragraph {line} with enough text to exercise layout."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let source = format!("```rust\n{code}\n```\n\n{prose}\n\nFinal reasoning paragraph.");
    assert!(
        source.len() >= crate::chat::rows::typography::WINDOWED_SOURCE_BYTES,
        "the fixture must cross the windowed byte threshold"
    );

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
        .expect("collapsed oversized reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    let probe = settled_frame_probe(cx);
    assert!(
        cx.debug_bounds("reasoning-viewport-0").is_some(),
        "an oversized trace must expand into the clamped scrollable viewport"
    );
    assert!(
        cx.update(|_, cx| reasoning_part(chat.read(cx))
            .expect("renderer")
            .scroll_max_offset())
            > px(0.),
        "the cap must hide content behind the internal scroll"
    );
    assert!(
        probe.code_text_elements <= 128,
        "the clamped viewport must bound per-frame code-text work ({} elements)",
        probe.code_text_elements
    );
}

/// Reasoning code blocks resolve the active palette in their custom renderer,
/// so a theme change must not churn the streaming markdown entity.
#[gpui::test]
fn theme_switch_preserves_the_reasoning_body(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
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
            .expect("streaming body")
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
            reasoning.body_entity_id().expect("streaming body"),
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
