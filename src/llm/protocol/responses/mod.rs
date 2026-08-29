//! OpenAI-compatible Responses request encoder and typed event state machine.
//!
//! Output state is correlated by `output_index`. The adapter retains message,
//! reasoning, item, and call identifiers required for stateless history replay.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::llm::{
    ContentBlock, FinishReason, GatewayError, GenerateRequest, GenerationEvent, ProviderMetadata,
    ResponsesReplayMetadata, Role, StreamMetadata, Usage, error::allowlisted_provider_token,
};

mod encoding;
mod state;
mod wire;

use self::encoding::encode_message_items;
use self::wire::{
    is_structural_event, output_index, parse_responses_usage, required_string,
    requires_output_index, response_failure,
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
                    events.push(GenerationEvent::TextStarted {
                        content_index: key.0,
                        id: id.clone(),
                    });
                }
                self.text_buffers.entry(key).or_default().push_str(delta);
                events.push(GenerationEvent::TextDelta {
                    content_index: key.0,
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
                if self.finished_reasoning.contains(&index) {
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
                    events.push(GenerationEvent::ReasoningStarted {
                        content_index: index,
                        id: id.clone(),
                    });
                }
                self.reasoning_buffers
                    .entry(index)
                    .or_default()
                    .push_str(delta);
                events.push(GenerationEvent::ReasoningDelta {
                    content_index: index,
                    id,
                    delta: delta.into(),
                });
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                self.require_output_kind(&value, OutputKind::Reasoning)?;
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
                    content_index: index,
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
}

#[cfg(test)]
mod tests;
