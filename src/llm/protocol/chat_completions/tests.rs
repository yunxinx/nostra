use super::*;

#[test]
fn accepts_usage_only_and_interleaved_tool_calls() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    session.ingest(r#"{"id":"r","choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"two","arguments":"{"}},{"index":0,"id":"a","function":{"name":"one","arguments":"{}"}}]},"finish_reason":null}]}"#).expect("chunk");
    session.ingest(r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#).expect("finish");
    session
        .ingest(
            r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}"#,
        )
        .expect("usage");
    let terminal = session
        .ingest("[DONE]")
        .expect("done")
        .terminal
        .expect("terminal");
    assert_eq!(terminal.finish_reason, FinishReason::ToolCalls);
    assert_eq!(terminal.usage.total_tokens, 5);
}

#[test]
fn null_usage_remains_unavailable() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":null}"#)
        .expect("finish chunk");
    assert!(
        !update
            .events
            .iter()
            .any(|event| matches!(event, GenerationEvent::UsageUpdated(_)))
    );
    let terminal = session
        .ingest("[DONE]")
        .expect("done")
        .terminal
        .expect("terminal");
    assert_eq!(terminal.usage.provenance, UsageProvenance::Unavailable);
}

#[test]
fn done_without_finish_reason_is_not_success() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    assert!(
        session
            .ingest("[DONE]")
            .expect("done marker")
            .terminal
            .is_none()
    );
    assert!(session.finish_eof().is_err());
}

#[test]
fn finish_reason_allows_clean_eof_without_done_marker() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    session
        .ingest(r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#)
        .expect("finish chunk");
    assert!(session.finish_eof().expect("clean eof").is_some());
}

#[test]
fn provider_error_keeps_safe_fields_clean_and_debug_free_of_upstream_text() {
    let error = provider_error(&json!({
        "message": "request echoed secret-key",
        "code": "unsafe code: secret-key"
    }))
    .with_upstream_body(r#"{"error":{"message":"request echoed secret-key"}}"#);
    // The safe tier is unchanged: fixed message, code rejected by the allowlist.
    assert_eq!(error.safe_message(), "provider rejected the request");
    assert_eq!(error.provider_code, None);
    // Debug is what reaches logs, so it must not carry the body.
    assert!(!format!("{error:?}").contains("secret-key"));
    // The captured frame itself is retained for the UI to render.
    assert_eq!(
        error.upstream_body(),
        Some(r#"{"error":{"message":"request echoed secret-key"}}"#)
    );
}

#[test]
fn in_stream_error_frame_is_retained_as_captured() {
    let frame = r#"{"error":{"message":"Rate limit reached","code":"rate_limit_exceeded"}}"#;
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let error = session.ingest(frame).expect_err("provider error frame");
    assert_eq!(error.upstream_body(), Some(frame));
    assert_eq!(error.provider_code.as_deref(), Some("rate_limit_exceeded"));
}

#[test]
fn reasoning_details_round_trip_through_terminal_metadata() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(
            r#"{"choices":[{"delta":{"reasoning_content":"think","reasoning_details":[{"type":"opaque","data":"x"}]},"finish_reason":"stop"}]}"#,
        )
        .expect("finish chunk");
    let terminal = update
        .terminal
        .or_else(|| session.finish_eof().ok().flatten())
        .expect("terminal");
    let message = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Reasoning {
            reasoning: crate::llm::ReasoningContent {
                display: "think".into(),
                replay: Some(terminal.provider_metadata.clone()),
            },
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let encoded =
        encode_message(&message, &CompatibilityProfile::default()).expect("encode history");
    assert_eq!(encoded[0]["reasoning_details"][0]["data"], "x");
}

#[test]
fn reasoning_text_round_trips_through_the_actual_wire_field() {
    for (field, expected) in [
        ("reasoning_content", ChatReasoningField::ReasoningContent),
        ("reasoning", ChatReasoningField::Reasoning),
        ("reasoning_text", ChatReasoningField::ReasoningText),
    ] {
        let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
        let chunk = json!({
            "choices": [{
                "delta": {field: "think"},
                "finish_reason": "stop"
            }]
        });
        let update = session.ingest(&chunk.to_string()).expect("finish chunk");
        let replay = update
            .events
            .iter()
            .find_map(|event| match event {
                GenerationEvent::ReasoningFinished { replay, .. } => replay.clone(),
                _ => None,
            })
            .expect("reasoning replay metadata");
        assert_eq!(
            replay
                .chat
                .as_ref()
                .and_then(|metadata| metadata.reasoning_field),
            Some(expected)
        );

        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                reasoning: crate::llm::ReasoningContent {
                    display: "think".into(),
                    replay: Some(replay),
                },
            }],
            provider_metadata: ProviderMetadata::default(),
        };
        let encoded =
            encode_message(&message, &CompatibilityProfile::default()).expect("encode history");
        assert_eq!(encoded[0][field], "think");
    }
}

