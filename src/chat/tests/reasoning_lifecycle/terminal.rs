use super::super::*;

/// Responses providers may forward empty text deltas. They carry no content and
/// cannot stand in for the separate reasoning-finished lifecycle event.
#[gpui::test]
fn empty_text_delta_does_not_finish_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.append_stream_text(1, "text-0".into(), "", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn)[0].1 && reasoning.is_expanded());
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { .. }]
        ));
    });
}

/// Direct view updates obey the same structural rule as the event bridge: a
/// text delta cannot close an independently identified reasoning block.
#[gpui::test]
fn visible_text_does_not_finish_an_independent_reasoning_block(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.append_stream_text(1, "text-0".into(), "interleaved text", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn)[0].1 && reasoning.is_expanded());
    });
}

/// Reasoning that runs to the end of a turn with no text after it — a failed
/// or cancelled turn, or a model that reasons and then stops — is closed by
/// `finish_reply` instead.
#[gpui::test]
fn terminating_a_turn_closes_an_open_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "interrupted mid-thought", cx);
            this.finish_reply(None, None, cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn)[0].1);
        assert!(!reasoning.is_expanded());
    });
}

/// As in pi's `message_end` handling, the complete terminal message is the
/// rendering authority even when it differs from the live delta projection.
#[gpui::test]
fn terminal_message_replaces_streamed_reasoning_projection(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "partial", cx);
            this.finish_reply(
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
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(
            reasoning_states(turn)[0].0,
            "authoritative terminal reasoning"
        );
    });
}

/// Some providers backfill omitted intermediate events in their terminal
/// object. The terminal snapshot must still create the presentation state.
#[gpui::test]
fn terminal_message_can_create_a_reasoning_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
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

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("terminal trace");
        assert_eq!(reasoning_states(turn)[0], ("backfilled reasoning", true));
        assert!(!reasoning.is_expanded());
    });
}
