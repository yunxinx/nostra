use super::*;

/// Once the user works the toggle, the stream stops deciding for them: a
/// trace the user re-opened after the automatic fold stays open through the
/// terminal reconciliation, while a trace the user left alone stays folded.
#[gpui::test]
fn manual_toggle_survives_the_auto_collapse(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "thinking", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "answer", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn, cx)[0].1, "the stream still ended");
        assert!(
            !reasoning.is_expanded(),
            "the finished trace folds down automatically"
        );
    });

    // The user re-opens the folded trace through the real toggle path.
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            let row_id = rows_of_kind(this, RowKind::Reasoning)
                .first()
                .map(|row| row.id())
                .expect("reasoning row");
            assert!(toggle_reasoning_row_by_id(this, row_id));
        });
    });
    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert!(
            reasoning_part(turn).is_some_and(|reasoning| reasoning.is_expanded()),
            "the user's toggle re-opens the trace"
        );
    });

    // A late authoritative snapshot must not override the user's choice.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(
                this,
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "thinking".into(),
                                replay: None,
                            },
                        },
                        ContentBlock::Text {
                            text: "answer".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert!(
            reasoning_part(turn).is_some_and(|reasoning| reasoning.is_expanded()),
            "explicit user intent outlives the terminal reconciliation"
        );
    });
}

/// The collapsed trigger is a chip, not a bar: it must lay out at its own
/// label width rather than stretching across the content column. Asserted
/// against the transcript's real geometry, since "does a flex child stretch"
/// depends on the container it ends up in and is easy to regress by moving
/// the element.
#[gpui::test]
fn the_collapsed_trigger_hugs_its_label(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    // Reason, then answer: that collapses the card down to its trigger.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "a thought", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "The answer.", cx);
        });
    });
    cx.run_until_parked();
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the collapsed trigger was drawn");

    assert!(
        trigger.size.width < crate::chat::CONTENT_MAX_WIDTH,
        "the trigger stretched to the content column ({:?}) instead of hugging its label",
        trigger.size.width
    );
    assert!(
        trigger.size.width > px(0.),
        "the trigger must still be wide enough to hit"
    );
}

/// Clicking the copy button puts the turn's complete reasoning on the
/// clipboard — the accumulated source, not the seven lines the card happens
/// to be showing.
#[gpui::test]
fn the_copy_button_copies_the_whole_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    // Use the same rooted element tree as production so click routing and
    // overlay ownership exercise the real window contract.
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    // Far more than the visible budget, so the copy would be lossy if it read
    // the rendered view instead of the source.
    let mut expected = String::new();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..30 {
                let delta = format!("Reasoning line {line}.\n\n");
                expected.push_str(&delta);
                test_support::append_reasoning(this, 0, "reasoning-0".into(), &delta, cx);
            }
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "The answer.", cx);
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let copy = cx
        .debug_bounds("reasoning-copy-0")
        .expect("the copy button is in the tree once there is reasoning");
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the reasoning trigger is in the tree");
    // The copy action is intentionally hidden from hit testing and keyboard
    // focus until its group is hovered. Exercise that real interaction
    // instead of clicking the hidden element by debug bounds.
    cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, expected,
        "the clipboard must carry the complete reasoning source"
    );

    // Copying is not a disclosure gesture. The button sits beside the trigger
    // rather than inside it, and `Clipboard` stops propagation, so a copy must
    // leave the card exactly as the user left it.
    assert!(
        chat.read_with(cx, |this, _| reasoning_part(this)
            .is_some_and(|reasoning| !reasoning.is_expanded())),
        "copying must not toggle the card open"
    );
}

/// Nothing to copy before the first delta lands, and nothing to copy while the
/// block is still streaming: a copy offered mid-stream would freeze a partial
/// thought. The button stays out of the tree until the stream boundary.
#[gpui::test]
fn the_copy_button_appears_only_once_reasoning_has_ended(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };

    // An empty protocol delta is not evidence that reasoning started.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "", cx)
        });
    });
    draw(cx);
    assert!(
        chat.read_with(cx, |this, _| reasoning_part(this).is_none()),
        "empty deltas must not allocate a trace"
    );
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_none(),
        "no copy button while there is nothing to copy"
    );

    // Reasoning streams but has not ended: the card is live, the copy stays
    // out.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "a thought", cx)
        });
    });
    draw(cx);
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_none(),
        "no copy button while reasoning is still streaming"
    );

    // The stream boundary is what earns the button.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx)
        });
    });
    draw(cx);
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_some(),
        "the button becomes available once the reasoning stream ends"
    );
}

/// The message-level copy button only appears once the turn has prose and its
/// stream has ended. A reasoning-only stream must not offer to copy an empty
/// string, and a still-streaming turn must not offer to freeze a partial
/// answer — the same gate the reasoning card applies to its own per-card copy
/// button.
#[gpui::test]
fn the_message_copy_button_appears_only_once_the_turn_finished_streaming(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    // Reasoning streams first; there is still no prose to copy.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "a thought", cx)
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-turnactions-2-0-copy").is_none(),
        "no message copy button while the turn has nothing to copy"
    );

    // Reasoning ends, then the first text delta lands — but the turn is still
    // streaming, so a copy offered now would freeze a partial answer.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "The answer.", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-turnactions-2-0-copy").is_none(),
        "no message copy button while the turn is still streaming"
    );

    // Ending the stream is what earns the button.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_text(this, 1, "text-0", None, cx)
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-turnactions-2-0-copy").is_some(),
        "the message copy button appears once the turn finished streaming"
    );
}

