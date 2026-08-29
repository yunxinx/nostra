use super::*;
use crate::llm::{CompatibilityProfile, InMemoryMetrics};
use futures::{AsyncRead, future::BoxFuture};
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use std::{
    io,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
    task::{Context, Poll},
};

struct BlockingClient {
    frame: &'static [u8],
    abort: Mutex<Option<futures::future::AbortHandle>>,
    body_dropped: Arc<AtomicBool>,
    calls: AtomicUsize,
}

impl HttpClient for BlockingClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        _: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let reader = AbortWhileReading {
            frame: Some(self.frame),
            abort: self.abort.lock().expect("abort handle lock").take(),
            dropped: self.body_dropped.clone(),
        };
        Box::pin(async move {
            Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from_reader(reader))?)
        })
    }
}

struct AbortWhileReading {
    frame: Option<&'static [u8]>,
    abort: Option<futures::future::AbortHandle>,
    dropped: Arc<AtomicBool>,
}

impl AsyncRead for AbortWhileReading {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(frame) = self.frame.take() {
            buffer[..frame.len()].copy_from_slice(frame);
            return Poll::Ready(Ok(frame.len()));
        }
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        Poll::Pending
    }
}

impl Drop for AbortWhileReading {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}

fn assert_midstream_cancellation(protocol: Protocol, frame: &'static [u8]) {
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let body_dropped = Arc::new(AtomicBool::new(false));
    let client = Arc::new(BlockingClient {
        frame,
        abort: Mutex::new(Some(abort)),
        body_dropped: body_dropped.clone(),
        calls: AtomicUsize::new(0),
    });
    let metrics = Arc::new(InMemoryMetrics::new(8));
    let mut generation = Generation {
        transport: HttpTransport::new(client.clone()),
        request: TransportRequest {
            url: "https://example.com/v1/stream".into(),
            api_key: Default::default(),
            body: Vec::new(),
        },
        session: GatewaySession::new(
            RequestContext::new("p", "m", protocol),
            ProtocolSession::new(protocol, CompatibilityProfile::default()),
            Some(metrics.clone()),
        ),
    };

    let aborted = futures::executor::block_on(futures::future::Abortable::new(
        generation.run(|_| true),
        registration,
    ));
    assert!(aborted.is_err());
    assert!(body_dropped.load(Ordering::Relaxed));
    let outcome = match generation.cancel() {
        Some(GenerationEvent::Finished(outcome)) => outcome,
        _ => panic!("cancellation must finish the generation"),
    };
    assert_eq!(outcome.status, OutcomeStatus::Cancelled);
    assert!(outcome.error.is_none());
    assert!(outcome.message.as_ref().is_some_and(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(&part.block, ContentBlock::Text { text, .. } if text == "partial"))
    }));
    assert!(generation.cancel().is_none());
    drop(generation);

    assert_eq!(client.calls.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.recent().len(), 1);
    assert_eq!(metrics.recent()[0].status, OutcomeStatus::Cancelled);
}

#[test]
fn cancellation_is_exactly_once_even_on_drop() {
    let metrics = Arc::new(InMemoryMetrics::new(8));
    let mut session = GatewaySession::new(
        RequestContext::new("p", "m", Protocol::Responses),
        ProtocolSession::new(Protocol::Responses, CompatibilityProfile::default()),
        Some(metrics.clone()),
    );
    assert!(session.cancel().is_some());
    assert!(session.cancel().is_none());
    drop(session);
    assert_eq!(metrics.recent().len(), 1);
    assert_eq!(metrics.recent()[0].status, OutcomeStatus::Cancelled);
}

#[test]
fn cancellation_while_reading_closes_both_protocol_streams_once() {
    assert_midstream_cancellation(
        Protocol::ChatCompletions,
        b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
    );
    assert_midstream_cancellation(
        Protocol::Responses,
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n",
    );
}

