//! In-process generation gateway: routing, canonical assembly, and lifecycle observation.
//!
//! The gateway resolves a stable profile/model selection, delegates wire details
//! to a protocol session and transport, then guarantees one terminal outcome for
//! every prepared generation, including cancellation and drop.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use crate::llm::{
    ContentBlock, FinishReason, GatewayError, GenerationEvent, GenerationOutcome,
    IndexedContentBlock, IndexedMessage, ModelSelection, OutcomeObserver, OutcomeStatus, Protocol,
    ProtocolSession, ProviderMetadata, ProviderProfile, ReasoningContent, Role, ToolCall, Usage,
    transport::{HttpTransport, TransportEvent, TransportRequest},
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub struct Gateway {
    transport: HttpTransport,
    observer: Option<Arc<dyn OutcomeObserver>>,
}

impl Gateway {
    pub fn new(transport: HttpTransport, observer: Option<Arc<dyn OutcomeObserver>>) -> Self {
        Self {
            transport,
            observer,
        }
    }

    pub fn prepare(
        &self,
        profiles: &[ProviderProfile],
        selection: &ModelSelection,
        mut request: crate::llm::GenerateRequest,
    ) -> Result<Generation, GatewayError> {
        // Resolve and encode before creating Generation: setup failures have no
        // stream lifecycle and therefore must not emit a terminal outcome.
        let (profile, model) = crate::llm::resolve_selection(profiles, selection)?;
        request.model = model.model_id.trim().to_string();

        let protocol = ProtocolSession::new(profile.protocol, profile.compatibility.clone());
        let body = serde_json::to_vec(&protocol.encode_request(&request)?)
            .map_err(|_| GatewayError::protocol("failed to encode provider request"))?;
        let base_url = profile.validated_base_url()?;
        let transport_request = TransportRequest {
            url: format!("{base_url}{}", profile.protocol.endpoint_path()),
            api_key: profile.api_key.clone(),
            body,
        };
        let context = RequestContext::with_correlation(
            profile.id.clone(),
            model.id.clone(),
            profile.protocol,
            request.conversation_id,
            request.turn_id,
        );
        Ok(Generation {
            transport: self.transport.clone(),
            request: transport_request,
            session: GatewaySession::new(context, protocol, self.observer.clone()),
        })
    }
}

pub struct Generation {
    transport: HttpTransport,
    request: TransportRequest,
    session: GatewaySession,
}

impl Generation {
    pub async fn run(&mut self, mut on_event: impl FnMut(GenerationEvent) -> bool) {
        let transport = self.transport.clone();
        let request = &self.request;
        let mut consumer_stopped = false;
        let result = transport
            .stream(request, |event| match event {
                TransportEvent::Attempt(attempt) => {
                    self.session.set_attempt(attempt);
                    true
                }
                TransportEvent::UpstreamResponse(status) => {
                    self.session.observe_upstream_response(status);
                    true
                }
                TransportEvent::SseData(data) => {
                    let keep_going = self
                        .session
                        .ingest_sse_data(&data)
                        .into_iter()
                        .all(&mut on_event);
                    consumer_stopped = !keep_going;
                    keep_going
                }
            })
            .await;

        if self.session.is_finished() {
            return;
        }
        // Stopping the consumer is cancellation, while a transport EOF/error is
        // finalized by the protocol session. Both paths converge on one outcome.
        if consumer_stopped {
            self.session.cancel();
            return;
        }
        let terminal = match result {
            Ok(()) => self.session.finish_eof(),
            Err(error) => self.session.fail_with(error),
        };
        if let Some(event) = terminal {
            on_event(event);
        }
    }

    pub fn cancel(&mut self) -> Option<GenerationEvent> {
        self.session.cancel()
    }
}

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub request_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub profile_id: String,
    pub model_id: String,
    pub protocol: Protocol,
    pub attempt: u32,
    pub started_at: Instant,
}

