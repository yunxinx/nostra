use super::super::*;

#[gpui::test]
fn separate_reasoning_rows_keep_independent_state(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "first", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "answer", cx);
            test_support::append_reasoning(this, 2, "reasoning-1".into(), "second", cx);
            test_support::finish_reasoning(this, 2, "reasoning-1", None, cx);

            assert_eq!(
                reasoning_states(this, cx),
                vec![("first", true), ("second", true)]
            );
            let reasoning_rows: Vec<RowId> = rows_of_kind(this, RowKind::Reasoning)
                .iter()
                .map(|row| row.id())
                .collect();
            let [first, _second] = reasoning_rows.as_slice() else {
                panic!("two reasoning cards");
            };
            assert!(toggle_reasoning_row_by_id(this, *first));
            let traces = reasoning_parts(this);
            assert!(traces[0].is_expanded());
            assert!(!traces[1].is_expanded());
        });
    });
}

/// Drive the real listeners for both cards. Stable element ids are not enough:
/// each closure must also resolve the same block identity when it reaches back
/// into `ChatView` for disclosure and clipboard content.
#[gpui::test]
fn separate_reasoning_rows_toggle_and_copy_independently(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "first", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "answer", cx);
            test_support::append_reasoning(this, 2, "reasoning-1".into(), "second", cx);
            test_support::finish_reasoning(this, 2, "reasoning-1", None, cx);
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
        let traces = reasoning_parts(this);
        assert!(traces[0].is_expanded());
        assert!(!traces[1].is_expanded());
    });

    let second_trigger = cx
        .debug_bounds("reasoning-trigger-2")
        .expect("second reasoning trigger");
    cx.simulate_click(second_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this);
        assert!(traces[0].is_expanded());
        assert!(traces[1].is_expanded());
    });

    let copy_and_read =
        |selector: &'static str, trigger: &'static str, cx: &mut gpui::VisualTestContext| {
            // Budgeted viewports are taller than the old seven-line cards;
            // bring the trigger row back into the window before interacting.
            chat.update(cx, |this, _| {
                this.view.list_state.scroll_to(ListOffset::default());
            });
            draw(cx);
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
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "first", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn, cx)[0].0, "first");
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "first"
        ));
    });
}

#[gpui::test]
fn replay_only_reasoning_is_closed_without_allocating_a_card(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
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
            test_support::start_reasoning(this, 0, "reasoning-0".into(), cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", Some(replay.clone()), cx);
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        assert!(
            reasoning_part(turn).is_none(),
            "no visible body was streamed"
        );
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
            [ContentBlock::Reasoning { reasoning }]
                if reasoning.display.is_empty() && reasoning.replay.as_ref() == Some(&replay)
        ));
    });
}

#[gpui::test]
fn terminal_snapshot_preserves_separate_reasoning_rows(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-0".into(), "partial first", cx);
            test_support::finish_reasoning(this, 0, "reasoning-0", None, cx);
            test_support::append_text(this, 1, "text-0".into(), "partial answer", cx);
            test_support::append_reasoning(this, 2, "reasoning-1".into(), "partial second", cx);
            let first = reasoning_part_mut(this).expect("first reasoning card");
            first.toggle_for_test();
            test_support::finish_reply(
                this,
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
        let turn = chat.read(cx);
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn, cx),
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
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    let (ui_id, body_id) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::start_tool_call(this, 0, 0, "call-0".into(), "lookup".into(), cx);
            test_support::append_reasoning(this, 1, "reasoning-0".into(), "partial", cx);
            test_support::finish_reasoning(this, 1, "reasoning-0", None, cx);
            let part_id = last_turn(this, cx)
                .parts
                .iter()
                .find(|part| part.content_index == 1)
                .map(|part| part.part_id)
                .expect("reasoning slot");
            let row_id = reasoning_row_for_part(this, part_id).expect("reasoning row");
            assert!(toggle_reasoning_row_by_id(this, row_id));
            let renderer = reasoning_renderer_by_part(this, part_id).expect("reasoning renderer");
            (
                part_id.as_u64(),
                renderer.body_entity_id().expect("reasoning body"),
            )
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(
                this,
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
        let this = chat.read(cx);
        assert_eq!(
            last_turn(this, cx).parts.len(),
            1,
            "unfinished tool placeholder was filtered"
        );
        let part = &last_turn(this, cx).parts[0];
        let PartSource::Reasoning { reasoning, .. } = &part.source else {
            panic!("terminal reasoning part")
        };
        let renderer = reasoning_renderer_by_part(this, part.part_id).expect("reasoning renderer");
        assert_eq!(part.part_id.as_u64(), ui_id);
        assert_eq!(renderer.body_entity_id().expect("reasoning body"), body_id);
        assert!(
            renderer.is_expanded(),
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
    let (chat, cx) = add_chat_window(cx);
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
            let part_id = last_turn(this, cx)
                .parts
                .iter()
                .find(|part| matches!(part.source, PartSource::Reasoning { .. }))
                .map(|part| part.part_id)
                .expect("reasoning part");
            let row_id = reasoning_row_for_part(this, part_id).expect("reasoning row");
            assert!(toggle_reasoning_row_by_id(this, row_id));
            let renderer = reasoning_renderer_by_part(this, part_id).expect("reasoning renderer");
            (
                part_id.as_u64(),
                renderer.body_entity_id().expect("reasoning body"),
                renderer.elapsed(),
            )
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
        let this = chat.read(cx);
        let part = &last_turn(this, cx).parts[0];
        let PartSource::Reasoning { reasoning, .. } = &part.source else {
            panic!("reasoning part")
        };
        let renderer = reasoning_renderer_by_part(this, part.part_id).expect("reasoning renderer");
        assert_eq!(part.part_id.as_u64(), ui_id);
        assert_eq!(renderer.body_entity_id().expect("reasoning body"), body_id);
        assert_eq!(renderer.elapsed(), elapsed);
        assert!(renderer.is_expanded() && part.finished);
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
    let (chat, cx) = add_chat_window(cx);
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
        let turn = chat.read(cx);
        assert_eq!(reasoning_parts(turn).len(), 2);
        assert!(matches!(
            last_llm(turn, cx).content.as_slice(),
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
