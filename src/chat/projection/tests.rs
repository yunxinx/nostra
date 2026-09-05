//! Pure-logic tests for the row projection and its height cache.

use gpui::{AppContext as _, px};

use super::*;
use crate::chat::transcript::{ResolvedStateSource, TranscriptPage, TranscriptSource as _};
use crate::llm::{
    ContentBlock, Message as LlmMessage, ProviderMetadata, ReasoningContent, ToolCall, ToolResult,
};
use crate::session::{EntryId, ResolvedMessage, ResolvedSessionState, Usage};

fn typography() -> TypographySnapshot {
    TypographySnapshot {
        line_height: px(20.),
        font_size: px(14.),
        typography_revision: 0,
        theme_revision: 0,
    }
}

fn text_message(role: crate::llm::Role, text: &str) -> LlmMessage {
    LlmMessage {
        role,
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    }
}

fn message(role: crate::llm::Role, blocks: Vec<ContentBlock>) -> LlmMessage {
    LlmMessage {
        role,
        content: blocks,
        provider_metadata: ProviderMetadata::default(),
    }
}

fn tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        tool_call: ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
            raw_arguments: "{}".into(),
            provider_metadata: ProviderMetadata::default(),
        },
    }
}

fn tool_result(call_id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_result: ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        },
    }
}

fn kinds(transcript: &Transcript) -> Vec<RowKind> {
    let typography = typography();
    let mut projection = RowProjection::default();
    projection.rebuild(transcript, &typography);
    projection.rows().iter().map(|row| row.kind()).collect()
}

#[gpui::test]
fn rows_follow_part_order_with_error_and_actions(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.push_canonical_turn(
            message(
                crate::llm::Role::Assistant,
                vec![
                    ContentBlock::Reasoning {
                        reasoning: ReasoningContent {
                            display: "thinking".into(),
                            replay: None,
                        },
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                        provider_metadata: ProviderMetadata::default(),
                    },
                ],
            ),
            cx,
        );
        transcript
    });

    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::Reasoning,
            RowKind::AssistantProse,
            RowKind::TurnActions,
        ]
    );
}

#[gpui::test]
fn tool_results_pair_into_the_calling_activity_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.push_canonical_turn(
            message(
                crate::llm::Role::Assistant,
                vec![tool_call("call-1", "lookup")],
            ),
            cx,
        );
        transcript.push_canonical_turn(
            message(
                crate::llm::Role::Tool,
                vec![tool_result("call-1", "result")],
            ),
            cx,
        );
        transcript
    });

    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::ToolActivity
        ]
    );
}

#[gpui::test]
fn an_unpaired_result_in_a_tool_turn_keeps_its_own_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.push_canonical_turn(
            message(
                crate::llm::Role::Tool,
                vec![tool_result("orphan", "result")],
            ),
            cx,
        );
        transcript
    });

    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::ToolActivity
        ]
    );
}

#[gpui::test]
fn consecutive_activities_collapse_into_one_group_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        let calls: Vec<ContentBlock> = (0..4)
            .map(|index| tool_call(&format!("call-{index}"), &format!("tool-{index}")))
            .collect();
        transcript.push_canonical_turn(message(crate::llm::Role::Assistant, calls), cx);
        transcript
    });

    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::ToolActivityGroup
        ]
    );

    // Expanding the group splits it back into one row per activity.
    let expanded = cx.update(|cx| {
        let mut projection = RowProjection::default();
        let typography = typography();
        projection.rebuild(transcript.read(cx), &typography);
        let group = projection
            .rows()
            .iter()
            .find(|row| row.kind() == RowKind::ToolActivityGroup)
            .map(|row| row.id())
            .expect("group row");
        projection.toggle_group(group, transcript.read(cx), &typography);
        projection
            .rows()
            .iter()
            .map(|row| row.kind())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        expanded,
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::ToolActivity,
            RowKind::ToolActivity,
            RowKind::ToolActivity,
            RowKind::ToolActivity,
        ]
    );
}

