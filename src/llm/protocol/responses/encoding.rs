//! Canonical message history encoding for Responses requests.

use serde_json::{Value, json};

use crate::llm::{ContentBlock, Role};

pub(super) fn encode_message_items(
    message: &crate::llm::Message,
    message_index: usize,
) -> Vec<Value> {
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

pub(super) fn encode_assistant_items(
    message: &crate::llm::Message,
    message_index: usize,
) -> Vec<Value> {
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

pub(super) fn push_non_text_item(block: &ContentBlock, items: &mut Vec<Value>) {
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

pub(super) fn normalize_id_part(id: &str) -> String {
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

pub(super) fn normalize_function_item_id(id: &str) -> String {
    let normalized = normalize_id_part(id);
    if normalized.starts_with("fc_") {
        normalized
    } else {
        normalize_id_part(&format!("fc_{normalized}"))
    }
}

pub(super) fn normalize_reasoning_item_id(id: &str) -> String {
    let normalized = normalize_id_part(id);
    if normalized.starts_with("rs_") {
        normalized
    } else {
        normalize_id_part(&format!("rs_{normalized}"))
    }
}

pub(super) fn normalize_message_id(id: &str) -> String {
    let normalized = normalize_id_part(id);
    if normalized.starts_with("msg_") {
        normalized
    } else {
        normalize_id_part(&format!("msg_{normalized}"))
    }
}