#[test]
fn auto_reasoning_uses_only_the_first_non_empty_alias() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(
            r#"{"choices":[{"delta":{"reasoning_content":"first","reasoning":"duplicate"},"finish_reason":"stop"}]}"#,
        )
        .expect("finish chunk");
    let deltas = update
        .events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::ReasoningDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas, vec!["first"]);
}

#[test]
fn content_type_transitions_create_new_ordered_blocks() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let chunks = [
        r#"{"choices":[{"delta":{"reasoning_content":"first"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"reasoning_content":"second"},"finish_reason":"stop"}]}"#,
    ];
    let events = chunks
        .into_iter()
        .flat_map(|chunk| session.ingest(chunk).expect("chunk").events)
        .collect::<Vec<_>>();

    assert!(matches!(
        events.as_slice(),
        [
            GenerationEvent::Started(_),
            GenerationEvent::ReasoningStarted { content_index: 0, id: first_start },
            GenerationEvent::ReasoningDelta { content_index: 0, id: first_delta, delta: first },
            GenerationEvent::ReasoningFinished { id: first_end, .. },
            GenerationEvent::TextStarted { content_index: 1, id: text_start },
            GenerationEvent::TextDelta { content_index: 1, id: text_delta, delta: text },
            GenerationEvent::TextFinished { id: text_end, .. },
            GenerationEvent::ReasoningStarted { content_index: 2, id: second_start },
            GenerationEvent::ReasoningDelta { content_index: 2, id: second_delta, delta: second },
            GenerationEvent::ReasoningFinished { id: second_end, .. },
        ] if first_start == "reasoning-0"
            && first_delta == first_start
            && first_end == first_start
            && first == "first"
            && text_start == "text-0"
            && text_delta == text_start
            && text_end == text_start
            && text == "answer"
            && second_start == "reasoning-1"
            && second_delta == second_start
            && second_end == second_start
            && second == "second"
    ));
}

#[test]
fn tool_calls_separate_reasoning_blocks() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let chunks = [
        r#"{"choices":[{"delta":{"reasoning_content":"before"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-0","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"reasoning_content":"after"},"finish_reason":"stop"}]}"#,
    ];
    let events = chunks
        .into_iter()
        .flat_map(|chunk| session.ingest(chunk).expect("chunk").events)
        .collect::<Vec<_>>();

    let structural = events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::ReasoningStarted { id, .. } => Some(("reasoning-start", id.as_str())),
            GenerationEvent::ReasoningFinished { id, .. } => {
                Some(("reasoning-finish", id.as_str()))
            }
            GenerationEvent::ToolCallStarted { id, .. } => Some(("tool-start", id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        structural,
        vec![
            ("reasoning-start", "reasoning-0"),
            ("reasoning-finish", "reasoning-0"),
            ("tool-start", "call-0"),
            ("reasoning-start", "reasoning-1"),
            ("reasoning-finish", "reasoning-1"),
        ]
    );

    assert!(events.iter().any(|event| matches!(
        event,
        GenerationEvent::ToolCallFinished { tool_call, .. }
            if tool_call.name == "lookup"
    )));
}

