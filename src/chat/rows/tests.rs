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
// Projection level: pairing and the step stack (AC5)
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
