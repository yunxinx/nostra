use super::super::*;

/// The regression this whole module exists for: reasoning deltas were being
/// folded into canonical content and then never rendered. A streaming trace
/// must reach a visible, expanded card *and* stay out of the prose body.
#[gpui::test]
fn streaming_reasoning_reaches_an_expanded_card(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "Weighing ", cx);
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "the options.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this;
        let reasoning = reasoning_part(turn).expect("a trace was created");

        assert_eq!(
            reasoning_states(turn, cx)[0].0,
            "Weighing the options.",
            "deltas accumulate into the card's own markdown source"
        );
        assert!(
            reasoning.is_expanded() && !reasoning_states(turn, cx)[0].1,
            "a live trace shows itself without being asked"
        );
        // Canonical content still carries it, for replay across turns.
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "Weighing the options."
        ));
        assert_eq!(
            last_turn(turn, cx).parts.len(),
            1,
            "no synthetic empty prose part"
        );
    });

    // Draws the whole view, card included, without panicking.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

/// The canonical reasoning-finished boundary collapses the card. Text remains
/// an independent content event, matching pi's thinking block lifecycle.
#[gpui::test]
fn reasoning_finished_collapses_the_card(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "thinking", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, None, cx);
            test_support::append_text(this, 1, "text-0".into(), "Here is the answer.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this;
        let reasoning = reasoning_part(turn).expect("trace");

        assert!(reasoning_states(turn, cx)[0].1, "the block was closed");
        assert!(
            !reasoning.is_expanded(),
            "a finished trace folds down to its trigger"
        );
        assert!(
            reasoning_states(turn, cx)[0].0.contains("thinking"),
            "the reasoning text is retained for re-expansion"
        );
        let parts = &last_turn(turn, cx).parts;
        assert!(matches!(&parts[0].source, PartSource::Reasoning { .. }));
        assert_eq!(parts[1].source.prose_text(), Some("Here is the answer."));
    });
}

/// Exercise the production assistant boundary, not just `ChatView` methods in
/// isolation: an explicit canonical end marker must remain ordered between the
/// final reasoning delta and the first prose delta.
#[gpui::test]
fn canonical_events_close_reasoning_before_prose_reaches_the_view(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        delta: "thinking".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-1".into(),
                        delta: "answer".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn, cx)[0].0, "thinking");
        assert!(
            reasoning_states(turn, cx)[0].1,
            "the explicit boundary was replayed"
        );
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
            [ContentBlock::Reasoning { reasoning }, ContentBlock::Text { text, .. }]
                if reasoning.display == "thinking" && text == "answer"
        ));
    });
}

/// Content deltas do not own another block's lifecycle. Protocol adapters emit
/// the explicit end boundary before a type transition, and the UI preserves it.
#[gpui::test]
fn prose_does_not_infer_a_reasoning_boundary(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        delta: "thinking".into(),
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-1".into(),
                        delta: String::new(),
                    },
                ],
                cx,
            );
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![crate::llm::GenerationEvent::TextDelta {
                    content_index: 1,
                    id: "text-1".into(),
                    delta: "answer".into(),
                }],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert!(reasoning_part(turn).is_some());
        assert!(
            !reasoning_states(turn, cx)[0].1,
            "prose cannot close a different content block"
        );
    });
}

/// Content type transitions are structural boundaries. A later reasoning run
/// gets a new card at its canonical position instead of reopening or appending
/// to the first card.
#[gpui::test]
fn reasoning_after_prose_creates_a_second_ordered_card(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        delta: "first".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::TextStarted {
                        content_index: 1,
                        id: "text-0".into(),
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-0".into(),
                        delta: "answer".into(),
                    },
                    crate::llm::GenerationEvent::TextFinished {
                        content_index: 1,
                        id: "text-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 2,
                        id: "reasoning-1".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 2,
                        id: "reasoning-1".into(),
                        delta: "second".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn, cx),
            vec![("first", true), ("second", false)]
        );
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
            [
                ContentBlock::Reasoning { reasoning: first },
                ContentBlock::Text { text, .. },
                ContentBlock::Reasoning { reasoning: second },
            ] if first.display == "first" && text == "answer" && second.display == "second"
        ));
    });

    // Both cards must coexist in the real GPUI element tree with independent
    // element ids and interaction state.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

/// R10: the finish event carries the coalescer's enqueue-time duration. The
/// transcript banks it on the part and the trigger label renders it — the
/// renderer owns no timer.
#[gpui::test]
fn a_streamed_finish_banks_the_measured_duration_on_the_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "thinking", cx);
            test_support::finish_reasoning(
                this,
                0,
                "reasoning-0",
                None,
                Some(Duration::from_millis(72_000)),
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let PartSource::Reasoning { reasoning, .. } = &last_turn(this, cx).parts[0].source else {
            panic!("reasoning part");
        };
        assert_eq!(
            reasoning.duration_ms,
            Some(72_000),
            "the finish path banks the duration on the part content"
        );
        let renderer = reasoning_part(this).expect("trace");
        assert_eq!(
            renderer.elapsed(),
            Some(Duration::from_millis(72_000)),
            "the renderer reads the banked duration, not its own clock"
        );
        assert!(
            renderer.label_for_test().contains("1 m 12 s"),
            "the label interpolates the adaptive duration format, got {:?}",
            renderer.label_for_test()
        );
    });
}

/// R7: the terminal message carries the gateway-banked duration, and the
/// terminal reconciliation keeps showing it (the streamed part's value makes
/// way for the authoritative one).
#[gpui::test]
fn the_terminal_message_keeps_a_banked_duration_on_the_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "thinking", cx);
            test_support::finish_reasoning(
                this,
                0,
                "reasoning-0",
                None,
                Some(Duration::from_millis(72_000)),
                cx,
            );
            test_support::finish_reply(
                this,
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "thinking".into(),
                                replay: None,
                                duration_ms: Some(31_000),
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
        let this = chat.read(cx);
        let renderer = reasoning_part(this).expect("trace");
        assert_eq!(
            renderer.elapsed(),
            Some(Duration::from_millis(31_000)),
            "the authoritative message's duration replaces the streamed one"
        );
        assert!(
            renderer.label_for_test().contains("31 s"),
            "got {:?}",
            renderer.label_for_test()
        );
    });
}

/// R7: a restored session materializes its reasoning from the persisted
/// message (`Turn::from_llm` → `Part::from_block`), so the banked duration on
/// that message is what the trigger shows — no live timer involved.
#[gpui::test]
fn a_restored_trace_shows_its_persisted_duration(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "a restored thought".into(),
                            replay: None,
                            duration_ms: Some(7_385_000),
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let renderer = reasoning_part(this).expect("restored trace");
        assert_eq!(
            renderer.elapsed(),
            Some(Duration::from_millis(7_385_000)),
            "the duration comes from the persisted part content"
        );
        assert!(
            renderer.label_for_test().contains("2 h 3 m"),
            "the adaptive format renders hours, got {:?}",
            renderer.label_for_test()
        );
    });
}