impl RequestContext {
    pub fn new(
        profile_id: impl Into<String>,
        model_id: impl Into<String>,
        protocol: Protocol,
    ) -> Self {
        Self::with_correlation(profile_id, model_id, protocol, String::new(), String::new())
    }

    pub fn with_correlation(
        profile_id: impl Into<String>,
        model_id: impl Into<String>,
        protocol: Protocol,
        conversation_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        let sequence = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            request_id: format!("nostra-{sequence}"),
            conversation_id: conversation_id.into(),
            turn_id: turn_id.into(),
            profile_id: profile_id.into(),
            model_id: model_id.into(),
            protocol,
            attempt: 1,
            started_at: Instant::now(),
        }
    }
}

enum AssembledBlock {
    Text {
        id: String,
        text: String,
        replay: Option<crate::llm::ReplayMetadata>,
        finished: bool,
    },
    Reasoning {
        id: String,
        display: String,
        replay: Option<crate::llm::ReplayMetadata>,
        finished: bool,
    },
    Tool {
        index: usize,
        tool_call: Option<ToolCall>,
    },
}

#[derive(Default)]
struct MessageAssembler {
    // The protocol-owned content index is stable across live events and
    // terminal backfill, so a missing earlier Responses output can be inserted
    // without changing canonical order.
    blocks: BTreeMap<usize, AssembledBlock>,
}

impl MessageAssembler {
    fn is_empty(&self) -> bool {
        !self.blocks.values().any(|block| match block {
            AssembledBlock::Text { text, .. } => !text.is_empty(),
            AssembledBlock::Reasoning {
                display, replay, ..
            } => !display.is_empty() || replay.is_some(),
            AssembledBlock::Tool { tool_call, .. } => tool_call.is_some(),
        })
    }

