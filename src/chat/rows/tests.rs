//! Renderer- and projection-level tests for the tool activity rows and the
//! step stack (AC4 / AC5).

use gpui::{AppContext as _, TestAppContext, px};

use crate::chat::projection::{
    ActivityDisclosure, DisclosureState, GROUP_THRESHOLD, ReasoningDisclosure, RowId, RowKind,
    RowProjection,
};
use crate::chat::rows::tool_activity::{ActivityStatus, ToolActivityRenderer};
use crate::chat::transcript::{Part, PartId, PartSource, Transcript, TurnId};
use crate::llm::{ContentBlock, Message as LlmMessage, ProviderMetadata, ToolCall, ToolResult};
use crate::ui::markdown::MarkdownPresentation;

use super::{DisclosureTarget, MaterializeContext, RowChange, RowRenderer};

fn typography() -> crate::chat::projection::TypographySnapshot {
    // Review-exempted: test fixture inputs to the height estimator, not
    // renderer style constants.
    crate::chat::projection::TypographySnapshot {
        line_height: px(20.),
        font_size: px(14.),
        typography_revision: 0,
        theme_revision: 0,
    }
}

fn call_part(part_id: u64, call_id: &str, name: &str, raw_arguments: &str) -> Part {
    Part {
        part_id: PartId::from_u64_for_test(part_id),
        content_index: 0,
        source: PartSource::ToolCall {
            index: 0,
            id: call_id.into(),
            name: name.into(),
            tool_call: Some(ToolCall {
                id: call_id.into(),
                name: name.into(),
                arguments: serde_json::json!({"q": "hi"}),
                raw_arguments: raw_arguments.into(),
                provider_metadata: ProviderMetadata::default(),
            }),
        },
        finished: true,
    }
}

fn activity_ctx<'a>(
    part: &'a Part,
    result: Option<&'a ToolResult>,
    presentation: &'a MarkdownPresentation,
) -> MaterializeContext<'a> {
    MaterializeContext {
        row_id: RowId::new(
            TurnId::from_u64_for_test(1),
            part.part_id,
            RowKind::ToolActivity,
        ),
        part: Some(part),
        paired_result: result,
        error: None,
        presentation,
        user_message_markdown: false,
        owner_id: crate::chat::next_body_owner_id(),
        append_replays_part: false,
    }
}

/// AC4, renderer level: the paired result's `MarkdownBody` does not exist
/// before the first expand, is created by it, and is released again by the
/// re-collapse. Reopening builds fresh entities.
#[gpui::test]
fn activity_result_body_is_lazy_and_released_on_collapse(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let part = call_part(1, "call-0", "lookup", r#"{"q":"hi"}"#);
        let result = ToolResult {
            call_id: "call-0".into(),
            content: "lookup output".into(),
            is_error: false,
        };
        let mut renderer = ToolActivityRenderer::new();
        let ctx = activity_ctx(&part, Some(&result), &presentation);
        renderer.materialize(&ctx, cx);

        assert_eq!(renderer.status_for_test(), ActivityStatus::Completed);
        assert!(
            renderer.result_body_entity_id().is_none(),
            "AC4: no result MarkdownBody entity before the first expand"
        );
        assert!(renderer.arguments_body_entity_id().is_none());

        renderer.toggle_disclosure(DisclosureTarget::Activity, cx);
        let result_id = renderer
            .result_body_entity_id()
            .expect("AC4: the result body is created by the first expand");
        let arguments_id = renderer
            .arguments_body_entity_id()
            .expect("the arguments body is created with the body");
        assert_eq!(
            renderer.disclosure().activity,
            ActivityDisclosure::Open {
                arguments_open: true
            }
        );

        // Re-collapse releases both entities (materialization-window rule).
        renderer.toggle_disclosure(DisclosureTarget::Activity, cx);
        assert!(renderer.result_body_entity_id().is_none());
        assert!(renderer.arguments_body_entity_id().is_none());

        // Reopening builds fresh entities, not resurrected ones.
        renderer.toggle_disclosure(DisclosureTarget::Activity, cx);
        assert_ne!(
            renderer.result_body_entity_id(),
            Some(result_id),
            "a fresh result body entity"
        );
        assert_ne!(
            renderer.arguments_body_entity_id(),
            Some(arguments_id),
            "a fresh arguments body entity"
        );

        // The arguments section folds independently and releases only itself.
        renderer.toggle_disclosure(DisclosureTarget::ActivityArguments, cx);
        assert!(renderer.arguments_body_entity_id().is_none());
        assert!(renderer.result_body_entity_id().is_some());
    });
}

