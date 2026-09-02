use super::*;

#[gpui::test]
fn seed_turn_without_generating_does_not_show_waiting(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    redraw(cx);

    assert!(
        cx.debug_bounds("assistant-waiting-1").is_none(),
        "an empty assistant placeholder is not a wait state"
    );
}

#[gpui::test]
fn empty_generating_assistant_shows_waiting_until_text_arrives(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.mark_generating_for_test(cx));
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("assistant-waiting-1").is_some(),
        "the assistant wait shimmer is the only content before the first visible part"
    );
    assert!(
        cx.debug_bounds("assistant-waiting-0").is_none(),
        "user turns never show the assistant wait shimmer"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, "text-0".into(), "Hello.", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("assistant-waiting-1").is_none(),
        "the wait shimmer must leave the tree once visible text arrives"
    );
}

#[gpui::test]
fn waiting_hides_when_reasoning_appears(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.mark_generating_for_test(cx));
    });
    redraw(cx);
    assert!(cx.debug_bounds("assistant-waiting-1").is_some());

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "a thought", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("assistant-waiting-1").is_none(),
        "a reasoning card is visible content, so waiting must not sit beside it"
    );
}

#[gpui::test]
fn waiting_hides_when_error_card_appears(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.mark_generating_for_test(cx));
    });
    redraw(cx);
    assert!(cx.debug_bounds("assistant-waiting-1").is_some());

    let error = crate::llm::GatewayError::http(503, Some("unavailable".into()));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.finish_reply(None, Some(error), cx));
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("assistant-waiting-1").is_none(),
        "an error card replaces waiting rather than sharing the column"
    );
}

#[gpui::test]
fn waiting_hides_when_a_named_tool_row_appears(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.mark_generating_for_test(cx));
    });
    redraw(cx);
    assert!(cx.debug_bounds("assistant-waiting-1").is_some());

    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            this.start_stream_tool_call(0, 0, "call-0".into(), "lookup".into());
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds("assistant-waiting-1").is_none(),
        "a named tool row is visible content"
    );
}

#[gpui::test]
fn waiting_only_on_the_in_flight_assistant(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            this.messages.push(Message::empty(Role::User));
            this.messages.push(Message::empty(Role::Assistant));
        });
    });
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.mark_generating_for_test(cx));
    });
    redraw(cx);

    assert!(
        cx.debug_bounds("assistant-waiting-1").is_none(),
        "a leftover empty assistant must not shimmer while a later turn generates"
    );
    assert!(
        cx.debug_bounds("assistant-waiting-3").is_some(),
        "only the in-flight assistant placeholder shows waiting"
    );
}
