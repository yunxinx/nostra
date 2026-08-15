use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::llm::{
    FinishReason, GatewayError, GenerationEvent, ProviderMetadata, ReasoningContent,
    ResponsesReplayMetadata, ToolCall,
};

use super::super::{ProtocolTerminal, ProtocolTerminalStatus, ProtocolUpdate};
use super::{
    OutputKind, ResponsesSession,
    wire::{
        call_id, collect_text_fields, field_string, nonempty, output_index, output_kind,
        parse_responses_usage, required_string,
    },
};

impl ResponsesSession {
    pub(super) fn capture_response(&mut self, value: &Value) {
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

    pub(super) fn item_added(
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
        // Responses normally closes a reasoning item before adding the next
        // output. Compatible endpoints may reveal the next block first; close
        // older reasoning here so visible text never inherits its live timer.
        self.finish_other_reasoning(index, events);
        match kind {
            OutputKind::FunctionCall => {
                let accumulator = self.functions.entry(index).or_default();
                accumulator.item_id = field_string(item, "id");
                accumulator.call_id = field_string(item, "call_id");
                accumulator.name = field_string(item, "name");
                accumulator.arguments = field_string(item, "arguments");
                accumulator.started = true;
                events.push(GenerationEvent::ToolCallStarted {
                    content_index: index,
                    index,
                    id: call_id(accumulator, index),
                    name: accumulator.name.clone(),
                });
            }
            OutputKind::Message => {}
            OutputKind::Reasoning => {
                self.capture_reasoning_replay(index, item);
            }
            OutputKind::Unsupported => {}
        }
        Ok(())
    }

    pub(super) fn function_delta(
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
                content_index: index,
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
            content_index: index,
            index,
            delta: delta.into(),
        });
        Ok(())
    }

    pub(super) fn function_done(&mut self, value: &Value) -> Result<(), GatewayError> {
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

    pub(super) fn item_done(
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
        let was_added = if let Some(added_kind) = self.output_kinds.get(&index) {
            if *added_kind != kind {
                return Err(GatewayError::protocol(
                    "Responses output item type changed before completion",
                ));
            }
            true
        } else {
            // Some compatible endpoints only send the authoritative done item.
            self.output_kinds.insert(index, kind);
            false
        };
        if !was_added {
            self.finish_other_reasoning(index, events);
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

    pub(super) fn require_output_kind(
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
                content_index: index,
                index,
                id: id.clone(),
                name: accumulator.name.clone(),
            });
        }
        let raw_arguments = accumulator.arguments;
        let arguments =
            serde_json::from_str(&raw_arguments).unwrap_or(Value::String(raw_arguments.clone()));
        events.push(GenerationEvent::ToolCallFinished {
            content_index: index,
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

    pub(super) fn close_open_parts(&mut self, events: &mut Vec<GenerationEvent>) {
        for (key, id) in std::mem::take(&mut self.open_text) {
            events.push(GenerationEvent::TextFinished {
                content_index: key.0,
                id,
                replay: None,
            });
            self.finished_text.insert(key);
        }
        for (key, id) in std::mem::take(&mut self.open_reasoning) {
            let output_index = key.0;
            events.push(GenerationEvent::ReasoningFinished {
                content_index: output_index,
                id,
                replay: self.reasoning_provider_metadata(output_index),
            });
            self.finished_reasoning.insert(output_index);
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
        let key = (output_index, 0);
        let mut emitted = if let Some(id) = self.open_reasoning.remove(&key) {
            events.push(GenerationEvent::ReasoningFinished {
                content_index: output_index,
                id,
                replay: self.reasoning_provider_metadata(output_index),
            });
            true
        } else {
            false
        };
        if !emitted
            && self
                .reasoning_replay
                .get(&output_index)
                .and_then(|metadata| metadata.encrypted_reasoning.as_ref())
                .is_some()
        {
            let id = format!("reasoning-{output_index}-0");
            events.push(GenerationEvent::ReasoningStarted {
                content_index: output_index,
                id: id.clone(),
            });
            events.push(GenerationEvent::ReasoningFinished {
                content_index: output_index,
                id,
                replay: self.reasoning_provider_metadata(output_index),
            });
            emitted = true;
        }
        if emitted {
            self.finished_reasoning.insert(output_index);
        }
    }

    fn finish_other_reasoning(&mut self, output_index: usize, events: &mut Vec<GenerationEvent>) {
        let open = self
            .open_reasoning
            .keys()
            .filter_map(|(index, _)| (*index != output_index).then_some(*index))
            .collect::<BTreeSet<_>>();
        for index in open {
            self.finish_reasoning_for_output(index, events);
        }
    }

    fn capture_reasoning_replay(&mut self, index: usize, item: &Value) -> bool {
        let metadata = self.reasoning_replay.entry(index).or_default();
        let previous = metadata.clone();
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
        *metadata != previous
    }

    fn reasoning_provider_metadata(&self, index: usize) -> Option<ProviderMetadata> {
        self.reasoning_replay
            .get(&index)
            .cloned()
            .map(|responses| ProviderMetadata {
                chat: None,
                responses: Some(responses),
            })
    }

    fn reasoning_snapshot(&self, index: usize, display: String) -> ReasoningContent {
        ReasoningContent {
            display,
            replay: self.reasoning_provider_metadata(index),
        }
    }

    fn capture_reasoning_item(
        &mut self,
        output_index: usize,
        item: &Value,
        events: &mut Vec<GenerationEvent>,
    ) -> Result<(), GatewayError> {
        let replay_changed = self.capture_reasoning_replay(output_index, item);
        let was_finished = self.finished_reasoning.contains(&output_index);
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
        // Match pi's `summary || content || streamed` precedence. Empty arrays
        // are not an instruction to erase reasoning already delivered live.
        let authoritative = (!summary.is_empty())
            .then_some(summary)
            .or_else(|| (!content.is_empty()).then_some(content));
        let streamed = self
            .reasoning_buffers
            .get(&output_index)
            .cloned()
            .unwrap_or_default();
        if was_finished {
            let display_changed = authoritative
                .as_ref()
                .is_some_and(|final_text| final_text != &streamed);
            if let Some(final_text) = authoritative.as_ref().filter(|_| display_changed) {
                self.reasoning_buffers
                    .insert(output_index, final_text.clone());
            }
            if display_changed || replay_changed {
                // A compatible endpoint may finish the item after the next
                // output starts. Update persistence without extending the card
                // timer or emitting a second lifecycle end.
                events.push(GenerationEvent::ReasoningSnapshotUpdated {
                    content_index: output_index,
                    id: format!("reasoning-{output_index}-0"),
                    reasoning: self
                        .reasoning_snapshot(output_index, authoritative.unwrap_or(streamed)),
                });
            }
            return Ok(());
        }

        let key = (output_index, 0);
        let display_changed = authoritative
            .as_ref()
            .is_some_and(|final_text| final_text != &streamed);
        let can_append = authoritative
            .as_ref()
            .filter(|_| display_changed)
            .and_then(|final_text| final_text.strip_prefix(&streamed));

        if let Some(suffix) = can_append {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.open_reasoning.entry(key)
            {
                let id = format!("reasoning-{output_index}-0");
                entry.insert(id.clone());
                events.push(GenerationEvent::ReasoningStarted {
                    content_index: output_index,
                    id,
                });
            }
            if !suffix.is_empty() {
                let id = self.open_reasoning[&key].clone();
                events.push(GenerationEvent::ReasoningDelta {
                    content_index: output_index,
                    id,
                    delta: suffix.to_string(),
                });
            }
            if let Some(final_text) = &authoritative {
                self.reasoning_buffers
                    .insert(output_index, final_text.clone());
            }
        }

        self.finish_reasoning_for_output(output_index, events);

        if display_changed
            && can_append.is_none()
            && let Some(final_text) = authoritative
        {
            self.reasoning_buffers
                .insert(output_index, final_text.clone());
            if self.finished_reasoning.contains(&output_index) {
                // `output_item.done` is the lifecycle boundary. A complete
                // provider snapshot may differ from streamed chunks (including
                // a trailing summary-part separator), so replace it only after
                // the card has closed rather than reopening the block.
                events.push(GenerationEvent::ReasoningSnapshotUpdated {
                    content_index: output_index,
                    id: format!("reasoning-{output_index}-0"),
                    reasoning: self.reasoning_snapshot(output_index, final_text),
                });
            }
        }
        Ok(())
    }

    pub(super) fn capture_text_done(
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
            events.push(GenerationEvent::TextStarted {
                content_index: key.0,
                id: id.clone(),
            });
        }
        if !suffix.is_empty() {
            events.push(GenerationEvent::TextDelta {
                content_index: key.0,
                id: id.clone(),
                delta: suffix.to_string(),
            });
            streamed.push_str(suffix);
        }
        if finish
            && !self.finished_text.contains(&key)
            && (self.open_text.remove(&key).is_some() || !final_text.is_empty())
        {
            events.push(GenerationEvent::TextFinished {
                content_index: key.0,
                id,
                replay,
            });
            self.finished_text.insert(key);
        }
        Ok(())
    }

    pub(super) fn capture_terminal_output(
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

    pub(super) fn failed_terminal(
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
                status: ProtocolTerminalStatus::Failed,
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
