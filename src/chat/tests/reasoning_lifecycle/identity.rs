use super::super::*;

#[gpui::test]
fn separate_reasoning_cards_keep_independent_state(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None, cx);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "second", cx);
            this.finish_stream_reasoning(2, "reasoning-1", None, cx);

            let turn = this.messages.last_mut().expect("assistant turn");
            assert_eq!(
                reasoning_states(turn),
                vec![("first", true), ("second", true)]
            );
            let reasoning_positions = turn
                .parts
                .iter()
                .enumerate()
                .filter_map(|(index, part)| {
                    matches!(part, MessagePart::Reasoning { .. }).then_some(index)
                })
                .collect::<Vec<_>>();
            let [first, second] = reasoning_positions.as_slice() else {
                panic!("two reasoning cards");
            };
            let (before_second, second_and_after) = turn.parts.split_at_mut(*second);
            let MessagePart::Reasoning {
                trace: Some(first_trace),
                ..
            } = &mut before_second[*first]
            else {
                panic!("first reasoning card");
            };
            let MessagePart::Reasoning {
                trace: Some(second_trace),
                ..
            } = &mut second_and_after[0]
            else {
                panic!("second reasoning card");
            };
            first_trace.toggle();
            assert!(first_trace.is_expanded());
            assert!(!second_trace.is_expanded());
        });
    });
}

/// Drive the real listeners for both cards. Stable element ids are not enough:
/// each closure must also resolve the same block identity when it reaches back
/// into `ChatView` for disclosure and clipboard content.
#[gpui::test]
fn separate_reasoning_cards_toggle_and_copy_independently(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None, cx);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "second", cx);
            this.finish_stream_reasoning(2, "reasoning-1", None, cx);
        });
    });

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };
    draw(cx);

    let first_trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("first reasoning trigger");
    cx.simulate_click(first_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this.messages.last().expect("assistant turn"));
        assert!(traces[0].is_expanded());
        assert!(!traces[1].is_expanded());
    });

    let second_trigger = cx
        .debug_bounds("reasoning-trigger-2")
        .expect("second reasoning trigger");
    cx.simulate_click(second_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this.messages.last().expect("assistant turn"));
        assert!(traces[0].is_expanded());
        assert!(traces[1].is_expanded());
    });

    let copy_and_read =
        |selector: &'static str, trigger: &'static str, cx: &mut gpui::VisualTestContext| {
            let trigger = cx.debug_bounds(trigger).expect("reasoning trigger");
            cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
            draw(cx);
            let copy = cx.debug_bounds(selector).expect("reasoning copy action");
            cx.simulate_click(copy.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .expect("reasoning copied")
        };

    assert_eq!(
        copy_and_read("reasoning-copy-0", "reasoning-trigger-0", cx),
        "first"
    );
    assert_eq!(
        copy_and_read("reasoning-copy-2", "reasoning-trigger-2", cx),
        "second"
    );
}

#[gpui::test]
fn a_finished_reasoning_id_cannot_be_reused(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None, cx);
            this.append_stream_reasoning(0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn)[0].0, "first");
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "first"
        ));
    });
}

#[gpui::test]
fn replay_only_reasoning_is_closed_without_allocating_a_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);
    let replay = ProviderMetadata {
        chat: Some(crate::llm::ChatReplayMetadata {
            reasoning_field: Some(crate::llm::ChatReasoningField::ReasoningContent),
            reasoning_details: None,
        }),
        responses: None,
    };

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_reasoning(0, "reasoning-0".into());
            this.finish_stream_reasoning(0, "reasoning-0", Some(replay.clone()), cx);
            this.append_stream_reasoning(0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(
            reasoning_part(turn).is_none(),
            "no visible body was streamed"
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }]
                if reasoning.display.is_empty() && reasoning.replay.as_ref() == Some(&replay)
        ));
    });
}

#[gpui::test]
fn terminal_snapshot_preserves_separate_reasoning_cards(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "partial first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None, cx);
            this.append_stream_text(1, "text-0".into(), "partial answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "partial second", cx);
            let first = this
                .messages
                .last_mut()
                .and_then(reasoning_part_mut)
                .expect("first reasoning card");
            first.toggle();
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "first".into(),
                                replay: None,
                            },
                        },
                        ContentBlock::Text {
                            text: "answer".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "second".into(),
                                replay: None,
                            },
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
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn),
            vec![("first", true), ("second", true)]
        );
        assert!(
            traces[0].is_expanded(),
            "terminal reconciliation preserves the first card's disclosure"
        );
        assert!(
            !traces[1].is_expanded(),
            "the second card retains its independent automatic disclosure"
        );
    });
}

