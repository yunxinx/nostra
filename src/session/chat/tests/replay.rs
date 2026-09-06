use super::*;
use crate::session::{ResolvedMessage, replayable_history};

fn resolved(messages: Vec<Message>) -> ResolvedSessionState {
    ResolvedSessionState {
        leaf_id: EntryId::new(),
        path: Vec::new(),
        context: Vec::new(),
        messages: messages
            .into_iter()
            .map(|message| ResolvedMessage {
                entry_id: EntryId::new(),
                message,
                turn_id: None,
                model: None,
                usage: Usage::default(),
            })
            .collect(),
        transcript_replays: Vec::new(),
        turn_results: Vec::new(),
        latest_config: None,
        latest_compaction: None,
    }
}

fn assistant_empty() -> Message {
    Message {
        role: Role::Assistant,
        content: Vec::new(),
        provider_metadata: Default::default(),
    }
}

/// The request-history contract lives on the session side (design §3.7): a
/// durable begin whose terminal never produced an assistant snapshot leaves
/// a trailing empty assistant message that must not reach the provider.
#[test]
fn replayable_history_trims_a_trailing_empty_assistant_turn() {
    let state = resolved(vec![
        text_message(Role::User, "question"),
        text_message(Role::Assistant, "answer"),
        text_message(Role::User, "durable begin"),
        assistant_empty(),
    ]);
    let history = replayable_history(&state);
    assert_eq!(history.len(), 3);
    assert_eq!(history[2], text_message(Role::User, "durable begin"));
}

/// Empty assistant placeholders anywhere in the path are dropped; every
/// other role survives with its content intact.
#[test]
fn replayable_history_drops_empty_assistant_turns_everywhere() {
    let state = resolved(vec![
        text_message(Role::User, "first"),
        assistant_empty(),
        text_message(Role::Assistant, "real answer"),
        tool_result_message("call-0", "ok"),
    ]);
    let history = replayable_history(&state);
    assert_eq!(history.len(), 3);
    assert_eq!(history[0], text_message(Role::User, "first"));
    assert_eq!(history[1], text_message(Role::Assistant, "real answer"));
    assert_eq!(history[2], tool_result_message("call-0", "ok"));
}

#[test]
fn replayable_history_keeps_the_canonical_path_unchanged() {
    let messages = vec![
        text_message(Role::User, "one"),
        text_message(Role::Assistant, "two"),
        text_message(Role::User, "three"),
    ];
    let state = resolved(messages.clone());
    let history = replayable_history(&state);
    assert_eq!(history, messages);
}

/// After a paged restore, `begin_turn`'s provider request must still carry
/// turns older than the loaded tail: the coordinator reads the history from
/// the full durable state (R5), not the UI's transcript entity.
#[test]
fn replayable_history_after_a_turn_includes_every_earlier_turn() {
    let mut messages = Vec::new();
    for turn in 0..60 {
        messages.push(text_message(Role::User, &format!("turn {turn} user")));
        messages.push(text_message(Role::Assistant, &format!("turn {turn} reply")));
    }
    let state = resolved(messages.clone());
    let mut durable = state.clone();
    durable.messages.push(ResolvedMessage {
        entry_id: EntryId::new(),
        message: text_message(Role::User, "new begin"),
        turn_id: None,
        model: None,
        usage: Usage::default(),
    });
    durable.messages.push(ResolvedMessage {
        entry_id: EntryId::new(),
        message: assistant_empty(),
        turn_id: None,
        model: None,
        usage: Usage::default(),
    });
    let history = replayable_history(&durable);
    assert_eq!(history.len(), 121);
    assert_eq!(history[0], messages[0]);
    assert_eq!(history[120], text_message(Role::User, "new begin"));
}

fn tool_result_message(call_id: &str, content: &str) -> Message {
    Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_result: crate::llm::ToolResult {
                call_id: call_id.into(),
                content: content.into(),
                is_error: false,
            },
        }],
        provider_metadata: Default::default(),
    }
}
