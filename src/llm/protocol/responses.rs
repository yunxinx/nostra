//! OpenAI-compatible Responses request encoder and typed event state machine.
//!
//! Output state is correlated by `output_index`. The adapter retains message,
//! reasoning, item, and call identifiers required for stateless history replay.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::llm::{
    ContentBlock, FinishReason, GatewayError, GenerateRequest, GenerationEvent, ProviderMetadata,
    ResponsesReplayMetadata, Role, StreamMetadata, ToolCall, Usage, UsageProvenance,
    error::{allowlisted_provider_code, allowlisted_provider_token},
};

use super::{
    CompatibilityProfile, ProtocolTerminal, ProtocolUpdate, ResponsesInstructionsPolicy,
    validate_messages,
};

#[derive(Default)]
struct FunctionAccumulator {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputKind {
    Message,
    Reasoning,
    FunctionCall,
    Unsupported,
}

pub(crate) struct ResponsesSession {
    compatibility: CompatibilityProfile,
    started: bool,
    terminal: bool,
    response_id: Option<String>,
    upstream_model: Option<String>,
    usage: Usage,
    functions: BTreeMap<usize, FunctionAccumulator>,
    finished_function_arguments: BTreeSet<usize>,
    finished_functions: BTreeSet<usize>,
    output_kinds: BTreeMap<usize, OutputKind>,
    finished_outputs: BTreeSet<usize>,
    open_text: BTreeMap<(usize, usize), String>,
    text_buffers: BTreeMap<(usize, usize), String>,
    finished_text: BTreeSet<(usize, usize)>,
    open_reasoning: BTreeMap<(usize, usize), String>,
    pending_reasoning_finish: BTreeMap<(usize, usize), String>,
    reasoning_buffers: BTreeMap<usize, String>,
    finished_reasoning: BTreeSet<usize>,
    reasoning_replay: BTreeMap<usize, ResponsesReplayMetadata>,
}

impl ResponsesSession {
    pub fn new(compatibility: CompatibilityProfile) -> Self {
        Self {
            compatibility,
            started: false,
            terminal: false,
            response_id: None,
            upstream_model: None,
            usage: Usage::default(),
            functions: BTreeMap::new(),
            finished_function_arguments: BTreeSet::new(),
            finished_functions: BTreeSet::new(),
            output_kinds: BTreeMap::new(),
            finished_outputs: BTreeSet::new(),
            open_text: BTreeMap::new(),
            text_buffers: BTreeMap::new(),
            finished_text: BTreeSet::new(),
            open_reasoning: BTreeMap::new(),
            pending_reasoning_finish: BTreeMap::new(),
            reasoning_buffers: BTreeMap::new(),
            finished_reasoning: BTreeSet::new(),
            reasoning_replay: BTreeMap::new(),
        }
    }

