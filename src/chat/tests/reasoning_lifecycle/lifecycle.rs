use super::super::*;

/// The regression this whole module exists for: reasoning deltas were being
/// folded into canonical content and then never rendered. A streaming trace
/// must reach a visible, expanded card *and* stay out of the prose body.
#[gpui::test]
fn streaming_reasoning_reaches_an_expanded_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "Weighing ", cx);
            this.append_stream_reasoning(0, "reasoning-0".into(), "the options.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this.messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("a trace was created");

        assert_eq!(
            reasoning_states(turn)[0].0,
            "Weighing the options.",
            "deltas accumulate into the card's own markdown source"
        );
        assert!(
            reasoning.is_expanded() && !reasoning_states(turn)[0].1,
            "a live trace shows itself without being asked"
        );
        // Canonical content still carries it, for replay across turns.
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "Weighing the options."
        ));
        assert_eq!(turn.parts.len(), 1, "no synthetic empty prose part");
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
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None, cx);
            this.append_stream_text(1, "text-0".into(), "Here is the answer.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this.messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");

        assert!(reasoning_states(turn)[0].1, "the block was closed");
        assert!(
            !reasoning.is_expanded(),
            "a finished trace folds down to its trigger"
        );
        assert!(
            reasoning_states(turn)[0].0.contains("thinking"),
            "the reasoning text is retained for re-expansion"
        );
        assert!(matches!(turn.parts.as_slice(), [MessagePart::Reasoning { .. }, MessagePart::Text { text, .. }] if text == "Here is the answer."));
    });
}

/// Exercise the production assistant boundary, not just `ChatView` methods in
/// isolation: an explicit canonical end marker must remain ordered between the
/// final reasoning delta and the first prose delta.
#[gpui::test]
fn canonical_events_close_reasoning_before_prose_reaches_the_view(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
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
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn)[0].0, "thinking");
        assert!(
            reasoning_states(turn)[0].1,
            "the explicit boundary was replayed"
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
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
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
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
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert!(
            !reasoning_states(turn)[0].1,
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
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
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
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn),
            vec![("first", true), ("second", false)]
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
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