/// AC4, renderer level: a result that arrives while the row is closed does
/// not create anything; the next open picks it up.
#[gpui::test]
fn a_result_arriving_while_closed_stays_lazy(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let part = call_part(1, "call-0", "lookup", "{}");
        let mut renderer = ToolActivityRenderer::new();
        let ctx = activity_ctx(&part, None, &presentation);
        renderer.materialize(&ctx, cx);
        assert_eq!(renderer.status_for_test(), ActivityStatus::Running);

        // The paired result is applied (PartInserted → Replace) while closed.
        let result = ToolResult {
            call_id: "call-0".into(),
            content: "late lookup output".into(),
            is_error: false,
        };
        let ctx = activity_ctx(&part, Some(&result), &presentation);
        renderer.apply(&RowChange::Replace, &ctx, cx);
        assert_eq!(renderer.status_for_test(), ActivityStatus::Completed);
        assert!(
            renderer.result_body_entity_id().is_none(),
            "nothing is created while the row stays folded"
        );

        renderer.toggle_disclosure(DisclosureTarget::Activity, cx);
        assert!(renderer.result_body_entity_id().is_some());
    });
}

/// A result whose capture carries `is_error` drives the failed status.
#[gpui::test]
fn an_error_result_marks_the_activity_failed(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let part = call_part(1, "call-0", "lookup", "{}");
        let result = ToolResult {
            call_id: "call-0".into(),
            content: "boom".into(),
            is_error: true,
        };
        let mut renderer = ToolActivityRenderer::new();
        renderer.materialize(&activity_ctx(&part, Some(&result), &presentation), cx);
        assert_eq!(renderer.status_for_test(), ActivityStatus::Failed);
    });
}

// ---------------------------------------------------------------------------
// Renderer layout dependencies
// ---------------------------------------------------------------------------

fn layout_dependency_ids(renderer: &dyn RowRenderer) -> Vec<gpui::EntityId> {
    let mut ids = Vec::new();
    renderer.visit_layout_dependencies(&mut |body| ids.push(body.entity_id()));
    ids
}

fn layout_completions(renderer: &dyn RowRenderer) -> Vec<bool> {
    let mut complete = Vec::new();
    renderer.visit_layout_dependencies(&mut |body| complete.push(body.layout_snapshot().complete));
    complete
}

#[gpui::test]
fn requested_windowed_layout_does_not_reuse_inactive_completion(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let renderer = cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let part = Part {
            part_id: PartId::from_u64_for_test(1),
            content_index: 0,
            source: PartSource::Prose {
                text: "x".repeat(super::typography::WINDOWED_SOURCE_BYTES),
                replay: ProviderMetadata::default(),
                stream_id: String::new(),
            },
            finished: true,
        };
        let mut ctx = activity_ctx(&part, None, &presentation);
        ctx.row_id = RowId::new(ctx.row_id.turn, part.part_id, RowKind::AssistantProse);
        let mut renderer = super::ProseRenderer::new();
        renderer.materialize(&ctx, cx);
        assert!(renderer.requests_windowed_layout());
        assert!(!renderer.is_layout_complete());
        renderer
    });
    cx.run_until_parked();
    assert_eq!(layout_dependency_ids(&renderer).len(), 1);
    renderer.visit_layout_dependencies(&mut |body| {
        let layout = body.layout_snapshot();
        assert!(layout.complete, "the source has finished parsing");
        assert!(!layout.windowed, "no frame has activated windowed layout");
    });
    assert!(!renderer.is_layout_complete());
}