    pub fn encode_request(&self, request: &GenerateRequest) -> Result<Value, GatewayError> {
        if request.model.trim().is_empty() {
            return Err(GatewayError::protocol("model must not be empty"));
        }
        validate_messages(&request.messages)?;
        let mut input = Vec::new();
        let mut instructions = Vec::new();
        for (message_index, message) in request.messages.iter().enumerate() {
            if matches!(message.role, Role::System | Role::Developer)
                && self.compatibility.responses_instructions
                    == ResponsesInstructionsPolicy::TopLevel
            {
                instructions.extend(message.content.iter().filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }));
                continue;
            }
            input.extend(encode_message_items(message, message_index));
        }
        let mut body = Map::from_iter([
            ("model".into(), Value::String(request.model.clone())),
            ("input".into(), Value::Array(input)),
            ("stream".into(), Value::Bool(true)),
            (
                "store".into(),
                Value::Bool(self.compatibility.responses_store),
            ),
        ]);
        if self.compatibility.responses_include_encrypted_reasoning {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        if !instructions.is_empty() {
            body.insert(
                "instructions".into(),
                Value::String(instructions.join("\n")),
            );
        }
        if let Some(max) = request.max_output_tokens {
            body.insert("max_output_tokens".into(), Value::from(max));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), Value::from(temperature));
        }
        if !request.tools.is_empty() {
            body.insert("tools".into(), Value::Array(request.tools.iter().map(|tool| json!({
                "type": "function", "name": tool.name, "description": tool.description, "parameters": tool.parameters
            })).collect()));
        }
        Ok(Value::Object(body))
    }

    pub fn ingest(&mut self, data: &str) -> Result<ProtocolUpdate, GatewayError> {
        if self.terminal {
            return Err(GatewayError::protocol(
                "Responses stream emitted data after terminal event",
            ));
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| GatewayError::protocol("invalid Responses stream JSON"))?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| GatewayError::protocol("Responses event is missing type"))?;
        if !self.started
            && !matches!(
                event_type,
                "response.created"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "error"
            )
        {
            return Err(GatewayError::protocol(
                "Responses event arrived before response.created",
            ));
        }
        if requires_output_index(event_type)
            && value.get("output_index").and_then(Value::as_u64).is_none()
        {
            return Err(GatewayError::protocol(
                "Responses output event is missing output_index",
            ));
        }
        let mut events = Vec::new();
        if !self.started
            && matches!(
                event_type,
                "response.completed" | "response.incomplete" | "response.failed"
            )
        {
            self.capture_response(&value);
            self.started = true;
            events.push(GenerationEvent::Started(StreamMetadata {
                response_id: self.response_id.clone(),
                upstream_model: self.upstream_model.clone(),
            }));
        }
        match event_type {
            "response.created" => {
                if self.started {
                    return Err(GatewayError::protocol(
                        "Responses stream emitted response.created more than once",
                    ));
                }
                self.capture_response(&value);
                self.started = true;
                events.push(GenerationEvent::Started(StreamMetadata {
                    response_id: self.response_id.clone(),
                    upstream_model: self.upstream_model.clone(),
                }));
            }
            "response.in_progress" => self.capture_response(&value),
            "response.output_item.added" => self.item_added(&value, &mut events)?,
            "response.output_text.delta" | "response.refusal.delta" => {
                self.require_output_kind(&value, OutputKind::Message)?;
                let delta =
                    required_string(&value, "delta", "Responses text delta is not a string")?;
                let key = (output_index(&value), 0);
                if self.finished_text.contains(&key) {
                    return Err(GatewayError::protocol(
                        "Responses text delta arrived after content completion",
                    ));
                }
                let is_new = !self.open_text.contains_key(&key);
                let id = self
                    .open_text
                    .entry(key)
                    .or_insert_with(|| format!("text-{}-{}", key.0, key.1))
                    .clone();
                if is_new {
                    events.push(GenerationEvent::TextStarted { id: id.clone() });
                }
                self.text_buffers.entry(key).or_default().push_str(delta);
                events.push(GenerationEvent::TextDelta {
                    id,
                    delta: delta.into(),
                });
            }
            "response.output_text.done" => {
                self.require_output_kind(&value, OutputKind::Message)?;
                self.capture_text_done(&value, "text", &mut events)?;
            }
            "response.refusal.done" => {
                self.require_output_kind(&value, OutputKind::Message)?;
                self.capture_text_done(&value, "refusal", &mut events)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let index = self.require_output_kind(&value, OutputKind::Reasoning)?;
                let delta =
                    required_string(&value, "delta", "Responses reasoning delta is not a string")?;
                let key = (index, 0);
                if self.pending_reasoning_finish.contains_key(&key)
                    || self.finished_reasoning.contains(&index)
                {
                    return Err(GatewayError::protocol(
                        "Responses reasoning delta arrived after reasoning completion",
                    ));
                }
                let is_new = !self.open_reasoning.contains_key(&key);
                let id = self
                    .open_reasoning
                    .entry(key)
                    .or_insert_with(|| format!("reasoning-{}-{}", key.0, key.1))
                    .clone();
                if is_new {
                    events.push(GenerationEvent::ReasoningStarted { id: id.clone() });
                }
                self.reasoning_buffers
                    .entry(index)
                    .or_default()
                    .push_str(delta);
                events.push(GenerationEvent::ReasoningDelta {
                    id,
                    delta: delta.into(),
                });
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                let index = self.require_output_kind(&value, OutputKind::Reasoning)?;
                let key = (index, 0);
                if let Some(id) = self.open_reasoning.remove(&key) {
                    self.pending_reasoning_finish.insert(key, id);
                }
            }
            "response.reasoning_summary_part.added" => {
                self.require_output_kind(&value, OutputKind::Reasoning)?;
            }
            "response.reasoning_summary_part.done" => {
                let index = self.require_output_kind(&value, OutputKind::Reasoning)?;
                let key = (index, 0);
                let Some(id) = self.open_reasoning.get(&key).cloned() else {
                    return Ok(ProtocolUpdate::events(events));
                };
                self.reasoning_buffers
                    .entry(index)
                    .or_default()
                    .push_str("\n\n");
                events.push(GenerationEvent::ReasoningDelta {
                    id,
                    delta: "\n\n".into(),
                });
            }
            "response.function_call_arguments.delta" => self.function_delta(&value, &mut events)?,
            "response.function_call_arguments.done" => self.function_done(&value)?,
            "response.output_item.done" => self.item_done(&value, &mut events)?,
            "response.content_part.added" | "response.content_part.done" => {
                let index = output_index(&value);
                if !self.output_kinds.contains_key(&index) {
                    return Err(GatewayError::protocol(
                        "Responses content part event arrived before output item was added",
                    ));
                }
            }
            "response.completed" => {
                self.capture_response(&value);
                self.capture_terminal_output(&value, &mut events)?;
                self.close_open_parts(&mut events);
                let usage = value
                    .pointer("/response/usage")
                    .or_else(|| value.get("usage"))
                    .filter(|usage| usage.is_object());
                if let Some(usage) = usage {
                    self.usage = parse_responses_usage(usage);
                    events.push(GenerationEvent::UsageUpdated(self.usage.clone()));
                }
                self.terminal = true;
                return Ok(ProtocolUpdate {
                    events,
                    terminal: Some(ProtocolTerminal {
                        status: super::ProtocolTerminalStatus::Completed,
                        finish_reason: if self.finished_functions.is_empty() {
                            FinishReason::Stop
                        } else {
                            FinishReason::ToolCalls
                        },
                        usage: self.usage.clone(),
                        response_id: self.response_id.clone(),
                        upstream_model: self.upstream_model.clone(),
                        provider_metadata: ProviderMetadata {
                            chat: None,
                            responses: Some(ResponsesReplayMetadata {
                                response_id: self.response_id.clone(),
                                ..Default::default()
                            }),
                        },
                        error: None,
                    }),
                });
            }
            "response.incomplete" => {
                let reason = value
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .and_then(allowlisted_provider_token)
                    .unwrap_or_else(|| "unknown".into());
                return self.failed_terminal(
                    &value,
                    FinishReason::Incomplete(reason),
                    GatewayError::provider(
                        "provider response was incomplete",
                        Some("response_incomplete".into()),
                    )
                    .with_upstream_body(data),
                    events,
                );
            }
            "response.failed" | "error" => {
                return self.failed_terminal(
                    &value,
                    FinishReason::Other("failed".into()),
                    // Raw frame, not a re-serialization: preserves the provider's
                    // own key order and any fields the adapter does not model.
                    response_failure(&value).with_upstream_body(data),
                    events,
                );
            }
            // Unknown non-structural response lifecycle events are forward-compatible.
            _ if event_type.starts_with("response.") && !is_structural_event(event_type) => {}
            _ => {
                return Err(GatewayError::protocol(format!(
                    "unknown Responses event type: {event_type}"
                )));
            }
        }
        Ok(ProtocolUpdate::events(events))
    }

    pub fn finish_eof(&mut self) -> Result<Option<ProtocolTerminal>, GatewayError> {
        if self.terminal {
            Ok(None)
        } else {
            Err(GatewayError::protocol(
                "Responses stream ended without response.completed",
            ))
        }
    }

    fn capture_response(&mut self, value: &Value) {
        let response = value.get("response").unwrap_or(value);
        self.response_id = response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.response_id.clone());
        self.upstream_model = response
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.upstream_model.clone());
    }

    fn item_added(
        &mut self,
        value: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let index = output_index(value);
        let item = value
            .get("item")
            .ok_or_else(|| GatewayError::protocol("output item added without item"))?;
        let kind = output_kind(item);
        if self.output_kinds.insert(index, kind).is_some() || self.finished_outputs.contains(&index)
        {
            return Err(GatewayError::protocol(
                "Responses output index was added more than once",
            ));
        }
        match kind {
            OutputKind::FunctionCall => {
                let accumulator = self.functions.entry(index).or_default();
                accumulator.item_id = field_string(item, "id");
                accumulator.call_id = field_string(item, "call_id");
                accumulator.name = field_string(item, "name");
                accumulator.arguments = field_string(item, "arguments");
                accumulator.started = true;
                events.push(GenerationEvent::ToolCallStarted {
                    index,
                    id: call_id(accumulator, index),
                    name: accumulator.name.clone(),
                });
            }
            OutputKind::Message => {}
            OutputKind::Reasoning => self.capture_reasoning_replay(index, item),
            OutputKind::Unsupported => {}
        }
        Ok(())
    }

    fn function_delta(
        &mut self,
        value: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let index = self.require_output_kind(value, OutputKind::FunctionCall)?;
        if self.finished_function_arguments.contains(&index) {
            return Err(GatewayError::protocol(
                "function arguments delta arrived after function completion",
            ));
        }
        let accumulator = self.functions.entry(index).or_default();
        if !accumulator.started {
            accumulator.started = true;
            events.push(GenerationEvent::ToolCallStarted {
                index,
                id: call_id(accumulator, index),
                name: accumulator.name.clone(),
            });
        }
        let delta = value
            .get("delta")
            .and_then(Value::as_str)
            .ok_or_else(|| GatewayError::protocol("function arguments delta is not a string"))?;
        accumulator.arguments.push_str(delta);
        events.push(GenerationEvent::ToolCallDelta {
            index,
            delta: delta.into(),
        });
        Ok(())
    }

    fn function_done(&mut self, value: &Value) -> Result<(), GatewayError> {
        let index = self.require_output_kind(value, OutputKind::FunctionCall)?;
        if self.finished_function_arguments.contains(&index) {
            return Err(GatewayError::protocol(
                "function arguments completed more than once",
            ));
        }
        let arguments = required_string(
            value,
            "arguments",
            "function arguments done value is not a string",
        )?;
        let accumulator = self.functions.entry(index).or_default();
        accumulator.arguments = arguments.into();
        self.finished_function_arguments.insert(index);
        Ok(())
    }

    fn item_done(
        &mut self,
        value: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let index = output_index(value);
        if self.finished_outputs.contains(&index) {
            return Err(GatewayError::protocol(
                "Responses output item completed more than once",
            ));
        }
        let item = value
            .get("item")
            .ok_or_else(|| GatewayError::protocol("output item done without item"))?;
        let kind = output_kind(item);
        if let Some(added_kind) = self.output_kinds.get(&index) {
            if *added_kind != kind {
                return Err(GatewayError::protocol(
                    "Responses output item type changed before completion",
                ));
            }
        } else {
            // Some compatible endpoints only send the authoritative done item.
            self.output_kinds.insert(index, kind);
        }
        match kind {
            OutputKind::FunctionCall if !self.finished_functions.contains(&index) => {
                let accumulator = self.functions.entry(index).or_default();
                if accumulator.item_id.is_empty() {
                    accumulator.item_id = field_string(item, "id");
                }
                if accumulator.call_id.is_empty() {
                    accumulator.call_id = field_string(item, "call_id");
                }
                if accumulator.name.is_empty() {
                    accumulator.name = field_string(item, "name");
                }
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                    accumulator.arguments = arguments.into();
                }
                self.emit_function(index, events)?;
            }
            OutputKind::Reasoning => self.capture_reasoning_item(index, item, events)?,
            OutputKind::Message => self.capture_message_item(index, item, events)?,
            OutputKind::FunctionCall | OutputKind::Unsupported => {}
        }
        self.finished_outputs.insert(index);
        Ok(())
    }

    fn require_output_kind(
        &self,
        value: &Value,
        expected: OutputKind,
    ) -> Result<usize, GatewayError> {
        let index = output_index(value);
        if self.finished_outputs.contains(&index) {
            return Err(GatewayError::protocol(
                "Responses delta arrived after output item completion",
            ));
        }
        match self.output_kinds.get(&index) {
            Some(kind) if *kind == expected => Ok(index),
            Some(_) => Err(GatewayError::protocol(
                "Responses event does not match its output item type",
            )),
            None => Err(GatewayError::protocol(
                "Responses delta arrived before output item was added",
            )),
        }
    }

    fn emit_function(
        &mut self,
        index: usize,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let Some(accumulator) = self.functions.remove(&index) else {
            return Ok(());
        };
        self.finished_functions.insert(index);
        let id = call_id(&accumulator, index);
        if !accumulator.started {
            events.push(GenerationEvent::ToolCallStarted {
                index,
                id: id.clone(),
                name: accumulator.name.clone(),
            });
        }
        let raw_arguments = accumulator.arguments;
        let arguments =
            serde_json::from_str(&raw_arguments).unwrap_or(Value::String(raw_arguments.clone()));
        events.push(GenerationEvent::ToolCallFinished {
            index,
            tool_call: Box::new(ToolCall {
                id,
                name: accumulator.name,
                arguments,
                raw_arguments,
                provider_metadata: ProviderMetadata {
                    chat: None,
                    responses: Some(ResponsesReplayMetadata {
                        item_id: nonempty(accumulator.item_id),
                        call_id: nonempty(accumulator.call_id),
                        ..Default::default()
                    }),
                },
            }),
        });
        Ok(())
    }

    fn close_open_parts(&mut self, events: &mut Vec<GenerationEvent>) {
        for (key, id) in std::mem::take(&mut self.open_text) {
            events.push(GenerationEvent::TextFinished { id, replay: None });
            self.finished_text.insert(key);
        }
        for (key, id) in std::mem::take(&mut self.open_reasoning) {
            self.pending_reasoning_finish.insert(key, id);
        }
        let pending = std::mem::take(&mut self.pending_reasoning_finish);
        for ((output_index, _), id) in pending {
            events.push(GenerationEvent::ReasoningFinished {
                id,
                replay: self
                    .reasoning_replay
                    .get(&output_index)
                    .cloned()
                    .map(|responses| ProviderMetadata {
                        chat: None,
                        responses: Some(responses),
                    }),
            });
        }
    }

    fn finish_reasoning_for_output(
        &mut self,
        output_index: usize,
        events: &mut Vec<GenerationEvent>,
    ) {
        if self.finished_reasoning.contains(&output_index) {
            return;
        }
        let open_keys = self
            .open_reasoning
            .keys()
            .filter(|(index, _)| *index == output_index)
            .copied()
            .collect::<Vec<_>>();
        for key in open_keys {
            if let Some(id) = self.open_reasoning.remove(&key) {
                self.pending_reasoning_finish.insert(key, id);
            }
        }
        let keys = self
            .pending_reasoning_finish
            .keys()
            .filter(|(index, _)| *index == output_index)
            .copied()
            .collect::<Vec<_>>();
        let mut emitted = false;
        for key in keys {
            let Some(id) = self.pending_reasoning_finish.remove(&key) else {
                continue;
            };
            events.push(GenerationEvent::ReasoningFinished {
                id,
                replay: self
                    .reasoning_replay
                    .get(&output_index)
                    .cloned()
                    .map(|responses| ProviderMetadata {
                        chat: None,
                        responses: Some(responses),
                    }),
            });
            emitted = true;
        }
        if !emitted
            && self
                .reasoning_replay
                .get(&output_index)
                .and_then(|metadata| metadata.encrypted_reasoning.as_ref())
                .is_some()
        {
            let id = format!("reasoning-{output_index}-0");
            events.push(GenerationEvent::ReasoningStarted { id: id.clone() });
            events.push(GenerationEvent::ReasoningFinished {
                id,
                replay: self
                    .reasoning_replay
                    .get(&output_index)
                    .cloned()
                    .map(|responses| ProviderMetadata {
                        chat: None,
                        responses: Some(responses),
                    }),
            });
            emitted = true;
        }
        if emitted {
            self.finished_reasoning.insert(output_index);
        }
    }

    fn capture_reasoning_replay(&mut self, index: usize, item: &Value) {
        let metadata = self.reasoning_replay.entry(index).or_default();
        if let Some(id) = nonempty(field_string(item, "id")) {
            metadata.item_id = Some(id);
        }
        if let Some(encrypted) = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            metadata.encrypted_reasoning = Some(encrypted.to_string());
        }
    }

    fn capture_reasoning_item(
        &mut self,
        output_index: usize,
        item: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        self.capture_reasoning_replay(output_index, item);
        let has_authoritative_text = item.get("summary").and_then(Value::as_array).is_some()
            || item.get("content").and_then(Value::as_array).is_some();
        let summary = item
            .get("summary")
            .and_then(Value::as_array)
            .map(|parts| collect_text_fields(parts, "text", "\n\n"))
            .unwrap_or_default();
        let content = item
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| collect_text_fields(parts, "text", "\n\n"))
            .unwrap_or_default();
        let final_text = if summary.is_empty() { content } else { summary };
        let streamed = self.reasoning_buffers.entry(output_index).or_default();
        if has_authoritative_text && !final_text.starts_with(streamed.as_str()) {
            return Err(GatewayError::protocol(
                "Responses final reasoning does not match streamed reasoning",
            ));
        }
        let suffix = if has_authoritative_text {
            &final_text[streamed.len()..]
        } else {
            ""
        };
        let key = (output_index, 0);
        if streamed.is_empty() && !final_text.is_empty() {
            let id = format!("reasoning-{output_index}-0");
            self.open_reasoning.insert(key, id.clone());
            events.push(GenerationEvent::ReasoningStarted { id });
        }
        if !suffix.is_empty() {
            let id = self
                .open_reasoning
                .get(&key)
                .or_else(|| self.pending_reasoning_finish.get(&key))
                .cloned()
                .unwrap_or_else(|| format!("reasoning-{output_index}-0"));
            events.push(GenerationEvent::ReasoningDelta {
                id,
                delta: suffix.to_string(),
            });
            streamed.push_str(suffix);
        }
        self.finish_reasoning_for_output(output_index, events);
        Ok(())
    }

    fn capture_text_done(
        &mut self,
        value: &Value,
        field: &str,
        _events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        if value.get(field).is_some_and(|value| !value.is_string()) {
            return Err(GatewayError::protocol(
                "Responses completed text is not a string",
            ));
        }
        Ok(())
    }

    fn capture_message_item(
        &mut self,
        output_index: usize,
        item: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let message_id = nonempty(field_string(item, "id"));
        let content = item
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| GatewayError::protocol("Responses message content is not an array"))?;
        // pi models one Responses output message as one text slot. Aggregate all
        // known text/refusal parts before attaching the message ID once.
        let mut final_text = String::new();
        for part in content {
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => final_text.push_str(required_string(
                    part,
                    "text",
                    "Responses output text is not a string",
                )?),
                Some("refusal") => final_text.push_str(required_string(
                    part,
                    "refusal",
                    "Responses refusal is not a string",
                )?),
                _ => {}
            }
        }
        let replay = message_id.map(|message_id| ProviderMetadata {
            chat: None,
            responses: Some(ResponsesReplayMetadata {
                message_id: Some(message_id),
                ..Default::default()
            }),
        });
        self.capture_text_part((output_index, 0), &final_text, replay, true, events)
    }

    fn capture_text_part(
        &mut self,
        key: (usize, usize),
        final_text: &str,
        replay: Option<ProviderMetadata>,
        finish: bool,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let streamed = self.text_buffers.entry(key).or_default();
        let suffix = if final_text.starts_with(streamed.as_str()) {
            &final_text[streamed.len()..]
        } else {
            return Err(GatewayError::protocol(
                "Responses final message does not match streamed text",
            ));
        };
        let id = format!("text-{}-{}", key.0, key.1);
        if streamed.is_empty() && !final_text.is_empty() {
            events.push(GenerationEvent::TextStarted { id: id.clone() });
        }
        if !suffix.is_empty() {
            events.push(GenerationEvent::TextDelta {
                id: id.clone(),
                delta: suffix.to_string(),
            });
            streamed.push_str(suffix);
        }
        if finish
            && !self.finished_text.contains(&key)
            && (self.open_text.remove(&key).is_some() || !final_text.is_empty())
        {
            events.push(GenerationEvent::TextFinished { id, replay });
            self.finished_text.insert(key);
        }
        Ok(())
    }

    fn capture_terminal_output(
        &mut self,
        value: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let Some(output) = value.pointer("/response/output").and_then(Value::as_array) else {
            return Ok(());
        };
        for (index, item) in output.iter().enumerate() {
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    self.capture_reasoning_item(index, item, events)?;
                }
                Some("message") => self.capture_message_item(index, item, events)?,
                Some("function_call") if !self.finished_functions.contains(&index) => {
                    let wrapper = json!({"output_index": index, "item": item});
                    self.item_done(&wrapper, events)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn failed_terminal(
        &mut self,
        value: &Value,
        finish_reason: FinishReason,
        error: GatewayError,
        mut events: Vec<GenerationEvent>,
    ) -> Result<ProtocolUpdate, GatewayError> {
        self.capture_response(value);
        self.capture_terminal_output(value, &mut events)?;
        self.close_open_parts(&mut events);
        if let Some(usage) = value
            .pointer("/response/usage")
            .or_else(|| value.get("usage"))
            .filter(|usage| usage.is_object())
        {
            self.usage = parse_responses_usage(usage);
            events.push(GenerationEvent::UsageUpdated(self.usage.clone()));
        }
        self.terminal = true;
        Ok(ProtocolUpdate {
            events,
            terminal: Some(ProtocolTerminal {
                status: super::ProtocolTerminalStatus::Failed,
                finish_reason,
                usage: self.usage.clone(),
                response_id: self.response_id.clone(),
                upstream_model: self.upstream_model.clone(),
                provider_metadata: ProviderMetadata {
                    chat: None,
                    responses: Some(ResponsesReplayMetadata {
                        response_id: self.response_id.clone(),
                        ..Default::default()
                    }),
                },
                error: Some(error),
            }),
        })
    }
}

fn encode_message_items(message: &crate::llm::Message, message_index: usize) -> Vec<Value> {
    let role = match message.role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    };
    if role == "assistant" {
        return encode_assistant_items(message, message_index);
    }

    let mut items = Vec::new();
    let content = message.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": if role == "assistant" { "output_text" } else { "input_text" }, "text": text})),
        _ => None,
    }).collect::<Vec<_>>();
    if !content.is_empty() {
        let item = json!({"type": "message", "role": role, "content": content});
        items.push(item);
    }
    for block in &message.content {
        push_non_text_item(block, &mut items);
    }
    items
}

