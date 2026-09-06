//! Natural-height rows request windowed layout above the activation thresholds.
//! Their measured heights become settled only after all blocks are measured.

use super::*;
use crate::chat::projection::Confidence;
use crate::chat::rows::typography::{WINDOWED_SOURCE_BLOCKS, WINDOWED_SOURCE_BYTES, windowed_body};

#[test]
fn windowed_activation_uses_either_threshold() {
    assert!(!windowed_body(WINDOWED_SOURCE_BYTES - 1, 0));
    assert!(windowed_body(WINDOWED_SOURCE_BYTES, 0));
    assert!(!windowed_body(0, WINDOWED_SOURCE_BLOCKS - 1));
    assert!(windowed_body(0, WINDOWED_SOURCE_BLOCKS));
}

/// A source far below 64 KiB still goes windowed once
/// it carries 300 blocks — many short paragraphs cost more to lay out than
/// one block of the same bytes.
#[gpui::test]
fn prose_flips_windowed_on_block_count_below_the_size_threshold(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    let source = (0..320)
        .map(|ix| format!("P{ix}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(source.len() < WINDOWED_SOURCE_BYTES);
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: source,
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    cx.run_until_parked();

    assert_eq!(
        prose_windowed_at(&chat, cx, 0),
        Some(true),
        "300+ blocks must engage the windowed layout even below 64 KiB"
    );
    assert_eq!(
        prose_confidence_at(&chat, cx, 0),
        Some(Confidence::Measured),
        "a windowed row's height still converges; it must not record as settled"
    );
    assert!(
        prose_layout_at(&chat, cx, 0).windowed,
        "parsed block-count updates must activate the component without an unrelated redraw"
    );
    assert!(!prose_layout_at(&chat, cx, 0).complete);
}

fn redraw(_chat: &gpui::Entity<ChatView>, cx: &mut gpui::VisualTestContext) {
    super::redraw_settled(cx);
}

fn prose_windowed_at(
    chat: &gpui::Entity<ChatView>,
    cx: &gpui::VisualTestContext,
    from_end: usize,
) -> Option<bool> {
    chat.read_with(cx, |chat, _| {
        let rows = rows_of_kind(chat, RowKind::AssistantProse);
        rows.get(rows.len() - 1 - from_end)
            .and_then(|row| renderer_for_row(chat, row))
            .map(|renderer| renderer.requests_windowed_layout())
    })
}

fn prose_layout_at(
    chat: &gpui::Entity<ChatView>,
    cx: &gpui::VisualTestContext,
    from_end: usize,
) -> crate::ui::markdown::MarkdownLayoutSnapshot {
    chat.read_with(cx, |chat, _| {
        let rows = rows_of_kind(chat, RowKind::AssistantProse);
        let renderer = rows
            .get(rows.len() - 1 - from_end)
            .and_then(|row| renderer_for_row(chat, row))
            .expect("prose renderer");
        let mut layout = None;
        renderer.visit_layout_dependencies(&mut |body| layout = Some(body.layout_snapshot()));
        layout.expect("materialized prose body")
    })
}

fn prose_confidence_at(
    chat: &gpui::Entity<ChatView>,
    cx: &gpui::VisualTestContext,
    from_end: usize,
) -> Option<Confidence> {
    chat.read_with(cx, |chat, _| {
        let rows = rows_of_kind(chat, RowKind::AssistantProse);
        rows.get(rows.len() - 1 - from_end)
            .and_then(|row| row.recorded_confidence())
    })
}

fn paragraphs_over(target_bytes: usize) -> String {
    let mut source = String::new();
    let mut line = 0;
    while source.len() < target_bytes {
        source.push_str(&format!(
            "Windowed threshold paragraph {line} with enough body text to be realistic.\n\n"
        ));
        line += 1;
    }
    source
}

/// A finished prose row below the threshold renders through the natural
/// natural path and records a settled measurement; once its source crosses
/// 64 KiB it flips to windowed and its measurement stops being settled so
/// the materialized window cannot take the still-converging height as final.
#[gpui::test]
fn prose_crosses_into_the_windowed_path_only_above_the_threshold(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Short answer.".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    redraw(&chat, cx);
    cx.run_until_parked();

    assert_eq!(
        prose_windowed_at(&chat, cx, 0),
        Some(false),
        "natural layout below the threshold"
    );
    assert_eq!(
        prose_confidence_at(&chat, cx, 0),
        Some(Confidence::Settled),
        "a settled non-windowed row measures once and stays settled"
    );

    // A canonical part carries no stream id, so the oversized row is staged
    // as its own streaming turn rather than appended onto the first.
    let delta = paragraphs_over(WINDOWED_SOURCE_BYTES);
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::start_text(chat, 0, "text-0".into(), cx);
            test_support::append_text(chat, 0, "text-0".into(), &delta, cx);
            test_support::finish_text(chat, 0, "text-0", None, cx);
        });
    });
    cx.run_until_parked();
    redraw(&chat, cx);
    cx.run_until_parked();

    assert_eq!(
        prose_windowed_at(&chat, cx, 0),
        Some(true),
        "a source past 64 KiB must render through the windowed block layout"
    );
    assert_eq!(
        prose_confidence_at(&chat, cx, 0),
        Some(Confidence::Measured),
        "a windowed row's height still converges; it must not record as settled"
    );
    assert_eq!(
        prose_windowed_at(&chat, cx, 1),
        Some(false),
        "the earlier short row keeps natural layout"
    );
}

