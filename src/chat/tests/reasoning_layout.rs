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
            "the budgeted viewport must use the retained scrollable TextView"
        );
        assert_eq!(
            renderer.scroll_offset(),
            point(px(0.), px(0.)),
            "opening historical reasoning must not jump to its tail"
        );
    });
}

/// AC2: the budgeted viewport's height is exactly
/// `max(12 lines, viewport × 45%)`, verified at two window heights, and the
/// full-text toggle switches to natural height with no inner scrollbar — then
/// back.
#[gpui::test]
fn budgeted_height_follows_the_viewport_and_full_text_is_natural(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

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

    let expand = |cx: &mut gpui::VisualTestContext| {
        let trigger = cx
            .debug_bounds("reasoning-trigger-0")
            .expect("collapsed reasoning trigger");
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
        redraw(cx);
        redraw(cx);
    };

    for window_height in [700., 400.] {
        cx.simulate_resize(gpui::size(px(900.), px(window_height)));
        // First height expands through the trigger; the second height keeps
        // the budgeted viewport open and must follow the resized window.
        if window_height == 700. {
            expand(cx);
        } else {
            redraw(cx);
            redraw(cx);
        }

        let budgeted = cx
            .debug_bounds("reasoning-body-0")
            .expect("budgeted reasoning body");
        let (expected, line_height) = cx.update(|window, cx| {
            let viewport = chat.read(cx).view.viewport_height;
            (
                (window.line_height() * crate::chat::rows::typography::BUDGET_MIN_LINES)
                    .max(viewport * crate::chat::rows::typography::BUDGET_VIEWPORT_RATIO),
                window.line_height(),
            )
        });
        assert!(
            (budgeted.size.height - expected).abs() < px(1.),
            "at window height {window_height} the budget was {:?}, expected {:?}",
            budgeted.size.height,
            expected
        );
        // The budget floor must actually bind at the shorter window: the
        // 12-line minimum exceeds 45% of a 400px viewport.
        if window_height == 400. {
            assert!(
                expected <= line_height * 12. + px(1.),
                "the short window must exercise the line-count floor, got {expected:?}"
            );
        }

        // Full text: natural height, no inner scrollbar, smaller than the
        // budget for content below it — then back to the budgeted viewport.
        // The trigger row sits above the tall body, so bring it back into
        // the window before clicking.
        chat.update(cx, |this, _| {
            this.view.list_state.scroll_to(ListOffset::default());
        });
        redraw(cx);
        let full_toggle = cx
            .debug_bounds("reasoning-full-0")
            .expect("full-text toggle");
        cx.simulate_click(full_toggle.center(), gpui::Modifiers::default());
        redraw(cx);
        redraw(cx);

        let full_body = cx
            .debug_bounds("reasoning-body-0")
            .expect("full-text reasoning body");
        assert!(
            cx.debug_bounds("reasoning-viewport-0").is_none(),
            "the full-text form must not render an inner scroll viewport"
        );
        let budget_height = budgeted.size.height;
        assert!(
            full_body.size.height > budget_height,
            "240 paragraphs must exceed the budget in natural height ({:?} vs {budget_height:?})",
            full_body.size.height
        );

        chat.update(cx, |this, _| {
            this.view.list_state.scroll_to(ListOffset::default());
        });
        redraw(cx);
        let full_toggle = cx
            .debug_bounds("reasoning-full-0")
            .expect("full-text toggle after expanding");
        cx.simulate_click(full_toggle.center(), gpui::Modifiers::default());
        redraw(cx);
        redraw(cx);
        let back = cx
            .debug_bounds("reasoning-body-0")
            .expect("budgeted reasoning body after collapsing full text");
        assert!(
            (back.size.height - expected).abs() < px(1.),
            "collapsing full text must return to the budgeted height"
        );
    }
}

/// The budgeted viewport's scrollbar host must use the viewport's full
/// width. Padding belongs inside the scrollable TextView; otherwise the
/// scrollbar is inset into the content column.
#[gpui::test]
fn budgeted_reasoning_viewport_reaches_the_rail_edge(cx: &mut TestAppContext) {
    init_app(cx);
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

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("budgeted reasoning body");
    let viewport = cx
        .debug_bounds("reasoning-viewport-0")
        .expect("budgeted reasoning viewport");
    assert_eq!(
        viewport.right(),
        body.right(),
        "the budgeted scrollbar host must reach the viewport's right edge"
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