fn encode_assistant_items(message: &crate::llm::Message, message_index: usize) -> Vec<Value> {
    let mut items = Vec::new();
    let mut text_index = 0;
    for block in &message.content {
        match block {
            ContentBlock::Text {
                text,
                provider_metadata,
            } => {
                let mut item = json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}],
                    "status": "completed",
                });
                let id = provider_metadata
                    .responses
                    .as_ref()
                    .and_then(|metadata| metadata.message_id.as_deref())
                    .map(normalize_message_id)
                    .unwrap_or_else(|| {
                        let suffix = if text_index == 0 {
                            String::new()
                        } else {
                            format!("_{text_index}")
                        };
                        format!("msg_nostra_{message_index}{suffix}")
                    });
                text_index += 1;
                item["id"] = Value::String(id);
                items.push(item);
            }
            _ => push_non_text_item(block, &mut items),
        }
    }
    items
}

fn push_non_text_item(block: &ContentBlock, items: &mut Vec<Value>) {
    match block {
        ContentBlock::ToolCall { tool_call } => {
            let metadata = tool_call.provider_metadata.responses.as_ref();
            let call_id = metadata
                .and_then(|metadata| metadata.call_id.as_deref())
                .unwrap_or(&tool_call.id);
            let mut item = json!({
                "type": "function_call",
                "call_id": normalize_id_part(call_id),
                "name": tool_call.name,
                "arguments": tool_call.raw_arguments,
            });
            if let Some(item_id) = metadata
                .and_then(|metadata| metadata.item_id.as_deref())
                .map(normalize_function_item_id)
            {
                item["id"] = Value::String(item_id);
            }
            items.push(item);
        }
        ContentBlock::ToolResult { tool_result } => items.push(json!({
            "type": "function_call_output",
            "call_id": normalize_id_part(&tool_result.call_id),
            "output": tool_result.content,
        })),
        ContentBlock::Reasoning { reasoning } => {
            if let Some(metadata) = reasoning
                .replay
                .as_ref()
                .and_then(|replay| replay.responses.as_ref())
                && let Some(encrypted) = &metadata.encrypted_reasoning
            {
                let mut item = json!({"type": "reasoning", "encrypted_content": encrypted});
                if let Some(id) = metadata.item_id.as_deref().map(normalize_reasoning_item_id) {
                    item["id"] = Value::String(id);
                }
                items.push(item);
            }
        }
        ContentBlock::Text { .. } => {}
    }
}

