use super::*;

#[test]
fn pending_deltas_coalesce_adjacent_kinds_and_preserve_order() {
    let mut pending = PendingDeltas::default();
    assert_eq!(
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "a".into(),
        }),
        FlushAction::Schedule
    );
    assert_eq!(
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "b".into(),
        }),
        FlushAction::Pending
    );
    pending.push(StreamDelta::ReasoningDelta {
        content_index: 1,
        id: "reasoning-0".into(),
        delta: "c".into(),
    });
    pending.push(StreamDelta::ReasoningFinished {
        content_index: 1,
        id: "reasoning-0".into(),
        replay: None,
    });
    pending.push(StreamDelta::TextDelta {
        content_index: 2,
        id: "text-1".into(),
        delta: "d".into(),
    });

    assert_eq!(
        pending.take(),
        vec![
            StreamDelta::TextDelta {
                content_index: 0,
                id: "text-0".into(),
                delta: "ab".into(),
            },
            StreamDelta::ReasoningDelta {
                content_index: 1,
                id: "reasoning-0".into(),
                delta: "c".into(),
            },
            StreamDelta::ReasoningFinished {
                content_index: 1,
                id: "reasoning-0".into(),
                replay: None,
            },
            StreamDelta::TextDelta {
                content_index: 2,
                id: "text-1".into(),
                delta: "d".into(),
            },
        ]
    );
}

#[test]
fn pending_deltas_schedule_each_non_empty_batch_once() {
    let mut pending = PendingDeltas::default();
    assert_eq!(
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "first".into(),
        }),
        FlushAction::Schedule
    );
    assert_eq!(
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "second".into(),
        }),
        FlushAction::Pending
    );
    pending.take();
    assert_eq!(
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "third".into(),
        }),
        FlushAction::Schedule
    );
}

#[test]
fn paced_frames_are_grapheme_safe_and_gate_lifecycle_events() {
    let mut small_burst = PendingDeltas::default();
    for _ in 0..1_000 {
        let action = small_burst.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "burst".into(),
            delta: "流".into(),
        });
        assert_ne!(
            action,
            FlushAction::Immediate,
            "adjacent text deltas must be paced as one transport burst"
        );
    }
    let first_burst_frame = small_burst.take_frame(false);
    let [StreamDelta::TextDelta { delta, .. }] = first_burst_frame.as_slice() else {
        panic!("first small-delta burst frame must contain only visible text");
    };
    assert!(delta.graphemes(true).count() <= MAX_VISIBLE_GRAPHEMES_PER_COMMIT);
    assert!(delta.graphemes(true).count() < 1_000);

    let source = format!("{}e\u{301}👩‍👩‍👧‍👦", "流".repeat(400));
    let source_graphemes = source.graphemes(true).count();
    let mut pending = PendingDeltas::default();
    pending.push(StreamDelta::TextDelta {
        content_index: 0,
        id: "text-0".into(),
        delta: source.clone(),
    });
    pending.push(StreamDelta::TextFinished {
        content_index: 0,
        id: "text-0".into(),
        replay: None,
    });

    let first = pending.take_frame(false);
    let [StreamDelta::TextDelta { delta: first, .. }] = first.as_slice() else {
        panic!("first paced frame must contain only visible text");
    };
    assert!(first.graphemes(true).count() <= MAX_VISIBLE_GRAPHEMES_PER_COMMIT);
    assert!(first.graphemes(true).count() < source_graphemes);

    let mut rendered = first.clone();
    let mut finished = false;
    while !pending.deltas.is_empty() {
        for delta in pending.take_frame(false) {
            match delta {
                StreamDelta::TextDelta { delta, .. } => {
                    assert!(!finished, "text must not cross its finish boundary");
                    rendered.push_str(&delta);
                }
                StreamDelta::TextFinished { .. } => {
                    assert_eq!(rendered, source);
                    finished = true;
                }
                other => panic!("unexpected paced delta: {other:?}"),
            }
        }
    }
    assert!(finished);
    assert_eq!(rendered, source);

    let mut split_grapheme = PendingDeltas::default();
    split_grapheme.push(StreamDelta::TextDelta {
        content_index: 0,
        id: "text-0".into(),
        delta: "e".into(),
    });
    assert!(split_grapheme.take_frame(false).is_empty());
    split_grapheme.push(StreamDelta::TextDelta {
        content_index: 0,
        id: "text-0".into(),
        delta: "\u{301}".into(),
    });
    split_grapheme.push(StreamDelta::TextFinished {
        content_index: 0,
        id: "text-0".into(),
        replay: None,
    });
    let combined = split_grapheme.take_frame(false);
    let [
        StreamDelta::TextDelta { delta, .. },
        StreamDelta::TextFinished { .. },
    ] = combined.as_slice()
    else {
        panic!("completed split grapheme must precede its finish boundary");
    };
    assert_eq!(delta, "e\u{301}");
    assert_eq!(delta.graphemes(true).count(), 1);
}

#[test]
fn failed_terminal_always_carries_the_outcome_request_id() {
    let fallback =
        terminal_failure(OutcomeStatus::Failed, None, "request-1".into()).expect("failed outcome");
    assert_eq!(fallback.request_id.as_deref(), Some("request-1"));

    let mut adapter_error = GatewayError::provider("failed", None);
    adapter_error.request_id = Some("adapter-id".into());
    let preserved = terminal_failure(
        OutcomeStatus::Failed,
        Some(adapter_error),
        "outcome-id".into(),
    )
    .expect("failed outcome");
    assert_eq!(preserved.request_id.as_deref(), Some("adapter-id"));

    assert!(terminal_failure(OutcomeStatus::Completed, None, "unused".into()).is_none());
}
