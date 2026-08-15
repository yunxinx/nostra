use std::{fs, time::Duration};

use super::*;
use crate::llm::{FinishReason, IndexedMessage, Protocol, UsageProvenance};
use crate::session::{
    InMemorySessionStore, LocalSessionStore, LocalStoreConfig, SessionBranchPreview,
    SessionBranchTreeSnapshot, SessionTreeSnapshot,
};
use crate::session::{
    SessionFlushStore, SessionLifecycleStore, SessionReadStore, SessionTreeStore,
};

fn model(id: &str) -> ModelSelection {
    ModelSelection {
        profile_id: "profile".into(),
        model_id: id.into(),
    }
}

fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: Default::default(),
        }],
        provider_metadata: Default::default(),
    }
}

fn usage(total_tokens: u64) -> Usage {
    Usage {
        provenance: UsageProvenance::Reported,
        input_tokens: total_tokens.saturating_sub(2),
        output_tokens: 2,
        total_tokens,
        ..Usage::default()
    }
}

fn generation(
    status: OutcomeStatus,
    message: Option<Message>,
    usage: Usage,
    error: Option<GatewayError>,
) -> GenerationOutcome {
    GenerationOutcome {
        request_id: "request-1".into(),
        profile_id: "profile".into(),
        model_id: "model-a".into(),
        protocol: Protocol::ChatCompletions,
        status,
        finish_reason: (status == OutcomeStatus::Completed).then_some(FinishReason::Stop),
        usage,
        response_id: Some("response-1".into()),
        upstream_model: None,
        time_to_first_event: Some(Duration::from_millis(1)),
        latency: Duration::from_millis(2),
        message: message.map(IndexedMessage::from_message),
        error,
    }
}

fn exercise_completed<S: SessionStore>(store: S) {
    let mut controller = ChatSessionController::new(store);
    assert!(controller.session_id().is_none());
    let start = controller
        .begin_turn(
            text_message(Role::User, "hello"),
            model("model-a"),
            "turn-1",
        )
        .expect("first message should create the session");
    let assistant = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Reasoning {
                reasoning: crate::llm::ReasoningContent {
                    display: "thinking".into(),
                    replay: None,
                },
            },
            ContentBlock::Text {
                text: "world".into(),
                provider_metadata: Default::default(),
            },
        ],
        provider_metadata: Default::default(),
    };
    let terminal = ChatTurnTerminal::from_generation(&generation(
        OutcomeStatus::Completed,
        Some(assistant.clone()),
        usage(12),
        None,
    ));
    controller
        .finish_turn("turn-1", &terminal)
        .expect("completed turn should persist");
    assert!(controller.pending_turn_id().is_none());
    let state = controller
        .restore(&start.session_id)
        .expect("completed session should restore");
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.messages[0].entry_id, start.user_entry_id);
    assert_eq!(state.messages[0].turn_id.as_deref(), Some("turn-1"));
    assert_eq!(state.messages[0].model.as_ref(), Some(&model("model-a")));
    assert_eq!(state.messages[1].message, assistant);
    assert_eq!(state.messages[1].usage, usage(12));
    assert_eq!(state.turn_results.len(), 1);
    assert_eq!(state.turn_results[0].result.status, TurnStatus::Completed);
    assert_eq!(state.turn_results[0].result.usage, usage(12));
    assert_eq!(
        state.latest_config.as_ref().map(|config| &config.model),
        Some(&model("model-a"))
    );
}

mod contracts;
mod retry;
