use super::*;
use crate::llm::{ToolCall, UsageProvenance};

#[test]
fn completed_is_the_only_success_terminal() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{"id":"resp","model":"gpt"}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item","call_id":"call","name":"weather","arguments":""}}"#).expect("item");
    session
        .ingest(
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}"#,
        )
        .expect("delta");
    let arguments_done = session
        .ingest(
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{}"}"#,
        )
        .expect("done");
    assert!(
        !arguments_done
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::ToolCallFinished { .. }))
    );
    let item_done = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"item","call_id":"call","name":"weather","arguments":"{}"}}"#).expect("item done");
    assert!(
        item_done
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::ToolCallFinished { .. }))
    );
    let terminal = session.ingest(r#"{"type":"response.completed","response":{"id":"resp","model":"gpt","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#).expect("completed").terminal.expect("terminal");
    assert_eq!(terminal.usage.total_tokens, 3);
    assert_eq!(terminal.response_id.as_deref(), Some("resp"));
}

#[test]
fn null_usage_remains_unavailable() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{"id":"resp"}}"#)
        .expect("created");
    let update = session
        .ingest(r#"{"type":"response.completed","response":{"id":"resp","usage":null}}"#)
        .expect("completed");
    assert!(
        !update
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::UsageUpdated(_)))
    );
    let terminal = update.terminal.expect("terminal");
    assert_eq!(terminal.usage.provenance, UsageProvenance::Unavailable);
}

#[test]
fn eof_without_completed_fails() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{"id":"resp"}}"#)
        .expect("created");
    assert!(session.finish_eof().is_err());
}

#[test]
fn provider_failure_keeps_safe_fields_clean_and_debug_free_of_upstream_text() {
    let error = response_failure(&json!({
        "error": {"message": "echoed secret-key", "code": "bad_request"}
    }))
    .with_upstream_body(r#"{"error":{"message":"echoed secret-key"}}"#);
    assert_eq!(error.safe_message(), "provider response failed");
    assert_eq!(error.provider_code.as_deref(), Some("bad_request"));
    assert!(!format!("{error:?}").contains("secret-key"));
    assert_eq!(
        error.upstream_body(),
        Some(r#"{"error":{"message":"echoed secret-key"}}"#)
    );
}

#[test]
fn failed_terminal_carries_the_raw_frame_to_the_outcome() {
    let frame = r#"{"type":"response.failed","response":{"id":"resp_1","error":{"code":"server_error","message":"upstream exploded"}}}"#;
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{"id":"resp_1"}}"#)
        .expect("created");
    let terminal = session
        .ingest(frame)
        .expect("failed terminal is not an Err")
        .terminal
        .expect("terminal present");
    let error = terminal.error.expect("terminal error");
    assert_eq!(error.upstream_body(), Some(frame));
}

#[test]
fn rejects_output_before_created() {
    let mut output = ResponsesSession::new(CompatibilityProfile::default());
    assert!(
        output
            .ingest(r#"{"type":"response.output_text.delta","output_index":0,"delta":"x"}"#)
            .is_err()
    );
}

#[test]
fn terminal_output_backfills_text_reasoning_and_replay_ids() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"},{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"first"}]},{"type":"message","id":"msg_2","content":[{"type":"output_text","text":"second"}]}]}}"#)
        .expect("completed");
    assert!(matches!(
        update.events.first(),
        Some(GenerationEvent::Started(_))
    ));
    let text_message_ids = update
        .events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::TextFinished {
                replay: Some(metadata),
                ..
            } => metadata
                .responses
                .as_ref()
                .and_then(|metadata| metadata.message_id.as_deref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(text_message_ids, vec!["msg_1", "msg_2"]);
    assert!(update.events.iter().any(|event| matches!(event, GenerationEvent::ReasoningFinished { replay: Some(metadata), .. } if metadata.responses.as_ref().and_then(|metadata| metadata.encrypted_reasoning.as_deref()) == Some("opaque"))));
    let terminal = update.terminal.expect("terminal");
    let metadata = terminal.provider_metadata.responses.expect("metadata");
    assert_eq!(metadata.response_id.as_deref(), Some("resp_1"));
}

#[test]
fn message_content_parts_round_trip_as_one_replay_item() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(
            r#"{"type":"response.completed","response":{"output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"first"},{"type":"refusal","refusal":"second"}]}]}}"#,
        )
        .expect("completed");

    let text = update
        .events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();
    let replays = update
        .events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::TextFinished {
                replay: Some(metadata),
                ..
            } => Some(metadata.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(text, "firstsecond");
    assert_eq!(replays.len(), 1);

    let message = crate::llm::Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text,
            provider_metadata: replays[0].clone(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let replayed = encode_message_items(&message, 0);
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0]["id"], "msg_1");
    assert_eq!(replayed[0]["content"][0]["text"], "firstsecond");
}

#[test]
fn rejects_malformed_known_message_content_parts() {
    for item in [
        json!({"type": "message", "id": "msg_1"}),
        json!({"type": "message", "id": "msg_1", "content": null}),
        json!({"type": "message", "id": "msg_1", "content": [
            {"type": "output_text"}
        ]}),
        json!({"type": "message", "id": "msg_1", "content": [
            {"type": "refusal", "refusal": 1}
        ]}),
    ] {
        let mut session = ResponsesSession::new(CompatibilityProfile::default());
        let event = json!({
            "type": "response.completed",
            "response": {"output": [item]},
        });
        assert!(session.ingest(&event.to_string()).is_err());
    }
}

#[test]
fn ignores_unknown_message_content_parts() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(
            r#"{"type":"response.completed","response":{"output":[{"type":"message","id":"msg_1","content":[{"type":"future_content","value":"opaque"}]}]}}"#,
        )
        .expect("unknown content remains forward-compatible");
    assert!(update.terminal.is_some());
}

