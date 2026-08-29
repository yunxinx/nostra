//! OpenAI-compatible Chat Completions request encoder and stream state machine.
//!
//! This adapter owns Chat-specific reasoning aliases, choice handling, and the
//! index/ID fallback required to assemble interleaved tool-call fragments.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};

use crate::llm::{
    ChatReasoningField, ChatReplayMetadata, ContentBlock, FinishReason, GatewayError,
    GenerateRequest, GenerationEvent, Message, ProviderMetadata, Role, StreamMetadata, ToolCall,
    Usage, UsageProvenance, error::allowlisted_provider_code,
};

use super::{
    CompatibilityProfile, MaxTokensField, ProtocolTerminal, ProtocolUpdate, ReasoningField,
    SystemRolePolicy, validate_messages,
};

#[derive(Default)]
struct ToolAccumulator {
    content_index: Option<usize>,
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

struct OpenContent {
    content_index: usize,
    id: String,
}

pub(crate) struct ChatCompletionsSession {
    compatibility: CompatibilityProfile,
    started: bool,
    terminal: bool,
    done: bool,
    finish_reason: Option<FinishReason>,
    response_id: Option<String>,
    upstream_model: Option<String>,
    usage: Usage,
    text: Option<OpenContent>,
    reasoning: Option<OpenContent>,
    next_text_index: usize,
    next_reasoning_index: usize,
    next_content_index: usize,
    reasoning_field: Option<ChatReasoningField>,
    reasoning_details: Option<Value>,
    tools: BTreeMap<usize, ToolAccumulator>,
    tool_indices_by_id: HashMap<String, usize>,
    next_tool_index: usize,
}

impl ChatCompletionsSession {
    pub fn new(compatibility: CompatibilityProfile) -> Self {
        Self {
            compatibility,
            started: false,
            terminal: false,
            done: false,
            finish_reason: None,
            response_id: None,
            upstream_model: None,
            usage: Usage::default(),
            text: None,
            reasoning: None,
            next_text_index: 0,
            next_reasoning_index: 0,
            next_content_index: 0,
            reasoning_field: None,
            reasoning_details: None,
            tools: BTreeMap::new(),
            tool_indices_by_id: HashMap::new(),
            next_tool_index: 0,
        }
    }

    pub fn encode_request(&self, request: &GenerateRequest) -> Result<Value, GatewayError> {
        if request.model.trim().is_empty() {
            return Err(GatewayError::protocol("model must not be empty"));
        }
        validate_messages(&request.messages)?;
        let messages = request
            .messages
            .iter()
            .map(|message| encode_message(message, &self.compatibility))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let tools = request.tools.iter().map(|tool| json!({
            "type": "function",
            "function": { "name": tool.name, "description": tool.description, "parameters": tool.parameters }
        })).collect::<Vec<_>>();
        let mut body = Map::from_iter([
            ("model".into(), Value::String(request.model.clone())),
            ("messages".into(), Value::Array(messages)),
            ("stream".into(), Value::Bool(true)),
        ]);
        if self.compatibility.include_stream_usage {
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(max) = request.max_output_tokens {
            let key = match self.compatibility.max_tokens_field {
                MaxTokensField::MaxTokens => "max_tokens",
                MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            };
            body.insert(key.into(), Value::from(max));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), Value::from(temperature));
        }
        Ok(Value::Object(body))
    }

    pub fn ingest(&mut self, data: &str) -> Result<ProtocolUpdate, GatewayError> {
        if data == "[DONE]" {
            self.done = true;
            return self.maybe_terminal();
        }
        if self.terminal {
            return Err(GatewayError::protocol(
                "chat stream emitted data after terminal event",
            ));
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| GatewayError::protocol("invalid Chat Completions stream JSON"))?;
        if let Some(error) = value.get("error") {
            // `data` rather than a re-serialization of `error`: the raw frame is
            // what the provider actually sent, with its own key order and any
            // sibling fields an adapter would drop.
            return Err(provider_error(error).with_upstream_body(data));
        }
        let mut events = Vec::new();
        self.response_id = string_at(&value, "id").or_else(|| self.response_id.clone());
        self.upstream_model = string_at(&value, "model").or_else(|| self.upstream_model.clone());
        if !self.started {
            self.started = true;
            events.push(GenerationEvent::Started(StreamMetadata {
                response_id: self.response_id.clone(),
                upstream_model: self.upstream_model.clone(),
            }));
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = parse_chat_usage(usage);
            events.push(GenerationEvent::UsageUpdated(self.usage.clone()));
        }
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        if let Some(choice) = choice {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    let (content_index, id) = self.start_text(&mut events);
                    events.push(GenerationEvent::TextDelta {
                        content_index,
                        id,
                        delta: text.into(),
                    });
                }
                if let Some((field, reasoning)) =
                    select_reasoning(delta, self.compatibility.reasoning_field)
                {
                    if !reasoning.is_empty() {
                        self.reasoning_field.get_or_insert(field);
                        let (content_index, id) = self.start_reasoning(&mut events);
                        events.push(GenerationEvent::ReasoningDelta {
                            content_index,
                            id,
                            delta: reasoning,
                        });
                    }
                }
                if let Some(details) = delta
                    .get("reasoning_details")
                    .filter(|value| !value.is_null())
                {
                    merge_reasoning_details(&mut self.reasoning_details, details.clone());
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    if !tool_calls.is_empty() {
                        self.finish_open_content(&mut events);
                    }
                    for (position, raw) in tool_calls.iter().enumerate() {
                        self.accumulate_tool(raw, position, &mut events)?;
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(parse_finish_reason(reason));
                self.finish_parts(&mut events)?;
            }
        }
        let mut update = ProtocolUpdate::events(events);
        if self.done && self.finish_reason.is_some() {
            update.terminal = self.take_terminal();
        }
        Ok(update)
    }