    fn observe(&mut self, event: &GenerationEvent) -> Result<(), GatewayError> {
        match event {
            GenerationEvent::TextStarted { content_index, id } => {
                self.start_text(*content_index, id)?;
            }
            GenerationEvent::TextDelta {
                content_index,
                id,
                delta,
            } => {
                let (text, _, finished) = self.text(*content_index, id)?;
                if *finished {
                    return Err(GatewayError::protocol(
                        "text delta arrived after content completion",
                    ));
                }
                text.push_str(delta);
            }
            GenerationEvent::TextFinished {
                content_index,
                id,
                replay,
            } => {
                let (_, current_replay, finished) = self.text(*content_index, id)?;
                *finished = true;
                if replay.is_some() {
                    *current_replay = replay.clone();
                }
            }
            GenerationEvent::ReasoningStarted { content_index, id } => {
                self.start_reasoning(*content_index, id)?;
            }
            GenerationEvent::ReasoningDelta {
                content_index,
                id,
                delta,
            } => {
                let (display, _, finished) = self.reasoning(*content_index, id)?;
                if *finished {
                    return Err(GatewayError::protocol(
                        "reasoning delta arrived after content completion",
                    ));
                }
                display.push_str(delta);
            }
            GenerationEvent::ReasoningFinished {
                content_index,
                id,
                replay,
            } => {
                let (_, current_replay, finished) = self.reasoning(*content_index, id)?;
                *finished = true;
                if replay.is_some() {
                    *current_replay = replay.clone();
                }
            }
            GenerationEvent::ReasoningSnapshotUpdated {
                content_index,
                id,
                reasoning,
            } => {
                let (display, replay, finished) = self.reasoning(*content_index, id)?;
                if !*finished {
                    return Err(GatewayError::protocol(
                        "reasoning snapshot arrived before content completion",
                    ));
                }
                display.clone_from(&reasoning.display);
                replay.clone_from(&reasoning.replay);
            }
            GenerationEvent::ToolCallStarted {
                content_index,
                index,
                ..
            } => match self.blocks.entry(*content_index) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(AssembledBlock::Tool {
                        index: *index,
                        tool_call: None,
                    });
                }
                std::collections::btree_map::Entry::Occupied(entry) if matches!(entry.get(), AssembledBlock::Tool { index: current, .. } if current == index) =>
                    {}
                std::collections::btree_map::Entry::Occupied(_) => {
                    return Err(GatewayError::protocol(
                        "content index was reused by a different block",
                    ));
                }
            },
            GenerationEvent::ToolCallFinished {
                content_index,
                index,
                tool_call,
            } => match self.blocks.get_mut(content_index) {
                Some(AssembledBlock::Tool {
                    index: current,
                    tool_call: slot,
                }) if current == index => {
                    *slot = Some((**tool_call).clone());
                }
                None => {
                    self.blocks.insert(
                        *content_index,
                        AssembledBlock::Tool {
                            index: *index,
                            tool_call: Some((**tool_call).clone()),
                        },
                    );
                }
                _ => {
                    return Err(GatewayError::protocol(
                        "tool completion did not match its content block",
                    ));
                }
            },
            _ => {}
        }
        Ok(())
    }

    fn start_text(&mut self, content_index: usize, id: &str) -> Result<(), GatewayError> {
        match self.blocks.entry(content_index) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(AssembledBlock::Text {
                    id: id.to_string(),
                    text: String::new(),
                    replay: None,
                    finished: false,
                });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(entry) if matches!(entry.get(), AssembledBlock::Text { id: current, .. } if current == id) => {
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => Err(GatewayError::protocol(
                "content index was reused by a different block",
            )),
        }
    }

    fn text(
        &mut self,
        content_index: usize,
        id: &str,
    ) -> Result<
        (
            &mut String,
            &mut Option<crate::llm::ReplayMetadata>,
            &mut bool,
        ),
        GatewayError,
    > {
        match self.blocks.get_mut(&content_index) {
            Some(AssembledBlock::Text {
                id: current,
                text,
                replay,
                finished,
            }) if current == id => Ok((text, replay, finished)),
            _ => Err(GatewayError::protocol(
                "text event did not match a started content block",
            )),
        }
    }

    fn start_reasoning(&mut self, content_index: usize, id: &str) -> Result<(), GatewayError> {
        match self.blocks.entry(content_index) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(AssembledBlock::Reasoning {
                    id: id.to_string(),
                    display: String::new(),
                    replay: None,
                    finished: false,
                });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(entry) if matches!(entry.get(), AssembledBlock::Reasoning { id: current, .. } if current == id) => {
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => Err(GatewayError::protocol(
                "content index was reused by a different block",
            )),
        }
    }

    fn reasoning(
        &mut self,
        content_index: usize,
        id: &str,
    ) -> Result<
        (
            &mut String,
            &mut Option<crate::llm::ReplayMetadata>,
            &mut bool,
        ),
        GatewayError,
    > {
        match self.blocks.get_mut(&content_index) {
            Some(AssembledBlock::Reasoning {
                id: current,
                display,
                replay,
                finished,
            }) if current == id => Ok((display, replay, finished)),
            _ => Err(GatewayError::protocol(
                "reasoning event did not match a started content block",
            )),
        }
    }

    fn message(&self) -> IndexedMessage {
        let content = self
            .blocks
            .iter()
            .filter_map(|(content_index, block)| match block {
                AssembledBlock::Text { text, replay, .. } if !text.is_empty() => {
                    Some(IndexedContentBlock {
                        content_index: *content_index,
                        block: ContentBlock::Text {
                            text: text.clone(),
                            provider_metadata: replay.clone().unwrap_or_default(),
                        },
                    })
                }
                AssembledBlock::Reasoning {
                    display, replay, ..
                } if !display.is_empty() || replay.is_some() => Some(IndexedContentBlock {
                    content_index: *content_index,
                    block: ContentBlock::Reasoning {
                        reasoning: ReasoningContent {
                            display: display.clone(),
                            replay: replay.clone(),
                        },
                    },
                }),
                AssembledBlock::Tool {
                    tool_call: Some(tool_call),
                    ..
                } => Some(IndexedContentBlock {
                    content_index: *content_index,
                    block: ContentBlock::ToolCall {
                        tool_call: tool_call.clone(),
                    },
                }),
                _ => None,
            })
            .collect();
        IndexedMessage {
            role: Role::Assistant,
            content,
            provider_metadata: ProviderMetadata::default(),
        }
    }
}

