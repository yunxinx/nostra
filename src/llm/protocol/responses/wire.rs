//! Small wire-format projections and validation helpers.

use serde_json::Value;

use crate::llm::{GatewayError, Usage, UsageProvenance, error::allowlisted_provider_code};

use super::{FunctionAccumulator, OutputKind};

pub(super) fn parse_responses_usage(value: &Value) -> Usage {
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

pub(super) fn response_failure(value: &Value) -> GatewayError {
    let error = value
        .pointer("/response/error")
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    GatewayError::provider(
        "provider response failed",
        allowlisted_provider_code(error.get("code")),
    )
}

pub(super) fn required_string<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<&'a str, GatewayError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::protocol(error))
}

pub(super) fn output_kind(item: &Value) -> OutputKind {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => OutputKind::Message,
        Some("reasoning") => OutputKind::Reasoning,
        Some("function_call") => OutputKind::FunctionCall,
        _ => OutputKind::Unsupported,
    }
}

pub(super) fn collect_text_fields(parts: &[Value], field: &str, separator: &str) -> String {
    parts
        .iter()
        .filter_map(|part| part.get(field).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(separator)
}

pub(super) fn output_index(value: &Value) -> usize {
    value
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}
pub(super) fn requires_output_index(event_type: &str) -> bool {
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

pub(super) fn is_structural_event(event_type: &str) -> bool {
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
pub(super) fn field_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
pub(super) fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
pub(super) fn call_id(value: &FunctionAccumulator, index: usize) -> String {
    if value.call_id.is_empty() {
        format!("call-{index}")
    } else {
        value.call_id.clone()
    }
}
