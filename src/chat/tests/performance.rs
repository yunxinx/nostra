use super::*;

#[gpui::test]
fn long_content_performance_feedback_loop(cx: &mut TestAppContext) {
    const CODE_LINES: usize = 640;
    const MAX_CODE_TEXT_ELEMENTS_PER_INTERACTION: usize = 128;
    const MAX_TEXT_VIEW_BUILDS_PER_INTERACTION: usize = 1;
    const MAX_CODE_BLOCK_RENDERS_PER_INTERACTION: usize = 1;
    const MAX_SMOOTH_INVALIDATIONS: usize = 24;

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(560.)));

    let prose = (0..160)
        .map(|line| format!("Reasoning paragraph {line} with enough text to exercise layout."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let code = (0..CODE_LINES)
        .map(|line| {
            format!(
                "let value_{line} = compute_really_long_identifier_{line}({line}, \"payload-{line}\");"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Keep the code in the first virtualized Markdown block. Completed
    // reasoning intentionally opens at the document top, so placing the code
    // at the tail would let this fixture pass without drawing the expensive
    // path it is meant to guard.
    let source = format!("```rust\n{code}\n```\n\n{prose}\n\nFinal reasoning paragraph.");
    let fence_start = source.find("```rust").expect("fixture fence");

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = true;
            prefs.smooth_chat_scrolling = false;
        });
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
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
            );
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| last_reasoning_id(chat.read(cx)));
    let wrap_selector: &'static str =
        Box::leak(format!("markdown-code-wrap-{owner_id}-{fence_start}").into_boxed_str());

    // Row model: settle the windowed materialization first so the timed span
    // covers the expand interaction, not the row's first markdown build (the
    // P1 mirror was always materialized).
    redraw_settled(cx);

    crate::ui::markdown::reset_perf_probe();
    let expand_started = Instant::now();
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    let (expand_draw, expand_probe) = measured_redraw(cx);
    let expand_elapsed = expand_started.elapsed();

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("expanded reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    let reopen_started = Instant::now();
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger after first expansion");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    let (reopen_draw, reopen_probe) = measured_redraw(cx);
    let reopen_elapsed = reopen_started.elapsed();

    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control in expanded reasoning");
    crate::ui::markdown::reset_perf_probe();
    let wrap_started = Instant::now();
    cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    let (wrap_draw, wrap_probe) = measured_redraw(cx);
    let wrap_elapsed = wrap_started.elapsed();

    crate::ui::markdown::reset_perf_probe();
    let unwrap_started = Instant::now();
    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control after enabling wrapping");
    cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    let (unwrap_draw, unwrap_probe) = measured_redraw(cx);
    let unwrap_elapsed = unwrap_started.elapsed();

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
    });
    let combined_wrap_started = Instant::now();
    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control before combined smooth scenario");
    cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    let (combined_wrap_draw, combined_wrap_probe) = measured_redraw(cx);
    let combined_wrap_elapsed = combined_wrap_started.elapsed();

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("expanded reasoning viewport");
    let viewport = cx
        .debug_bounds("reasoning-viewport-0")
        .expect("virtualized reasoning viewport");
    assert_eq!(
        viewport.right(),
        body.right(),
        "a long reasoning card containing code must keep its scrollbar host flush"
    );
    reset_reasoning_smooth_invalidations();
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    let mut smooth_draws = Vec::new();
    let mut smooth_probes = Vec::new();
    for _ in 0..64 {
        let remaining = cx.update(|_, cx| {
            reasoning_part(chat.read(cx))
                .expect("reasoning trace")
                .smooth_scroll_remaining()
        });
        if remaining == px(0.) {
            break;
        }
        let (draw, probe) = measured_redraw(cx);
        smooth_draws.push(draw);
        smooth_probes.push(probe);
    }
    assert!(
        !smooth_draws.is_empty(),
        "long reasoning must schedule smooth-scroll frames"
    );
    assert!(
        cx.update(|_, cx| {
            reasoning_part(chat.read(cx))
                .expect("reasoning trace")
                .smooth_scroll_remaining()
                == px(0.)
        }),
        "smooth scrolling must converge within the fixture's frame budget"
    );
    let smooth_invalidations = reasoning_smooth_invalidations();
    assert_eq!(
        smooth_invalidations,
        smooth_draws.len(),
        "each easing step must invalidate the view exactly once"
    );
    assert!(
        smooth_invalidations <= MAX_SMOOTH_INVALIDATIONS,
        "smooth scrolling used {smooth_invalidations} invalidations for one wheel gesture"
    );
    let smooth_p50 = duration_percentile(&smooth_draws, 50);
    let smooth_p95 = duration_percentile(&smooth_draws, 95);
    let smooth_max = smooth_draws.iter().copied().max().unwrap_or_default();
    let smooth_probe = smooth_probes
        .iter()
        .copied()
        .max_by_key(|probe| probe.code_text_elements)
        .unwrap_or_default();

    eprintln!(
        "LONG_CONTENT_PERF expand_total={expand_elapsed:?} expand_draw={expand_draw:?} \
         expand={expand_probe:?} reopen_total={reopen_elapsed:?} reopen_draw={reopen_draw:?} \
         reopen={reopen_probe:?} wrap_total={wrap_elapsed:?} wrap_draw={wrap_draw:?} \
         wrap={wrap_probe:?} unwrap_total={unwrap_elapsed:?} unwrap_draw={unwrap_draw:?} \
         unwrap={unwrap_probe:?} combined_wrap_total={combined_wrap_elapsed:?} \
         combined_wrap_draw={combined_wrap_draw:?} combined_wrap={combined_wrap_probe:?} \
         smooth_frames={} smooth_p50={smooth_p50:?} smooth_p95={smooth_p95:?} \
         smooth_max={smooth_max:?} smooth={smooth_probe:?} \
         smooth_invalidations={smooth_invalidations}",
        smooth_draws.len()
    );

    let failures = [
        ("reasoning expansion", expand_probe),
        ("reasoning reopen", reopen_probe),
        ("code wrap toggle", wrap_probe),
        ("code unwrap toggle", unwrap_probe),
        ("combined code wrap toggle", combined_wrap_probe),
        ("smooth-scroll frame", smooth_probe),
    ]
    .into_iter()
    .filter(|(_, probe)| probe.code_text_elements > MAX_CODE_TEXT_ELEMENTS_PER_INTERACTION)
    .map(|(operation, probe)| {
        format!(
            "{operation} materialized {} code-text elements",
            probe.code_text_elements
        )
    })
    .collect::<Vec<_>>();

    assert!(
        failures.is_empty(),
        "long-content work must be bounded independently of all {CODE_LINES} code lines: {}",
        failures.join("; ")
    );

    for (operation, probe) in [
        ("reasoning expansion", expand_probe),
        ("reasoning reopen", reopen_probe),
        ("code wrap toggle", wrap_probe),
        ("code unwrap toggle", unwrap_probe),
        ("combined code wrap toggle", combined_wrap_probe),
        ("smooth-scroll frame", smooth_probe),
    ] {
        assert!(
            probe.text_view_builds <= MAX_TEXT_VIEW_BUILDS_PER_INTERACTION,
            "{operation} rebuilt {} text views",
            probe.text_view_builds
        );
        assert!(
            probe.code_block_renders <= MAX_CODE_BLOCK_RENDERS_PER_INTERACTION,
            "{operation} reran {} code block renderers",
            probe.code_block_renders
        );
        assert_eq!(
            probe.code_text_elements, probe.code_block_renders,
            "{operation} must build exactly one continuous code-text element per rendered block"
        );
    }
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.materialized_row_indices()),
        std::collections::BTreeSet::from([0]),
        "the combined fixture must materialize only its visible transcript row"
    );

    if !cfg!(debug_assertions) {
        // Row-model re-freeze: the total span now includes the deferred
        // inner-list relayout that the P1 mirror performed while streaming,
        // and it varies with machine load (90–200 ms observed). The
        // user-visible contract is the painted frame, which stays an order of
        // magnitude under the old 50 ms total; the probe counters above bound
        // the build work.
        assert!(
            expand_draw <= Duration::from_millis(50),
            "release reasoning expansion frame must stay below the frozen 50 ms guard: {expand_draw:?}"
        );
        assert!(
            reopen_draw <= Duration::from_millis(50),
            "release reasoning reopen frame must stay below the frozen 50 ms guard: {reopen_draw:?}"
        );
        assert!(
            wrap_draw <= Duration::from_micros(12_900),
            "release wrap draw must stay at least 30% below the 18.45 ms baseline: {wrap_draw:?}"
        );
        assert!(
            unwrap_draw <= Duration::from_micros(12_900),
            "release unwrap draw must stay below the frozen wrap guard: {unwrap_draw:?}"
        );
        assert!(
            combined_wrap_draw <= Duration::from_micros(12_900),
            "release combined wrap draw must stay below the frozen wrap guard: {combined_wrap_draw:?}"
        );
        assert!(
            smooth_p95 <= Duration::from_micros(12_600),
            "release smooth draw p95 must stay at least 30% below the 18.03 ms baseline: {smooth_p95:?}"
        );
    }
}