#[gpui::test]
fn reasoning_height_dependency_tracks_its_expanded_disclosure(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        for finished in [false, true] {
            let presentation = MarkdownPresentation::for_test(cx);
            let part = Part {
                part_id: PartId::from_u64_for_test(1),
                content_index: 0,
                source: PartSource::Reasoning {
                    reasoning: crate::llm::ReasoningContent {
                        display: "x".repeat(super::typography::WINDOWED_SOURCE_BYTES),
                        replay: None,
                        duration_ms: None,
                    },
                    stream_id: String::new(),
                },
                finished,
            };
            let mut ctx = activity_ctx(&part, None, &presentation);
            ctx.row_id = RowId::new(ctx.row_id.turn, part.part_id, RowKind::Reasoning);
            let mut renderer = super::ReasoningRenderer::new();
            renderer.materialize(&ctx, cx);
            let body_id = renderer.body_entity_id().expect("reasoning body");
            assert!(layout_dependency_ids(&renderer).is_empty());
            assert!(renderer.is_layout_complete());
            renderer.toggle_disclosure(DisclosureTarget::Reasoning, cx);
            assert_eq!(
                layout_dependency_ids(&renderer),
                if finished { vec![body_id] } else { Vec::new() },
                "streaming remains a fixed-height preview even when expanded"
            );
            assert!(!renderer.requests_windowed_layout());
            assert_eq!(renderer.is_layout_complete(), !finished);
            renderer.toggle_disclosure(DisclosureTarget::Reasoning, cx);
            assert!(layout_dependency_ids(&renderer).is_empty());
            assert!(renderer.is_layout_complete());
            assert_eq!(renderer.body_entity_id(), Some(body_id));
        }
    });
}

/// The expanded body clamps exactly when its painted natural height crosses
/// the cap, and the clamp decision latches: a repeated report cannot flip it
/// back and forth while the deferred remeasure converges.
#[gpui::test]
fn reasoning_clamps_once_the_painted_body_exceeds_the_cap(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let part = Part {
            part_id: PartId::from_u64_for_test(1),
            content_index: 0,
            source: PartSource::Reasoning {
                reasoning: crate::llm::ReasoningContent {
                    display: "a modest trace".into(),
                    replay: None,
                    duration_ms: None,
                },
                stream_id: String::new(),
            },
            finished: true,
        };
        let mut ctx = activity_ctx(&part, None, &presentation);
        ctx.row_id = RowId::new(ctx.row_id.turn, part.part_id, RowKind::Reasoning);
        let mut renderer = super::ReasoningRenderer::new();
        renderer.materialize(&ctx, cx);
        renderer.toggle_disclosure(DisclosureTarget::Reasoning, cx);

        let cap = px(300.);
        assert!(
            !renderer.note_body_height(px(120.), cap),
            "content below the cap keeps the natural form"
        );
        assert!(
            renderer.note_body_height(px(400.), cap),
            "the first report above the cap must flip to the clamped form"
        );
        assert!(
            !renderer.note_body_height(px(400.), cap),
            "a repeated report must not re-flip the form"
        );
        assert!(
            renderer.note_body_height(px(280.), cap),
            "a report back below the cap must return the form to natural height"
        );
    });
}

#[gpui::test]
fn user_markdown_height_waits_for_parse_and_plain_text_has_no_dependency(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (mut renderer, part, presentation) = cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let part = Part {
            part_id: PartId::from_u64_for_test(1),
            content_index: 0,
            source: PartSource::Prose {
                text: "User paragraph.\n\n".repeat(400),
                replay: ProviderMetadata::default(),
                stream_id: String::new(),
            },
            finished: true,
        };
        let mut ctx = activity_ctx(&part, None, &presentation);
        ctx.row_id = RowId::new(ctx.row_id.turn, part.part_id, RowKind::UserBubble);
        ctx.user_message_markdown = true;
        let mut renderer = super::UserBubbleRenderer::new();
        renderer.materialize(&ctx, cx);
        assert_eq!(
            layout_dependency_ids(&renderer),
            [renderer.body_entity_for_test().expect("Markdown body")]
        );
        assert!(!renderer.requests_windowed_layout());
        assert!(!renderer.is_layout_complete());
        (renderer, part, presentation)
    });
    cx.run_until_parked();
    assert!(renderer.is_layout_complete());

    cx.update(|cx| {
        renderer.release(cx);
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
        let mut ctx = activity_ctx(&part, None, &presentation);
        ctx.row_id = RowId::new(ctx.row_id.turn, part.part_id, RowKind::UserBubble);
        renderer.materialize(&ctx, cx);
        assert!(renderer.body_entity_for_test().is_none());
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
    });
}