    pub fn finish_eof(&mut self) -> Result<Option<ProtocolTerminal>, GatewayError> {
        if self.terminal {
            Ok(None)
        } else if self.finish_reason.is_some() {
            Ok(self.take_terminal())
        } else {
            Err(GatewayError::protocol(
                "Chat Completions stream ended without a valid finish reason",
            ))
        }
    }

    fn accumulate_tool(
        &mut self,
        raw: &Value,
        fallback_index: usize,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let raw_id =
            optional_tool_string(raw, "id", self.compatibility.allow_nullable_tool_fields)?;
        let index = if let Some(index) = raw.get("index").and_then(Value::as_u64) {
            let index = index as usize;
            self.next_tool_index = self.next_tool_index.max(index.saturating_add(1));
            index
        } else if let Some(id) = raw_id.as_deref() {
            if let Some(index) = self.tool_indices_by_id.get(id) {
                *index
            } else {
                let index = self.next_tool_index;
                self.next_tool_index = self.next_tool_index.saturating_add(1);
                self.tool_indices_by_id.insert(id.to_string(), index);
                index
            }
        } else {
            fallback_index
        };
        let accumulator = self.tools.entry(index).or_default();
        if let Some(id) = raw_id {
            accumulator.id = id.clone();
            self.tool_indices_by_id.insert(id, index);
        }
        let function = raw.get("function").unwrap_or(&Value::Null);
        if let Some(name) = optional_tool_string(
            function,
            "name",
            self.compatibility.allow_nullable_tool_fields,
        )? {
            if accumulator.name.is_empty() {
                accumulator.name = name;
            }
        }
        if !accumulator.started {
            accumulator.started = true;
            accumulator.content_index = Some(self.next_content_index);
            events.push(GenerationEvent::ToolCallStarted {
                content_index: self.next_content_index,
                index,
                id: if accumulator.id.is_empty() {
                    format!("call-{index}")
                } else {
                    accumulator.id.clone()
                },
                name: accumulator.name.clone(),
            });
            self.next_content_index = self.next_content_index.saturating_add(1);
        }
        if let Some(arguments) = function.get("arguments") {
            let fragment = if let Some(text) = arguments.as_str() {
                text.to_string()
            } else if self.compatibility.allow_object_tool_arguments && arguments.is_object() {
                serde_json::to_string(arguments)
                    .map_err(|_| GatewayError::protocol("invalid tool arguments"))?
            } else if arguments.is_null() && self.compatibility.allow_nullable_tool_fields {
                String::new()
            } else {
                return Err(GatewayError::protocol(
                    "tool arguments must be a string or configured object",
                ));
            };
            accumulator.arguments.push_str(&fragment);
            if !fragment.is_empty() {
                events.push(GenerationEvent::ToolCallDelta {
                    content_index: accumulator.content_index.ok_or_else(|| {
                        GatewayError::protocol("started tool call is missing its content index")
                    })?,
                    index,
                    delta: fragment,
                });
            }
        }
        Ok(())
    }

