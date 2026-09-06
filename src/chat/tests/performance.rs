use super::*;

fn init_performance_app(cx: &mut TestAppContext) {
    init_app(cx);
    // Decorative animations use wall-clock time; scroll easing is driven explicitly.
    cx.update(|cx| cx.set_reduce_motion(true));
}

#[gpui::test]
fn long_content_performance_feedback_loop(cx: &mut TestAppContext) {
    const CODE_LINES: usize = 640;
    const MAX_CODE_TEXT_ELEMENTS_PER_SETTLED_FRAME: usize = 128;
    const MAX_TEXT_VIEW_BUILDS_PER_SETTLED_FRAME: usize = 1;
    const MAX_CODE_BLOCK_RENDERS_PER_SETTLED_FRAME: usize = 1;
    const MAX_SMOOTH_INVALIDATIONS: usize = 24;

    init_performance_app(cx);
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

    // Complete initial materialization before measuring disclosure changes.
    settle_frame_callbacks(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger");
    let (expand_draw, expand_probe) = measured_interaction(cx, |cx| {
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    });

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("expanded reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    settle_frame_callbacks(cx);
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger after first expansion");
    let (reopen_draw, reopen_probe) = measured_interaction(cx, |cx| {
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    });

    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control in expanded reasoning");
    let (wrap_draw, wrap_probe) = measured_interaction(cx, |cx| {
        cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    });

    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control after enabling wrapping");
    let (unwrap_draw, unwrap_probe) = measured_interaction(cx, |cx| {
        cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    });

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
    });
    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control before combined smooth scenario");
    let (combined_wrap_draw, combined_wrap_probe) = measured_interaction(cx, |cx| {
        cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    });

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
    let mut smooth_draws = measure_draws(cx, |cx| {
        cx.simulate_event(ScrollWheelEvent {
            position: body.center(),
            delta: ScrollDelta::Lines(point(0., -3.)),
            ..Default::default()
        });
    });
    let mut smooth_steps = 0;
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
        let draws = measure_draws(cx, |cx| {
            assert!(
                cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
                "queued reasoning motion must have a scheduled frame"
            );
        });
        assert!(!draws.is_empty(), "each reasoning easing step must draw");
        smooth_steps += 1;
        smooth_draws.extend(draws);
        // Structural counts come from a separate settled frame; this extra
        // draw does not advance easing and is excluded from the timings.
        smooth_probes.push(settled_frame_probe(cx));
    }
    assert!(
        smooth_steps > 0,
        "long reasoning must schedule smooth-scroll steps"
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
    smooth_draws.extend(measure_draws(cx, settle_frame_callbacks));
    let smooth_invalidations = reasoning_smooth_invalidations();
    assert_eq!(
        smooth_invalidations, smooth_steps,
        "each easing step must invalidate the view exactly once"
    );
    assert!(
        smooth_invalidations <= MAX_SMOOTH_INVALIDATIONS,
        "smooth scrolling used {smooth_invalidations} invalidations for one wheel gesture"
    );
    let smooth_p50 = duration_percentile(&smooth_draws, 50);
    let smooth_p95 = duration_percentile(&smooth_draws, 95);
    let smooth_max = smooth_draws.iter().copied().max().expect("smooth draws");
    let smooth_probe = smooth_probes
        .iter()
        .copied()
        .max_by_key(|probe| probe.code_text_elements)
        .unwrap_or_default();

    eprintln!(
        "LONG_CONTENT_PERF expand_draw={expand_draw:?} \
         expand={expand_probe:?} reopen_draw={reopen_draw:?} \
         reopen={reopen_probe:?} wrap_draw={wrap_draw:?} \
         wrap={wrap_probe:?} unwrap_draw={unwrap_draw:?} unwrap={unwrap_probe:?} \
         combined_wrap_draw={combined_wrap_draw:?} combined_wrap={combined_wrap_probe:?} \
         smooth_steps={smooth_steps} smooth_draws={} \
         smooth_p50={smooth_p50:?} smooth_p95={smooth_p95:?} \
         smooth_max={smooth_max:?} smooth={smooth_probe:?} \
         smooth_invalidations={smooth_invalidations}",
        smooth_draws.len()
    );

    // Each probe describes one settled frame, independently of how many
    // actual frames the interaction needed to finish.
    let failures = [
        ("reasoning expansion", expand_probe),
        ("reasoning reopen", reopen_probe),
        ("code wrap toggle", wrap_probe),
        ("code unwrap toggle", unwrap_probe),
        ("combined code wrap toggle", combined_wrap_probe),
        ("smooth-scroll frame", smooth_probe),
    ]
    .into_iter()
    .filter(|(_, probe)| probe.code_text_elements > MAX_CODE_TEXT_ELEMENTS_PER_SETTLED_FRAME)
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
            probe.text_view_builds <= MAX_TEXT_VIEW_BUILDS_PER_SETTLED_FRAME,
            "{operation} rebuilt {} text views",
            probe.text_view_builds
        );
        assert!(
            probe.code_block_renders <= MAX_CODE_BLOCK_RENDERS_PER_SETTLED_FRAME,
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
        // Each interaction value is the slowest actual draw, including
        // frames caused by deferred layout convergence.
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
            "release wrap draw must stay below 12.9 ms: {wrap_draw:?}"
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
            "release smooth draw p95 must stay below 12.6 ms: {smooth_p95:?}"
        );
    }
}

