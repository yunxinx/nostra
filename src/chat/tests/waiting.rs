use super::*;

#[gpui::test]
fn seed_turn_without_generating_does_not_show_waiting(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    redraw(cx);

    assert!(
        cx.debug_bounds("row-prose-2-0").is_none(),
        "an empty assistant placeholder is not a wait state"
    );
}

#[gpui::test]
fn empty_generating_assistant_shows_waiting_until_text_arrives(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, test_support::mark_generating);
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-prose-2-0").is_some(),
        "the assistant wait shimmer is the only content before the first visible part"
    );
    assert!(
        cx.debug_bounds("row-prose-1-0").is_none(),
        "user turns never show the assistant wait shimmer"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_text(this, 0, "text-0".into(), "Hello.", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-prose-2-0").is_none(),
        "the wait shimmer must leave the tree once visible text arrives"
    );
}

#[gpui::test]
fn waiting_hides_when_reasoning_appears(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, test_support::mark_generating);
    });
    redraw(cx);
    assert!(cx.debug_bounds("row-prose-2-0").is_some());

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "a thought", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-prose-2-0").is_none(),
        "a reasoning card is visible content, so waiting must not sit beside it"
    );
}

#[gpui::test]
fn waiting_hides_when_the_error_row_appears(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, test_support::mark_generating);
    });
    redraw(cx);
    assert!(cx.debug_bounds("row-prose-2-0").is_some());

    let error = crate::llm::GatewayError::http(503, Some("unavailable".into()));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, None, Some(error), cx)
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-prose-2-0").is_none(),
        "an error card replaces waiting rather than sharing the column"
    );
}

#[gpui::test]
fn waiting_hides_when_a_named_tool_row_appears(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, test_support::mark_generating);
    });
    redraw(cx);
    assert!(cx.debug_bounds("row-prose-2-0").is_some());

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::start_tool_call(this, 0, 0, "call-0".into(), "lookup".into(), cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("row-prose-2-0").is_none(),
        "a named tool row is visible content"
    );
}

#[gpui::test]
fn waiting_only_on_the_in_flight_assistant(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_empty(this, Role::User, cx);
            test_support::push_empty(this, Role::Assistant, cx);
        });
    });
    cx.update(|_, cx| {
        chat.update(cx, test_support::mark_generating);
    });
    redraw(cx);

    assert!(
        cx.debug_bounds("row-prose-2-0").is_none(),
        "a leftover empty assistant must not shimmer while a later turn generates"
    );
    assert!(
        cx.debug_bounds("row-prose-4-0").is_some(),
        "only the in-flight assistant placeholder shows waiting"
    );
}

#[gpui::test]
fn a_tool_result_does_not_end_waiting(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_empty(this, Role::User, cx);
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::ToolResult {
                        tool_result: crate::llm::ToolResult {
                            call_id: "call-0".into(),
                            content: "lookup output".into(),
                            is_error: false,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
            test_support::mark_generating(this, cx);
        });
    });
    redraw(cx);
    // The turn's only row (the unpaired result) is replaced by the wait
    // shimmer while the conversation generates, so the row's own selector
    // resolves to the shimmer element.
    assert!(
        cx.debug_bounds("row-toolactivity-2-1").is_some(),
        "a tool result is not visible wait-ending content"
    );
}

#[gpui::test]
fn a_tool_turn_renders_as_a_muted_result_card(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "lookup".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_result: crate::llm::ToolResult {
                            call_id: "call-0".into(),
                            content: "lookup output".into(),
                            is_error: false,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);

    assert!(
        cx.debug_bounds("row-userbubble-2-2").is_none(),
        "a tool turn is not a user bubble"
    );
    assert!(
        cx.debug_bounds("row-toolactivity-2-2").is_some(),
        "a tool turn renders the muted result card"
    );
}