#[test]
fn next_output_finishes_reasoning_before_text_and_late_replay_metadata() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");
    session.ingest(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"content_index":0,"delta":"think"}"#).expect("delta");
    let done = session
        .ingest(
            r#"{"type":"response.reasoning_summary_text.done","output_index":0,"content_index":0}"#,
        )
        .expect("done");
    assert!(
        !done
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
    );
    let message = session.ingest(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#).expect("message item");
    assert!(message.events.iter().any(|event| matches!(event, GenerationEvent::ReasoningFinished { replay: Some(metadata), .. } if metadata.responses.as_ref().and_then(|metadata| metadata.item_id.as_deref()) == Some("rs_1"))));
    let text = session.ingest(r#"{"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"answer"}"#).expect("text");
    assert!(matches!(
        text.events.as_slice(),
        [
            GenerationEvent::TextStarted { content_index: 1, .. },
            GenerationEvent::TextDelta { content_index: 1, delta, .. }
        ] if delta == "answer"
    ));
    let item = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}}"#).expect("item done");
    assert!(item.events.iter().any(|event| matches!(event, GenerationEvent::ReasoningSnapshotUpdated { reasoning, .. } if reasoning.replay.as_ref().and_then(|metadata| metadata.responses.as_ref()).and_then(|metadata| metadata.encrypted_reasoning.as_deref()) == Some("opaque"))));
    assert!(
        !item
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
    );
    let completed = session.ingest(r#"{"type":"response.completed","response":{"output":[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}]}}"#).expect("completed");
    assert!(
        !completed
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
    );
}