/// Long assistant code follows a retained reasoning viewport. Outer list
/// easing must keep layout work bounded inside each visible row.
#[gpui::test]
fn long_content_performance_feedback_loop_for_assistant_code_and_transcript(
    cx: &mut TestAppContext,
) {
    const CODE_LINES: usize = 640;
    // Each visible row owns one text view: user text, reasoning, or prose.
    const MAX_TEXT_VIEW_BUILDS_PER_SETTLED_FRAME: usize = 3;
    const MAX_SMOOTH_STEPS: usize = 24;

    init_performance_app(cx);
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
            // Anchor on reasoning so its trigger and the answer below it
            // participate in the same viewport.
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
                .is_scrollable(),
            "combined reasoning must use the retained path"
        );
        last_prose_id(this)
    });
    let selector = |kind: &str| -> &'static str {
        Box::leak(format!("markdown-code-{kind}-{owner_id}-{fence_start}").into_boxed_str())
    };
    let wrap_selector = selector("wrap");
    let block_selector = selector("block");

    // Reveal the answer before measuring its wrap controls.
    cx.update(|_, cx| {
        chat.read(cx).view.list_state.scroll_to_reveal_item(7);
    });
    redraw(cx);
    redraw(cx);

    let measure_toggle = |cx: &mut gpui::VisualTestContext| {
        let wrap = cx.debug_bounds(wrap_selector).expect("answer wrap control");
        measured_interaction(cx, |cx| {
            cx.simulate_click(wrap.center(), Modifiers::default());
        })
    };

    let (nowrap_to_wrap_draw, nowrap_to_wrap) = measure_toggle(cx);
    let (wrap_to_nowrap_draw, wrap_to_nowrap) = measure_toggle(cx);
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
    });
    let (combined_wrap_draw, combined_wrap) = measure_toggle(cx);

    cx.update(|_, cx| {
        chat.read(cx).view.list_state.scroll_to_reveal_item(7);
    });
    redraw_settled_math(cx);
    let block = cx
        .debug_bounds(block_selector)
        .expect("wrapped answer code block");
    let before_scroll = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    // The nowrap block is ~13k px tall; with the answer row revealed its top
    // sits at the viewport top and the body fills the rest of the window, so
    // aim inside the visible part, clear of the floating composer.
    let wheel_point = point(block.left() + px(20.), px(300.));
    let mut smooth_draws = measure_draws(cx, |cx| {
        cx.simulate_event(ScrollWheelEvent {
            position: wheel_point,
            delta: ScrollDelta::Lines(point(0., -3.)),
            ..Default::default()
        });
    });
    assert!(
        cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining) > px(0.),
        "outer transcript input must queue easing in the combined fixture"
    );

    let mut smooth_steps = 0;
    let mut smooth_probes = Vec::new();
    for _ in 0..64 {
        if cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining) == px(0.) {
            break;
        }
        let draws = measure_draws(cx, |cx| {
            assert!(
                cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
                "queued transcript motion must have a scheduled frame"
            );
        });
        assert!(!draws.is_empty(), "each transcript easing step must draw");
        smooth_steps += 1;
        smooth_draws.extend(draws);
        // Probe one settled frame without advancing another easing step.
        smooth_probes.push(settled_frame_probe(cx));
    }
    assert!(
        smooth_steps > 0 && smooth_steps <= MAX_SMOOTH_STEPS,
        "transcript easing must converge within {MAX_SMOOTH_STEPS} steps"
    );
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).view.smooth_scroll.remaining),
        px(0.),
        "transcript easing must converge"
    );
    smooth_draws.extend(measure_draws(cx, settle_frame_callbacks));
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
    // These counters describe separate settled frames, not the sum of the
    // event and convergence draws captured by the timing helpers.
    for (operation, probe) in [
        ("answer wrap", nowrap_to_wrap),
        ("answer unwrap", wrap_to_nowrap),
        ("combined answer wrap", combined_wrap),
        // Crossing a row boundary can expose two code blocks together.
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
        assert!(
            probe.code_block_renders <= bound,
            "{operation} rendered {} code blocks (bound {bound})",
            probe.code_block_renders
        );
        assert!(
            probe.text_view_builds <= MAX_TEXT_VIEW_BUILDS_PER_SETTLED_FRAME,
            "{operation} rebuilt {} text views",
            probe.text_view_builds
        );
    }

    let smooth_p50 = duration_percentile(&smooth_draws, 50);
    let smooth_p95 = duration_percentile(&smooth_draws, 95);
    let smooth_max = smooth_draws.iter().copied().max().expect("smooth draws");
    eprintln!(
        "OUTER_LONG_CONTENT_PERF wrap_draw={nowrap_to_wrap_draw:?} \
         unwrap_draw={wrap_to_nowrap_draw:?} combined_wrap_draw={combined_wrap_draw:?} \
         smooth_steps={smooth_steps} smooth_draws={} \
         smooth_p50={smooth_p50:?} smooth_p95={smooth_p95:?} smooth_max={smooth_max:?}",
        smooth_draws.len()
    );
}

