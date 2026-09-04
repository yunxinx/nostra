use gpui::{AppContext as _, TestAppContext};

use crate::llm::{
    ContentBlock, IndexedContentBlock, IndexedMessage, Message as LlmMessage, ProviderMetadata,
    ReasoningContent, Role as LlmRole, ToolCall, ToolResult,
};

use super::{
    PartChange, PartKind, PartSource, ResolvedStateSource, Role, Transcript, TranscriptEvent,
    TranscriptSource as _, copyable_text, derive_title, is_replayable,
};
use crate::chat::conversation_runtime::ConversationStreamEvent;
use crate::session::{EntryId, ResolvedMessage, ResolvedSessionState, Usage};

fn user_text(text: &str) -> LlmMessage {
    LlmMessage {
        role: LlmRole::User,
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    }
}

fn assistant_text(text: &str) -> LlmMessage {
    LlmMessage {
        role: LlmRole::Assistant,
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    }
}

fn tool_result(call_id: &str, content: &str) -> LlmMessage {
    LlmMessage {
        role: LlmRole::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_result: ToolResult {
                call_id: call_id.into(),
                content: content.into(),
                is_error: false,
            },
        }],
        provider_metadata: ProviderMetadata::default(),
    }
}

#[gpui::test]
fn begin_turn_appends_user_and_empty_assistant(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            let (assistant_id, update) = transcript.begin_turn(user_text("hello runtime"), cx);
            assert_eq!(transcript.turns().len(), 2);
            assert_eq!(transcript.turns()[0].role, Role::User);
            assert_eq!(transcript.turns()[1].role, Role::Assistant);
            assert_eq!(transcript.turns()[1].turn_id, assistant_id);
            assert!(transcript.turns()[1].parts.is_empty());
            assert!(
                matches!(update.event(), TranscriptEvent::TailAppended { turn_ids } if turn_ids.len() == 2)
            );
            let history = transcript.replayable_history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0], user_text("hello runtime"));
            assert_eq!(
                transcript.title().as_deref(),
                Some("hello runtime")
            );
        });
    });
}

#[gpui::test]
fn empty_assistant_placeholders_are_not_replayed(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(user_text("hi"), cx);
            let history = transcript.replayable_history();
            assert_eq!(history.len(), 1);
            assert!(is_replayable(&history[0]));
            assert!(!is_replayable(&LlmMessage {
                role: LlmRole::Assistant,
                content: Vec::new(),
                provider_metadata: ProviderMetadata::default(),
            }));
        });
    });
}

#[gpui::test]
fn stream_lifecycle_retains_part_ids_on_authoritative_replace(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(user_text("q"), cx);
            transcript.apply_stream_batch(
                &[
                    ConversationStreamEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0".into(),
                    },
                    ConversationStreamEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        delta: "thinking".into(),
                    },
                    ConversationStreamEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    ConversationStreamEvent::TextStarted {
                        content_index: 1,
                        id: "text-0".into(),
                    },
                    ConversationStreamEvent::TextDelta {
                        content_index: 1,
                        id: "text-0".into(),
                        delta: "Here is the answer.".into(),
                    },
                    ConversationStreamEvent::TextFinished {
                        content_index: 1,
                        id: "text-0".into(),
                        replay: None,
                    },
                ],
                cx,
            );
            let before = transcript.turns()[1]
                .parts
                .iter()
                .map(|part| (part.part_id, part.kind(), part.content_index))
                .collect::<Vec<_>>();
            assert_eq!(before.len(), 2);
            assert_eq!(before[0].1, PartKind::Reasoning);
            assert_eq!(before[1].1, PartKind::Prose);

            transcript.finish_turn(
                Some(IndexedMessage {
                    role: LlmRole::Assistant,
                    content: vec![
                        IndexedContentBlock {
                            content_index: 0,
                            block: ContentBlock::Reasoning {
                                reasoning: ReasoningContent {
                                    display: "thinking".into(),
                                    replay: None,
                                },
                            },
                        },
                        IndexedContentBlock {
                            content_index: 1,
                            block: ContentBlock::Text {
                                text: "Here is the answer.".into(),
                                provider_metadata: ProviderMetadata::default(),
                            },
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                }),
                None,
                cx,
            );
            let after = transcript.turns()[1]
                .parts
                .iter()
                .map(|part| (part.part_id, part.kind(), part.content_index))
                .collect::<Vec<_>>();
            assert_eq!(before, after);
            assert!(transcript.turns()[1].parts.iter().all(|part| part.finished));
        });
    });
}

#[gpui::test]
fn tool_role_is_retained_in_history(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(user_text("lookup"), cx);
            transcript.push_canonical_turn(assistant_text("calling"), cx);
            transcript.push_canonical_turn(tool_result("call-0", "ok"), cx);
            let history = transcript.replayable_history();
            assert_eq!(history.len(), 3);
            assert_eq!(history[2].role, LlmRole::Tool);
            assert_eq!(transcript.turns()[2].role, Role::Tool);
        });
    });
}