    fn finish_parts(&mut self, events: &mut Vec<GenerationEvent>) -> Result<(), GatewayError> {
        self.finish_open_content(events);
        for (index, accumulator) in &self.tools {
            let id = if accumulator.id.is_empty() {
                format!("call-{index}")
            } else {
                accumulator.id.clone()
            };
            let arguments = serde_json::from_str(&accumulator.arguments)
                .unwrap_or(Value::String(accumulator.arguments.clone()));
            events.push(GenerationEvent::ToolCallFinished {
                content_index: accumulator.content_index.ok_or_else(|| {
                    GatewayError::protocol("finished tool call is missing its content index")
                })?,
                index: *index,
                tool_call: Box::new(ToolCall {
                    id,
                    name: accumulator.name.clone(),
                    arguments,
                    raw_arguments: accumulator.arguments.clone(),
                    provider_metadata: ProviderMetadata::default(),
                }),
            });
        }
        Ok(())
    }

    fn start_text(&mut self, events: &mut Vec<GenerationEvent>) -> (usize, String) {
        self.finish_reasoning(events);
        if let Some(open) = &self.text {
            return (open.content_index, open.id.clone());
        }
        let open = OpenContent {
            content_index: self.next_content_index,
            id: format!("text-{}", self.next_text_index),
        };
        self.next_text_index = self.next_text_index.saturating_add(1);
        self.next_content_index = self.next_content_index.saturating_add(1);
        events.push(GenerationEvent::TextStarted {
            content_index: open.content_index,
            id: open.id.clone(),
        });
        let result = (open.content_index, open.id.clone());
        self.text = Some(open);
        result
    }

    fn start_reasoning(&mut self, events: &mut Vec<GenerationEvent>) -> (usize, String) {
        self.finish_text(events);
        if let Some(open) = &self.reasoning {
            return (open.content_index, open.id.clone());
        }
        let open = OpenContent {
            content_index: self.next_content_index,
            id: format!("reasoning-{}", self.next_reasoning_index),
        };
        self.next_reasoning_index = self.next_reasoning_index.saturating_add(1);
        self.next_content_index = self.next_content_index.saturating_add(1);
        events.push(GenerationEvent::ReasoningStarted {
            content_index: open.content_index,
            id: open.id.clone(),
        });
        let result = (open.content_index, open.id.clone());
        self.reasoning = Some(open);
        result
    }

    fn finish_open_content(&mut self, events: &mut Vec<GenerationEvent>) {
        self.finish_text(events);
        self.finish_reasoning(events);
    }

    fn finish_text(&mut self, events: &mut Vec<GenerationEvent>) {
        if let Some(open) = self.text.take() {
            events.push(GenerationEvent::TextFinished {
                content_index: open.content_index,
                id: open.id,
                replay: None,
            });
        }
    }

    fn finish_reasoning(&mut self, events: &mut Vec<GenerationEvent>) {
        if let Some(open) = self.reasoning.take() {
            events.push(GenerationEvent::ReasoningFinished {
                content_index: open.content_index,
                id: open.id,
                replay: self.chat_replay_metadata(),
            });
        }
    }

    fn maybe_terminal(&mut self) -> Result<ProtocolUpdate, GatewayError> {
        let mut update = ProtocolUpdate::events(Vec::new());
        if self.finish_reason.is_some() {
            update.terminal = self.take_terminal();
        }
        Ok(update)
    }

    fn take_terminal(&mut self) -> Option<ProtocolTerminal> {
        if self.terminal {
            return None;
        }
        let finish_reason = self.finish_reason.clone()?;
        self.terminal = true;
        Some(ProtocolTerminal {
            status: super::ProtocolTerminalStatus::Completed,
            finish_reason,
            usage: self.usage.clone(),
            response_id: self.response_id.clone(),
            upstream_model: self.upstream_model.clone(),
            provider_metadata: ProviderMetadata {
                chat: self
                    .chat_replay_metadata()
                    .and_then(|metadata| metadata.chat),
                responses: None,
            },
            error: None,
        })
    }

    fn chat_replay_metadata(&self) -> Option<ProviderMetadata> {
        (self.reasoning_field.is_some() || self.reasoning_details.is_some()).then(|| {
            ProviderMetadata {
                chat: Some(ChatReplayMetadata {
                    reasoning_field: self.reasoning_field,
                    reasoning_details: self.reasoning_details.clone(),
                }),
                responses: None,
            }
        })
    }
}

fn optional_tool_string(
    value: &Value,
    field: &str,
    allow_null: bool,
) -> Result<Option<String>, GatewayError> {
    match value.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) if allow_null => Ok(None),
        Some(Value::Null) => Err(GatewayError::protocol(format!(
            "tool {field} must not be null"
        ))),
        Some(_) => Err(GatewayError::protocol(format!(
            "tool {field} must be a string"
        ))),
    }
}