#[gpui::test]
fn an_expanded_group_round_trips_through_its_leader_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        let calls: Vec<ContentBlock> = (0..4)
            .map(|index| tool_call(&format!("call-{index}"), &format!("tool-{index}")))
            .collect();
        transcript.push_canonical_turn(message(crate::llm::Role::Assistant, calls), cx);
        transcript
    });

    cx.update(|cx| {
        let mut projection = RowProjection::default();
        let typography = typography();
        projection.rebuild(transcript.read(cx), &typography);
        let group = projection
            .rows()
            .iter()
            .find(|row| row.kind() == RowKind::ToolActivityGroup)
            .map(|row| row.id())
            .expect("group row");

        let expanded_ids = |projection: &RowProjection| {
            projection
                .rows()
                .iter()
                .filter(|row| row.group().is_some())
                .map(|row| row.id())
                .collect::<Vec<_>>()
        };

        // Expanding: every member declares the group, exactly one leads it,
        // and the leader carries the group id the collapse control sends.
        projection.toggle_group(group, transcript.read(cx), &typography);
        let members = expanded_ids(&projection);
        assert_eq!(members.len(), 4);
        assert_eq!(
            projection
                .rows()
                .iter()
                .filter(|row| row.leads_group())
                .map(|row| row.id())
                .collect::<Vec<_>>(),
            vec![members[0]],
            "exactly the first member leads the expanded group"
        );
        assert_eq!(
            projection.rows()[projection.row_index(members[0]).expect("leader")].group(),
            Some(group),
            "the leader must carry the stable group id"
        );

        // Collapsing through that same id restores the folded shape.
        projection.toggle_group(group, transcript.read(cx), &typography);
        assert_eq!(
            projection
                .rows()
                .iter()
                .map(|row| row.kind())
                .collect::<Vec<_>>(),
            vec![
                RowKind::UserBubble,
                RowKind::TurnActions,
                RowKind::ToolActivityGroup
            ]
        );

        // Expanding again reproduces the same member rows.
        projection.toggle_group(group, transcript.read(cx), &typography);
        assert_eq!(expanded_ids(&projection), members);
    });
}

#[gpui::test]
fn an_empty_assistant_turn_keeps_a_wait_placeholder_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.push_canonical_turn(message(crate::llm::Role::Assistant, vec![]), cx);
        transcript
    });
    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::AssistantProse
        ]
    );
    // The placeholder row is turn-scoped (part NONE) and carries no text, so
    // it does not end waiting.
    let rows = cx.update(|cx| {
        let mut projection = RowProjection::default();
        projection.rebuild(transcript.read(cx), &typography());
        projection.rows().to_vec()
    });
    let borrow: Vec<&Row> = rows.iter().collect();
    // Only the synthetic wait placeholder row (the last) ends nothing.
    assert!(!turn_has_wait_ending_row(&borrow[2..]));
}

#[gpui::test]
fn an_error_turn_appends_the_error_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.finish_turn(None, Some(crate::llm::GatewayError::http(503, None)), cx);
        transcript
    });

    // The user turn carries its actions row; the errored assistant turn
    // ends with the error card instead.
    // finish_turn attached the error to the last (user) turn, so the turn
    // shows the error card and offers no actions.
    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![RowKind::UserBubble, RowKind::TurnError]
    );
}

#[gpui::test]
fn tail_appends_splice_at_the_end(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(Transcript::new);
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        let update = transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx)
        });
        let outcome = projection.apply(transcript.read(cx), update.event(), &typography);
        // Filling an empty projection is cheapest as a reset.
        assert_eq!(
            outcome.diff,
            ProjectionDiff::Rebuild,
            "first push resets the empty list"
        );

        let update = transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(message(crate::llm::Role::Assistant, vec![]), cx)
        });
        let outcome = projection.apply(transcript.read(cx), update.event(), &typography);
        let debug = format!("{:?}", outcome.diff);
        assert!(
            matches!(
                outcome.diff,
                ProjectionDiff::Splice { range, inserted: 1 } if range.start == 2
            ),
            "the second push splices at the end: {debug}"
        );
    });
}