/// A lazy tool result combines 4000 code lines with an inline formula. Its
/// expanded viewport renders one continuous code-text element, and folding
/// the row releases the result body.
#[gpui::test]
fn long_tool_result_with_code_and_math_expands_under_the_row_model(cx: &mut TestAppContext) {
    const CODE_LINES: usize = 4000;

    init_performance_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(560.)));

    let code = (0..CODE_LINES)
        .map(|line| {
            format!(
                "let value_{line} = compute_really_long_identifier_{line}({line}, \"payload-{line}\");"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let result = format!("Result for $E=mc^2$:\n\n```rust\n{code}\n```\n\nDone.");
    let formula_start = result.find("$E=mc^2$").expect("fixture formula");

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = false;
            prefs.smooth_chat_scrolling = false;
        });
        chat.update(cx, |chat, cx| {
            for message in [
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Run the fixture tool.".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::ToolCall {
                        tool_call: crate::llm::ToolCall {
                            id: "call-perf".into(),
                            name: "codegen".into(),
                            arguments: serde_json::json!({}),
                            raw_arguments: "{}".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                LlmMessage {
                    role: crate::llm::Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_result: crate::llm::ToolResult {
                            call_id: "call-perf".into(),
                            content: result,
                            is_error: false,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
            ] {
                test_support::push_canonical(chat, message, cx);
            }
        });
    });
    redraw_settled(cx);

    // A folded activity retains the result data without creating its body.
    crate::ui::markdown::reset_perf_probe();
    cx.update(|_, cx| {
        let activity = last_activity_renderer(chat.read(cx)).expect("activity row");
        assert!(
            activity.result_body_entity_id().is_none(),
            "the result body is lazy before the first expand"
        );
    });
    assert_eq!(
        crate::ui::markdown::perf_probe().code_text_elements,
        0,
        "a folded activity row builds no code text"
    );

    // Expansion creates argument and result bodies. Time all actual draws,
    // then count their code elements in one separate settled frame.
    let row_id = chat.read_with(cx, |chat, _| {
        rows_of_kind(chat, RowKind::ToolActivity)
            .last()
            .expect("activity row")
            .id()
    });
    let header_selector: &'static str =
        Box::leak(format!("{}-header", row_id.debug_name()).into_boxed_str());
    let header = cx.debug_bounds(header_selector).expect("activity header");
    let (expand_draw, expand_probe) = measured_interaction(cx, |cx| {
        cx.simulate_click(header.center(), Modifiers::default());
    });

    let owner_id = cx.update(|_, cx| {
        last_activity_renderer(chat.read(cx))
            .and_then(|activity| activity.result_body_owner_for_test())
            .expect("the expanded result body")
    });
    let math_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    redraw_settled_math(cx);
    assert!(
        cx.debug_bounds(math_selector).is_some(),
        "the result body renders its inline formula"
    );

    eprintln!(
        "TOOL_RESULT_PERF expand_draw={expand_draw:?} \
         expand={expand_probe:?}"
    );
    assert!(
        expand_probe.code_text_elements <= 2,
        "expanding builds one code-text element per body (arguments + result), \
         never per line of the {CODE_LINES}-line result: {}",
        expand_probe.code_text_elements
    );
    assert_eq!(
        expand_probe.code_text_elements, expand_probe.code_block_renders,
        "one continuous code-text element per rendered block"
    );
    assert!(
        expand_probe.text_view_builds <= 2,
        "the settled expand draw builds one text view per body (arguments + \
         result): {}",
        expand_probe.text_view_builds
    );

    // Tail following can clip the header after expansion; reveal it before
    // clicking to release the result body.
    chat.read_with(cx, |chat, _| {
        let item_ix = chat
            .view
            .projection
            .row_index(row_id)
            .expect("activity row");
        chat.view.list_state.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(0.),
        });
    });
    redraw_settled(cx);
    let header = cx.debug_bounds(header_selector).expect("activity header");
    assert!(chat.read_with(cx, |chat, _| {
        chat.view
            .list_state
            .viewport_bounds()
            .contains(&header.center())
    }));
    cx.simulate_click(header.center(), Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let activity = last_activity_renderer(chat.read(cx)).expect("activity row");
        assert!(
            activity.result_body_entity_id().is_none(),
            "collapse releases the result body"
        );
    });
}

