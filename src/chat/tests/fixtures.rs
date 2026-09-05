//! Windowed-transcript fixtures: large conversations, paged loads, and
//! completed exchanges for the structural tests in `chat::view`.

use gpui::Context;

use crate::chat::ChatView;
use crate::llm::{ContentBlock, Message as LlmMessage, ProviderMetadata, ToolCall, ToolResult};

use super::test_support;

fn text_message(role: crate::llm::Role, text: String) -> LlmMessage {
    LlmMessage {
        role,
        content: vec![ContentBlock::Text {
            text,
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    }
}

/// Seed `turns` alternating user/assistant turns; `tool_activities` assistant
/// turns carry a tool call paired with a following tool-result turn.
pub(in crate::chat) fn seed_large_conversation(
    chat: &mut ChatView,
    turns: usize,
    tool_activities: usize,
    cx: &mut Context<ChatView>,
) {
    let tool_every = turns.checked_div(tool_activities).unwrap_or(usize::MAX);
    for turn in 0..turns {
        test_support::push_canonical(
            chat,
            text_message(crate::llm::Role::User, format!("user turn {turn}")),
            cx,
        );
        if turn > 0 && turn % tool_every == 0 {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::ToolCall {
                        tool_call: ToolCall {
                            id: format!("call-{turn}"),
                            name: "lookup".into(),
                            arguments: serde_json::json!({}),
                            raw_arguments: "{}".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_result: ToolResult {
                            call_id: format!("call-{turn}"),
                            content: format!("result for turn {turn}"),
                            is_error: false,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        } else {
            test_support::push_canonical(
                chat,
                text_message(
                    crate::llm::Role::Assistant,
                    format!("assistant reply {turn} with a short body"),
                ),
                cx,
            );
        }
    }
}

/// Seed a transcript that reports an earlier page, and queue that page for
/// `load_before`.
pub(in crate::chat) fn seed_paged_conversation(chat: &mut ChatView, cx: &mut Context<ChatView>) {
    use crate::chat::transcript::{ResolvedStateSource, TranscriptCursor, TranscriptSource as _};

    let state = |texts: &[&str]| crate::session::ResolvedSessionState {
        leaf_id: crate::session::EntryId::new(),
        path: Vec::new(),
        context: Vec::new(),
        messages: texts
            .iter()
            .enumerate()
            .map(|(index, text)| crate::session::ResolvedMessage {
                entry_id: crate::session::EntryId::new(),
                message: text_message(
                    if index % 2 == 0 {
                        crate::llm::Role::User
                    } else {
                        crate::llm::Role::Assistant
                    },
                    (*text).to_string(),
                ),
                turn_id: None,
                model: None,
                usage: crate::llm::Usage::default(),
            })
            .collect(),
        transcript_replays: Vec::new(),
        turn_results: Vec::new(),
        latest_config: None,
        latest_compaction: None,
    };

    let earlier_page =
        ResolvedStateSource::new(state(&["early question", "early reply"])).load_tail(usize::MAX);
    let tail_page =
        ResolvedStateSource::new(state(&["late question", "late reply"])).load_tail(usize::MAX);
    let cursor = TranscriptCursor { index: 0 };
    let update = chat.transcript.update(cx, |transcript, cx| {
        transcript.load(tail_page, Some(cursor), cx)
    });
    chat.handle_transcript_update(&update, cx);
    test_support::queue_prepend_page(chat, earlier_page);
}

/// Drive `load_before` with the queued page.
pub(in crate::chat) fn load_earlier_page(chat: &mut ChatView, cx: &mut Context<ChatView>) {
    chat.load_before(cx);
}

/// One user turn plus a completed, copyable assistant answer.
pub(in crate::chat) fn seed_completed_exchange(chat: &mut ChatView, cx: &mut Context<ChatView>) {
    test_support::push_canonical(
        chat,
        text_message(crate::llm::Role::User, "question".into()),
        cx,
    );
    test_support::push_canonical(
        chat,
        text_message(crate::llm::Role::Assistant, "a complete answer".into()),
        cx,
    );
}