#[gpui::test]
fn activity_height_waits_for_each_visible_body_and_ignores_hidden_bodies(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let part = call_part(1, "call-0", "lookup", r#"{"q":"hi"}"#);
    let mut result = ToolResult {
        call_id: "call-0".into(),
        content: "initial result".into(),
        is_error: false,
    };
    let (mut renderer, presentation) = cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let mut renderer = ToolActivityRenderer::new();
        renderer.materialize(&activity_ctx(&part, Some(&result), &presentation), cx);
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
        renderer.toggle_disclosure(DisclosureTarget::Activity, cx);
        assert_eq!(layout_completions(&renderer), [false, false]);
        assert!(!renderer.is_layout_complete());
        (renderer, presentation)
    });
    cx.run_until_parked();
    let arguments_id = renderer.arguments_body_entity_id().expect("arguments body");
    let result_id = renderer.result_body_entity_id().expect("result body");
    assert_eq!(layout_dependency_ids(&renderer), [arguments_id, result_id]);
    assert_eq!(layout_completions(&renderer), [true, true]);
    assert!(renderer.is_layout_complete());

    cx.update(|cx| {
        result.content = "x".repeat(super::typography::RESULT_BUDGET_BYTES);
        renderer.apply(
            &RowChange::Replace,
            &activity_ctx(&part, Some(&result), &presentation),
            cx,
        );
        assert_eq!(layout_dependency_ids(&renderer), [arguments_id, result_id]);
        assert_eq!(layout_completions(&renderer), [true, false]);
        assert!(!renderer.is_layout_complete());
    });
    cx.run_until_parked();
    assert!(renderer.is_layout_complete());

    cx.update(|cx| {
        let part = call_part(1, "call-0", "lookup", &"argument ".repeat(700));
        renderer.apply(
            &RowChange::Replace,
            &activity_ctx(&part, Some(&result), &presentation),
            cx,
        );
        assert_eq!(layout_dependency_ids(&renderer), [arguments_id, result_id]);
        assert_eq!(layout_completions(&renderer), [false, true]);
        assert!(!renderer.is_layout_complete());

        renderer.sync_disclosure(DisclosureState {
            activity: ActivityDisclosure::Open {
                arguments_open: false,
            },
            ..DisclosureState::default()
        });
        assert_eq!(renderer.arguments_body_entity_id(), Some(arguments_id));
        assert_eq!(layout_dependency_ids(&renderer), [result_id]);
        assert!(renderer.is_layout_complete());
        renderer.sync_disclosure(DisclosureState::default());
        assert!(renderer.result_body_entity_id().is_some());
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
        renderer.release(cx);
        assert!(layout_dependency_ids(&renderer).is_empty());
    });
}

#[gpui::test]
fn activity_result_height_dependency_matches_the_viewport_budget(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let budget = super::typography::RESULT_BUDGET_BYTES;
        for (unpaired, bytes) in [(false, budget), (false, budget + 1), (true, budget + 1)] {
            let result = ToolResult {
                call_id: "call-0".into(),
                content: "x".repeat(bytes),
                is_error: false,
            };
            let part = if unpaired {
                Part {
                    part_id: PartId::from_u64_for_test(1),
                    content_index: 0,
                    source: PartSource::ToolResult(result.clone()),
                    finished: true,
                }
            } else {
                call_part(1, "call-0", "lookup", "")
            };
            let mut renderer = ToolActivityRenderer::new();
            renderer.materialize(&activity_ctx(&part, Some(&result), &presentation), cx);
            if !unpaired {
                renderer.toggle_disclosure(DisclosureTarget::Activity, cx);
            }
            let result_id = renderer
                .result_body_entity_id()
                .expect("visible result body");
            let natural_height = unpaired || bytes <= budget;
            assert_eq!(
                layout_dependency_ids(&renderer),
                if natural_height {
                    vec![result_id]
                } else {
                    Vec::new()
                },
                "unpaired={unpaired}, result bytes={bytes}"
            );
            assert_eq!(renderer.is_layout_complete(), !natural_height);
            assert!(!renderer.requests_windowed_layout());
            renderer.release(cx);
            assert!(layout_dependency_ids(&renderer).is_empty());
        }
    });
}