#[gpui::test]
fn append_to_a_streaming_part_remeasures_only_its_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(Transcript::new);
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        });
        let (_, update) = transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(text_message(crate::llm::Role::User, "second"), cx)
        });
        projection.apply(transcript.read(cx), update.event(), &typography);

        let updates = transcript.update(cx, |transcript, cx| {
            transcript.apply_stream_batch(
                &[
                    crate::chat::conversation_runtime::ConversationStreamEvent::TextStarted {
                        content_index: 0,
                        id: "text-0".into(),
                    },
                ],
                cx,
            )
        });
        for update in &updates {
            projection.apply(transcript.read(cx), update.event(), &typography);
        }
        let prose_row = projection
            .rows()
            .iter()
            .find(|row| row.kind() == RowKind::AssistantProse && row.id().part != PartId::NONE)
            .map(|row| row.id())
            .expect("streaming prose row");

        let updates = transcript.update(cx, |transcript, cx| {
            transcript.apply_stream_batch(
                &[
                    crate::chat::conversation_runtime::ConversationStreamEvent::TextDelta {
                        content_index: 0,
                        id: "text-0".into(),
                        delta: "hello".into(),
                    },
                ],
                cx,
            )
        });
        assert_eq!(updates.len(), 1);
        let outcome = projection.apply(transcript.read(cx), updates[0].event(), &typography);
        // The fast path changes no rows and remeasures exactly the changed
        // row (AC2): the declarations are orthogonal, neither swallows the
        // other.
        assert_eq!(
            outcome.diff,
            ProjectionDiff::None,
            "append must not move the row set: {:?}",
            outcome.diff
        );
        let prose_ix = projection.row_index(prose_row).expect("prose row");
        assert_eq!(outcome.remeasure, vec![prose_ix]);
        assert_eq!(outcome.row_changes.len(), 1);
        assert_eq!(outcome.row_changes[0].0, prose_row);
        assert_eq!(outcome.row_changes[0].1, RowChangeKind::Append);
    });
}

#[gpui::test]
fn the_append_fast_path_touches_only_its_row(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(Transcript::new);
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        });
        let (_, update) = transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(text_message(crate::llm::Role::User, "second"), cx)
        });
        projection.apply(transcript.read(cx), update.event(), &typography);
        let updates = transcript.update(cx, |transcript, cx| {
            transcript.apply_stream_batch(
                &[
                    crate::chat::conversation_runtime::ConversationStreamEvent::TextStarted {
                        content_index: 0,
                        id: "text-0".into(),
                    },
                ],
                cx,
            )
        });
        for update in &updates {
            projection.apply(transcript.read(cx), update.event(), &typography);
        }
        let prose_row = projection
            .rows()
            .iter()
            .find(|row| row.kind() == RowKind::AssistantProse && row.id().part != PartId::NONE)
            .map(|row| row.id())
            .expect("streaming prose row");
        let prose_ix = projection.row_index(prose_row).expect("prose row");
        let ids_before: Vec<RowId> = projection.rows().iter().map(|row| row.id()).collect();

        // A same-key measurement serves until the append bumps the content
        // revision.
        let key = typography.measurement_key(48);
        projection.record_height(prose_row, px(240.), key, false);
        assert_eq!(projection.rows()[prose_ix].effective_height(&key), px(240.));

        let updates = transcript.update(cx, |transcript, cx| {
            transcript.apply_stream_batch(
                &[
                    crate::chat::conversation_runtime::ConversationStreamEvent::TextDelta {
                        content_index: 0,
                        id: "text-0".into(),
                        delta: "hello".into(),
                    },
                ],
                cx,
            )
        });
        assert_eq!(updates.len(), 1);
        let outcome = projection.apply(transcript.read(cx), updates[0].event(), &typography);

        // No rebuild: the row set is untouched and only this row is declared.
        assert_eq!(outcome.diff, ProjectionDiff::None);
        assert_eq!(outcome.remeasure, vec![prose_ix]);
        assert_eq!(
            outcome.row_changes,
            vec![(prose_row, RowChangeKind::Append)]
        );
        let ids_after: Vec<RowId> = projection.rows().iter().map(|row| row.id()).collect();
        assert_eq!(
            ids_before, ids_after,
            "the fast path must not re-derive rows"
        );

        // The measurement is kept but stale: the effective height falls back
        // to the refreshed estimate.
        let row = &projection.rows()[prose_ix];
        assert_eq!(row.measured_key(), Some(key));
        assert_ne!(row.effective_height(&key), px(240.));
        assert_eq!(row.effective_height(&key), row.estimated_height());
    });
}