fn normalize_id_part(id: &str) -> String {
    let mut normalized = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    if normalized.is_empty() {
        "call_nostra".into()
    } else {
        normalized
    }
}

fn normalize_function_item_id(id: &str) -> String {
    let normalized = normalize_id_part(id);
    if normalized.starts_with("fc_") {
        normalized
    } else {
        normalize_id_part(&format!("fc_{normalized}"))
    }
}

fn normalize_reasoning_item_id(id: &str) -> String {
    let normalized = normalize_id_part(id);
    if normalized.starts_with("rs_") {
        normalized
    } else {
        normalize_id_part(&format!("rs_{normalized}"))
    }
}

fn normalize_message_id(id: &str) -> String {
    let normalized = normalize_id_part(id);
    if normalized.starts_with("msg_") {
        normalized
    } else {
        normalize_id_part(&format!("msg_{normalized}"))
    }
}

fn parse_responses_usage(value: &Value) -> Usage {
    let total_input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = value
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = value
        .pointer("/input_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        provenance: UsageProvenance::Reported,
        input_tokens: total_input.saturating_sub(cache_read.saturating_add(cache_write)),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        output_tokens: output,
        reasoning_tokens: value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(total_input.saturating_add(output)),
    }
}

fn response_failure(value: &Value) -> GatewayError {
    let error = value
        .pointer("/response/error")
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    GatewayError::provider(
        "provider response failed",
        allowlisted_provider_code(error.get("code")),
    )
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<&'a str, GatewayError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::protocol(error))
}