fn encode_message(
    message: &Message,
    compatibility: &CompatibilityProfile,
) -> Result<Vec<Value>, GatewayError> {
    let role = match (message.role, compatibility.system_role) {
        (Role::System | Role::Developer, SystemRolePolicy::System) => "system",
        (Role::System | Role::Developer, SystemRolePolicy::Developer) => "developer",
        (Role::System, _) => "system",
        (Role::Developer, _) => "developer",
        (Role::User, _) => "user",
        (Role::Assistant, _) => "assistant",
        (Role::Tool, _) => "tool",
    };
    let mut object = Map::from_iter([(String::from("role"), Value::String(role.into()))]);
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if !text.is_empty() {
        object.insert("content".into(), Value::String(text));
    }
    let calls = message.content.iter().filter_map(|block| match block { ContentBlock::ToolCall { tool_call } => Some(json!({"id": normalize_chat_call_id(&tool_call.id), "type": "function", "function": {"name": tool_call.name, "arguments": tool_call.raw_arguments}})), _ => None }).collect::<Vec<_>>();
    if !calls.is_empty() {
        object.insert("tool_calls".into(), Value::Array(calls));
    }
    let tool_results = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_result } => Some(json!({
                "role": "tool",
                "tool_call_id": normalize_chat_call_id(&tool_result.call_id),
                "content": tool_result.content,
            })),
            _ => None,
        })
        .collect::<Vec<_>>();
    if message.role == Role::Tool && !tool_results.is_empty() {
        return Ok(tool_results);
    }
    if let Some(details) = message
        .provider_metadata
        .chat
        .as_ref()
        .and_then(|metadata| metadata.reasoning_details.clone())
        .or_else(|| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Reasoning { reasoning } => reasoning
                    .replay
                    .as_ref()
                    .and_then(|metadata| metadata.chat.as_ref())
                    .and_then(|metadata| metadata.reasoning_details.clone()),
                _ => None,
            })
        })
    {
        object.insert("reasoning_details".into(), details);
    }
    for block in &message.content {
        let ContentBlock::Reasoning { reasoning } = block else {
            continue;
        };
        if reasoning.display.is_empty() {
            continue;
        }
        let Some(field) = reasoning
            .replay
            .as_ref()
            .and_then(|metadata| metadata.chat.as_ref())
            .and_then(|metadata| metadata.reasoning_field)
        else {
            continue;
        };
        object
            .entry(field.as_str())
            .and_modify(|value| {
                if let Some(existing) = value.as_str() {
                    *value = Value::String(format!("{existing}\n{}", reasoning.display));
                }
            })
            .or_insert_with(|| Value::String(reasoning.display.clone()));
    }
    let mut messages = vec![Value::Object(object)];
    messages.extend(tool_results);
    Ok(messages)
}

fn normalize_chat_call_id(id: &str) -> String {
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
    if normalized.starts_with("call_") && normalized.len() > 5 {
        normalized
    } else if normalized.is_empty() {
        "call_nostra".into()
    } else {
        format!("call_{normalized}")
    }
}

fn merge_reasoning_details(current: &mut Option<Value>, next: Value) {
    if let (Some(Value::Array(current)), Value::Array(next)) = (current.as_mut(), &next) {
        current.extend(next.iter().cloned());
    } else {
        *current = Some(next);
    }
}

fn select_reasoning(delta: &Value, policy: ReasoningField) -> Option<(ChatReasoningField, String)> {
    let fields: &[ChatReasoningField] = match policy {
        ReasoningField::Auto => &[
            ChatReasoningField::ReasoningContent,
            ChatReasoningField::Reasoning,
            ChatReasoningField::ReasoningText,
        ],
        ReasoningField::ReasoningContent => &[ChatReasoningField::ReasoningContent],
        ReasoningField::Reasoning => &[ChatReasoningField::Reasoning],
        ReasoningField::ReasoningText => &[ChatReasoningField::ReasoningText],
    };
    fields.iter().find_map(|field| {
        delta
            .get(field.as_str())
            .and_then(Value::as_str)
            .map(|value| (*field, value.to_string()))
    })
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.into()),
    }
}

fn parse_chat_usage(value: &Value) -> Usage {
    let total_input = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| value.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = value
        .pointer("/prompt_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        provenance: UsageProvenance::Reported,
        input_tokens: total_input.saturating_sub(cache_read.saturating_add(cache_write)),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        output_tokens: output,
        reasoning_tokens: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(total_input.saturating_add(output)),
    }
}

fn provider_error(value: &Value) -> GatewayError {
    GatewayError::provider(
        "provider rejected the request",
        allowlisted_provider_code(value.get("code")),
    )
}

#[cfg(test)]
mod tests;