#[gpui::test]
fn an_append_without_its_row_falls_back_to_a_rebuild(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(Transcript::new);
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        });
        let (_, update) = transcript.update(cx, |transcript, cx| {
            transcript.begin_turn(text_message(crate::llm::Role::User, "second"), cx)
        });
        projection.apply(transcript.read(cx), update.event(), &typography);

        let updates = transcript.update(cx, |transcript, cx| {
            transcript.apply_stream_batch(
                &[
                    crate::chat::conversation_runtime::ConversationStreamEvent::TextStarted {
                        content_index: 0,
                        id: "text-0".into(),
                    },
                    crate::chat::conversation_runtime::ConversationStreamEvent::TextDelta {
                        content_index: 0,
                        id: "text-0".into(),
                        delta: "hello".into(),
                    },
                ],
                cx,
            )
        });
        assert_eq!(updates.len(), 2);
        // The projection missed the PartInserted (out-of-band delivery): the
        // append finds no row of its own and must fall back to a rebuild.
        let outcome = projection.apply(transcript.read(cx), updates[1].event(), &typography);
        assert!(
            matches!(
                outcome.diff,
                ProjectionDiff::Splice { .. } | ProjectionDiff::Rebuild
            ),
            "the fallback must converge onto the missing row: {:?}",
            outcome.diff
        );
        assert!(
            projection
                .rows()
                .iter()
                .any(|row| row.kind() == RowKind::AssistantProse && row.id().part != PartId::NONE),
            "the prose row exists after the fallback"
        );
    });
}

#[gpui::test]
fn height_cache_prefers_fresh_measurements_and_falls_back() {
    let typography = typography();
    let key = typography.measurement_key(48);
    let mut height = RowHeight::new(px(100.));
    assert_eq!(height.effective(&key, 0), px(100.));

    height.record(Measured {
        height: px(240.),
        key,
        content_revision: 0,
        confidence: Confidence::Settled,
    });
    assert_eq!(height.effective(&key, 0), px(240.));
    // Different width bucket or stale revision falls back to the estimate.
    assert_eq!(
        height.effective(&typography.measurement_key(32), 0),
        px(100.)
    );
    assert_eq!(height.effective(&key, 1), px(100.));
    // Settled measurements serve cold-restore first frames.
    assert_eq!(height.settled_height(&key), Some(px(240.)));
    height.invalidate();
    assert_eq!(height.settled_height(&key), None);
}

#[test]
fn estimates_grow_with_source_and_line_height() {
    let small = typography();
    let mut large = small;
    large.line_height = px(28.);
    large.typography_revision = 1;
    let short = RowHeight::estimate(RowKind::AssistantProse, 10, 1, &small);
    let long = RowHeight::estimate(RowKind::AssistantProse, 2_000, 1, &small);
    let larger_type = RowHeight::estimate(RowKind::AssistantProse, 10, 1, &large);
    assert!(long > short, "longer source estimates taller");
    assert!(larger_type > short, "larger line height estimates taller");
    // Action rows are chrome-sized regardless of source.
    assert_eq!(
        RowHeight::estimate(RowKind::TurnActions, 0, 0, &small),
        RowHeight::estimate(RowKind::TurnActions, 9_999, 0, &small)
    );
}

#[gpui::test]
fn rebuild_preserves_measurement_and_disclosure_by_row_id(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::Assistant, "answer"), cx);
        transcript
    });
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        projection.rebuild(transcript.read(cx), &typography);
        let prose = projection
            .rows()
            .iter()
            .find(|row| row.kind() == RowKind::AssistantProse)
            .map(|row| row.id())
            .expect("prose row");
        let key = typography.measurement_key(48);
        projection.record_height(prose, px(333.), key, true);
        projection.set_disclosure(prose, DisclosureState::EXPANDED);

        // Rebuilding after a new turn must preserve both.
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(text_message(crate::llm::Role::User, "again"), cx)
        });
        projection.rebuild(transcript.read(cx), &typography);
        let row_ix = projection.row_index(prose).expect("prose row survives");
        let row = &projection.rows()[row_ix];
        assert_eq!(row.effective_height(&key), px(333.));
        assert_eq!(row.disclosure(), DisclosureState::EXPANDED);
    });
}