fn output_kind(item: &Value) -> OutputKind {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => OutputKind::Message,
        Some("reasoning") => OutputKind::Reasoning,
        Some("function_call") => OutputKind::FunctionCall,
        _ => OutputKind::Unsupported,
    }
}

fn collect_text_fields(parts: &[Value], field: &str, separator: &str) -> String {
    parts
        .iter()
        .filter_map(|part| part.get(field).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(separator)
}

fn output_index(value: &Value) -> usize {
    value
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}
fn requires_output_index(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.output_item.added"
            | "response.output_item.done"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.content_part.added"
            | "response.content_part.done"
    )
}

fn is_structural_event(event_type: &str) -> bool {
    [
        "response.output_",
        "response.reasoning_",
        "response.refusal.",
        "response.function_call_",
        "response.content_part.",
        "response.custom_tool_call_",
        "response.image_generation_",
        "response.code_interpreter_",
        "response.file_search_",
        "response.web_search_",
        "response.mcp_",
    ]
    .iter()
    .any(|prefix| event_type.starts_with(prefix))
}
fn field_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
fn call_id(value: &FunctionAccumulator, index: usize) -> String {
    if value.call_id.is_empty() {
        format!("call-{index}")
    } else {
        value.call_id.clone()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_is_the_only_success_terminal() {
        let mut session = ResponsesSession::new(CompatibilityProfile::default());
        session
            .ingest(r#"{"type":"response.created","response":{"id":"resp","model":"gpt"}}"#)
            .expect("created");
        session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item","call_id":"call","name":"weather","arguments":""}}"#).expect("item");
        session.ingest(r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}"#).expect("delta");
        let arguments_done = session.ingest(r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{}"}"#).expect("done");
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
    fn reasoning_finish_waits_for_encrypted_item_metadata() {
        let mut session = ResponsesSession::new(CompatibilityProfile::default());
        session
            .ingest(r#"{"type":"response.created","response":{}}"#)
            .expect("created");
        session.ingest(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#).expect("item");
        session.ingest(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"content_index":0,"delta":"think"}"#).expect("delta");
        let done = session.ingest(r#"{"type":"response.reasoning_summary_text.done","output_index":0,"content_index":0}"#).expect("done");
        assert!(
            !done
                .events
                .iter()
                .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
        );
        let item = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}}"#).expect("item done");
        assert!(item.events.iter().any(|event| matches!(event, GenerationEvent::ReasoningFinished { replay: Some(metadata), .. } if metadata.responses.as_ref().and_then(|metadata| metadata.encrypted_reasoning.as_deref()) == Some("opaque"))));
        let completed = session.ingest(r#"{"type":"response.completed","response":{"output":[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}]}}"#).expect("completed");
        assert!(
            !completed
                .events
                .iter()
                .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
        );
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
        assert!(reasoning.ingest(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":null}"#).is_err());
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
        session.ingest(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"first"}"#).expect("first");
        let separator = session
            .ingest(r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#)
            .expect("part");
        assert!(separator.events.iter().any(|event| matches!(event, GenerationEvent::ReasoningDelta { delta, .. } if delta == "\n\n")));
        session.ingest(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"second"}"#).expect("second");
        let done = session.ingest(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"first"},{"type":"summary_text","text":"second"}]}}"#).expect("done");
        assert!(
            done.events
                .iter()
                .any(|event| matches!(event, GenerationEvent::ReasoningFinished { .. }))
        );
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
        assert!(reasoning_done.events.iter().any(|event| matches!(event, GenerationEvent::ReasoningDelta { delta, .. } if delta == "thought")));

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
}