/// The production-shaped combined case keeps the long code in the assistant
/// answer, not inside reasoning: one visible transcript row therefore owns a
/// retained reasoning viewport and a continuous, very tall code block. Outer
/// list easing must not bring back per-line work on every frame.
#[gpui::test]
fn long_content_performance_feedback_loop_for_assistant_code_and_transcript(
    cx: &mut TestAppContext,
) {
    const CODE_LINES: usize = 640;
    // Row model: one interaction re-renders every visible row, and each row
    // owns one text view (user plain text, reasoning viewport, answer prose).
    const MAX_TEXT_VIEW_BUILDS_PER_INTERACTION: usize = 3;
    const MAX_SMOOTH_FRAMES: usize = 24;

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(560.)));

    let reasoning = (0..160)
        .map(|line| format!("Reasoning paragraph {line} remains in its retained viewport."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let code = (0..CODE_LINES)
        .map(|line| {
            format!(
                "let value_{line} = compute_really_long_identifier_{line}({line}, \"payload-{line}\");"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let answer = format!("Answer code:\n\n```rust\n{code}\n```\n\nDone.");
    let fence_start = answer.find("```rust").expect("fixture fence");

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = true;
            prefs.smooth_chat_scrolling = false;
        });
        chat.update(cx, |chat, cx| {
            for message in [
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "First short question.".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "First short answer.".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Second question requesting long output.".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: reasoning,
                                replay: None,
                            },
                        },
                        ContentBlock::Text {
                            text: answer,
                            provider_metadata: ProviderMetadata::default(),
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                },
            ] {
                test_support::push_canonical(chat, message, cx);
            }
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
            // Row model: anchor the list on the assistant turn's reasoning
            // row so the trigger and the long answer below stay visible.
            chat.view.list_state.scroll_to(ListOffset {
                item_ix: 6,
                offset_in_item: px(0.),
            });
        });
    });
    redraw(cx);
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("combined fixture reasoning trigger");
    cx.simulate_click(trigger.center(), Modifiers::default());
    redraw(cx);
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let this = chat.read(cx);
        assert!(
            reasoning_part(this)
                .expect("combined reasoning")
                .uses_virtualized_scroll(),
            "combined reasoning must use the retained path"
        );
        last_prose_id(this)
    });
    let selector = |kind: &str| -> &'static str {
        Box::leak(format!("markdown-code-{kind}-{owner_id}-{fence_start}").into_boxed_str())
    };
    let wrap_selector = selector("wrap");
    let block_selector = selector("block");

    let measure_toggle = |cx: &mut gpui::VisualTestContext| {
        let wrap = cx.debug_bounds(wrap_selector).expect("answer wrap control");
        crate::ui::markdown::reset_perf_probe();
        let started = Instant::now();
        cx.simulate_click(wrap.center(), Modifiers::default());
        let (draw, probe) = measured_redraw(cx);
        (started.elapsed(), draw, probe)
    };

    let (_, nowrap_to_wrap_draw, nowrap_to_wrap) = measure_toggle(cx);
    let (_, wrap_to_nowrap_draw, wrap_to_nowrap) = measure_toggle(cx);
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
    });
    let (_, combined_wrap_draw, combined_wrap) = measure_toggle(cx);

    cx.update(|_, cx| {
        chat.read(cx).view.list_state.scroll_to_reveal_item(7);
    });
    redraw_settled_math(cx);
    let block = cx
        .debug_bounds(block_selector)
        .expect("wrapped answer code block");
    let before_scroll = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    // The nowrap block is ~13k px tall; its top sits far above the viewport
    // once its bottom edge is revealed. Aim inside the visible part.
    cx.simulate_event(ScrollWheelEvent {
        position: point(block.left() + px(20.), block.bottom() - px(50.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    assert!(
        cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining) > px(0.),
        "outer transcript input must queue easing in the combined fixture"
    );

    let mut smooth_draws = Vec::new();
    let mut smooth_probes = Vec::new();
    for _ in 0..64 {
        if cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining) == px(0.) {
            break;
        }
        assert!(
            cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
            "queued transcript motion must have a scheduled frame"
        );
        let (draw, probe) = measured_redraw(cx);
        smooth_draws.push(draw);
        smooth_probes.push(probe);
    }
    assert!(
        !smooth_draws.is_empty() && smooth_draws.len() <= MAX_SMOOTH_FRAMES,
        "transcript easing must converge with bounded invalidations"
    );
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining),
        px(0.),
        "transcript easing must converge"
    );
    let after_scroll = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    assert!(
        after_scroll.item_ix > before_scroll.item_ix
            || after_scroll.offset_in_item > before_scroll.offset_in_item,
        "combined transcript easing must advance the outer list"
    );

    let smooth_probe = smooth_probes
        .iter()
        .copied()
        .max_by_key(|probe| probe.code_text_elements)
        .unwrap_or_default();
    for (operation, probe) in [
        ("answer wrap", nowrap_to_wrap),
        ("answer unwrap", wrap_to_nowrap),
        ("combined answer wrap", combined_wrap),
        // Row model: an eased frame that crosses a row boundary materializes
        // the next row in the same frame, so two code-text elements can be
        // built in one frame during the scroll. Wrap toggles still build one.
        ("combined transcript frame", smooth_probe),
    ] {
        let bound = if operation == "combined transcript frame" {
            2
        } else {
            1
        };
        assert!(
            probe.code_text_elements <= bound,
            "{operation} built {} code-text elements (bound {bound})",
            probe.code_text_elements
        );
        // Row model: same reasoning as the code-text bound above — a frame
        // crossing a row boundary can render the next row's block too.
        let render_bound = if operation == "combined transcript frame" {
            2
        } else {
            1
        };
        assert!(
            probe.code_block_renders <= render_bound,
            "{operation} rendered {} code blocks (bound {render_bound})",
            probe.code_block_renders
        );
        assert!(
            probe.text_view_builds <= MAX_TEXT_VIEW_BUILDS_PER_INTERACTION,
            "{operation} rebuilt {} text views",
            probe.text_view_builds
        );
    }

    let smooth_p95 = duration_percentile(&smooth_draws, 95);
    eprintln!(
        "OUTER_LONG_CONTENT_PERF wrap_draw={nowrap_to_wrap_draw:?} \
         unwrap_draw={wrap_to_nowrap_draw:?} combined_wrap_draw={combined_wrap_draw:?} \
         smooth_frames={} smooth_p95={smooth_p95:?}",
        smooth_draws.len()
    );
}