/// Initial content and newly exposed blocks must fit the frame budget while
/// remaining selectable through the transcript's outer scroll owner.
#[gpui::test]
fn long_windowed_prose_first_frame_and_scroll_stay_in_budget(cx: &mut TestAppContext) {
    // Parsing six megabytes of Markdown in a debug build costs minutes for no
    // signal (budgets are release-only below), so debug runs the same shape
    // at one tenth scale.
    const LINES: usize = if cfg!(debug_assertions) {
        10_000
    } else {
        100_000
    };
    const SCROLL_TARGET: &str = "WINDOWED_SCROLL_TARGET";
    const TAIL_TARGET: &str = "WINDOWED_TAIL_TARGET";

    init_performance_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(560.)));
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.smooth_chat_scrolling = false;
            prefs.code_block_wrap = false;
            prefs.code_block_line_numbers = false;
        });
    });
    settle_frame_callbacks(cx);

    // One block per pair of source lines, with two short selectable markers.
    let mut source = String::with_capacity(LINES * 96);
    let mut target_offset = 0;
    for line in (0..LINES).step_by(2) {
        if line == 120 {
            target_offset = source.len();
            source.push_str(&format!("```text\n{SCROLL_TARGET}\n```\n\n"));
        } else if line % 50 == 0 {
            source.push_str(&format!("## Section {}\n\n", line / 50));
        } else {
            source.push_str(&format!(
                "Windowed paragraph {line} carries enough body text to exercise real layout, \
                 with inline markup like `code` and **emphasis**.\n\n"
            ));
        }
    }
    let tail_offset = source.len();
    source.push_str(&format!("```text\n{TAIL_TARGET}\n```\n"));

    let initial_draws = measure_draws(cx, |cx| {
        cx.update(|_, cx| {
            chat.update(cx, |chat, cx| {
                test_support::push_canonical(
                    chat,
                    LlmMessage {
                        role: crate::llm::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: source,
                            provider_metadata: ProviderMetadata::default(),
                        }],
                        provider_metadata: ProviderMetadata::default(),
                    },
                    cx,
                );
            });
        });
        settle_frame_callbacks(cx);
    });
    let initial_max = initial_draws
        .iter()
        .copied()
        .max()
        .expect("initial content must produce a frame");
    let (owner_id, visible_bottom) = cx.update(|window, cx| {
        let chat = chat.read(cx);
        assert!(
            renderer_for_row(
                chat,
                rows_of_kind(chat, RowKind::AssistantProse)
                    .first()
                    .expect("the long prose row"),
            )
            .expect("prose renderer")
            .requests_windowed_layout(),
            "the long prose row must request windowed block layout"
        );
        (
            last_prose_id(chat),
            window.viewport_size().height - chat.composer_height,
        )
    });
    let target_selector: &'static str =
        Box::leak(format!("markdown-code-line-{owner_id}-{target_offset}-0").into_boxed_str());
    let tail_selector: &'static str =
        Box::leak(format!("markdown-code-line-{owner_id}-{tail_offset}-0").into_boxed_str());
    let visible = |bounds: &gpui::Bounds<gpui::Pixels>| {
        bounds.top() >= px(0.) && bounds.bottom() <= visible_bottom
    };
    let tail = cx
        .debug_bounds(tail_selector)
        .filter(visible)
        .expect("initial tail-follow must display actual tail text");
    assert_code_line_copy(cx, tail, TAIL_TARGET);

    // Start the wheel traversal above a target outside the initial overdraw.
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            chat.view
                .list_state
                .set_follow_mode(gpui::FollowMode::Normal);
            chat.view.list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
            cx.notify();
        });
    });
    settle_frame_callbacks(cx);
    assert!(
        cx.debug_bounds(target_selector).is_none(),
        "the wheel target must begin outside the materialized band"
    );
    let before_scroll = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());

    let wheel_point = point(px(380.), px(280.));
    let mut scroll_draws = Vec::new();
    let mut copied_target = false;
    for _ in 0..48 {
        let mut target_in_event_frame = false;
        let draws = measure_draws(cx, |cx| {
            cx.simulate_event(ScrollWheelEvent {
                position: wheel_point,
                delta: ScrollDelta::Lines(point(0., -9.)),
                ..Default::default()
            });
            target_in_event_frame = cx
                .debug_bounds(target_selector)
                .is_some_and(|b| visible(&b));
            settle_frame_callbacks(cx);
        });
        assert!(
            !draws.is_empty(),
            "a wheel gesture must produce a measured frame"
        );
        scroll_draws.extend(draws);
        if !copied_target && target_in_event_frame {
            let target = cx
                .debug_bounds(target_selector)
                .filter(visible)
                .expect("the exposed target must remain visible after convergence");
            assert_code_line_copy(cx, target, SCROLL_TARGET);
            copied_target = true;
        }
    }
    let after_scroll = cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top());
    assert!(
        after_scroll.item_ix > before_scroll.item_ix
            || after_scroll.offset_in_item > before_scroll.offset_in_item,
        "the wheel gestures must actually advance the transcript or the \
         scroll timings above prove nothing"
    );
    assert!(
        copied_target,
        "scrolling must expose and select the target text"
    );
    let scroll_p95 = duration_percentile(&scroll_draws, 95);

    eprintln!(
        "WINDOWED_PROSE_PERF lines={LINES} initial_frames={} initial_max={initial_max:?} \
         scroll_frames={} scroll_p95={scroll_p95:?} scroll_max={:?}",
        initial_draws.len(),
        scroll_draws.len(),
        scroll_draws.iter().copied().max().unwrap_or_default(),
    );

    if !cfg!(debug_assertions) {
        assert!(
            initial_max <= Duration::from_millis(16),
            "every initial content frame must stay inside the 16 ms budget: {initial_max:?}"
        );
        assert!(
            scroll_p95 <= Duration::from_millis(16),
            "release windowed scroll-only p95 must stay inside the frame budget: {scroll_p95:?}"
        );
    }
}

fn assert_code_line_copy(
    cx: &mut gpui::VisualTestContext,
    bounds: gpui::Bounds<gpui::Pixels>,
    expected: &str,
) {
    let start = point(bounds.left() + px(1.), bounds.center().y);
    let end = point(bounds.right() - px(1.), bounds.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.dispatch_action(gpui_component::input::Copy);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(expected.to_string()),
        "the visible code text must participate in drag selection"
    );
    cx.update(gpui_base::TextSelection::clear);
}