#[test]
fn silent_eof_becomes_one_failed_outcome() {
    let metrics = Arc::new(InMemoryMetrics::new(8));
    let mut session = GatewaySession::new(
        RequestContext::new("p", "m", Protocol::ChatCompletions),
        ProtocolSession::new(Protocol::ChatCompletions, CompatibilityProfile::default()),
        Some(metrics.clone()),
    );
    assert!(session.finish_eof().is_some());
    assert!(session.finish_eof().is_none());
    assert_eq!(metrics.recent()[0].status, OutcomeStatus::Failed);
}

#[test]
fn chat_error_frame_reaches_live_outcome_and_observer_verbatim() {
    struct LiveObserver(Mutex<Option<String>>);

    impl OutcomeObserver for LiveObserver {
        fn on_finish(&self, outcome: &GenerationOutcome) {
            *self.0.lock().expect("observer lock") = outcome
                .error
                .as_ref()
                .and_then(GatewayError::upstream_body)
                .map(str::to_string);
        }
    }

    let frame = r#"{"error":{"message":"raw provider detail","code":"bad_request"}}"#;
    let observer = Arc::new(LiveObserver(Mutex::new(None)));
    let mut session = GatewaySession::new(
        RequestContext::new("p", "m", Protocol::ChatCompletions),
        ProtocolSession::new(Protocol::ChatCompletions, CompatibilityProfile::default()),
        Some(observer.clone()),
    );

    let outcome = session
        .ingest_sse_data(frame)
        .into_iter()
        .find_map(|event| match event {
            GenerationEvent::Finished(outcome) => Some(outcome),
            _ => None,
        })
        .expect("failed outcome");

    assert_eq!(
        outcome.error.as_ref().and_then(GatewayError::upstream_body),
        Some(frame)
    );
    assert_eq!(
        observer.0.lock().expect("observer lock").as_deref(),
        Some(frame),
        "live observers intentionally receive the original response; storage observers must discard it"
    );
}

#[test]
fn responses_replay_metadata_survives_terminal_assembly() {
    let mut session = GatewaySession::new(
        RequestContext::new("p", "m", Protocol::Responses),
        ProtocolSession::new(Protocol::Responses, CompatibilityProfile::default()),
        None,
    );
    let mut reasoning_finish_count = 0;
    let mut snapshot_update_count = 0;
    for event in [
        r#"{"type":"response.created","response":{"id":"resp"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"reasoning-item"}}"#,
        r#"{"type":"response.reasoning_text.delta","output_index":0,"content_index":0,"delta":"streamed draft"}"#,
        r#"{"type":"response.reasoning_text.done","output_index":0,"content_index":0}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"message-item"}}"#,
        r#"{"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"answer"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"reasoning-item","encrypted_content":"opaque","summary":[{"type":"summary_text","text":"authoritative summary"}]}}"#,
    ] {
        let events = session.ingest_sse_data(event);
        reasoning_finish_count += events
            .iter()
            .filter(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
            .count();
        snapshot_update_count += events
            .iter()
            .filter(|event| matches!(event, GenerationEvent::ReasoningSnapshotUpdated { .. }))
            .count();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, GenerationEvent::Finished(_)))
        );
    }
    assert_eq!(reasoning_finish_count, 1);
    assert_eq!(snapshot_update_count, 1);
    let events =
        session.ingest_sse_data(r#"{"type":"response.completed","response":{"id":"resp","output":[{"type":"reasoning","id":"reasoning-item","encrypted_content":"opaque","summary":[{"type":"summary_text","text":"authoritative summary"}]},{"type":"message","id":"message-item","content":[{"type":"output_text","text":"answer"}]}]}}"#);
    let outcome = events
        .into_iter()
        .find_map(|event| match event {
            GenerationEvent::Finished(outcome) => Some(outcome),
            _ => None,
        })
        .expect("terminal outcome");
    let message = outcome.message.expect("assembled message").into_message();
    let metadata = message
        .provider_metadata
        .responses
        .expect("message metadata");
    assert_eq!(metadata.response_id.as_deref(), Some("resp"));
    let reasoning = message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Reasoning { reasoning } => Some(reasoning),
            _ => None,
        })
        .expect("reasoning content");
    assert_eq!(reasoning.display, "authoritative summary");
    let replay = reasoning
        .replay
        .as_ref()
        .and_then(|metadata| metadata.responses.as_ref())
        .expect("reasoning replay metadata");
    assert_eq!(replay.item_id.as_deref(), Some("reasoning-item"));
    assert_eq!(replay.encrypted_reasoning.as_deref(), Some("opaque"));
}

