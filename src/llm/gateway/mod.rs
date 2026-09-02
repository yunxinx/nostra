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
    ProtocolSession, ProviderCatalogSnapshot, ProviderMetadata, ReasoningContent, Role, ToolCall,
    Usage,
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
        catalog: &ProviderCatalogSnapshot,
        selection: &ModelSelection,
        mut request: crate::llm::GenerateRequest,
    ) -> Result<Generation, GatewayError> {
        // Resolve and encode before creating Generation: setup failures have no
        // stream lifecycle and therefore must not emit a terminal outcome.
        let (profile, model) = catalog.resolve_selection(selection)?;
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
mod tests;