#[gpui::test]
fn error_height_dependency_follows_the_displayed_body(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let mut renderer = cx.update(|cx| {
        let presentation = MarkdownPresentation::for_test(cx);
        let error = crate::llm::GatewayError::http(502, None)
            .with_upstream_body("Provider diagnostic line.\n".repeat(400));
        let ctx = MaterializeContext {
            row_id: RowId::new(
                TurnId::from_u64_for_test(1),
                PartId::NONE,
                RowKind::TurnError,
            ),
            part: None,
            paired_result: None,
            error: Some(&error),
            presentation: &presentation,
            user_message_markdown: false,
            owner_id: crate::chat::next_body_owner_id(),
            append_replays_part: false,
        };
        let mut renderer = super::TurnErrorRenderer::new();
        renderer.materialize(&ctx, cx);
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
        renderer.toggle_disclosure(DisclosureTarget::ErrorBody, cx);
        assert_eq!(
            layout_dependency_ids(&renderer),
            [renderer.body_entity_id().expect("expanded error body")]
        );
        assert!(!renderer.is_layout_complete());
        assert!(!renderer.requests_windowed_layout());
        renderer
    });
    cx.run_until_parked();
    assert!(renderer.is_layout_complete());
    cx.update(|cx| {
        renderer.toggle_disclosure(DisclosureTarget::ErrorBody, cx);
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
        renderer.toggle_disclosure(DisclosureTarget::ErrorBody, cx);
        assert!(!renderer.is_layout_complete());
        renderer.release(cx);
        assert!(layout_dependency_ids(&renderer).is_empty());
        assert!(renderer.is_layout_complete());
    });
}

// ---------------------------------------------------------------------------
// Projection level: pairing and the step stack
// ---------------------------------------------------------------------------

fn text_message(role: crate::llm::Role, text: &str) -> LlmMessage {
    LlmMessage {
        role,
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: Default::default(),
    }
}

fn call_block(call_id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        tool_call: ToolCall {
            id: call_id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
            raw_arguments: "{}".into(),
            provider_metadata: ProviderMetadata::default(),
        },
    }
}

fn result_block(call_id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_result: ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        },
    }
}

fn kinds(projection: &RowProjection) -> Vec<RowKind> {
    projection.rows().iter().map(|row| row.kind()).collect()
}

/// AC4, projection level: a tool call and its result pair into exactly one
/// activity row — the result never grows a row of its own.
#[gpui::test]
fn a_call_and_its_result_pair_into_one_activity_row(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(text_message(crate::llm::Role::User, "hi"), cx);
        transcript.push_canonical_turn(
            LlmMessage {
                role: crate::llm::Role::Assistant,
                content: vec![call_block("call-0", "lookup")],
                provider_metadata: ProviderMetadata::default(),
            },
            cx,
        );
        transcript
    });
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        projection.rebuild(transcript.read(cx), &typography);
        // The user turn carries its own actions row; the call is one row.
        assert_eq!(
            kinds(&projection),
            vec![
                RowKind::UserBubble,
                RowKind::TurnActions,
                RowKind::ToolActivity
            ],
        );

        // The result arrives in its own tool turn; no second row appears.
        transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(
                LlmMessage {
                    role: crate::llm::Role::Tool,
                    content: vec![result_block("call-0", "output")],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            )
        });
        projection.rebuild(transcript.read(cx), &typography);
        assert_eq!(
            kinds(&projection),
            vec![
                RowKind::UserBubble,
                RowKind::TurnActions,
                RowKind::ToolActivity
            ],
            "AC4: call + result stay one row"
        );
    });
}

