use super::*;

#[gpui::test]
fn smooth_scrolling_defers_discrete_wheel_movement_when_enabled(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
        chat.update(cx, |chat, cx| {
            for index in 0..20 {
                chat.messages.push(Message::from_canonical(
                    LlmMessage {
                        role: crate::llm::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: format!("message {index}\n\n{}", "body ".repeat(30)),
                            provider_metadata: ProviderMetadata::default(),
                        }],
                        provider_metadata: ProviderMetadata::default(),
                    },
                    cx,
                ));
            }
            chat.sync_message_list_count();
            chat.list_state.scroll_to(ListOffset::default());
        });
    });
    redraw(cx);

    let before = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    cx.simulate_event(ScrollWheelEvent {
        position: point(px(320.), px(100.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });

    let deferred = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    let pending = cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining);
    assert!(pending > px(0.), "wheel event must queue smooth motion");
    assert_eq!(deferred.item_ix, before.item_ix);
    assert_eq!(deferred.offset_in_item, before.offset_in_item);

    assert!(
        cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
        "the wheel event must schedule an animation frame"
    );
    redraw(cx);
    let eased = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    assert!(
        eased.item_ix > before.item_ix || eased.offset_in_item > before.offset_in_item,
        "an animation frame must advance the deferred wheel distance"
    );
}

/// A window behind a focused settings window can still receive macOS wheel
/// input. It must use the native one-shot scroll path rather than queueing a
/// custom animation whose inactive-window frame delivery is throttled.
#[gpui::test]
fn inactive_chat_window_does_not_queue_smooth_scroll(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
        chat.update(cx, |chat, cx| {
            for index in 0..20 {
                chat.messages.push(Message::from_canonical(
                    LlmMessage {
                        role: crate::llm::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: format!("message {index}\n\n{}", "body ".repeat(30)),
                            provider_metadata: ProviderMetadata::default(),
                        }],
                        provider_metadata: ProviderMetadata::default(),
                    },
                    cx,
                ));
            }
            chat.sync_message_list_count();
            chat.list_state.scroll_to(ListOffset::default());
        });
    });
    redraw(cx);
    let before_active = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    cx.simulate_event(ScrollWheelEvent {
        position: point(px(320.), px(100.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    assert!(
        cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining) > px(0.),
        "active line-wheel input must queue easing before the focus transition"
    );
    cx.deactivate_window();
    assert!(!cx.update(|window, _| window.is_window_active()));

    cx.update(|window, cx| {
        assert!(window.simulate_next_frame(cx) > 0);
    });
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining),
        px(0.),
        "a frame delivered after deactivation must cancel pending easing"
    );

    cx.simulate_event(ScrollWheelEvent {
        position: point(px(320.), px(100.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });

    let after_inactive = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    assert!(
        after_inactive.item_ix > before_active.item_ix
            || after_inactive.offset_in_item > before_active.offset_in_item,
        "inactive wheel input must still use the native transcript scroll path"
    );

    assert_eq!(
        cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining),
        px(0.),
        "inactive chat windows must not queue custom smooth scrolling"
    );
}

#[gpui::test]
fn inactive_reasoning_window_does_not_queue_card_smooth_scroll(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    seed_turn(&chat, cx);
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
        chat.update(cx, |this, cx| {
            for line in 0..60 {
                this.append_stream_reasoning(
                    0,
                    "inactive-reasoning".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    redraw(cx);
    redraw(cx);
    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the reasoning card must be visible");

    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Lines(point(0., 3.)),
        ..Default::default()
    });
    assert!(
        cx.update(|_, cx| {
            let turn = chat.read(cx).messages.last().expect("assistant turn");
            reasoning_part(turn)
                .expect("reasoning trace")
                .smooth_scroll_remaining()
        }) > px(0.),
        "active card wheel input must queue easing before the focus transition"
    );

    cx.deactivate_window();
    assert!(!cx.update(|window, _| window.is_window_active()));
    cx.update(|window, cx| {
        assert!(window.simulate_next_frame(cx) > 0);
    });
    assert_eq!(
        cx.update(|_, cx| {
            let turn = chat.read(cx).messages.last().expect("assistant turn");
            reasoning_part(turn)
                .expect("reasoning trace")
                .smooth_scroll_remaining()
        }),
        px(0.),
        "a card frame delivered after deactivation must cancel pending easing"
    );

    let before_inactive = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        reasoning_part(turn)
            .expect("reasoning trace")
            .scroll_offset()
            .y
    });
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Lines(point(0., 3.)),
        ..Default::default()
    });

    let after_inactive = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        reasoning_part(turn)
            .expect("reasoning trace")
            .scroll_offset()
            .y
    });
    assert!(
        after_inactive > before_inactive,
        "inactive wheel input must still move the native reasoning viewport"
    );

    assert_eq!(
        cx.update(|_, cx| {
            let turn = chat.read(cx).messages.last().expect("assistant turn");
            reasoning_part(turn)
                .expect("reasoning trace")
                .smooth_scroll_remaining()
        }),
        px(0.),
        "inactive reasoning windows must not queue card animation"
    );
}
