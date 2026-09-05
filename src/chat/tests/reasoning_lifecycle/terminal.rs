use super::super::*;

/// Responses providers may forward empty text deltas. They carry no content and
/// cannot stand in for the separate reasoning-finished lifecycle event.
#[gpui::test]
fn empty_text_delta_does_not_finish_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "thinking", cx);
            test_support::append_text(this, 1, "text-0".into(), "", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn, cx)[0].1 && reasoning.is_expanded());
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
            [ContentBlock::Reasoning { .. }]
        ));
    });
}

/// Direct view updates obey the same structural rule as the event bridge: a
/// text delta cannot close an independently identified reasoning block.
#[gpui::test]
fn visible_text_does_not_finish_an_independent_reasoning_block(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "thinking", cx);
            test_support::append_text(this, 1, "text-0".into(), "interleaved text", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn, cx)[0].1 && reasoning.is_expanded());
    });
}

/// Reasoning that runs to the end of a turn with no text after it — a failed
/// or cancelled turn, or a model that reasons and then stops — is closed by
/// `finish_reply` instead.
#[gpui::test]
fn terminating_a_turn_closes_an_open_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(
                this,
                0,
                "reasoning-0".into(),
                "interrupted mid-thought",
                cx,
            );
            test_support::finish_reply(this, None, None, cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn, cx)[0].1);
        assert!(!reasoning.is_expanded());
    });
}

/// As in pi's `message_end` handling, the complete terminal message is the
/// rendering authority even when it differs from the live delta projection.
#[gpui::test]
fn terminal_message_replaces_streamed_reasoning_projection(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "partial", cx);
            test_support::finish_reply(
                this,
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "authoritative terminal reasoning".into(),
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

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert_eq!(
            reasoning_states(turn, cx)[0].0,
            "authoritative terminal reasoning"
        );
    });
}

/// Some providers backfill omitted intermediate events in their terminal
/// object. The terminal snapshot must still create the presentation state.
#[gpui::test]
fn terminal_message_can_create_a_reasoning_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(
                this,
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "backfilled reasoning".into(),
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

    // The terminal reconcile creates a new row; the deferred window sync
    // materializes it once the list has laid out. Force the scheduled frame
    // so the sync runs, then draw the materialized content.
    redraw(cx);
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let reasoning = reasoning_part(turn).expect("terminal trace");
        assert_eq!(
            reasoning_states(turn, cx)[0],
            ("backfilled reasoning", true)
        );
        assert!(!reasoning.is_expanded());
    });
}