/// Terminal canonical content may omit an unfinished tool placeholder. The
/// later reasoning block must retain its GPUI identity even though its vector
/// position changes when that placeholder disappears.
#[gpui::test]
fn terminal_filter_preserves_reasoning_identity_by_content_index(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let (ui_id, body_id) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_tool_call(0, 0, "call-0".into(), "lookup".into());
            this.append_stream_reasoning(1, "reasoning-0".into(), "partial", cx);
            this.finish_stream_reasoning(1, "reasoning-0", None, cx);
            let turn = this.messages.last_mut().expect("assistant turn");
            let MessagePart::Reasoning {
                ui_id,
                trace: Some(trace),
                ..
            } = turn
                .parts
                .iter_mut()
                .find(|part| part.content_index() == 1)
                .expect("reasoning slot")
            else {
                panic!("reasoning part")
            };
            trace.toggle();
            (*ui_id, trace.body_entity_id())
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![IndexedContentBlock {
                        content_index: 1,
                        block: ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "authoritative".into(),
                                replay: None,
                            },
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                }),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(
            turn.parts.len(),
            1,
            "unfinished tool placeholder was filtered"
        );
        let MessagePart::Reasoning {
            ui_id: current_ui_id,
            reasoning,
            trace: Some(trace),
            ..
        } = &turn.parts[0]
        else {
            panic!("terminal reasoning part")
        };
        assert_eq!(*current_ui_id, ui_id);
        assert_eq!(trace.body_entity_id(), body_id);
        assert!(
            trace.is_expanded(),
            "manual disclosure survives reconciliation"
        );
        assert_eq!(reasoning.display, "authoritative");
    });
}

/// A late Responses `output_item.done` updates the already-closed card in place.
/// It must not restart timing, reset disclosure, or replace the markdown entity.
#[gpui::test]
fn late_reasoning_snapshot_preserves_card_state_and_identity(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let (ui_id, body_id, elapsed) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                        delta: "streamed draft".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                        replay: None,
                    },
                ],
                cx,
            );
            let turn = this.messages.last_mut().expect("assistant turn");
            let MessagePart::Reasoning {
                ui_id,
                trace: Some(trace),
                ..
            } = &mut turn.parts[0]
            else {
                panic!("reasoning part")
            };
            trace.toggle();
            (*ui_id, trace.body_entity_id(), trace.elapsed())
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![crate::llm::GenerationEvent::ReasoningSnapshotUpdated {
                    content_index: 0,
                    id: "reasoning-0-0".into(),
                    reasoning: crate::llm::ReasoningContent {
                        display: "authoritative summary".into(),
                        replay: Some(ProviderMetadata {
                            chat: None,
                            responses: Some(ResponsesReplayMetadata {
                                item_id: Some("rs_1".into()),
                                encrypted_reasoning: Some("opaque".into()),
                                ..Default::default()
                            }),
                        }),
                    },
                }],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let MessagePart::Reasoning {
            ui_id: current_ui_id,
            reasoning,
            finished,
            trace: Some(trace),
            ..
        } = &turn.parts[0]
        else {
            panic!("reasoning part")
        };
        assert_eq!(*current_ui_id, ui_id);
        assert_eq!(trace.body_entity_id(), body_id);
        assert_eq!(trace.elapsed(), elapsed);
        assert!(trace.is_expanded() && *finished);
        assert_eq!(reasoning.display, "authoritative summary");
        assert_eq!(
            reasoning
                .replay
                .as_ref()
                .and_then(|metadata| metadata.responses.as_ref())
                .and_then(|metadata| metadata.encrypted_reasoning.as_deref()),
            Some("opaque")
        );
    });
}

#[gpui::test]
fn reasoning_after_a_tool_call_creates_a_second_ordered_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let tool_call = crate::llm::ToolCall {
        id: "call-0".into(),
        name: "lookup".into(),
        arguments: serde_json::json!({"query": "Nostra"}),
        raw_arguments: r#"{"query":"Nostra"}"#.into(),
        provider_metadata: ProviderMetadata::default(),
    };
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
                        delta: "before tool".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::ToolCallStarted {
                        content_index: 1,
                        index: 0,
                        id: "call-0".into(),
                        name: "lookup".into(),
                    },
                    crate::llm::GenerationEvent::ToolCallFinished {
                        content_index: 1,
                        index: 0,
                        tool_call: Box::new(tool_call.clone()),
                    },
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 2,
                        id: "reasoning-1".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 2,
                        id: "reasoning-1".into(),
                        delta: "after tool".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(reasoning_parts(turn).len(), 2);
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [
                ContentBlock::Reasoning { reasoning: first },
                ContentBlock::ToolCall { tool_call: middle },
                ContentBlock::Reasoning { reasoning: second },
            ] if first.display == "before tool"
                && middle == &tool_call
                && second.display == "after tool"
        ));
    });
}
