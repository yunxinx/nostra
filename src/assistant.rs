//! Mock streaming assistant reply.
//!
//! Swap `stream_reply` for a real HTTP/SSE client when wiring a backend.

use std::time::Duration;

use gpui::{Context, Entity, Task};
use gpui_component::text::TextViewState;

use crate::chat::ChatView;

/// Kick off a mock streaming reply.
///
/// Replace with a real HTTP/SSE client (OpenAI, Ollama, ...) when wiring a
/// backend: feed markdown fragments into `target` via `push_str`, then call
/// `chat.finish_reply(cx)` when the stream ends.
pub fn stream_reply(
    prompt: &str,
    target: Entity<TextViewState>,
    cx: &mut Context<ChatView>,
) -> Task<()> {
    let chunks = mock_chunks(prompt);

    cx.spawn(async move |view, cx| {
        for chunk in chunks {
            cx.background_executor()
                .timer(Duration::from_millis(28))
                .await;
            target.update(cx, |state, cx| state.push_str(&chunk, cx));
            if view.update(cx, |chat, _| chat.follow_stream()).is_err() {
                return;
            }
        }

        view.update(cx, |chat, cx| chat.finish_reply(cx)).ok();
    })
}

fn mock_chunks(prompt: &str) -> Vec<String> {
    let template = format!(
        "This is a mocked reply to your message: **{}**.\n\n\
         Wire a real API into `assistant::stream_reply` when you want live \
         answers.  The rendering path already supports:\n\n\
         - Streaming markdown (`TextViewState::push_str`)\n\
         - Fenced code blocks\n\
         - Ordered and unordered lists\n\n\
         ```rust\n\
         fn hello() {{\n    println!(\"world\");\n}}\n\
         ```\n",
        prompt.trim()
    );

    let mut chunks = Vec::new();
    let mut buf = String::new();
    for ch in template.chars() {
        buf.push(ch);
        if buf.chars().count() >= 3 || ch == '\n' {
            chunks.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        chunks.push(buf);
    }
    chunks
}