#[test]
fn late_reasoning_item_done_does_not_finish_the_new_reasoning_block() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_0"}}"#).expect("first item");
    session
        .ingest(r#"{"type":"response.reasoning_text.delta","output_index":0,"delta":"first"}"#)
        .expect("first delta");

    let second = session.ingest(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("second item");
    assert!(second.events.iter().any(|event| matches!(
        event,
        GenerationEvent::ReasoningFinished {
            content_index: 0,
            ..
        }
    )));
    session
        .ingest(r#"{"type":"response.reasoning_text.delta","output_index":1,"delta":"second"}"#)
        .expect("second delta");

    let first_done = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_0","encrypted_content":"opaque"}}"#).expect("late first done");
    assert!(!first_done.events.iter().any(|event| matches!(
        event,
        GenerationEvent::ReasoningFinished {
            content_index: 1,
            ..
        }
    )));

    let second_done = session.ingest(r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("second done");
    assert!(second_done.events.iter().any(|event| matches!(
        event,
        GenerationEvent::ReasoningFinished {
            content_index: 1,
            ..
        }
    )));
}

#[test]
fn incomplete_preserves_usage_and_response_metadata() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    let terminal = session.ingest(r#"{"type":"response.incomplete","response":{"id":"resp_1","model":"gpt","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":12,"output_tokens":4,"total_tokens":16,"input_tokens_details":{"cached_tokens":2}}}}"#).expect("incomplete").terminal.expect("terminal");
    assert_eq!(
        terminal.status,
        crate::llm::protocol::ProtocolTerminalStatus::Failed
    );
    assert_eq!(terminal.usage.input_tokens, 10);
    assert_eq!(terminal.usage.cache_read_tokens, 2);
    assert_eq!(terminal.response_id.as_deref(), Some("resp_1"));
    assert_eq!(terminal.upstream_model.as_deref(), Some("gpt"));
}

#[test]
fn failed_terminal_does_not_require_created() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(
            r#"{"type":"response.failed","response":{"id":"resp_1","error":{"code":"server_error"}}}"#,
        )
        .expect("failed terminal");
    assert!(matches!(
        update.events.first(),
        Some(GenerationEvent::Started(_))
    ));
    assert_eq!(
        update.terminal.expect("terminal").status,
        crate::llm::protocol::ProtocolTerminalStatus::Failed
    );
}

#[test]
fn rejects_delta_without_matching_added_output_item() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    assert!(
        session
            .ingest(
                r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"x"}"#
            )
            .is_err()
    );
}

#[test]
fn rejects_missing_or_non_string_structural_deltas() {
    let mut text = ResponsesSession::new(CompatibilityProfile::default());
    text.ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    text.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#)
        .expect("item");
    assert!(
        text.ingest(r#"{"type":"response.output_text.delta","output_index":0}"#)
            .is_err()
    );

    let mut reasoning = ResponsesSession::new(CompatibilityProfile::default());
    reasoning
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    reasoning.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");
    assert!(
        reasoning
            .ingest(
                r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":null}"#
            )
            .is_err()
    );
}

#[test]
fn assistant_history_preserves_canonical_block_order() {
    let message = crate::llm::Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Reasoning {
                reasoning: crate::llm::ReasoningContent {
                    display: "thought".into(),
                    replay: Some(ProviderMetadata {
                        chat: None,
                        responses: Some(ResponsesReplayMetadata {
                            item_id: Some("rs_1".into()),
                            encrypted_reasoning: Some("opaque".into()),
                            ..Default::default()
                        }),
                    }),
                },
            },
            ContentBlock::Text {
                text: "answer".into(),
                provider_metadata: ProviderMetadata {
                    chat: None,
                    responses: Some(ResponsesReplayMetadata {
                        message_id: Some("msg_1".into()),
                        ..Default::default()
                    }),
                },
            },
            ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "call_1".into(),
                    name: "weather".into(),
                    arguments: json!({}),
                    raw_arguments: "{}".into(),
                    provider_metadata: ProviderMetadata::default(),
                },
            },
        ],
        provider_metadata: ProviderMetadata::default(),
    };

    let items = encode_message_items(&message, 0);
    assert_eq!(
        items
            .iter()
            .filter_map(|item| item.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        vec!["reasoning", "message", "function_call"]
    );
    assert_eq!(items[1]["id"], "msg_1");
}

#[test]
fn assistant_history_generates_an_id_for_every_text_item() {
    let message = crate::llm::Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "first".into(),
                provider_metadata: ProviderMetadata::default(),
            },
            ContentBlock::Reasoning {
                reasoning: crate::llm::ReasoningContent {
                    display: "thought".into(),
                    replay: None,
                },
            },
            ContentBlock::Text {
                text: "second".into(),
                provider_metadata: ProviderMetadata::default(),
            },
        ],
        provider_metadata: ProviderMetadata {
            chat: None,
            responses: Some(ResponsesReplayMetadata {
                message_id: Some("message_level_id_is_not_a_text_signature".into()),
                ..Default::default()
            }),
        },
    };

    let items = encode_message_items(&message, 3);
    assert_eq!(items[0]["id"], "msg_nostra_3");
    assert_eq!(items[1]["id"], "msg_nostra_3_1");
}