/// Reasoning's budgeted viewport is scrollable (the fork ignores the
/// windowed flag there), and its full-text form below the threshold is the
/// natural path.
#[gpui::test]
fn reasoning_full_text_stays_natural_below_the_threshold(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    let source = (0..240)
        .map(|line| format!("Natural reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source,
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    redraw(&chat, cx);
    cx.run_until_parked();

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(&chat, cx);

    cx.update(|_, cx| {
        let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
        assert!(
            !renderer.requests_windowed_layout(),
            "the budgeted viewport is scrollable; the fork ignores windowed there"
        );
    });

    chat.update(cx, |this, _| {
        this.view.list_state.scroll_to(ListOffset::default());
    });
    redraw(&chat, cx);
    let full_toggle = cx
        .debug_bounds("reasoning-full-0")
        .expect("full-text toggle");
    cx.simulate_click(full_toggle.center(), gpui::Modifiers::default());
    redraw(&chat, cx);

    cx.update(|_, cx| {
        let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
        assert!(
            !renderer.requests_windowed_layout(),
            "a below-threshold full-text body uses natural layout"
        );
    });
}

/// The windowed arm of the reasoning full-text form: above the threshold the
/// Full disclosure reports windowed, so its row measurement records as not
/// settled while the block cache converges.
#[gpui::test]
fn reasoning_full_text_flips_windowed_above_the_threshold(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    let source = paragraphs_over(WINDOWED_SOURCE_BYTES);
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            test_support::push_canonical(
                chat,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source,
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
    redraw(&chat, cx);
    cx.run_until_parked();

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(&chat, cx);

    cx.update(|_, cx| {
        let renderer = reasoning_part(chat.read(cx)).expect("reasoning renderer");
        assert!(
            !renderer.requests_windowed_layout(),
            "budgeted viewport is scrollable"
        );
    });

    chat.update(cx, |this, _| {
        this.view.list_state.scroll_to(ListOffset::default());
    });
    redraw(&chat, cx);
    let full_toggle = cx
        .debug_bounds("reasoning-full-0")
        .expect("full-text toggle");
    cx.simulate_click(full_toggle.center(), gpui::Modifiers::default());
    redraw(&chat, cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx);
        let renderer = reasoning_part(turn).expect("reasoning renderer");
        assert!(
            renderer.requests_windowed_layout(),
            "a source past 64 KiB must render through the windowed block layout"
        );
        let confidence = rows_of_kind(turn, RowKind::Reasoning)
            .first()
            .and_then(|row| row.recorded_confidence());
        assert_eq!(
            confidence,
            Some(Confidence::Measured),
            "a windowed row's height still converges; it must not record as settled"
        );
    });
}

#[gpui::test]
fn windowed_prose_becomes_settled_after_every_block_is_measured(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(500.)));
    let source = (0..320)
        .map(|ix| format!("Paragraph {ix}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    chat.update(cx, |chat, cx| {
        test_support::push_canonical(
            chat,
            LlmMessage {
                role: crate::llm::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: source,
                    provider_metadata: ProviderMetadata::default(),
                }],
                provider_metadata: ProviderMetadata::default(),
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(prose_layout_at(&chat, cx, 0).windowed);
    assert!(!prose_layout_at(&chat, cx, 0).complete);
    assert_eq!(
        prose_confidence_at(&chat, cx, 0),
        Some(Confidence::Measured)
    );

    cx.simulate_resize(gpui::size(px(900.), px(30000.)));
    cx.run_until_parked();
    assert!(prose_layout_at(&chat, cx, 0).windowed);
    assert!(prose_layout_at(&chat, cx, 0).complete);
    assert_eq!(prose_confidence_at(&chat, cx, 0), Some(Confidence::Settled));
}