#[test]
fn missing_indices_use_stable_tool_ids_and_always_start_before_finish() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let first = session
        .ingest(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"one"}},{"id":"b","function":{"name":"two"}}]},"finish_reason":null}]}"#,
        )
        .expect("tool calls");
    assert!(matches!(
        first.events.as_slice(),
        [
            GenerationEvent::Started(_),
            GenerationEvent::ToolCallStarted { index: 0, .. },
            GenerationEvent::ToolCallStarted { index: 1, .. }
        ]
    ));
    let finished = session
        .ingest(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .expect("finish");
    let indices = finished
        .events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::ToolCallFinished { index, .. } => Some(*index),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(indices, vec![0, 1]);
}

#[test]
fn usage_separates_cached_and_written_input_tokens() {
    let usage = parse_chat_usage(&json!({
        "prompt_tokens": 20,
        "completion_tokens": 5,
        "total_tokens": 25,
        "prompt_tokens_details": {"cached_tokens": 7, "cache_write_tokens": 3}
    }));
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.cache_read_tokens, 7);
    assert_eq!(usage.cache_write_tokens, 3);
}

#[test]
fn replay_call_ids_are_normalized_consistently() {
    let call = ToolCall {
        id: "remote/id".into(),
        name: "f".into(),
        arguments: json!({}),
        raw_arguments: "{}".into(),
        provider_metadata: ProviderMetadata::default(),
    };
    let assistant = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolCall { tool_call: call }],
        provider_metadata: ProviderMetadata::default(),
    };
    let tool = Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_result: crate::llm::ToolResult {
                call_id: "remote/id".into(),
                content: "ok".into(),
                is_error: false,
            },
        }],
        provider_metadata: ProviderMetadata::default(),
    };
    let assistant = encode_message(&assistant, &CompatibilityProfile::default()).expect("call");
    let tool = encode_message(&tool, &CompatibilityProfile::default()).expect("result");
    assert_eq!(assistant[0]["tool_calls"][0]["id"], tool[0]["tool_call_id"]);
    assert_eq!(tool[0]["tool_call_id"], "call_remote_id");
}

#[test]
fn encodes_every_tool_result_as_a_separate_message() {
    let result = |call_id: &str, content: &str| ContentBlock::ToolResult {
        tool_result: crate::llm::ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        },
    };
    let message = Message {
        role: Role::Tool,
        content: vec![result("call_1", "first"), result("call_2", "second")],
        provider_metadata: ProviderMetadata::default(),
    };

    let encoded = encode_message(&message, &CompatibilityProfile::default()).expect("tool results");

    assert_eq!(encoded.len(), 2);
    assert_eq!(encoded[0]["tool_call_id"], "call_1");
    assert_eq!(encoded[0]["content"], "first");
    assert_eq!(encoded[1]["tool_call_id"], "call_2");
    assert_eq!(encoded[1]["content"], "second");
}

#[test]
fn consumes_only_the_first_choice() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    let update = session
        .ingest(r#"{"choices":[{"delta":{"content":"first"},"finish_reason":"stop"},{"delta":{"content":"second"},"finish_reason":"stop"}]}"#)
        .expect("chunk");
    let text = update
        .events
        .iter()
        .filter_map(|event| match event {
            GenerationEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "first");
}

#[test]
fn repeated_complete_tool_name_is_not_duplicated() {
    let mut session = ChatCompletionsSession::new(CompatibilityProfile::default());
    session.ingest(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"weather"}}]},"finish_reason":null}]}"#).expect("first");
    session.ingest(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"weather","arguments":"{}"}}]},"finish_reason":null}]}"#).expect("second");
    let update = session
        .ingest(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .expect("finish");
    assert!(update.events.iter().any(|event| matches!(event, GenerationEvent::ToolCallFinished { tool_call, .. } if tool_call.name == "weather")));
}