struct GatewaySession {
    context: RequestContext,
    protocol: ProtocolSession,
    observer: Option<Arc<dyn OutcomeObserver>>,
    assembler: MessageAssembler,
    first_event_at: Option<Instant>,
    latest_usage: Usage,
    finished: bool,
}

#[derive(Default)]
struct OutcomeDetails {
    finish_reason: Option<FinishReason>,
    usage: Usage,
    response_id: Option<String>,
    upstream_model: Option<String>,
    provider_metadata: Option<ProviderMetadata>,
    error: Option<GatewayError>,
}

impl GatewaySession {
    pub fn new(
        context: RequestContext,
        protocol: ProtocolSession,
        observer: Option<Arc<dyn OutcomeObserver>>,
    ) -> Self {
        let this = Self {
            context,
            protocol,
            observer,
            assembler: MessageAssembler::default(),
            first_event_at: None,
            latest_usage: Usage::default(),
            finished: false,
        };
        if let Some(observer) = &this.observer {
            observer.on_start(&this.context);
        }
        this
    }

    pub fn ingest_sse_data(&mut self, data: &str) -> Vec<GenerationEvent> {
        if self.finished {
            return Vec::new();
        }
        match self.protocol.ingest_sse_data(data) {
            Ok(update) => {
                let mut events = update.events;
                if !events.is_empty() && self.first_event_at.is_none() {
                    self.first_event_at = Some(Instant::now());
                }
                for event in &events {
                    if let GenerationEvent::UsageUpdated(usage) = event {
                        self.latest_usage = usage.clone();
                    }
                    if let Err(error) = self.assembler.observe(event) {
                        return vec![self.fail(error)];
                    }
                    if let Some(observer) = &self.observer {
                        observer.on_event(&self.context, event);
                    }
                }
                if let Some(terminal) = update.terminal {
                    events.push(self.protocol_terminal(terminal));
                }
                events
            }
            Err(error) => vec![self.fail(error)],
        }
    }

    pub fn finish_eof(&mut self) -> Option<GenerationEvent> {
        if self.finished {
            return None;
        }
        match self.protocol.finish_eof() {
            Ok(Some(terminal)) => Some(self.protocol_terminal(terminal)),
            Ok(None) => None,
            Err(error) => Some(self.fail(error)),
        }
    }

    pub fn cancel(&mut self) -> Option<GenerationEvent> {
        if self.finished {
            return None;
        }
        let outcome = self.outcome(
            OutcomeStatus::Cancelled,
            OutcomeDetails {
                usage: self.latest_usage.clone(),
                ..Default::default()
            },
        );
        Some(self.finish(outcome))
    }