#[gpui::test]
fn load_reset_rebuilds_from_resolved_state(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        let state = ResolvedSessionState {
            leaf_id: EntryId::new(),
            path: Vec::new(),
            context: Vec::new(),
            messages: vec![
                ResolvedMessage {
                    entry_id: EntryId::new(),
                    message: user_text("restored title"),
                    turn_id: Some("turn-1".into()),
                    model: None,
                    usage: Usage::default(),
                },
                ResolvedMessage {
                    entry_id: EntryId::new(),
                    message: assistant_text("restored body"),
                    turn_id: Some("turn-1".into()),
                    model: None,
                    usage: Usage::default(),
                },
            ],
            transcript_replays: Vec::new(),
            turn_results: Vec::new(),
            latest_config: None,
            latest_compaction: None,
        };
        transcript.update(cx, |transcript, cx| {
            let page = ResolvedStateSource::new(state).load_tail(usize::MAX);
            let update = transcript.load(page, None, cx);
            assert!(matches!(update.event(), TranscriptEvent::Reset));
            assert_eq!(transcript.turns().len(), 2);
            assert_eq!(transcript.title().as_deref(), Some("restored title"));
            assert_eq!(transcript.replayable_history().len(), 2);
            assert!(!update.snapshot().has_earlier());
        });
    });
}

#[gpui::test]
fn load_tail_reports_earlier_pages_when_truncated(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        let state = ResolvedSessionState {
            leaf_id: EntryId::new(),
            path: Vec::new(),
            context: Vec::new(),
            messages: vec![
                ResolvedMessage {
                    entry_id: EntryId::new(),
                    message: user_text("older"),
                    turn_id: Some("turn-1".into()),
                    model: None,
                    usage: Usage::default(),
                },
                ResolvedMessage {
                    entry_id: EntryId::new(),
                    message: assistant_text("newer"),
                    turn_id: Some("turn-2".into()),
                    model: None,
                    usage: Usage::default(),
                },
            ],
            transcript_replays: Vec::new(),
            turn_results: Vec::new(),
            latest_config: None,
            latest_compaction: None,
        };
        let source = ResolvedStateSource::new(state);
        let tail = source.load_tail(1);
        assert_eq!(tail.turns.len(), 1);
        assert!(tail.cursor_before.is_some());
        transcript.update(cx, |transcript, cx| {
            let update = transcript.load(tail, None, cx);
            assert!(update.snapshot().has_earlier());
            assert_eq!(transcript.turns().len(), 1);
            assert_eq!(
                transcript.turns()[0].parts[0].source.prose_text(),
                Some("newer")
            );
        });
    });
}

#[gpui::test]
fn copyable_text_joins_prose_parts(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(
                LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "one".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                        ContentBlock::Reasoning {
                            reasoning: ReasoningContent {
                                display: "hidden".into(),
                                replay: None,
                            },
                        },
                        ContentBlock::Text {
                            text: "two".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
            let turn_id = transcript.turns()[0].turn_id;
            assert_eq!(
                transcript.copyable_text(turn_id).as_deref(),
                Some("one\ntwo")
            );
            assert_eq!(copyable_text(&transcript.turns()[0]).as_str(), "one\ntwo");
        });
    });
}

#[test]
fn derive_title_trims_and_truncates() {
    assert_eq!(derive_title("hello").as_str(), "hello");
    assert_eq!(derive_title("a\nb").as_str(), "a b");
    let long = "x".repeat(50);
    let title = derive_title(&long);
    assert!(title.ends_with("..."));
    assert_eq!(title.chars().count(), 40);
}

#[gpui::test]
fn append_creates_missing_text_part(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(user_text("q"), cx);
            let updates = transcript.apply_stream_batch(
                &[ConversationStreamEvent::TextDelta {
                    content_index: 0,
                    id: "text-0".into(),
                    delta: "Hello.".into(),
                }],
                cx,
            );
            assert!(
                updates.iter().any(|update| {
                    matches!(update.event(), TranscriptEvent::PartInserted { .. })
                }),
                "a delta for an unseen part must insert it before appending"
            );
            assert!(updates.iter().any(|update| {
                matches!(
                    update.event(),
                    TranscriptEvent::PartChanged {
                        change: PartChange::Append,
                        ..
                    }
                )
            }));
            match &transcript.turns()[1].parts[0].source {
                PartSource::Prose { text, .. } => assert_eq!(text, "Hello."),
                other => panic!("expected prose, got {other:?}"),
            }
        });
    });
}

#[gpui::test]
fn tool_call_stream_then_result_turn(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let transcript = cx.new(Transcript::new);
        transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(user_text("lookup"), cx);
            transcript.apply_stream_batch(
                &[
                    ConversationStreamEvent::ToolCallStarted {
                        content_index: 0,
                        index: 0,
                        id: "call-0".into(),
                        name: "lookup".into(),
                    },
                    ConversationStreamEvent::ToolCallFinished {
                        content_index: 0,
                        index: 0,
                        tool_call: Box::new(ToolCall {
                            id: "call-0".into(),
                            name: "lookup".into(),
                            arguments: serde_json::json!({}),
                            raw_arguments: "{}".into(),
                            provider_metadata: ProviderMetadata::default(),
                        }),
                    },
                ],
                cx,
            );
            assert_eq!(transcript.turns()[1].parts[0].kind(), PartKind::ToolCall);
            transcript.push_canonical_turn(tool_result("call-0", "{}"), cx);
            assert_eq!(transcript.turns()[2].role, Role::Tool);
        });
    });
}