/// Clicking the message-level copy button puts the turn's complete prose on
/// the clipboard — every text part in canonical order, reasoning excluded.
#[gpui::test]
fn the_message_copy_button_copies_the_whole_answer(cx: &mut TestAppContext) {
    init_app(cx);
    // Use the same rooted element tree as production so click routing and
    // overlay ownership exercise the real window contract.
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "a private thought", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "First part.", cx);
            test_support::append_text(this, 2, "text-1".into(), "Second part.", cx);
            // End the stream so the turn becomes copyable: the copy gate
            // requires every streamed block to have finished.
            test_support::finish_text(this, 1, "text-0", None, cx);
            test_support::finish_text(this, 2, "text-1", None, cx);
        });
    });
    redraw(cx);

    let copy = cx
        .debug_bounds("row-turnactions-2-0-copy")
        .expect("the message copy button is in the tree once the turn finished streaming");
    let content = cx
        .debug_bounds("row-prose-2-2")
        .expect("the assistant content is in the tree");
    // The copy action is intentionally hidden from hit testing and keyboard
    // focus until its group is hovered. Exercise that real interaction
    // instead of clicking the hidden element by debug bounds.
    cx.simulate_mouse_move(content.center(), None, gpui::Modifiers::default());
    redraw(cx);
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, "First part.\nSecond part.",
        "the clipboard must carry every text part in canonical order, excluding reasoning"
    );
}

/// A failed assistant turn offers no message-level copy button even when prose
/// finished streaming before the failure: the error card owns the "copy raw
/// response" affordance, and a second copy would either duplicate it or copy
/// partial prose.
#[gpui::test]
fn a_failed_turn_offers_no_message_copy_button(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    // Prose lands and its stream ends, so the message-level button is up.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_text(
                this,
                0,
                "text-0".into(),
                "Partial answer before the failure.",
                cx,
            );
            test_support::finish_text(this, 0, "text-0", None, cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-turnactions-2-0-copy").is_some(),
        "the message copy button appears once the turn finished streaming"
    );

    // The turn then fails; the error card takes over the copy affordance.
    let error = crate::llm::GatewayError::http(503, Some("unavailable".into()));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, None, Some(error), cx)
        });
    });
    redraw(cx);
    assert!(
        chat.read_with(cx, |this, app| this
            .transcript
            .read(app)
            .turns()
            .last()
            .is_some_and(|turn| turn.error.is_some())),
        "the failed turn carries its error card"
    );
    assert!(
        cx.debug_bounds("row-turnactions-2-0-copy").is_none(),
        "a failed turn must not offer a message-level copy button"
    );
}

/// The message copy row and the reasoning card share the message's hover
/// group, so hovering the nested reasoning trigger reveals the message-level
/// button too. The copy action stays hidden from hit testing until then —
/// this test exercises the real hover → reveal → click interaction.
#[gpui::test]
fn hovering_a_reasoning_row_reveals_the_message_copy_button(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "a private thought", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "The visible answer.", cx);
            test_support::finish_text(this, 1, "text-0", None, cx);
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the reasoning trigger is in the tree");
    let copy = cx
        .debug_bounds("row-turnactions-2-0-copy")
        .expect("the message copy button is in the tree once the turn finished streaming");
    // Hover only the nested reasoning card — the message body itself is never
    // hovered — and the shared message hover group must still reveal the row.
    cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
    redraw(cx);
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, "The visible answer.",
        "hovering the reasoning card must reveal and arm the message copy button"
    );
}

/// The copy gate applies to user turns too: a user message with prose gets the
/// same message-level copy button, revealing on hover over its own bubble.
#[gpui::test]
fn a_user_turn_gets_the_message_copy_button_too(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    // A user turn with prose, built the way the transcript grows one. The
    // stream is ended the same way production finishes a user turn's part, so
    // the copy gate treats it as settled.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_empty(this, Role::User, cx);
            test_support::append_text(
                this,
                0,
                "text-0".into(),
                "What is the capital of France?",
                cx,
            );
            test_support::finish_text(this, 0, "text-0", None, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    assert!(
        cx.debug_bounds("row-turnactions-1-0-copy").is_some(),
        "a user message with prose offers the message-level copy button"
    );

    let bubble = cx
        .debug_bounds("row-userbubble-1-1-bubble")
        .expect("the user bubble is in the tree");
    let copy = cx
        .debug_bounds("row-turnactions-1-0-copy")
        .expect("the message copy button is in the tree");
    cx.simulate_mouse_move(bubble.center(), None, gpui::Modifiers::default());
    redraw(cx);
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, "What is the capital of France?",
        "the user's own prose is what lands on the clipboard"
    );
}

/// A non-reasoning assistant turn has no synthetic placeholder part. Its first
/// visible text event creates the exact part that render consumes.
#[gpui::test]
fn a_turn_without_reasoning_creates_only_its_text_part(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_text(this, 0, "text-0".into(), "answer", cx);
        });
    });
    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert_eq!(
            last_turn(turn, cx).parts[0].source.prose_text(),
            Some("answer")
        );
    });
}