/// AC5: three or more consecutive activities collapse into one step-stack
/// row naming the latest step; expanding splits them into individually
/// expandable rows, and the per-member disclosure survives the split round
/// trip and later rebuilds.
#[gpui::test]
fn the_step_stack_splits_and_preserves_member_disclosure(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        let calls: Vec<ContentBlock> = (0..GROUP_THRESHOLD)
            .map(|index| call_block(&format!("call-{index}"), &format!("tool-{index}")))
            .collect();
        transcript.push_canonical_turn(
            LlmMessage {
                role: crate::llm::Role::Assistant,
                content: calls,
                provider_metadata: ProviderMetadata::default(),
            },
            cx,
        );
        transcript
    });
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        projection.rebuild(transcript.read(cx), &typography);
        assert_eq!(kinds(&projection), vec![RowKind::ToolActivityGroup]);
        let group = projection.rows()[0].id();
        assert_eq!(projection.rows()[0].group_count(), GROUP_THRESHOLD);
        let expected_latest = format!("tool-{}", GROUP_THRESHOLD - 1);
        assert_eq!(
            projection.rows()[0].group_latest(),
            Some(expected_latest.as_str()),
            "the header names the most recent step"
        );

        // Expanding splits into individual activity rows; the first leads.
        projection.toggle_group(group, transcript.read(cx), &typography);
        assert_eq!(
            kinds(&projection),
            vec![RowKind::ToolActivity; GROUP_THRESHOLD]
        );
        let members: Vec<RowId> = projection.rows().iter().map(|row| row.id()).collect();
        assert!(projection.rows()[0].leads_group());
        assert_eq!(projection.rows()[0].group(), Some(group));

        // One member opens its body.
        let last = *members.last().expect("member");
        projection.set_disclosure(
            last,
            DisclosureState {
                reasoning: ReasoningDisclosure::Collapsed,
                reasoning_user_controlled: false,
                activity: ActivityDisclosure::Open {
                    arguments_open: false,
                },
                group_open: false,
            },
        );

        // Collapsing the stack removes the member rows…
        projection.toggle_group(group, transcript.read(cx), &typography);
        assert_eq!(kinds(&projection), vec![RowKind::ToolActivityGroup]);

        // …and re-expanding restores each member's disclosure.
        projection.toggle_group(group, transcript.read(cx), &typography);
        assert_eq!(
            kinds(&projection),
            vec![RowKind::ToolActivity; GROUP_THRESHOLD]
        );
        let restored = projection.rows()[GROUP_THRESHOLD - 1].disclosure();
        assert_eq!(
            restored.activity,
            ActivityDisclosure::Open {
                arguments_open: false
            },
            "the member's fold state survives the split round trip"
        );

        // A later rebuild (new content elsewhere) keeps it too.
        projection.rebuild(transcript.read(cx), &typography);
        let ix = projection.row_index(last).expect("member survives");
        assert_eq!(
            projection.rows()[ix].disclosure().activity,
            ActivityDisclosure::Open {
                arguments_open: false
            },
            "AC5: disclosure survives a rebuild"
        );
    });
}

/// Below [`GROUP_THRESHOLD`] consecutive activities render individually.
#[gpui::test]
fn short_activity_runs_stay_individual(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let transcript = cx.new(|cx| {
        let mut transcript = Transcript::new(cx);
        transcript.push_canonical_turn(
            LlmMessage {
                role: crate::llm::Role::Assistant,
                content: vec![
                    call_block("call-0", "tool-0"),
                    call_block("call-1", "tool-1"),
                ],
                provider_metadata: ProviderMetadata::default(),
            },
            cx,
        );
        transcript
    });
    let typography = typography();
    cx.update(|cx| {
        let mut projection = RowProjection::default();
        projection.rebuild(transcript.read(cx), &typography);
        assert_eq!(
            kinds(&projection),
            vec![RowKind::ToolActivity, RowKind::ToolActivity],
        );
    });
}

/// R7: the trigger duration format adapts across its three tiers, with the
/// zero-value units omitted and ASCII digits throughout.
#[test]
fn reasoning_duration_formats_seconds_minutes_and_hours() {
    use std::time::Duration;

    use super::reasoning::format_reasoning_duration;

    assert_eq!(format_reasoning_duration(Duration::ZERO), "0 s");
    assert_eq!(format_reasoning_duration(Duration::from_millis(999)), "0 s");
    assert_eq!(format_reasoning_duration(Duration::from_secs(59)), "59 s");
    assert_eq!(format_reasoning_duration(Duration::from_secs(60)), "1 m");
    assert_eq!(
        format_reasoning_duration(Duration::from_secs(61)),
        "1 m 1 s"
    );
    assert_eq!(
        format_reasoning_duration(Duration::from_secs(3_599)),
        "59 m 59 s"
    );
    assert_eq!(format_reasoning_duration(Duration::from_secs(3_600)), "1 h");
    assert_eq!(
        format_reasoning_duration(Duration::from_secs(3_661)),
        "1 h 1 m"
    );
    assert_eq!(
        format_reasoning_duration(Duration::from_millis(7_385_000)),
        "2 h 3 m"
    );
}