    pub fn fail_with(&mut self, error: GatewayError) -> Option<GenerationEvent> {
        (!self.finished).then(|| self.fail(error))
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn observe_upstream_response(&self, status: u16) {
        if let Some(observer) = &self.observer {
            observer.on_upstream_response(&self.context, status);
        }
    }

    pub fn set_attempt(&mut self, attempt: u32) {
        self.context.attempt = attempt;
    }

    fn complete(
        &mut self,
        reason: FinishReason,
        usage: Usage,
        response_id: Option<String>,
        upstream_model: Option<String>,
        provider_metadata: ProviderMetadata,
    ) -> GenerationEvent {
        let outcome = self.outcome(
            OutcomeStatus::Completed,
            OutcomeDetails {
                finish_reason: Some(reason),
                usage,
                response_id,
                upstream_model,
                provider_metadata: Some(provider_metadata),
                error: None,
            },
        );
        self.finish(outcome)
    }

    fn protocol_terminal(
        &mut self,
        mut terminal: crate::llm::protocol::ProtocolTerminal,
    ) -> GenerationEvent {
        match terminal.status {
            crate::llm::protocol::ProtocolTerminalStatus::Completed => self.complete(
                terminal.finish_reason,
                terminal.usage,
                terminal.response_id,
                terminal.upstream_model,
                terminal.provider_metadata,
            ),
            crate::llm::protocol::ProtocolTerminalStatus::Failed => {
                if let Some(error) = &mut terminal.error {
                    error.request_id = Some(self.context.request_id.clone());
                    error.output_started = self.first_event_at.is_some();
                }
                let outcome = self.outcome(
                    OutcomeStatus::Failed,
                    OutcomeDetails {
                        finish_reason: Some(terminal.finish_reason),
                        usage: terminal.usage,
                        response_id: terminal.response_id,
                        upstream_model: terminal.upstream_model,
                        provider_metadata: Some(terminal.provider_metadata),
                        error: terminal.error,
                    },
                );
                self.finish(outcome)
            }
        }
    }

    fn fail(&mut self, mut error: GatewayError) -> GenerationEvent {
        error.request_id = Some(self.context.request_id.clone());
        error.output_started = self.first_event_at.is_some();
        let outcome = self.outcome(
            OutcomeStatus::Failed,
            OutcomeDetails {
                usage: self.latest_usage.clone(),
                error: Some(error),
                ..Default::default()
            },
        );
        self.finish(outcome)
    }

    fn outcome(&self, status: OutcomeStatus, details: OutcomeDetails) -> GenerationOutcome {
        let now = Instant::now();
        let mut message = (!self.assembler.is_empty()).then(|| self.assembler.message());
        if let (Some(message), Some(metadata)) = (&mut message, details.provider_metadata) {
            message.provider_metadata = metadata;
        }
        GenerationOutcome {
            request_id: self.context.request_id.clone(),
            profile_id: self.context.profile_id.clone(),
            model_id: self.context.model_id.clone(),
            protocol: self.context.protocol,
            status,
            finish_reason: details.finish_reason,
            usage: details.usage,
            response_id: details.response_id,
            upstream_model: details.upstream_model,
            time_to_first_event: self
                .first_event_at
                .map(|first| first.saturating_duration_since(self.context.started_at)),
            latency: now.saturating_duration_since(self.context.started_at),
            message,
            error: details.error,
        }
    }

    fn finish(&mut self, outcome: GenerationOutcome) -> GenerationEvent {
        // All normal, failed, cancelled, and Drop paths funnel through here so
        // observers and callers see the terminal outcome exactly once.
        debug_assert!(!self.finished, "terminal outcome must be emitted once");
        self.finished = true;
        // Observers receive the same live outcome as the UI, including captured
        // upstream text. This is intentional: the text must remain verbatim and
        // available to the error card. Storage implementations own the separate
        // no-persistence boundary (InMemoryMetrics strips it before enqueueing).
        if let Some(observer) = &self.observer {
            observer.on_finish(&outcome);
        }
        GenerationEvent::Finished(Box::new(outcome))
    }
}

impl Drop for GatewaySession {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

#[cfg(test)]
mod tests {
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
            message.content.iter().any(
                |part| matches!(&part.block, ContentBlock::Text { text, .. } if text == "partial"),
            )
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
                GenerationEvent::Finished(outcome) => {
                    outcome.message.map(IndexedMessage::into_message)
                }
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
        let context = RequestContext::with_correlation(
            "p",
            "m",
            Protocol::Responses,
            "conversation-1",
            "turn-2",
        );
        assert_eq!(context.conversation_id, "conversation-1");
        assert_eq!(context.turn_id, "turn-2");
    }
}