#[test]
fn responses_unsupported_output_keeps_the_later_canonical_index() {
    let mut session = GatewaySession::new(
        RequestContext::new("p", "m", Protocol::Responses),
        ProtocolSession::new(Protocol::Responses, CompatibilityProfile::default()),
        None,
    );
    for event in [
        r#"{"type":"response.created","response":{"id":"resp"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"future_output","id":"future"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"future_output","id":"future"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"reasoning-item"}}"#,
        r#"{"type":"response.reasoning_text.delta","output_index":1,"delta":"thought"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"reasoning-item","summary":[{"type":"summary_text","text":"thought"}]}}"#,
    ] {
        assert!(
            !session
                .ingest_sse_data(event)
                .iter()
                .any(|event| matches!(event, GenerationEvent::Finished(_)))
        );
    }

    let events = session.ingest_sse_data(
        r#"{"type":"response.completed","response":{"id":"resp","output":[{"type":"future_output","id":"future"},{"type":"reasoning","id":"reasoning-item","summary":[{"type":"summary_text","text":"thought"}]}]}}"#,
    );
    let message = events
        .into_iter()
        .find_map(|event| match event {
            GenerationEvent::Finished(outcome) => outcome.message,
            _ => None,
        })
        .expect("terminal message");

    assert!(matches!(
        message.content.as_slice(),
        [IndexedContentBlock {
            content_index: 1,
            block: ContentBlock::Reasoning { reasoning },
        }] if reasoning.display == "thought"
    ));
}