#[gpui::test]
fn reset_drops_cached_heights_and_disclosure(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    // First session: canonical pushes allocate ids from 1.
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        let calls: Vec<ContentBlock> = (0..4)
            .map(|index| tool_call(&format!("call-{index}"), &format!("tool-{index}")))
            .collect();
        transcript.push_canonical_turn(message(crate::llm::Role::Assistant, calls), cx);
        transcript
    });
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        projection.rebuild(transcript.read(cx), &typography);
        let user_row = projection.rows()[0].id();
        let group = projection
            .rows()
            .iter()
            .find(|row| row.kind() == RowKind::ToolActivityGroup)
            .map(|row| row.id())
            .expect("group row");
        let key = typography.measurement_key(48);
        projection.record_height(user_row, px(333.), key, true);
        projection.toggle_group(group, transcript.read(cx), &typography);
        assert_eq!(projection.rows()[0].effective_height(&key), px(333.));
        assert!(projection.group_is_expanded(group));

        // `Transcript::load` re-allocates ids from 1 and publishes `Reset`:
        // the same RowIds would otherwise resurrect cross-session state.
        let page = page_of(&[
            text_message(crate::llm::Role::User, "hi"),
            message(
                crate::llm::Role::Assistant,
                (0..4)
                    .map(|index| tool_call(&format!("call-{index}"), &format!("tool-{index}")))
                    .collect(),
            ),
        ]);
        let loaded = cx.new(Transcript::new);
        let update = loaded.update(cx, |transcript, cx| transcript.load(page, None, cx));
        assert_eq!(*update.event(), TranscriptEvent::Reset);
        let outcome = projection.apply(loaded.read(cx), update.event(), &typography);
        assert_eq!(outcome.diff, ProjectionDiff::Rebuild);

        // Identical ids, but nothing survived the reset: the measured height
        // fell back to the estimate and the group folded again.
        assert_eq!(projection.row_index(user_row), Some(0));
        assert_ne!(
            projection.rows()[0].effective_height(&key),
            px(333.),
            "a cross-session height must not survive the reset"
        );
        assert_eq!(
            projection.rows()[0].effective_height(&key),
            projection.rows()[0].estimated_height()
        );
        assert!(
            !projection.group_is_expanded(group),
            "disclosure must reset with the session"
        );
        assert_eq!(
            projection
                .rows()
                .iter()
                .map(|row| row.kind())
                .collect::<Vec<_>>(),
            vec![
                RowKind::UserBubble,
                RowKind::TurnActions,
                RowKind::ToolActivityGroup
            ]
        );
    });
}

#[gpui::test]
fn invalidate_typography_drops_stale_measurements(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript
    });
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        let typography = typography();
        projection.rebuild(transcript.read(cx), &typography);
        let row = projection.rows()[0].id();
        projection.record_height(row, px(120.), typography.measurement_key(48), true);

        let mut larger = typography;
        larger.line_height = px(24.);
        larger.typography_revision = 1;
        let changed = projection.invalidate_typography(&larger);
        assert!(
            changed.contains(&projection.row_index(row).expect("row")),
            "the measured row's estimate moved with the line height"
        );
        let row_ix = projection.row_index(row).expect("row");
        assert_eq!(
            projection.rows()[row_ix].settled_height(&larger.measurement_key(48)),
            None,
            "stale measurement must not serve as a settled placeholder"
        );
    });
}

#[test]
fn splice_diff_prefers_prefix_suffix_over_rebuild() {
    let a = RowId {
        turn: crate::chat::transcript::TurnId::from_u64_for_test(1),
        part: PartId::from_u64_for_test(1),
        kind: RowKind::UserBubble,
    };
    let b = RowId {
        turn: crate::chat::transcript::TurnId::from_u64_for_test(2),
        part: PartId::from_u64_for_test(2),
        kind: RowKind::UserBubble,
    };
    let c = RowId {
        turn: crate::chat::transcript::TurnId::from_u64_for_test(3),
        part: PartId::from_u64_for_test(3),
        kind: RowKind::UserBubble,
    };
    assert_eq!(splice_diff(&[a, b], &[a, b]), ProjectionDiff::None);
    assert_eq!(
        splice_diff(&[a, b], &[a, b, c]),
        ProjectionDiff::Splice {
            range: 2..2,
            inserted: 1
        }
    );
    assert_eq!(
        splice_diff(&[a, b], &[b]),
        ProjectionDiff::Splice {
            range: 0..1,
            inserted: 0
        }
    );
}