#[test]
fn assistant_history_preserves_each_text_items_message_id() {
    let text = |value: &str, message_id: &str| ContentBlock::Text {
        text: value.into(),
        provider_metadata: ProviderMetadata {
            chat: None,
            responses: Some(ResponsesReplayMetadata {
                message_id: Some(message_id.into()),
                ..Default::default()
            }),
        },
    };
    let message = crate::llm::Message {
        role: Role::Assistant,
        content: vec![text("first", "msg_1"), text("second", "msg_2")],
        provider_metadata: ProviderMetadata::default(),
    };

    let items = encode_message_items(&message, 0);

    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "msg_1");
    assert_eq!(items[1]["id"], "msg_2");
}

#[test]
fn handles_reasoning_summary_parts_and_authoritative_summary() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");
    session
        .ingest(
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"first"}"#,
        )
        .expect("first");
    let text_done = session
        .ingest(r#"{"type":"response.reasoning_summary_text.done","output_index":0}"#)
        .expect("summary text done");
    assert!(
        !text_done
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
    );
    let separator = session
        .ingest(r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#)
        .expect("part");
    assert!(separator.events.iter().any(
        |event| matches!(event, GenerationEvent::ReasoningDelta { delta, .. } if delta == "\n\n")
    ));
    session
        .ingest(
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"second"}"#,
        )
        .expect("second");
    session
        .ingest(r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#)
        .expect("final part");
    let done = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"first"},{"type":"summary_text","text":"second"}]}}"#).expect("done");
    assert!(matches!(
        done.events.as_slice(),
        [
            GenerationEvent::ReasoningFinished { .. },
            GenerationEvent::ReasoningSnapshotUpdated { reasoning, .. }
        ] if reasoning.display == "first\n\nsecond"
    ));
}

#[test]
fn final_summary_part_separator_is_reconciled_by_authoritative_snapshot() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");
    session.ingest(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thought"}"#).expect("delta");
    session
        .ingest(r#"{"type":"response.reasoning_summary_text.done","output_index":0}"#)
        .expect("text done");
    session
        .ingest(r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#)
        .expect("part done");

    let done = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thought"}]}}"#).expect("item done");

    assert!(matches!(
        done.events.as_slice(),
        [
            GenerationEvent::ReasoningFinished { .. },
            GenerationEvent::ReasoningSnapshotUpdated { reasoning, .. }
        ] if reasoning.display == "thought"
    ));
}

#[test]
fn authoritative_reasoning_replaces_non_prefix_streamed_text() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");
    session
        .ingest(
            r#"{"type":"response.reasoning_text.delta","output_index":0,"delta":"streamed draft"}"#,
        )
        .expect("delta");

    let done = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"authoritative summary"}]}}"#).expect("item done");

    assert!(matches!(
        done.events.as_slice(),
        [
            GenerationEvent::ReasoningFinished { .. },
            GenerationEvent::ReasoningSnapshotUpdated { reasoning, .. }
        ] if reasoning.display == "authoritative summary"
    ));
}

#[test]
fn accepts_empty_reasoning_summary_part() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");

    let done = session
        .ingest(r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#)
        .expect("empty summary part");

    assert!(done.events.is_empty());
}

#[test]
fn done_only_reasoning_and_refusal_are_backfilled() {
    let mut reasoning = ResponsesSession::new(CompatibilityProfile::default());
    reasoning
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    let reasoning_done = reasoning.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thought"}]}}"#).expect("reasoning done");
    assert!(reasoning_done.events.iter().any(
        |event| matches!(event, GenerationEvent::ReasoningDelta { delta, .. } if delta == "thought")
    ));

    let mut refusal = ResponsesSession::new(CompatibilityProfile::default());
    refusal
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    let refusal_done = refusal.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"refusal","refusal":"cannot comply"}]}}"#).expect("refusal done");
    assert!(refusal_done.events.iter().any(|event| matches!(event, GenerationEvent::TextDelta { delta, .. } if delta == "cannot comply")));
}

#[test]
fn ignores_unknown_non_structural_events_but_rejects_structural_ones() {
    let mut session = ResponsesSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"type":"response.created","response":{}}"#)
        .expect("created");
    assert!(
        session
            .ingest(r#"{"type":"response.rate_limits.updated"}"#)
            .is_ok()
    );
    assert!(
        session
            .ingest(r#"{"type":"response.reasoning_unknown.delta","output_index":0}"#)
            .is_err()
    );
}
