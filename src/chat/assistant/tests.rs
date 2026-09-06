use super::*;
use unicode_segmentation::UnicodeSegmentation as _;

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
        duration: None,
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
                duration: None,
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
fn markdown_tail_hold_keeps_ambiguous_prefixes_unrevealed() {
    for source in [
        "- item",
        "---",
        "> quote",
        "1. first",
        "1. **bold**",
        "```rust\ncode",
        "[link](u)",
    ] {
        let frames = stream_revealed_prefixes(source);
        for (index, revealed) in frames.iter().enumerate() {
            assert!(
                !revealed_tail_is_ambiguous(revealed),
                "unsafe prefix at frame {index} of {source:?}: {revealed:?}"
            );
        }
        assert_eq!(frames.last().map(String::as_str), Some(source));
    }
}

#[test]
fn fence_interior_does_not_hold_list_markers() {
    let source = "```\n- item\n```";
    let frames = stream_revealed_prefixes(source);
    assert!(
        frames
            .iter()
            .any(|revealed| revealed.contains("- i") || revealed.contains("- item")),
        "list-like text inside a fence must not be held: {frames:?}"
    );
    assert_eq!(frames.last().map(String::as_str), Some(source));
}

fn stream_revealed_prefixes(source: &str) -> Vec<String> {
    let mut pending = PendingDeltas::default();
    let mut revealed = String::new();
    let mut frames = Vec::new();
    for character in source.chars() {
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: character.to_string(),
        });
        for delta in pending.take_frame(false) {
            if let StreamDelta::TextDelta { delta, .. } = delta {
                revealed.push_str(&delta);
            }
        }
        frames.push(revealed.clone());
    }
    pending.push(StreamDelta::TextFinished {
        content_index: 0,
        id: "text-0".into(),
        replay: None,
    });
    for delta in pending.take_frame(true) {
        if let StreamDelta::TextDelta { delta, .. } = delta {
            revealed.push_str(&delta);
        }
    }
    frames.push(revealed.clone());
    frames
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

/// R10: the coalescer stamps a reasoning block's duration when the finish
/// event is *enqueued*, not when the queue drains it. Both stamps land at
/// event arrival, so the duration measures the stream — a dequeue-time stamp
/// would also swallow the pacing delay between the two.
#[test]
fn reasoning_duration_is_stamped_at_enqueue_time_not_drain_time() {
    let mut pending = PendingDeltas::default();
    pending.push(StreamDelta::ReasoningStarted {
        content_index: 0,
        id: "reasoning-0".into(),
    });
    pending.push(StreamDelta::ReasoningDelta {
        content_index: 0,
        id: "reasoning-0".into(),
        delta: "thinking".into(),
    });
    // The block streams for a while before its finish event arrives.
    std::thread::sleep(std::time::Duration::from_millis(40));
    pending.push(StreamDelta::ReasoningFinished {
        content_index: 0,
        id: "reasoning-0".into(),
        replay: None,
        duration: None,
    });
    // The queue only drains later; a dequeue-time stamp would include this.
    std::thread::sleep(std::time::Duration::from_millis(60));

    let finished = pending
        .take()
        .into_iter()
        .find_map(|delta| match delta {
            StreamDelta::ReasoningFinished { duration, .. } => Some(duration),
            _ => None,
        })
        .expect("the finish event drains");
    let duration = finished.expect("a started block banks a duration");
    assert!(
        duration >= std::time::Duration::from_millis(35),
        "the duration must reflect the enqueue interval, got {duration:?}"
    );
    assert!(
        duration < std::time::Duration::from_millis(95),
        "the duration must not include drain time, got {duration:?}"
    );
}

/// A done-item backfill pushes the whole block's lifecycle at once; its
/// duration is honestly ~0 (and `Duration` cannot be negative).
#[test]
fn a_burst_backfill_reasoning_finish_banks_a_near_zero_duration() {
    let mut pending = PendingDeltas::default();
    pending.push(StreamDelta::ReasoningStarted {
        content_index: 0,
        id: "reasoning-0".into(),
    });
    pending.push(StreamDelta::ReasoningDelta {
        content_index: 0,
        id: "reasoning-0".into(),
        delta: "the whole thought".into(),
    });
    pending.push(StreamDelta::ReasoningFinished {
        content_index: 0,
        id: "reasoning-0".into(),
        replay: None,
        duration: None,
    });

    let finished = pending
        .take()
        .into_iter()
        .find_map(|delta| match delta {
            StreamDelta::ReasoningFinished { duration, .. } => Some(duration),
            _ => None,
        })
        .expect("the finish event drains");
    let duration = finished.expect("a started block banks a duration");
    assert!(
        duration < std::time::Duration::from_millis(100),
        "a same-frame backfill is honestly ~0, got {duration:?}"
    );
}

/// The start table is per block: a finish without its start banks nothing,
/// and one block's finish does not consume another's start.
#[test]
fn reasoning_duration_is_keyed_by_block_identity() {
    let mut pending = PendingDeltas::default();
    pending.push(StreamDelta::ReasoningStarted {
        content_index: 0,
        id: "reasoning-0".into(),
    });
    pending.push(StreamDelta::ReasoningFinished {
        content_index: 1,
        id: "reasoning-1".into(),
        replay: None,
        duration: None,
    });
    let events = pending.take();
    let durations: Vec<Option<std::time::Duration>> = events
        .into_iter()
        .filter_map(|delta| match delta {
            StreamDelta::ReasoningFinished { duration, .. } => Some(duration),
            _ => None,
        })
        .collect();
    assert_eq!(
        durations,
        vec![None],
        "a finish that never saw its own start banks no duration"
    );
}