#[test]
fn responses_preserves_message_id_for_each_text_block() {
    let mut session = GatewaySession::new(
        RequestContext::new("p", "m", Protocol::Responses),
        ProtocolSession::new(Protocol::Responses, CompatibilityProfile::default()),
        None,
    );
    for event in [
        r#"{"type":"response.created","response":{"id":"resp"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"first"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"first"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_2"}}"#,
        r#"{"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"second"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_2","content":[{"type":"output_text","text":"second"}]}}"#,
    ] {
        session.ingest_sse_data(event);
    }
    let events =
        session.ingest_sse_data(r#"{"type":"response.completed","response":{"id":"resp"}}"#);
    let message = events
        .into_iter()
        .find_map(|event| match event {
            GenerationEvent::Finished(outcome) => outcome.message.map(IndexedMessage::into_message),
            _ => None,
        })
        .expect("assembled message");
    let message_ids = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text {
                provider_metadata, ..
            } => provider_metadata
                .responses
                .as_ref()
                .and_then(|metadata| metadata.message_id.as_deref()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(message_ids, vec!["msg_1", "msg_2"]);
}

#[test]
fn assembler_preserves_started_block_order() {
    let mut assembler = MessageAssembler::default();
    assembler
        .observe(&GenerationEvent::TextStarted {
            content_index: 0,
            id: "t".into(),
        })
        .expect("text start");
    assembler
        .observe(&GenerationEvent::TextDelta {
            content_index: 0,
            id: "t".into(),
            delta: "answer".into(),
        })
        .expect("text delta");
    assembler
        .observe(&GenerationEvent::ReasoningStarted {
            content_index: 1,
            id: "r".into(),
        })
        .expect("reasoning start");
    assembler
        .observe(&GenerationEvent::ReasoningDelta {
            content_index: 1,
            id: "r".into(),
            delta: "later".into(),
        })
        .expect("reasoning delta");
    let message = assembler.message();
    assert!(matches!(
        message.content[0].block,
        ContentBlock::Text { .. }
    ));
    assert!(matches!(
        message.content[1].block,
        ContentBlock::Reasoning { .. }
    ));
    assert_eq!(
        message
            .content
            .iter()
            .map(|part| part.content_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn assembler_orders_terminal_backfill_by_content_index() {
    let mut assembler = MessageAssembler::default();
    for event in [
        GenerationEvent::TextStarted {
            content_index: 1,
            id: "text".into(),
        },
        GenerationEvent::TextDelta {
            content_index: 1,
            id: "text".into(),
            delta: "answer".into(),
        },
        GenerationEvent::ReasoningStarted {
            content_index: 0,
            id: "reasoning".into(),
        },
        GenerationEvent::ReasoningDelta {
            content_index: 0,
            id: "reasoning".into(),
            delta: "first".into(),
        },
    ] {
        assembler.observe(&event).expect("valid event");
    }
    let message = assembler.message();
    assert_eq!(
        message
            .content
            .iter()
            .map(|part| part.content_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(matches!(
        message.content.as_slice(),
        [
            IndexedContentBlock {
                block: ContentBlock::Reasoning { .. },
                ..
            },
            IndexedContentBlock {
                block: ContentBlock::Text { .. },
                ..
            }
        ]
    ));
}

#[test]
fn assembler_rejects_delta_after_block_finish() {
    let mut assembler = MessageAssembler::default();
    assembler
        .observe(&GenerationEvent::ReasoningStarted {
            content_index: 0,
            id: "reasoning".into(),
        })
        .expect("start");
    assembler
        .observe(&GenerationEvent::ReasoningFinished {
            content_index: 0,
            id: "reasoning".into(),
            replay: None,
        })
        .expect("finish");
    assert!(
        assembler
            .observe(&GenerationEvent::ReasoningDelta {
                content_index: 0,
                id: "reasoning".into(),
                delta: "late".into()
            })
            .is_err()
    );
}

#[test]
fn assembler_preserves_reasoning_around_a_tool_call() {
    let mut assembler = MessageAssembler::default();
    let tool_call = ToolCall {
        id: "call-0".into(),
        name: "lookup".into(),
        arguments: serde_json::json!({}),
        raw_arguments: "{}".into(),
        provider_metadata: ProviderMetadata::default(),
    };
    let events = [
        GenerationEvent::ReasoningStarted {
            content_index: 0,
            id: "reasoning-0".into(),
        },
        GenerationEvent::ReasoningDelta {
            content_index: 0,
            id: "reasoning-0".into(),
            delta: "before".into(),
        },
        GenerationEvent::ToolCallStarted {
            content_index: 1,
            index: 0,
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
        },
        GenerationEvent::ReasoningStarted {
            content_index: 2,
            id: "reasoning-1".into(),
        },
        GenerationEvent::ReasoningDelta {
            content_index: 2,
            id: "reasoning-1".into(),
            delta: "after".into(),
        },
        GenerationEvent::ToolCallFinished {
            content_index: 1,
            index: 0,
            tool_call: Box::new(tool_call.clone()),
        },
    ];
    for event in &events {
        assembler.observe(event).expect("valid event");
    }

    let message = assembler.message();
    assert!(matches!(
        message.content.as_slice(),
        [
            IndexedContentBlock { block: ContentBlock::Reasoning { reasoning: first }, .. },
            IndexedContentBlock { block: ContentBlock::ToolCall { tool_call: middle }, .. },
            IndexedContentBlock { block: ContentBlock::Reasoning { reasoning: second }, .. },
        ] if first.display == "before"
            && middle == &tool_call
            && second.display == "after"
    ));
    assert_eq!(
        message
            .content
            .iter()
            .map(|part| part.content_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn request_context_retains_conversation_and_turn_ids() {
    let context =
        RequestContext::with_correlation("p", "m", Protocol::Responses, "conversation-1", "turn-2");
    assert_eq!(context.conversation_id, "conversation-1");
    assert_eq!(context.turn_id, "turn-2");
}