#[gpui::test]
fn restored_sessions_project_identical_rows(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let state = resolved_state(vec![
        text_message(crate::llm::Role::User, "question"),
        text_message(crate::llm::Role::Assistant, "reply"),
    ]);
    let page = ResolvedStateSource::new(state).load_tail(usize::MAX);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.load(page, None, cx);
        transcript
    });
    assert_eq!(
        cx.update(|cx| kinds(transcript.read(cx))),
        vec![
            RowKind::UserBubble,
            RowKind::TurnActions,
            RowKind::AssistantProse,
            RowKind::TurnActions
        ]
    );
}

#[gpui::test]
fn prepended_pages_splice_rows_at_the_top(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "late"), cx);
        transcript
    });
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        let typography = typography();
        projection.rebuild(transcript.read(cx), &typography);

        let update = transcript.update(cx, |transcript, cx| {
            transcript.prepend(page_of(&[text_message(crate::llm::Role::User, "early")]), cx)
        });
        let outcome = projection.apply(transcript.read(cx), update.event(), &typography);
        let diff_text = format!("{:?}", outcome.diff);
        assert!(
            matches!(outcome.diff, ProjectionDiff::Splice { range, inserted: 2 } if range.start == 0),
            "prepend must splice at the top: {diff_text}"
        );
        assert_eq!(projection.rows()[0].kind(), RowKind::UserBubble);
    });
}

#[gpui::test]
fn a_prepend_page_lands_before_the_existing_turns(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "late"), cx);
        transcript
    });
    cx.update(|cx| {
        transcript.update(cx, |transcript, cx| {
            transcript.prepend(
                page_of(&[text_message(crate::llm::Role::User, "early")]),
                cx,
            );
        });
        let texts: Vec<Option<&str>> = transcript
            .read(cx)
            .turns()
            .iter()
            .map(|turn| {
                turn.parts.iter().find_map(|part| match &part.source {
                    PartSource::Prose { text, .. } => Some(text.as_str()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(texts, vec![Some("early"), Some("late")]);
    });
}

#[gpui::test]
fn turn_replaced_reports_replace_changes_for_its_rows(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.begin_turn(text_message(crate::llm::Role::User, "again"), cx);
        transcript.apply_stream_batch(
            &[
                crate::chat::conversation_runtime::ConversationStreamEvent::TextStarted {
                    content_index: 0,
                    id: "text-0".into(),
                },
            ],
            cx,
        );
        transcript
    });
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        let typography = typography();
        projection.rebuild(transcript.read(cx), &typography);
        let turn_id = transcript.read(cx).turns().last().unwrap().turn_id;

        let update = transcript.update(cx, |transcript, cx| transcript.finish_turn(None, None, cx));
        let outcome = projection.apply(
            transcript.read(cx),
            update.expect("update").event(),
            &typography,
        );
        assert!(
            outcome
                .row_changes
                .iter()
                .any(|(id, change)| id.turn == turn_id && *change == RowChangeKind::Replace)
        );
        // The stream ended without prose, so no actions row exists to report.
        assert!(
            !outcome
                .row_changes
                .iter()
                .any(|(id, _)| id.kind == RowKind::TurnActions)
        );
    });
}

#[test]
fn reasoning_estimates_track_their_source_length() {
    let typography = typography();
    let short = RowHeight::estimate(RowKind::Reasoning, 5, 1, &typography);
    let long = RowHeight::estimate(RowKind::Reasoning, 4_096, 1, &typography);
    assert!(long > short);
}

fn resolved_state(messages: Vec<LlmMessage>) -> ResolvedSessionState {
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

fn page_of(messages: &[LlmMessage]) -> TranscriptPage {
    let state = resolved_state(messages.to_vec());
    ResolvedStateSource::new(state).load_tail(usize::MAX)
}
