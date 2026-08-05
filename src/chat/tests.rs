use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    IntoElement as _, ListOffset, Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent,
    TestAppContext, point, px,
};
use gpui_component::input::InputEvent;

use crate::llm::{
    ContentBlock, IndexedContentBlock, IndexedMessage, Message as LlmMessage, ProviderMetadata,
    ResponsesReplayMetadata,
};
use crate::preferences;

use super::{
    CONTENT_MAX_WIDTH, ChatEvent, ChatView, Message, MessagePart, ReasoningTrace, Role,
    SMOOTH_SCROLL_FINISH_THRESHOLD, SMOOTH_SCROLL_FRAME_FRACTION, STICK_THRESHOLD,
    SmoothScrollState, is_replayable, reasoning_smooth_invalidations,
    reset_reasoning_smooth_invalidations,
};

#[test]
fn smooth_scroll_state_eases_and_accumulates_wheel_distance() {
    let mut state = SmoothScrollState::default();
    state.enqueue(px(240.));

    let first = state.next_step().expect("a queued scroll has a first step");
    assert_eq!(first, px(240. * SMOOTH_SCROLL_FRAME_FRACTION));
    assert!(state.remaining < px(240.));

    state.enqueue(px(-60.));
    let mut applied = first;
    let mut frames = 1;
    while let Some(step) = state.next_step() {
        applied += step;
        frames += 1;
        if frames > 100 {
            panic!("smooth scroll did not converge");
        }
    }

    assert!((applied - px(180.)).as_f32().abs() < 0.01);
    assert!(state.remaining.as_f32().abs() <= SMOOTH_SCROLL_FINISH_THRESHOLD.as_f32());

    state.enqueue(px(2_400.));
    assert_eq!(state.remaining, px(2_400.));
}

/// A completed user turn plus the assistant placeholder a reply streams
/// into. Pushed directly rather than through `submit`, which is gated on a
/// configured provider that a unit test has no reason to stand up.
fn seed_turn(chat: &gpui::Entity<ChatView>, cx: &mut gpui::VisualTestContext) {
    cx.update(|_, cx| {
        chat.update(cx, |this, _cx| {
            for role in [Role::User, Role::Assistant] {
                this.messages.push(Message::empty(role));
            }
        });
    });
}

fn reasoning_part(message: &Message) -> Option<&ReasoningTrace> {
    message.parts.iter().find_map(|part| match part {
        MessagePart::Reasoning {
            trace: Some(trace), ..
        } => Some(trace),
        _ => None,
    })
}

fn reasoning_part_mut(message: &mut Message) -> Option<&mut ReasoningTrace> {
    message.parts.iter_mut().find_map(|part| match part {
        MessagePart::Reasoning {
            trace: Some(trace), ..
        } => Some(trace),
        _ => None,
    })
}

fn reasoning_parts(message: &Message) -> Vec<&ReasoningTrace> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Reasoning {
                trace: Some(trace), ..
            } => Some(trace),
            _ => None,
        })
        .collect()
}

fn reasoning_states(message: &Message) -> Vec<(&str, bool)> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Reasoning {
                reasoning,
                finished,
                ..
            } => Some((reasoning.display.as_str(), *finished)),
            _ => None,
        })
        .collect()
}

fn init_app(cx: &mut TestAppContext) {
    let prefs = preferences::Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        preferences::init_global(prefs, cx);
    });
}

fn add_chat_window(
    cx: &mut TestAppContext,
) -> (gpui::Entity<ChatView>, &mut gpui::VisualTestContext) {
    let (root, cx) = cx.add_window_view(|window, cx| {
        let chat = ChatView::view(window, cx);
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ChatView>()
            .expect("Root must contain the ChatView")
    });
    (chat, cx)
}

fn redraw(cx: &mut gpui::VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
}

fn redraw_settled_math(cx: &mut gpui::VisualTestContext) {
    redraw(cx);
    // Formula generation and SVG rasterization are deliberately performed on
    // the background executor. Drain that work and draw once more so visual
    // assertions observe the settled image rather than the text fallback.
    redraw(cx);
}

fn measured_redraw(
    cx: &mut gpui::VisualTestContext,
) -> (std::time::Duration, crate::ui::markdown::MarkdownPerfProbe) {
    crate::ui::markdown::reset_perf_probe();
    let started = Instant::now();
    redraw(cx);
    (started.elapsed(), crate::ui::markdown::perf_probe())
}

fn duration_percentile(samples: &[Duration], percentile: usize) -> Duration {
    assert!(
        !samples.is_empty(),
        "a percentile needs at least one sample"
    );
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) * percentile.min(100)) / 100;
    sorted[index]
}

/// Agent-runnable feedback loop for the user-visible long-content stall.
///
/// Keep this test deterministic: elapsed time is diagnostic, while the bound
/// on continuous code-text elements is the pass/fail signal. A long block must
/// not be rebuilt line-by-line for disclosure, wrap, and every smooth-scroll frame.
#[gpui::test]
fn long_content_performance_feedback_loop(cx: &mut TestAppContext) {
    const CODE_LINES: usize = 640;
    const MAX_CODE_TEXT_ELEMENTS_PER_INTERACTION: usize = 128;
    const MAX_TEXT_VIEW_BUILDS_PER_INTERACTION: usize = 1;
    const MAX_CODE_BLOCK_RENDERS_PER_INTERACTION: usize = 1;
    const MAX_SMOOTH_INVALIDATIONS: usize = 24;

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(560.)));

    let prose = (0..160)
        .map(|line| format!("Reasoning paragraph {line} with enough text to exercise layout."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let code = (0..CODE_LINES)
        .map(|line| {
            format!(
                "let value_{line} = compute_really_long_identifier_{line}({line}, \"payload-{line}\");"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Keep the code in the first virtualized Markdown block. Completed
    // reasoning intentionally opens at the document top, so placing the code
    // at the tail would let this fixture pass without drawing the expensive
    // path it is meant to guard.
    let source = format!("```rust\n{code}\n```\n\n{prose}\n\nFinal reasoning paragraph.");
    let fence_start = source.find("```rust").expect("fixture fence");

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = true;
            prefs.smooth_chat_scrolling = false;
        });
        chat.update(cx, |chat, cx| {
            chat.messages.push(Message::from_canonical(
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
            ));
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("reasoning turn");
        let MessagePart::Reasoning { ui_id, .. } = &turn.parts[0] else {
            panic!("reasoning part")
        };
        *ui_id
    });
    let wrap_selector: &'static str =
        Box::leak(format!("markdown-code-wrap-{owner_id}-{fence_start}").into_boxed_str());

    crate::ui::markdown::reset_perf_probe();
    let expand_started = Instant::now();
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    let (expand_draw, expand_probe) = measured_redraw(cx);
    let expand_elapsed = expand_started.elapsed();

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("expanded reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    let reopen_started = Instant::now();
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed reasoning trigger after first expansion");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    let (reopen_draw, reopen_probe) = measured_redraw(cx);
    let reopen_elapsed = reopen_started.elapsed();

    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control in expanded reasoning");
    crate::ui::markdown::reset_perf_probe();
    let wrap_started = Instant::now();
    cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    let (wrap_draw, wrap_probe) = measured_redraw(cx);
    let wrap_elapsed = wrap_started.elapsed();

    crate::ui::markdown::reset_perf_probe();
    let unwrap_started = Instant::now();
    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control after enabling wrapping");
    cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    let (unwrap_draw, unwrap_probe) = measured_redraw(cx);
    let unwrap_elapsed = unwrap_started.elapsed();

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
    });
    let combined_wrap_started = Instant::now();
    let wrap = cx
        .debug_bounds(wrap_selector)
        .expect("wrap control before combined smooth scenario");
    cx.simulate_click(wrap.center(), gpui::Modifiers::default());
    let (combined_wrap_draw, combined_wrap_probe) = measured_redraw(cx);
    let combined_wrap_elapsed = combined_wrap_started.elapsed();

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("expanded reasoning viewport");
    reset_reasoning_smooth_invalidations();
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    let mut smooth_draws = Vec::new();
    let mut smooth_probes = Vec::new();
    for _ in 0..64 {
        let remaining = cx.update(|_, cx| {
            let turn = chat.read(cx).messages.last().expect("reasoning turn");
            reasoning_part(turn)
                .expect("reasoning trace")
                .smooth_scroll_remaining()
        });
        if remaining == px(0.) {
            break;
        }
        assert!(
            cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
            "queued reasoning motion must have a scheduled frame"
        );
        let (draw, probe) = measured_redraw(cx);
        smooth_draws.push(draw);
        smooth_probes.push(probe);
    }
    assert!(
        !smooth_draws.is_empty(),
        "long reasoning must schedule smooth-scroll frames"
    );
    assert!(
        cx.update(|_, cx| {
            let turn = chat.read(cx).messages.last().expect("reasoning turn");
            reasoning_part(turn)
                .expect("reasoning trace")
                .smooth_scroll_remaining()
                == px(0.)
        }),
        "smooth scrolling must converge within the fixture's frame budget"
    );
    let smooth_invalidations = reasoning_smooth_invalidations();
    assert_eq!(
        smooth_invalidations,
        smooth_draws.len(),
        "each easing step must invalidate the view exactly once"
    );
    assert!(
        smooth_invalidations <= MAX_SMOOTH_INVALIDATIONS,
        "smooth scrolling used {smooth_invalidations} invalidations for one wheel gesture"
    );
    let smooth_p50 = duration_percentile(&smooth_draws, 50);
    let smooth_p95 = duration_percentile(&smooth_draws, 95);
    let smooth_max = smooth_draws.iter().copied().max().unwrap_or_default();
    let smooth_probe = smooth_probes
        .iter()
        .copied()
        .max_by_key(|probe| probe.code_text_elements)
        .unwrap_or_default();

    eprintln!(
        "LONG_CONTENT_PERF expand_total={expand_elapsed:?} expand_draw={expand_draw:?} \
         expand={expand_probe:?} reopen_total={reopen_elapsed:?} reopen_draw={reopen_draw:?} \
         reopen={reopen_probe:?} wrap_total={wrap_elapsed:?} wrap_draw={wrap_draw:?} \
         wrap={wrap_probe:?} unwrap_total={unwrap_elapsed:?} unwrap_draw={unwrap_draw:?} \
         unwrap={unwrap_probe:?} combined_wrap_total={combined_wrap_elapsed:?} \
         combined_wrap_draw={combined_wrap_draw:?} combined_wrap={combined_wrap_probe:?} \
         smooth_frames={} smooth_p50={smooth_p50:?} smooth_p95={smooth_p95:?} \
         smooth_max={smooth_max:?} smooth={smooth_probe:?} \
         smooth_invalidations={smooth_invalidations}",
        smooth_draws.len()
    );

    let failures = [
        ("reasoning expansion", expand_probe),
        ("reasoning reopen", reopen_probe),
        ("code wrap toggle", wrap_probe),
        ("code unwrap toggle", unwrap_probe),
        ("combined code wrap toggle", combined_wrap_probe),
        ("smooth-scroll frame", smooth_probe),
    ]
    .into_iter()
    .filter(|(_, probe)| probe.code_text_elements > MAX_CODE_TEXT_ELEMENTS_PER_INTERACTION)
    .map(|(operation, probe)| {
        format!(
            "{operation} materialized {} code-text elements",
            probe.code_text_elements
        )
    })
    .collect::<Vec<_>>();

    assert!(
        failures.is_empty(),
        "long-content work must be bounded independently of all {CODE_LINES} code lines: {}",
        failures.join("; ")
    );

    for (operation, probe) in [
        ("reasoning expansion", expand_probe),
        ("reasoning reopen", reopen_probe),
        ("code wrap toggle", wrap_probe),
        ("code unwrap toggle", unwrap_probe),
        ("combined code wrap toggle", combined_wrap_probe),
        ("smooth-scroll frame", smooth_probe),
    ] {
        assert!(
            probe.text_view_builds <= MAX_TEXT_VIEW_BUILDS_PER_INTERACTION,
            "{operation} rebuilt {} text views",
            probe.text_view_builds
        );
        assert!(
            probe.code_block_renders <= MAX_CODE_BLOCK_RENDERS_PER_INTERACTION,
            "{operation} reran {} code block renderers",
            probe.code_block_renders
        );
        assert_eq!(
            probe.code_text_elements, probe.code_block_renders,
            "{operation} must build exactly one continuous code-text element per rendered block"
        );
    }
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).materialized_message_indices.clone()),
        std::collections::BTreeSet::from([0]),
        "the combined fixture must materialize only its visible transcript row"
    );

    if !cfg!(debug_assertions) {
        assert!(
            expand_elapsed <= Duration::from_millis(50),
            "release reasoning expansion must stay below the frozen 50 ms guard: {expand_elapsed:?}"
        );
        assert!(
            reopen_elapsed <= Duration::from_millis(50),
            "release reasoning reopen must stay below the frozen 50 ms guard: {reopen_elapsed:?}"
        );
        assert!(
            wrap_draw <= Duration::from_micros(12_900),
            "release wrap draw must stay at least 30% below the 18.45 ms baseline: {wrap_draw:?}"
        );
        assert!(
            unwrap_draw <= Duration::from_micros(12_900),
            "release unwrap draw must stay below the frozen wrap guard: {unwrap_draw:?}"
        );
        assert!(
            combined_wrap_draw <= Duration::from_micros(12_900),
            "release combined wrap draw must stay below the frozen wrap guard: {combined_wrap_draw:?}"
        );
        assert!(
            smooth_p95 <= Duration::from_micros(12_600),
            "release smooth draw p95 must stay at least 30% below the 18.03 ms baseline: {smooth_p95:?}"
        );
    }
}

#[gpui::test]
fn smooth_scrolling_defers_discrete_wheel_movement_when_enabled(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.smooth_chat_scrolling = true);
        chat.update(cx, |chat, cx| {
            for index in 0..20 {
                chat.messages.push(Message::from_canonical(
                    LlmMessage {
                        role: crate::llm::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: format!("message {index}\n\n{}", "body ".repeat(30)),
                            provider_metadata: ProviderMetadata::default(),
                        }],
                        provider_metadata: ProviderMetadata::default(),
                    },
                    cx,
                ));
            }
            chat.sync_message_list_count();
            chat.list_state.scroll_to(ListOffset::default());
        });
    });
    redraw(cx);

    let before = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    cx.simulate_event(ScrollWheelEvent {
        position: point(px(320.), px(100.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });

    let deferred = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    let pending = cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining);
    assert!(pending > px(0.), "wheel event must queue smooth motion");
    assert_eq!(deferred.item_ix, before.item_ix);
    assert_eq!(deferred.offset_in_item, before.offset_in_item);

    assert!(
        cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
        "the wheel event must schedule an animation frame"
    );
    redraw(cx);
    let eased = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
    assert!(
        eased.item_ix > before.item_ix || eased.offset_in_item > before.offset_in_item,
        "an animation frame must advance the deferred wheel distance"
    );
}

#[test]
fn complete_syntax_registry_supports_common_fence_aliases() {
    let registry = gpui_component::highlighter::LanguageRegistry::singleton();
    for language in [
        "rust",
        "rs",
        "python",
        "py",
        "javascript",
        "js",
        "typescript",
        "ts",
        "bash",
        "sh",
    ] {
        assert!(
            registry.language(language).is_some(),
            "{language} must resolve through the story crate's language set"
        );
    }

    let code = "fn main() { println!(\"highlighted\"); }";
    let rope = gpui_component::Rope::from(code);
    let mut highlighter = gpui_component::highlighter::SyntaxHighlighter::new("rust");
    assert!(highlighter.update(None, &rope, None));
    assert!(
        !highlighter
            .styles(
                &(0..code.len()),
                &gpui_component::highlighter::HighlightTheme::default_dark(),
            )
            .is_empty(),
        "a registered grammar must produce token styles, not just monospace text"
    );
}

#[gpui::test]
fn streamed_fences_never_fall_back_to_the_native_code_block_renderer(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(1600.), px(3000.)));
    seed_turn(&chat, cx);

    let first = "这里为你提供三段符合要求的 Python 代码：\n\n### 第一段：正常输出\n```python\n# 这是一段简单的打印代码\nname = \"世界\"\nprint(f\"你好，{name}！\")\n```";
    let second = "\n\n### 第二段：内容很长\n```python\n# 这是一段通过拼接长字符串实现的代码，模拟数据量较大的处理场景\nlong_data = \"数据节点\" * 1000 + \"，处理过程正在进行中...\" + \"状态码：200\" * 500\nresult = f\"完整日志信息如下：{long_data}\"\nprint(f\"输出字符串长度为：{len(result)}\")\nprocess_log = [f\"Item_{i}\" for i in range(1000)]\nprint(f\"已处理列表长度：{len(process_log)}\")\n```";
    let third = "\n\n### 第三段：每行都很长\n```python\nextremely_long_variable_name_to_demonstrate_the_horizontal_length_of_the_code_block = \"这是一段非常长的字符串\" + \"重复拼接\" * 20\ndef extremely_complex_and_redundant_function_with_too_many_parameters_to_fit_on_a_standard_screen(param1_that_is_very_long, param2_that_is_also_very_long): return [f\"value_{i}\" for i in range(10)]\nprint(extremely_long_variable_name_to_demonstrate_the_horizontal_length_of_the_code_block)\n```";
    let id = "streamed-python".to_string();

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    // The first draw installs the custom extensions. Before the regression
    // fix, a later background append resumed from its older native parse.
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let (owner_id, first_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, text.find("```").expect("first fence"))
    });
    let first_header: &'static str =
        Box::leak(format!("markdown-code-header-{owner_id}-{first_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(first_header).is_some(),
        "the first draw must already install the custom fenced-code renderer"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id.clone(), second, cx);
        });
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds(first_header).is_some(),
        "later appends must not replace the first custom fence"
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, third, cx);
        });
    });
    redraw(cx);

    let (owner_id, source) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, text.clone())
    });
    let fences = source
        .match_indices("```")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    assert_eq!(fences.len(), 6);

    for start in fences.into_iter().step_by(2) {
        let header: &'static str =
            Box::leak(format!("markdown-code-header-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(header).is_some(),
            "every streamed fence must use Nostra's renderer, including the first at {start}"
        );
    }
}

#[gpui::test]
fn streamed_indented_code_keeps_the_native_renderer(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-indented-code".to_string();

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), "    print('first')", cx);
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        *ui_id
    });
    let header: &'static str =
        Box::leak(format!("markdown-code-header-{owner_id}-0").into_boxed_str());
    assert!(
        cx.debug_bounds(header).is_none(),
        "four-space indented code must not receive the fenced-code toolbar"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, "\n    print('second')", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds(header).is_none(),
        "later appends must keep indented code on the native renderer"
    );
    let rendered_text = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[1].parts[0] else {
                panic!("streamed assistant text")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(
        rendered_text.trim(),
        "print('first')\nprint('second')",
        "native rendering must preserve every streamed code line"
    );
}

#[gpui::test]
fn raw_html_dollars_remain_literal_and_selectable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let source = "<kbd>$raw$</kbd>";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, "raw-html-math".to_string(), cx);
            this.append_stream_text(0, "raw-html-math".to_string(), source, cx);
        });
    });
    redraw(cx);

    let (owner_id, start, selected) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text {
                ui_id, text, body, ..
            } = &this.messages[1].parts[0]
            else {
                panic!("streamed assistant text")
            };
            (
                *ui_id,
                text.find("$raw$").expect("literal dollars"),
                body.select_all_text(cx),
            )
        })
    });
    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula_selector).is_none(),
        "raw HTML text must not be claimed by the math renderer"
    );
    assert_eq!(selected.trim(), "$raw$");
}

#[gpui::test]
fn streamed_math_keeps_custom_nodes_after_later_appends(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-math".to_string();
    let first = "根据定义：\n$$\nE = mc^2\n$$";
    let second = "\n继续说明 $x^2$ 的含义。";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    redraw_settled_math(cx);

    let (owner_id, display_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, text.find("$$").expect("display formula"))
    });
    let display: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{display_start}").into_boxed_str());
    assert!(cx.debug_bounds(display).is_some());

    // The append parser runs from its own document snapshot. This second
    // delta verifies that installing the math extensions on the first draw
    // cannot later restore a native paragraph and erase the custom formula.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, second, cx);
        });
    });
    redraw_settled_math(cx);

    let inline_start = cx.update(|_, cx| {
        let MessagePart::Text { text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        text.find("$x^2$").expect("inline formula")
    });
    let inline: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{inline_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(display).is_some(),
        "a later append must preserve the earlier display formula"
    );
    assert!(
        cx.debug_bounds(inline).is_some(),
        "the appended inline formula must use the custom renderer"
    );
}

#[gpui::test]
fn incomplete_streamed_math_keeps_stable_text_visible_and_recovers(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-incomplete-math".to_string();
    let chunks = [
        "先说明结论。\n\n",
        "$$",
        "\nE = mc^2",
        "\n$$",
        "\n\n最后补充内联公式 $E = mc^2$ 与文字。",
    ];
    let mut expected = String::new();

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
        });
    });

    for chunk in chunks {
        expected.push_str(chunk);
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                this.append_stream_text(0, id.clone(), chunk, cx);
            });
        });
        redraw_settled_math(cx);

        let selected = cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                let MessagePart::Text { body, .. } = &this.messages[1].parts[0] else {
                    panic!("streamed assistant text")
                };
                body.select_all_text(cx)
            })
        });
        for line in expected.lines().filter(|line| !line.is_empty()) {
            assert!(
                selected.contains(line),
                "every streamed prefix must remain visible: missing {line:?} in {selected:?}"
            );
        }
    }

    let (owner_id, display_start, inline_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (
            *ui_id,
            text.find("$$").expect("display formula"),
            text.rfind("$E = mc^2$").expect("inline formula"),
        )
    });
    for start in [display_start, inline_start] {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "the completed formula at {start} must recover its custom renderer"
        );
    }
}

#[gpui::test]
fn streamed_reference_definitions_prepare_later_formula_labels(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-reference-math".to_string();
    let first = r"[label \(x\)]: https://example.com/collapsed
[shortcut \(y\)]: https://example.com/shortcut
[full-math]: https://example.com/full
[Cost $5 and \(w\)]: https://example.com/currency

retained";
    let second = r"

[label \(x\)][]
[shortcut \(y\)]
[full \(z\)][full-math]
[Cost $5 and \(w\)][]";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    redraw(cx);

    let (owner_id, body_id) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, body, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, body.entity_id())
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, second, cx);
        });
    });
    redraw_settled_math(cx);

    for (formula_source, destination) in [
        (r"\(x\)", "https://example.com/collapsed"),
        (r"\(y\)", "https://example.com/shortcut"),
        (r"\(z\)", "https://example.com/full"),
        (r"\(w\)", "https://example.com/currency"),
    ] {
        let formula_start = first.len()
            + second
                .find(formula_source)
                .expect("appended reference formula");
        let formula: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
        let bounds = cx
            .debug_bounds(formula)
            .expect("appended reference formula bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some(destination));
    }

    let selected = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { text, body, .. } = &this.messages[1].parts[0] else {
                panic!("streamed assistant text")
            };
            assert_eq!(body.entity_id(), body_id);
            assert_eq!(text, &format!("{first}{second}"));
            body.select_all_text(cx)
        })
    });
    assert_eq!(
        selected.trim(),
        "retained\nlabel \\(x\\)\nshortcut \\(y\\)\nfull \\(z\\)\nCost $5 and \\(w\\)"
    );
}

#[gpui::test]
fn streaming_does_not_resolve_a_latent_prepared_reference(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-latent-reference".to_string();
    let first = "[label \\(x\\)]: https://example.com/slash\n\nretained";
    let second = "\n\n[label \\(x\\)][] and [$a$][label \\(x\\)] and [label $$x$$][]";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    redraw(cx);
    let (owner_id, body_id) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, body, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, body.entity_id())
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, second, cx);
        });
    });
    redraw_settled_math(cx);

    let latent_start = first.len() + second.find("$$x$$").expect("latent formula");
    let latent: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{latent_start}").into_boxed_str());
    let latent_bounds = cx.debug_bounds(latent).expect("latent formula bounds");
    cx.simulate_click(latent_bounds.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url(),
        None,
        "preparation must not turn unresolved reference syntax into a link"
    );

    let linked_start = first.len() + second.find("$a$").expect("linked formula");
    let linked: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{linked_start}").into_boxed_str());
    let linked_bounds = cx.debug_bounds(linked).expect("linked formula bounds");
    cx.simulate_click(linked_bounds.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/slash")
    );

    cx.update(|_, cx| {
        let MessagePart::Text { text, body, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        assert_eq!(body.entity_id(), body_id);
        assert_eq!(text, &format!("{first}{second}"));
    });
}

#[gpui::test]
fn streaming_does_not_block_the_next_turn_model_selection(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    let next = crate::llm::ModelSelection {
        profile_id: "next-provider".into(),
        model_id: "next-model".into(),
    };
    let observed = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        let observed = observed.clone();
        cx.subscribe(&chat, move |_, event: &ChatEvent, _| {
            if let ChatEvent::SelectionChanged(selection) = event {
                observed.borrow_mut().push(selection.clone());
            }
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.pending = true;
            // Drive the public command path, not the state projection helper:
            // this also proves persistence and SelectionChanged emission stay
            // enabled while the current generation remains pending.
            this.select_model(next.clone(), cx);
            assert!(
                this.pending,
                "switching models must not alter the active reply"
            );
            assert_eq!(this.selection.as_ref(), Some(&next));
        });
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        assert_eq!(crate::providers::last_selection(cx), Some(next.clone()));
    });
    assert_eq!(
        observed.borrow().as_slice(),
        std::slice::from_ref(&next),
        "the owning ChatApp receives exactly one selection synchronization event"
    );
}

#[gpui::test]
fn block_math_after_markdown_text_is_rendered_as_its_own_block(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(1200.)));
    let markdown = "上文\n\n$$\n\\sum_{n=1}^{\\infty} \\frac{1}{n^2}\n$$\n\n下文";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$$").expect("block math delimiter"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "block math must produce a dedicated rendered formula element"
    );
    let formula_bounds = cx.debug_bounds(formula).expect("formula bounds");
    assert!(
        formula_bounds.size.width > px(0.) && formula_bounds.size.height > px(0.),
        "block formula must have visible layout bounds: {formula_bounds:?}"
    );
    let row: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-{formula_start}").into_boxed_str());
    let row_bounds = cx.debug_bounds(row).expect("display formula row bounds");
    assert!(
        (formula_bounds.center().x - row_bounds.center().x).abs() < px(1.),
        "a display formula narrower than its viewport must stay centered: {formula_bounds:?} vs {row_bounds:?}"
    );

    let content: &'static str =
        Box::leak("assistant-message-content-0".to_string().into_boxed_str());
    let content_bounds = cx.debug_bounds(content).expect("assistant content bounds");
    assert!(
        formula_bounds.top() >= content_bounds.top()
            && formula_bounds.bottom() <= content_bounds.bottom(),
        "display formula must be contained by the assistant content: {formula_bounds:?} vs {content_bounds:?}"
    );
}

#[gpui::test]
fn standalone_same_line_display_math_is_centered_as_a_block(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(900.)));
    let markdown = "上文\n\n$$x^2 + y^2$$\n\n下文";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    let row: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-{formula_start}").into_boxed_str());
    let formula_bounds = cx.debug_bounds(formula).expect("display formula bounds");
    let row_bounds = cx
        .debug_bounds(row)
        .expect("standalone display formula must use the block row");
    assert!(
        (formula_bounds.center().x - row_bounds.center().x).abs() < px(1.),
        "standalone display math must be centered: {formula_bounds:?} vs {row_bounds:?}"
    );
}

#[gpui::test]
fn blockquote_display_math_uses_container_free_formula_source(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "> $$\n> x^2 + y^2\n> $$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "blockquote container markers must not be passed to RaTeX"
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "$$\nx^2 + y^2\n$$");
}

#[gpui::test]
fn list_display_math_uses_container_free_formula_source(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "- $$\n  x^2 + y^2\n  $$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "list display math must render from its container-free AST value"
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "$$\nx^2 + y^2\n$$");
}

#[gpui::test]
fn display_formula_participates_in_reverse_drag_and_copy(cx: &mut TestAppContext) {
    use gpui_component::WindowExt as _;

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$$\nx^2 + y^2\n$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let formula = cx.debug_bounds(formula_selector).expect("formula bounds");
    let right = point(formula.right() - px(1.), formula.center().y);
    let left = point(formula.left() + px(1.), formula.center().y);
    cx.simulate_mouse_down(right, MouseButton::Left, Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_move(left, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_up(left, MouseButton::Left, Modifiers::default());
    redraw(cx);

    let selected = cx.update(|window, cx| window.selected_text(cx));
    assert_eq!(selected.trim(), markdown);

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn adjacent_markdown_and_display_math_keep_their_ordered_inline_flow(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "根据**定义**：\n$$\nE = mc^2\n$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "native Markdown blocks must retain the adjacent display formula"
    );

    // A second draw exercises the same TextView and display-flow state rather
    // than rebuilding nested Markdown around the formula.
    redraw(cx);
    assert!(cx.debug_bounds(formula).is_some());

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "根据定义：\n$$\nE = mc^2\n$$");
}

#[gpui::test]
fn inline_math_does_not_turn_markdown_marks_into_literal_text(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(240.), px(1200.)));
    let markdown = "**Bold** before $x^2$ and after";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$x^2$").expect("inline math"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "inline math must produce a dedicated rendered formula element"
    );
    let formula_bounds = cx.debug_bounds(formula).expect("formula bounds");
    let content_bounds = cx
        .debug_bounds("assistant-message-content-0")
        .expect("assistant content bounds");
    assert!(
        formula_bounds.size.width > px(0.) && formula_bounds.size.height > px(0.),
        "inline formula must have visible layout bounds: {formula_bounds:?}"
    );
    assert!(
        formula_bounds.left() >= content_bounds.left()
            && formula_bounds.right() <= content_bounds.right(),
        "wrapped inline formula must remain inside native Markdown content: {formula_bounds:?} vs {content_bounds:?}"
    );
}

#[gpui::test]
fn streamed_list_continuation_keeps_surrounding_math_renderable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let markdown = "before $a$\n\n- item\n    $$\n    x^2\n    $$\n\nafter $b$";
    let id = "streamed-list-continuation-math".to_string();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.start_stream_text(0, id.clone(), cx));
    });
    for chunk in markdown.split_inclusive('\n') {
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                this.append_stream_text(0, id.clone(), chunk, cx);
            });
        });
        redraw(cx);
    }
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let snapshots = crate::ui::math::formula_cache_snapshots(owner_id);
    assert_eq!(
        snapshots
            .iter()
            .map(|(_, snapshot)| (snapshot.source.as_str(), snapshot.inline, snapshot.ready))
            .collect::<Vec<_>>(),
        vec![("a", true, true), ("x^2", false, true), ("b", true, true)],
        "every formula around a four-space list continuation must settle: {snapshots:#?}",
    );
}

#[gpui::test]
fn pending_quoted_display_does_not_downgrade_stable_math(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(2000.)));
    seed_turn(&chat, cx);
    let id = "quoted-pending-math".to_string();
    let source = "$$\na\n$$\n\nbefore $s_0$\n\n> quote\n> $$\n> y^2\n> $$\n";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.start_stream_text(0, id.clone(), cx));
    });
    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    for chunk in source.split_inclusive('\n') {
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                this.append_stream_text(0, id.clone(), chunk, cx);
            });
        });
        redraw(cx);
    }
    redraw_settled_math(cx);

    let terminal = IndexedMessage::from_message(LlmMessage {
        role: crate::llm::Role::Assistant,
        content: vec![ContentBlock::Text {
            text: source.to_string(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    });
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.finish_reply(Some(terminal), None, cx));
    });
    redraw_settled_math(cx);

    let terminal_owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    assert_eq!(terminal_owner_id, owner_id);
    let start = source.find("$s_0$").expect("stable inline formula");
    let selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
    assert!(
        cx.debug_bounds(selector).is_some(),
        "the terminal snapshot must retain stable math rendered before a quoted pending display"
    );
}

#[gpui::test]
fn heading_and_inline_display_math_use_distinct_formula_nodes(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "# 标题 $x^2$\n\n块：$$\\int_0^1 x dx$$ 和 $$y^2$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let starts = [
        markdown.find("$x^2$").expect("heading formula"),
        markdown.find("$$\\int").expect("first inline display"),
        markdown.rfind("$$y^2$$").expect("second inline display"),
    ];
    for start in starts {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "formula at {start} was not rendered"
        );
    }
}

#[gpui::test]
fn formula_rendering_inherits_native_marks_and_heading_weight(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$x+y+z$\n\n***~~$x+y+z$~~***\n\n# $x+y+z$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let starts = [
        markdown.find('$').expect("plain formula"),
        markdown.find("$x+y+z$~~").expect("marked formula"),
        markdown.rfind('$').expect("heading formula") - "$x+y+z".len(),
    ];
    let bounds = starts.map(|start| {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        cx.debug_bounds(selector).expect("rendered formula bounds")
    });

    assert!(
        bounds[1].size != bounds[0].size,
        "bold/italic/strikethrough marks must affect the rendered formula: {bounds:?}"
    );
    assert!(
        bounds[2].size.height > bounds[0].size.height,
        "the native heading size/weight must reach the formula renderer: {bounds:?}"
    );
    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "$x+y+z$\n$x+y+z$\n$x+y+z$");
}

#[gpui::test]
fn theme_switch_keeps_markdown_state_and_regenerates_formula_color(cx: &mut TestAppContext) {
    init_app(cx);
    cx.update(|cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
    });
    let (chat, cx) = add_chat_window(cx);
    let markdown = "theme-aware $x+y$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start, body_id) = cx.update(|_, cx| {
        let MessagePart::Text {
            ui_id, text, body, ..
        } = &chat.read(cx).messages[0].parts[0]
        else {
            panic!("assistant text part")
        };
        (
            *ui_id,
            text.find("$x+y$").expect("formula"),
            body.entity_id(),
        )
    });
    let before = crate::ui::math::formula_cache_snapshot(owner_id, formula_start)
        .expect("settled dark-theme formula cache");
    assert!(before.ready);
    assert_eq!(before.source, "x+y");
    assert!(before.inline);

    // `Theme::change` avoids persisting into the user's real preferences.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    redraw_settled_math(cx);

    let after = crate::ui::math::formula_cache_snapshot(owner_id, formula_start)
        .expect("settled light-theme formula cache");
    assert!(after.ready);
    assert_eq!(after.source, before.source);
    assert_eq!(after.inline, before.inline);
    assert_ne!(
        after.color, before.color,
        "the resolved text color must change"
    );
    assert!(
        after.generation > before.generation,
        "the keyed formula cache must regenerate after its color fingerprint changes: {before:?} -> {after:?}"
    );
    cx.update(|_, cx| {
        let MessagePart::Text { body, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        assert_eq!(
            body.entity_id(),
            body_id,
            "theme changes must not replace the authoritative Markdown state"
        );
    });
}

#[gpui::test]
fn failed_inline_and_display_formulas_remain_selectable_fallbacks(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = concat!(
        "inline $\\includegraphics{missing.png}$\n\n",
        "$$\n\\includegraphics{missing.png}\n$$",
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, inline_start, display_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (
            *ui_id,
            text.find("$\\includegraphics").expect("inline fallback"),
            text.find("$$\n").expect("display fallback"),
        )
    });
    for start in [inline_start, display_start] {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_none(),
            "an unsupported formula must stay on its selectable text fallback"
        );
    }
    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(
        selected_all.trim(),
        "inline $\\includegraphics{missing.png}$\n$$\n\\includegraphics{missing.png}\n$$"
    );
}

#[gpui::test]
fn currency_closing_context_stays_native_and_selectable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "Cost $5 and$10 today";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, dollar_start, selected_all) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text {
                ui_id, text, body, ..
            } = &this.messages[0].parts[0]
            else {
                panic!("assistant text part")
            };
            (
                *ui_id,
                text.find('$').expect("currency dollar"),
                body.select_all_text(cx),
            )
        })
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{dollar_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_none(),
        "a closing dollar followed by a digit must not become a formula"
    );
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn inline_formula_participates_in_reverse_drag_and_copy(cx: &mut TestAppContext) {
    use gpui_component::WindowExt as _;

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "before $x^2$ after";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$x^2$").expect("inline formula"))
    });

    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    let formula = cx.debug_bounds(formula_selector).expect("formula bounds");

    let formula_right = point(formula.right() - px(1.), formula.center().y);
    let formula_left = point(formula.left() + px(1.), formula.center().y);
    cx.simulate_mouse_down(formula_right, MouseButton::Left, Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_move(formula_left, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    let selected_inside_formula = cx.update(|window, cx| window.selected_text(cx));
    assert!(
        selected_inside_formula.contains("$x^2$"),
        "dragging inside the atomic formula did not select it: {selected_inside_formula:?}"
    );
    let text_endpoint = point(formula.left() - px(2.), formula.center().y);
    cx.simulate_mouse_move(text_endpoint, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_up(text_endpoint, MouseButton::Left, Modifiers::default());
    redraw(cx);

    let selected = cx.update(|window, cx| window.selected_text(cx));
    assert!(
        selected.contains("$x^2$"),
        "reverse drag dropped the atomic formula fallback: {selected:?}"
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn display_formula_drag_preserves_interleaved_markdown_order(cx: &mut TestAppContext) {
    use gpui_component::WindowExt as _;

    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$$\na\n$$\nmiddle\n$$\nb\n$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, second_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.rfind("$$\nb").expect("second formula"))
    });
    let first_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let second_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{second_start}").into_boxed_str());
    let first = cx.debug_bounds(first_selector).expect("first formula");
    let second = cx.debug_bounds(second_selector).expect("second formula");
    let start = point(first.left() + px(1.), first.center().y);
    let end = point(second.right() - px(1.), second.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    redraw(cx);

    assert_eq!(
        cx.update(|window, cx| window.selected_text(cx)).trim(),
        markdown
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn oversized_display_formula_scrolls_horizontally_and_bubbles_vertical_wheel(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(320.), px(900.)));
    let terms = std::iter::repeat_n("x_i^2", 40)
        .collect::<Vec<_>>()
        .join(" + ");
    let markdown = format!("$$\n{terms}\n$$\n\n{}", "tail\n\n".repeat(80));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);
    cx.update(|_, cx| {
        chat.read(cx).list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let row_selector: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-0").into_boxed_str());
    let before = cx
        .debug_bounds(formula_selector)
        .expect("wide formula bounds");
    let row = cx.debug_bounds(row_selector).expect("display row bounds");
    assert!(
        before.size.width > row.size.width,
        "fixture must exceed its viewport: {before:?} vs {row:?}"
    );

    cx.simulate_event(ScrollWheelEvent {
        position: row.center(),
        delta: ScrollDelta::Pixels(point(px(-80.), px(-10.))),
        ..Default::default()
    });
    redraw(cx);

    let after = cx
        .debug_bounds(formula_selector)
        .expect("scrolled formula bounds");
    assert!(
        after.left() < before.left(),
        "horizontal input did not move the oversized formula: {before:?} -> {after:?}"
    );

    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y > px(0.)),
        "the transcript fixture must have vertical overflow"
    );
    let transcript_before_vertical =
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y);
    let row = cx.debug_bounds(row_selector).expect("display row bounds");
    cx.simulate_event(ScrollWheelEvent {
        position: row.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
        ..Default::default()
    });
    redraw(cx);

    assert_eq!(
        cx.debug_bounds(formula_selector)
            .expect("vertically scrolled formula bounds")
            .left(),
        after.left(),
        "vertical wheel input must not be remapped into horizontal formula scrolling"
    );
    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y)
            < transcript_before_vertical,
        "vertical wheel input over a display formula must continue to scroll the transcript"
    );
}

#[gpui::test]
fn reasoning_delimiter_ellipsis_stays_on_the_native_text_path(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    cx.simulate_resize(gpui::size(px(210.), px(700.)));
    let reasoning = "我们要求输出一些数学公式，包括块级和内联。块级公式用$$...$$，内联用$...$。随便写点数学公式即可。注意格式。我们输出即可。";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-math".into(), reasoning, cx);
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let MessagePart::Reasoning { ui_id, .. } = &turn.parts[0] else {
            panic!("reasoning part")
        };
        let inline_marker = "内联用$...$";
        let inline_start = reasoning.find(inline_marker).expect("inline phrase") + "内联用".len();
        (*ui_id, inline_start)
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_none(),
        "dot-only delimiter examples must remain native text instead of an atomic formula image"
    );
}

#[gpui::test]
fn linked_inline_formula_opens_its_destination(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "[$x^2$](https://example.com/math)";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find('$').expect("linked formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    let bounds = cx.debug_bounds(formula).expect("linked formula bounds");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/math"));
}

#[gpui::test]
fn native_markdown_image_remains_clickable_beside_a_formula(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown =
        "$x$[![native](https://example.com/image.svg)](https://example.com/native-image)";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let formula: &'static str = Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let formula = cx.debug_bounds(formula).expect("formula bounds");
    let image_center = point(formula.right() + px(4.), formula.center().y);
    cx.simulate_click(image_center, gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/native-image"),
        "math preparation must preserve the adjacent native image and its link"
    );
}

#[gpui::test]
fn reference_image_alt_hazards_keep_native_images_and_chat_rendering(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$q$[![a $$x][r]](https://click-a.test) ![b y$$][s]\n\n[r]: https://a.test/i.svg\n[s]: https://b.test/i.svg";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        assert_eq!(text, markdown);
        *ui_id
    });
    let formula: &'static str = Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let formula = cx.debug_bounds(formula).expect("leading formula bounds");
    let image_center = point(formula.right() + px(4.), formula.center().y);
    cx.simulate_click(image_center, gpui::Modifiers::default());
    assert_eq!(cx.opened_url().as_deref(), Some("https://click-a.test"));

    let unclaimed_math_start = markdown.find("$$").expect("image alt delimiter");
    let unclaimed_math: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{unclaimed_math_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(unclaimed_math).is_none(),
        "dollars spanning reference-image alt text must remain native image content"
    );
}

#[gpui::test]
fn reference_linked_formula_keeps_native_link_behavior(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = r"[$x^2$][math]
[label \(y\)][]
[shortcut \(z\)]

[math]: https://example.com/reference-math
[label \(y\)]: https://example.com/collapsed-math
[shortcut \(z\)]: https://example.com/shortcut-math";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        assert_eq!(text, markdown);
        *ui_id
    });
    for (formula_source, destination) in [
        ("$x^2$", "https://example.com/reference-math"),
        (r"\(y\)", "https://example.com/collapsed-math"),
        (r"\(z\)", "https://example.com/shortcut-math"),
    ] {
        let formula_start = markdown
            .find(formula_source)
            .expect("reference-linked formula");
        let formula: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
        let bounds = cx.debug_bounds(formula).expect("reference formula bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some(destination));
    }
    let selected = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected.trim(), "$x^2$\nlabel \\(y\\)\nshortcut \\(z\\)");
}

#[gpui::test]
fn colliding_prepared_reference_identifiers_keep_distinct_destinations(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = r"[label \(x\)][] and [$a$][label \(x\)]
[label $$x$$][]

[label \(x\)]: https://example.com/slash
[label $$x$$]: https://example.com/dollar";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    for (formula_source, destination) in [
        ("$a$", "https://example.com/slash"),
        ("$$x$$", "https://example.com/dollar"),
    ] {
        let formula_start = markdown.find(formula_source).expect("linked formula");
        let formula: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
        let bounds = cx.debug_bounds(formula).expect("linked formula bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some(destination));
    }

    let selected = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected.trim(), "label (x) and $a$\nlabel $$x$$");
}

#[gpui::test]
fn table_cell_formula_uses_native_table_flow(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "| 描述 | 数学表达式 |\n| :--- | :--- |\n| 模长大于 R | $|z| > R$ |";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start, selected) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text {
                ui_id, text, body, ..
            } = &this.messages[0].parts[0]
            else {
                panic!("assistant text part")
            };
            (
                *ui_id,
                text.find("$|z| > R$").expect("table formula"),
                body.select_all_text(cx),
            )
        })
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "the native table cell must retain its custom inline formula"
    );
    assert_eq!(selected.trim(), "描述 数学表达式\n模长大于 R $|z| > R$");
}

#[gpui::test]
fn long_inline_riemann_formulas_all_render(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let formulas = [
        r"$R^{\rho}_{\sigma\mu\nu}$",
        r"$R^{\rho}_{\sigma\mu\nu} = \partial_{\mu} \Gamma^{\rho}_{\nu\sigma} - \partial_{\nu} \Gamma^{\rho}_{\mu\sigma}$",
        r"$R^{\rho}_{\sigma\mu\nu} = \partial_{\mu} \Gamma^{\rho}_{\nu\sigma} - \partial_{\nu} \Gamma^{\rho}_{\mu\sigma} + \Gamma^{\rho}_{\mu\lambda} \Gamma^{\lambda}_{\nu\sigma} - \Gamma^{\rho}_{\nu\lambda} \Gamma^{\lambda}_{\mu\sigma}$",
        r"$\displaystyle R^{\rho}_{\sigma\mu\nu} = \partial_{\mu} \Gamma^{\rho}_{\nu\sigma} - \partial_{\nu} \Gamma^{\rho}_{\mu\sigma}$",
        r"$R^{\rho}_{\sigma\,\mu\,\nu}$",
    ];
    let markdown = format!(
        "好的，这是你需要渲染的黎曼几何算子公式，直接输出如下：\n\n{}",
        formulas.join("\n\n")
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    for formula in formulas {
        let start = markdown.find(formula).expect("formula source");
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "inline formula did not render: {formula}; caches: {:?}",
            crate::ui::math::formula_cache_snapshots(owner_id),
        );
    }
}

#[gpui::test]
fn multiline_inline_math_paragraph_lays_out_without_panicking(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(360.), px(1200.)));
    let markdown = "第一行\n第二行 $x^2$ 结尾";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });

    // The first draw exercises custom flow measurement. The old implementation
    // forwarded the embedded newline to `shape_line`, whose single-line
    // contract deliberately panics in debug builds.
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$x^2$").expect("inline math"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(cx.debug_bounds(formula).is_some());
}

#[gpui::test]
fn multiple_display_formulas_stack_without_overlap(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(1600.)));
    let markdown = "before\n\n$$x^2$$\n\nmiddle\n\n\\[\\frac{1}{2}\\]\n\nafter";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, dollar_formula_start, bracket_formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (
            *ui_id,
            text.find("$$").expect("dollar formula"),
            text.find(r"\[").expect("bracket formula"),
        )
    });
    let formula_starts = [dollar_formula_start, bracket_formula_start];
    let bounds = formula_starts
        .into_iter()
        .map(|start| {
            let selector: &'static str =
                Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
            cx.debug_bounds(selector).expect("display formula bounds")
        })
        .collect::<Vec<_>>();
    assert!(bounds[0].size.width > px(0.) && bounds[0].size.height > px(0.));
    assert!(bounds[1].size.width > px(0.) && bounds[1].size.height > px(0.));
    assert!(
        bounds[0].bottom() <= bounds[1].top() || bounds[1].bottom() <= bounds[0].top(),
        "multiple display formulas must be vertically ordered without overlap: {bounds:?}"
    );
}

#[gpui::test]
fn matrix_display_formula_survives_markdown_block_tokenization(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(1800.)));
    let markdown = "以下是一些数学公式：\n\n内联公式示例：勾股定理 \\(a^2+b^2=c^2\\)，欧拉公式 \\(e^{i\\pi}+1=0\\)，以及极限 \\(\\lim_{x\\to 0}\\frac{\\sin x}{x}=1\\)。\n\n块级公式示例：\n\n$$\n\\int_{-\\infty}^{\\infty} e^{-x^2}\\,dx = \\sqrt{\\pi}\n$$\n\n$$\n\\sum_{n=1}^{\\infty} \\frac{1}{n^2} = \\frac{\\pi^2}{6}\n$$\n\n$$\n\\begin{pmatrix}\n1 & 2 \\\\\n3 & 4\n\\end{pmatrix}\n\\begin{pmatrix}\nx \\\\ y\n\\end{pmatrix}\n=\n\\begin{pmatrix}\n5 \\\\ 6\n\\end{pmatrix}\n$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let opening_fences = markdown
        .match_indices("$$")
        .enumerate()
        .filter_map(|(index, (start, _))| index.is_multiple_of(2).then_some(start))
        .collect::<Vec<_>>();
    assert_eq!(
        opening_fences.len(),
        3,
        "test fixture must contain three blocks"
    );

    for (formula_ix, start) in opening_fences.into_iter().enumerate() {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "display formula {} must be rendered as an image-backed math node; caches: {:?}",
            formula_ix + 1,
            crate::ui::math::formula_cache_snapshots(owner_id),
        );
    }
}

#[gpui::test]
fn fenced_blocks_show_language_and_copy_their_own_code(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "Before\n\n```rust\nfn first() {}\n```\n\n```unknown-language-tag-that-must-truncate-cleanly\nsecond\n```\n\n```\nplain\n```";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let (owner_id, body_id_before) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, body, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, body.entity_id())
    });
    let fences = markdown
        .match_indices("```")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let first_copy: &'static str =
        Box::leak(format!("markdown-code-copy-{owner_id}-{}", fences[0]).into_boxed_str());
    let second_copy: &'static str =
        Box::leak(format!("markdown-code-copy-{owner_id}-{}", fences[2]).into_boxed_str());
    let third_copy: &'static str =
        Box::leak(format!("markdown-code-copy-{owner_id}-{}", fences[4]).into_boxed_str());
    let first_language: &'static str =
        Box::leak(format!("markdown-code-language-{owner_id}-{}", fences[0]).into_boxed_str());
    let second_language: &'static str =
        Box::leak(format!("markdown-code-language-{owner_id}-{}", fences[2]).into_boxed_str());
    let third_language: &'static str =
        Box::leak(format!("markdown-code-language-{owner_id}-{}", fences[4]).into_boxed_str());
    let second_block: &'static str =
        Box::leak(format!("markdown-code-block-{owner_id}-{}", fences[2]).into_boxed_str());
    let second_header: &'static str =
        Box::leak(format!("markdown-code-header-{owner_id}-{}", fences[2]).into_boxed_str());
    let second_line: &'static str =
        Box::leak(format!("markdown-code-line-{owner_id}-{}-0", fences[2]).into_boxed_str());
    let second_wrap: &'static str =
        Box::leak(format!("markdown-code-wrap-{owner_id}-{}", fences[2]).into_boxed_str());

    assert!(cx.debug_bounds(first_language).is_some(), "rust label");
    assert!(cx.debug_bounds(second_language).is_some(), "unknown label");
    assert!(
        cx.debug_bounds(third_language).is_none(),
        "an untagged block must not render an empty label"
    );

    cx.simulate_resize(gpui::size(px(320.), px(600.)));
    redraw(cx);
    let block_bounds = cx.debug_bounds(second_block).expect("code block");
    let header_bounds = cx.debug_bounds(second_header).expect("code header");
    let language_bounds = cx.debug_bounds(second_language).expect("unknown label");
    let line_bounds = cx.debug_bounds(second_line).expect("first code line");
    let wrap_bounds = cx.debug_bounds(second_wrap).expect("wrap button");
    let copy_bounds = cx.debug_bounds(second_copy).expect("code copy button");
    assert_eq!(
        line_bounds.top() - header_bounds.bottom(),
        px(6.),
        "the code content must sit 6px below the header"
    );
    assert_eq!(
        (
            header_bounds.top(),
            header_bounds.left(),
            header_bounds.right()
        ),
        (
            block_bounds.top(),
            block_bounds.left(),
            block_bounds.right()
        ),
        "the header background must reach the code block's top and side edges"
    );
    assert_eq!(
        language_bounds.left(),
        line_bounds.left(),
        "the language label must align with the code content inside the full-width header"
    );
    assert_eq!(
        wrap_bounds.top() - block_bounds.top(),
        px(6.),
        "the header controls must sit 6px below the block top"
    );
    assert_eq!(
        header_bounds.bottom() - wrap_bounds.bottom(),
        px(6.),
        "the header controls must have 6px of bottom padding"
    );
    assert!(
        language_bounds.right() <= wrap_bounds.left()
            && wrap_bounds.right() <= copy_bounds.left()
            && copy_bounds.right() <= header_bounds.right(),
        "the language must stay left while wrap and copy stay right"
    );
    assert!(
        copy_bounds.right() <= px(320.),
        "the copy action must stay inside a narrow chat window"
    );

    for (selector, expected) in [
        (first_copy, "fn first() {}"),
        (second_copy, "second"),
        (third_copy, "plain"),
    ] {
        let bounds = cx.debug_bounds(selector).expect("code copy button");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(expected)
        );
    }

    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let MessagePart::Text { body, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        assert_eq!(
            body.entity_id(),
            body_id_before,
            "the renderer updates its palette without replacing message state"
        );
    });
}

#[test]
fn code_block_labels_resolve_in_every_locale() {
    for locale in ["en", "zh-CN"] {
        for key in ["chat.code.copy", "chat.code.wrap"] {
            let resolved = rust_i18n::t!(key, locale = locale).to_string();
            assert!(!resolved.contains(key), "{key} unresolved for {locale}");
        }
    }
}

#[gpui::test]
fn code_block_display_controls_apply_at_their_own_scopes(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let long_line = "long code line ".repeat(40);
    let markdown = format!("```rust\n{long_line}\nsecond line\n```\n\n```text\n{long_line}\n```");
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let fences = markdown
        .match_indices("```")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let selectors = |kind: &str, block: usize, suffix: &str| -> &'static str {
        Box::leak(
            format!(
                "markdown-code-{kind}-{owner_id}-{}{suffix}",
                fences[block * 2]
            )
            .into_boxed_str(),
        )
    };
    let first_block = selectors("block", 0, "");
    let first_header = selectors("header", 0, "");
    let first_line = selectors("line", 0, "-0");
    let first_scroll = selectors("scroll", 0, "");
    let first_wrap = selectors("wrap", 0, "");
    let first_copy = selectors("copy", 0, "");
    let first_number = selectors("line-number", 0, "-0");
    let second_line = selectors("line", 1, "-0");
    let second_wrap = selectors("wrap", 1, "");
    let second_number = selectors("line-number", 1, "-0");

    cx.update(|_, cx| {
        assert!(!crate::ui::markdown::global_wrap_enabled(cx));
        assert!(!crate::ui::markdown::line_numbers_enabled(cx));
    });
    assert!(cx.debug_bounds(first_number).is_none());
    assert!(cx.debug_bounds(second_number).is_none());

    let block_bounds = cx.debug_bounds(first_block).expect("first code block");
    let header_bounds = cx.debug_bounds(first_header).expect("code header");
    let nowrap_line = cx.debug_bounds(first_line).expect("first code line");
    let nowrap_second = cx.debug_bounds(second_line).expect("second code block");
    let scroll_bounds = cx.debug_bounds(first_scroll).expect("horizontal viewport");
    let wrap_bounds = cx.debug_bounds(first_wrap).expect("wrap control");
    let copy_bounds = cx.debug_bounds(first_copy).expect("copy control");
    assert!(
        nowrap_line.size.width > scroll_bounds.size.width,
        "nowrap code must retain its intrinsic width inside a horizontal viewport"
    );
    assert_eq!(
        nowrap_line.top() - header_bounds.bottom(),
        px(6.),
        "the header and code content must be separated by exactly 6px"
    );
    assert!(
        wrap_bounds.right() <= copy_bounds.left()
            && copy_bounds.right() <= header_bounds.right()
            && header_bounds.right() <= block_bounds.right(),
        "actions must remain ordered at the right side of the header"
    );

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = true;
        });
        cx.refresh_windows();
    });
    redraw(cx);

    cx.update(|_, cx| {
        assert!(!crate::ui::markdown::global_wrap_enabled(cx));
        assert!(crate::ui::markdown::line_numbers_enabled(cx));
    });
    let number_bounds = cx.debug_bounds(first_number).expect("fixed line number");
    let numbered_scroll_bounds = cx
        .debug_bounds(first_scroll)
        .expect("numbered horizontal viewport");
    assert!(
        number_bounds.right() <= numbered_scroll_bounds.left(),
        "line numbers must stay outside the horizontal scrolling viewport"
    );
    assert!(cx.debug_bounds(second_number).is_some());

    let first_wrap_bounds = cx.debug_bounds(first_wrap).expect("first wrap control");
    cx.simulate_click(first_wrap_bounds.center(), gpui::Modifiers::default());
    redraw(cx);

    let locally_wrapped_first = cx.debug_bounds(first_line).expect("locally wrapped block");
    let unchanged_second = cx
        .debug_bounds(second_line)
        .expect("unchanged second block");
    assert!(locally_wrapped_first.size.height > nowrap_line.size.height);
    assert_eq!(unchanged_second.size.height, nowrap_second.size.height);
    cx.update(|_, cx| assert!(!crate::ui::markdown::global_wrap_enabled(cx)));

    cx.update(|_, cx| crate::ui::markdown::set_global_wrap_in_memory(true, cx));
    redraw(cx);

    cx.update(|_, cx| {
        assert!(crate::ui::markdown::global_wrap_enabled(cx));
        assert!(crate::ui::markdown::line_numbers_enabled(cx));
    });
    let wrapped_first = cx.debug_bounds(first_line).expect("wrapped first block");
    let wrapped_second = cx.debug_bounds(second_line).expect("wrapped second block");
    assert!(
        wrapped_first.size.height > nowrap_line.size.height
            && wrapped_second.size.height > nowrap_line.size.height,
        "a global change must reset every code block to the new value"
    );

    cx.update(|_, cx| crate::ui::markdown::set_global_wrap_in_memory(false, cx));
    redraw(cx);

    let reset_first = cx
        .debug_bounds(first_line)
        .expect("globally reset first block");
    let reset_second = cx
        .debug_bounds(second_line)
        .expect("globally reset second block");
    assert_eq!(reset_first.size.height, nowrap_line.size.height);
    assert_eq!(reset_second.size.height, nowrap_second.size.height);
    cx.update(|_, cx| assert!(!crate::ui::markdown::global_wrap_enabled(cx)));

    cx.update(|_, cx| crate::ui::markdown::set_global_wrap_in_memory(true, cx));
    redraw(cx);

    let first_wrap_bounds = cx.debug_bounds(first_wrap).expect("first wrap control");
    cx.simulate_click(first_wrap_bounds.center(), gpui::Modifiers::default());
    redraw(cx);

    let locally_unwrapped_first = cx
        .debug_bounds(first_line)
        .expect("locally unwrapped block");
    let still_wrapped_second = cx
        .debug_bounds(second_line)
        .expect("still wrapped second block");
    assert_eq!(locally_unwrapped_first.size.height, nowrap_line.size.height);
    assert!(still_wrapped_second.size.height > nowrap_second.size.height);
    assert!(cx.debug_bounds(second_wrap).is_some());
    cx.update(|_, cx| assert!(crate::ui::markdown::global_wrap_enabled(cx)));
}

#[gpui::test]
fn nowrap_code_block_exposes_a_horizontal_scrollbar(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    let long_line = "horizontal overflow ".repeat(80);
    let markdown = format!("```rust\n{long_line}\n```\n\n{}", "tail\n\n".repeat(80));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let scrollbar: &'static str =
        Box::leak(format!("markdown-code-scrollbar-{owner_id}-0").into_boxed_str());
    let first_line: &'static str =
        Box::leak(format!("markdown-code-line-{owner_id}-0-0").into_boxed_str());
    let block: &'static str =
        Box::leak(format!("markdown-code-block-{owner_id}-0").into_boxed_str());
    let viewport: &'static str =
        Box::leak(format!("markdown-code-scroll-{owner_id}-0").into_boxed_str());

    let scrollbar_bounds = cx
        .debug_bounds(scrollbar)
        .expect("horizontal scrollbar layer");
    let line_bounds = cx.debug_bounds(first_line).expect("first code line");
    let block_bounds = cx.debug_bounds(block).expect("code block");
    let viewport_bounds = cx.debug_bounds(viewport).expect("code viewport");
    assert_eq!(
        scrollbar_bounds.size.height,
        px(16.),
        "the horizontal scrollbar must have a stable interaction track"
    );
    assert!(
        scrollbar_bounds.top() >= line_bounds.bottom(),
        "the horizontal scrollbar must sit below the code instead of covering it: scrollbar={scrollbar_bounds:?}, line={line_bounds:?}"
    );
    assert_eq!(
        block_bounds.bottom() - scrollbar_bounds.bottom(),
        px(2.),
        "the horizontal scrollbar must sit exactly 2px above the code block bottom"
    );
    assert_eq!(
        viewport_bounds.size.width - scrollbar_bounds.size.width,
        px(2.),
        "the horizontal scrollbar must be 2px narrower than its viewport"
    );
}

#[gpui::test]
fn nowrap_code_block_hides_the_horizontal_scrollbar_without_overflow(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    let markdown = "```rust\nlet short = true;\n```";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let scrollbar: &'static str =
        Box::leak(format!("markdown-code-scrollbar-{owner_id}-0").into_boxed_str());

    assert!(
        cx.debug_bounds(scrollbar).is_none(),
        "a nowrap code block without horizontal overflow must not render a scrollbar track"
    );
}

#[gpui::test]
fn nowrap_code_block_updates_scrollbar_visibility_after_resize(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(1000.), px(420.)));
    let medium_line = "medium-width-code ".repeat(4);
    let markdown = format!("```rust\n{medium_line}\n```");
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let scrollbar: &'static str =
        Box::leak(format!("markdown-code-scrollbar-{owner_id}-0").into_boxed_str());

    assert!(
        cx.debug_bounds(scrollbar).is_none(),
        "a wide viewport must not render the scrollbar track"
    );

    cx.simulate_resize(gpui::size(px(360.), px(420.)));
    redraw(cx);
    redraw(cx);
    assert!(
        cx.debug_bounds(scrollbar).is_some(),
        "narrowing the viewport past the content width must reveal the scrollbar track"
    );

    cx.simulate_resize(gpui::size(px(1000.), px(420.)));
    redraw(cx);
    redraw(cx);
    redraw(cx);
    assert!(
        cx.debug_bounds(scrollbar).is_none(),
        "widening the viewport again must remove the stale scrollbar track"
    );
}

#[gpui::test]
fn horizontal_code_scroll_does_not_move_the_transcript(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    let long_line = "horizontal overflow ".repeat(80);
    let markdown = format!(
        "Intro\n\nIntro\n\n```rust\n{long_line}\n```\n\n{}",
        "tail\n\n".repeat(80)
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);
    cx.update(|_, cx| {
        chat.read(cx)
            .list_state
            .set_offset_from_scrollbar(point(px(0.), px(0.)));
    });
    redraw(cx);

    let (owner_id, fence_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, markdown.find("```").expect("code fence"))
    });
    let selector = |kind: &str, suffix: &str| -> &'static str {
        Box::leak(format!("markdown-code-{kind}-{owner_id}-{fence_start}{suffix}").into_boxed_str())
    };
    let viewport = selector("scroll", "");
    let first_line = selector("line", "-0");

    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y > px(0.)),
        "the transcript fixture must have vertical overflow"
    );
    let line_before = cx.debug_bounds(first_line).expect("first code line");
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(-80.), px(0.))),
        ..Default::default()
    });
    redraw(cx);
    let line_after_right = cx.debug_bounds(first_line).expect("scrolled code line");
    assert!(
        line_after_right.left() < line_before.left(),
        "horizontal wheel input must move the code content"
    );

    cx.update(|_, cx| {
        chat.read(cx)
            .list_state
            .set_offset_from_scrollbar(point(px(0.), px(-20.)));
    });
    redraw(cx);
    let transcript_before_left =
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y);
    let line_before_left = cx
        .debug_bounds(first_line)
        .expect("right-scrolled code line");
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(40.), px(0.))),
        ..Default::default()
    });
    redraw(cx);

    let line_after_left = cx
        .debug_bounds(first_line)
        .expect("left-scrolled code line");
    assert!(
        line_after_left.left() > line_before_left.left(),
        "leftward navigation must move the code content back toward its origin"
    );
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y),
        transcript_before_left,
        "horizontal scrolling inside a code block must never move the transcript"
    );

    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(1000.), px(0.))),
        ..Default::default()
    });
    redraw(cx);
    let line_at_left_boundary = cx.debug_bounds(first_line).expect("left-aligned code line");
    assert_eq!(line_at_left_boundary.left(), line_before.left());

    let transcript_at_left_boundary =
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y);
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(40.), px(0.))),
        ..Default::default()
    });
    redraw(cx);
    assert_eq!(
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y),
        transcript_at_left_boundary,
        "continuing left at the code boundary must still not scroll the transcript"
    );

    let code_x_before_vertical = line_at_left_boundary.left();
    let transcript_before_vertical =
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y);
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
        ..Default::default()
    });
    redraw(cx);

    assert_eq!(
        cx.debug_bounds(first_line)
            .expect("vertically scrolled code line")
            .left(),
        code_x_before_vertical,
        "vertical wheel input must not be remapped into horizontal code scrolling"
    );
    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y)
            < transcript_before_vertical,
        "vertical wheel input over a code block must continue to scroll the transcript"
    );
}

#[gpui::test]
fn long_formula_transcript_materializes_only_viewport_and_overdraw(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(640.)));

    let initial_scroll_height_estimate = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for index in 0..100 {
                let text = if index % 3 == 0 {
                    format!("$x_{{{index}}}^2 + y_{{{index}}}^2$")
                } else {
                    format!("message {index}")
                };
                this.messages.push(Message::from_canonical(
                    LlmMessage {
                        role: crate::llm::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text,
                            provider_metadata: ProviderMetadata::default(),
                        }],
                        provider_metadata: ProviderMetadata::default(),
                    },
                    cx,
                ));
            }
            this.sync_message_list_count();
            this.list_state.max_offset_for_scrollbar().y
        })
    });
    assert!(
        initial_scroll_height_estimate > px(15_000.),
        "unmeasured rows must contribute their height hints to the first-frame scrollbar: {initial_scroll_height_estimate:?}"
    );
    redraw_settled_math(cx);

    let (first_owner, last_owner, bottom_materialized) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let owner = |index: usize| match &chat.messages[index].parts[0] {
            MessagePart::Text { ui_id, .. } => *ui_id,
            _ => panic!("text fixture"),
        };
        (owner(0), owner(99), chat.materialized_message_indices.len())
    });
    assert!(
        bottom_materialized <= 80,
        "virtual list materialized {bottom_materialized} of 100 messages"
    );
    assert!(
        cx.debug_bounds(Box::leak(
            format!("markdown-math-{last_owner}-0").into_boxed_str()
        ))
        .is_some(),
        "tail formula must render while following the bottom"
    );

    cx.update(|_, cx| {
        chat.read(cx).list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    });
    redraw_settled_math(cx);
    let top_materialized = cx.update(|_, cx| chat.read(cx).materialized_message_indices.len());
    assert!(
        top_materialized <= 80,
        "top materialized {top_materialized} messages"
    );
    assert!(
        cx.debug_bounds(Box::leak(
            format!("markdown-math-{first_owner}-0").into_boxed_str()
        ))
        .is_some(),
        "head formula must regenerate after scrolling to the top; materialized={:?}, offset={:?}",
        cx.update(|_, cx| chat.read(cx).materialized_message_indices.clone()),
        cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top())
    );
    let head_cache = crate::ui::math::formula_cache_snapshot(first_owner, 0)
        .expect("head formula cache after top render");
    assert!(head_cache.active && head_cache.ready, "{head_cache:?}");

    let top_before_stream = cx.update(|_, cx| {
        let chat = chat.read(cx);
        assert!(!chat.list_state.is_following_tail());
        chat.list_state.logical_scroll_top()
    });
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| chat.finish_stream_batch(cx));
    });
    redraw(cx);
    cx.update(|_, cx| {
        let chat = chat.read(cx);
        assert!(
            !chat.list_state.is_following_tail(),
            "a streaming update must not re-arm follow while the user is reading the top"
        );
        let after_stream = chat.list_state.logical_scroll_top();
        assert_eq!(after_stream.item_ix, top_before_stream.item_ix);
        assert_eq!(
            after_stream.offset_in_item,
            top_before_stream.offset_in_item
        );
    });

    let messages = cx
        .debug_bounds(Box::leak(
            format!("markdown-math-{first_owner}-0").into_boxed_str(),
        ))
        .expect("visible head formula bounds");
    cx.simulate_event(ScrollWheelEvent {
        position: messages.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-100_000.))),
        ..Default::default()
    });
    redraw_settled_math(cx);
    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.is_following_tail()),
        "scrolling back to the true bottom must re-arm tail following"
    );
    assert!(
        cx.debug_bounds(Box::leak(
            format!("markdown-math-{last_owner}-0").into_boxed_str()
        ))
        .is_some(),
        "tail formula must regenerate after a complete round trip"
    );
    let released = crate::ui::math::formula_cache_snapshot(first_owner, 0)
        .expect("released head formula probe");
    assert!(
        !released.active,
        "offscreen formula cache stayed active: {released:?}"
    );
    assert_eq!(released.release_count, 1, "{released:?}");
    assert_eq!(released.image_drop_count, 1, "{released:?}");
}

#[test]
fn empty_assistant_placeholders_are_not_replayed() {
    let empty_assistant = LlmMessage {
        role: crate::llm::Role::Assistant,
        content: Vec::new(),
        provider_metadata: ProviderMetadata::default(),
    };
    let user = LlmMessage {
        role: crate::llm::Role::User,
        content: vec![ContentBlock::Text {
            text: "hi".into(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    };

    assert!(!is_replayable(&empty_assistant));
    assert!(is_replayable(&user));
}

/// A failed turn must render its upstream error card through a real view
/// pass: the card reads window-keyed collapse state, which is only available
/// with a rendering view on the stack.
#[gpui::test]
fn failed_turn_renders_the_upstream_error_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let mut error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
        .with_upstream_body(
            r#"{"error":{"message":"Rate limit reached","code":"rate_limit_exceeded"}}"#,
        );
    error.request_id = Some("nostra-1".into());
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.finish_reply(None, Some(error), cx));
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        // Attached to the assistant turn, not the user's message.
        let assistant = this.messages.last().expect("assistant turn");
        assert_eq!(assistant.role, Role::Assistant);
        assert!(
            assistant.error.is_some(),
            "card attached to the failed turn"
        );
        assert_eq!(
            assistant
                .error
                .as_ref()
                .and_then(crate::chat::error_card::TurnError::request_id),
            Some("nostra-1"),
            "the visible card retains the correlation id"
        );
        assert!(
            this.messages[0].error.is_none(),
            "the user's own turn carries no error"
        );
        // The provider's error text must not leak into replayable history.
        assert!(
            this.messages
                .iter()
                .all(
                    |message| message.canonical().content.iter().all(|block| !matches!(
                        block,
                        ContentBlock::Text { text, .. } if text.contains("rate_limit_exceeded")
                    ))
                ),
            "error text must stay out of canonical content"
        );
    });

    // Draws the whole view, card included, without panicking.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );

    let error_body_before_theme_switch = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|message| message.error.as_ref())
            .and_then(|error| error.body_entity_id())
            .expect("error body entity")
    });

    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let error = this
            .messages
            .last()
            .and_then(|message| message.error.as_ref())
            .expect("the card survives a theme switch");
        let error_body_after_theme_switch = error.body_entity_id().expect("error body entity");
        assert_eq!(
            error_body_after_theme_switch, error_body_before_theme_switch,
            "theme changes must refresh native highlights without replacing TextView state"
        );
    });

    // Re-draws cleanly against the new theme.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

/// The greeting is laid out against the *resting* composer height, so a
/// growing draft must not move that number — otherwise the empty state
/// gets pushed up the panel one row at a time.
#[gpui::test]
fn growing_draft_leaves_the_resting_composer_height_alone(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        preferences::init_global(preferences::Preferences::default(), cx);
    });
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    let input = cx.update(|_, cx| chat.read(cx).input.clone());

    // First measurement of an empty composer sets both heights.
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            assert!(this.record_composer_height(px(96.)));
            assert_eq!(this.composer_height, px(96.));
            assert_eq!(this.base_composer_height, px(96.));
        });
    });

    // A draft grows the composer: the live height tracks it, the resting
    // height stays where the greeting was placed.
    cx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.set_value("line\nline\nline", window, cx);
            cx.emit(InputEvent::Change);
        });
    });
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            assert!(!this.input_empty);
            assert!(this.record_composer_height(px(168.)));
            assert_eq!(this.composer_height, px(168.));
            assert_eq!(this.base_composer_height, px(96.));
        });
    });

    // Clearing the draft re-measures the resting height, which is how a
    // font or text-size change recalibrates it.
    cx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            cx.emit(InputEvent::Change);
        });
    });
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            assert!(this.input_empty);
            assert!(this.record_composer_height(px(104.)));
            assert_eq!(this.base_composer_height, px(104.));
            // Idempotent: the same measurement asks for no re-render.
            assert!(!this.record_composer_height(px(104.)));
        });
    });
}

/// A user bubble must obey the conversation column when the window narrows.
/// Its Markdown body has an intrinsic width, so this is asserted through the
/// real flex tree instead of a standalone style value.
#[gpui::test]
fn user_message_bubble_shrinks_with_the_viewport(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Please extract factorio-headless.tar.xz into factorio-2.1.12 without changing its contents."
                            .into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    cx.run_until_parked();

    let draw = |width: f32, cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(width), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
        cx.debug_bounds("user-message-bubble-0")
            .expect("the user bubble was drawn")
    };

    let narrow_viewport_width = 440.;
    let content_inset = px(24.);
    let wide = draw(900., cx);
    let narrow = draw(narrow_viewport_width, cx);

    assert_eq!(wide.size.width, px(560.), "wide bubbles keep their cap");
    assert!(
        narrow.size.width < wide.size.width,
        "the bubble stayed {:?} wide in a {narrow_viewport_width}px viewport",
        narrow.size.width
    );
    assert!(
        narrow.left() >= content_inset,
        "the bubble left edge {:?} escaped the padded content column",
        narrow.left()
    );
    assert!(
        narrow.right() <= px(narrow_viewport_width) - content_inset,
        "the bubble right edge {:?} escaped the padded content column",
        narrow.right()
    );
}

/// The regression this whole module exists for: reasoning deltas were being
/// folded into canonical content and then never rendered. A streaming trace
/// must reach a visible, expanded card *and* stay out of the prose body.
#[gpui::test]
fn streaming_reasoning_reaches_an_expanded_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "Weighing ", cx);
            this.append_stream_reasoning(0, "reasoning-0".into(), "the options.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this.messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("a trace was created");

        assert_eq!(
            reasoning_states(turn)[0].0,
            "Weighing the options.",
            "deltas accumulate into the card's own markdown source"
        );
        assert!(
            reasoning.is_expanded() && !reasoning_states(turn)[0].1,
            "a live trace shows itself without being asked"
        );
        // Canonical content still carries it, for replay across turns.
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "Weighing the options."
        ));
        assert_eq!(turn.parts.len(), 1, "no synthetic empty prose part");
    });

    // Draws the whole view, card included, without panicking.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

/// The canonical reasoning-finished boundary collapses the card. Text remains
/// an independent content event, matching pi's thinking block lifecycle.
#[gpui::test]
fn reasoning_finished_collapses_the_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "Here is the answer.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this.messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");

        assert!(reasoning_states(turn)[0].1, "the block was closed");
        assert!(
            !reasoning.is_expanded(),
            "a finished trace folds down to its trigger"
        );
        assert!(
            reasoning_states(turn)[0].0.contains("thinking"),
            "the reasoning text is retained for re-expansion"
        );
        assert!(matches!(turn.parts.as_slice(), [MessagePart::Reasoning { .. }, MessagePart::Text { text, .. }] if text == "Here is the answer."));
    });
}

/// Exercise the production assistant boundary, not just `ChatView` methods in
/// isolation: an explicit canonical end marker must remain ordered between the
/// final reasoning delta and the first prose delta.
#[gpui::test]
fn canonical_events_close_reasoning_before_prose_reaches_the_view(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        delta: "thinking".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-1".into(),
                        delta: "answer".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn)[0].0, "thinking");
        assert!(
            reasoning_states(turn)[0].1,
            "the explicit boundary was replayed"
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }, ContentBlock::Text { text, .. }]
                if reasoning.display == "thinking" && text == "answer"
        ));
    });
}

/// Content deltas do not own another block's lifecycle. Protocol adapters emit
/// the explicit end boundary before a type transition, and the UI preserves it.
#[gpui::test]
fn prose_does_not_infer_a_reasoning_boundary(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        delta: "thinking".into(),
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-1".into(),
                        delta: String::new(),
                    },
                ],
                cx,
            );
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![crate::llm::GenerationEvent::TextDelta {
                    content_index: 1,
                    id: "text-1".into(),
                    delta: "answer".into(),
                }],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert!(
            !reasoning_states(turn)[0].1,
            "prose cannot close a different content block"
        );
    });
}

/// Content type transitions are structural boundaries. A later reasoning run
/// gets a new card at its canonical position instead of reopening or appending
/// to the first card.
#[gpui::test]
fn reasoning_after_prose_creates_a_second_ordered_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        delta: "first".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::TextStarted {
                        content_index: 1,
                        id: "text-0".into(),
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-0".into(),
                        delta: "answer".into(),
                    },
                    crate::llm::GenerationEvent::TextFinished {
                        content_index: 1,
                        id: "text-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 2,
                        id: "reasoning-1".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 2,
                        id: "reasoning-1".into(),
                        delta: "second".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn),
            vec![("first", true), ("second", false)]
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [
                ContentBlock::Reasoning { reasoning: first },
                ContentBlock::Text { text, .. },
                ContentBlock::Reasoning { reasoning: second },
            ] if first.display == "first" && text == "answer" && second.display == "second"
        ));
    });

    // Both cards must coexist in the real GPUI element tree with independent
    // element ids and interaction state.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

#[gpui::test]
fn separate_reasoning_cards_keep_independent_state(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "second", cx);
            this.finish_stream_reasoning(2, "reasoning-1", None);

            let turn = this.messages.last_mut().expect("assistant turn");
            assert_eq!(
                reasoning_states(turn),
                vec![("first", true), ("second", true)]
            );
            let reasoning_positions = turn
                .parts
                .iter()
                .enumerate()
                .filter_map(|(index, part)| {
                    matches!(part, MessagePart::Reasoning { .. }).then_some(index)
                })
                .collect::<Vec<_>>();
            let [first, second] = reasoning_positions.as_slice() else {
                panic!("two reasoning cards");
            };
            let (before_second, second_and_after) = turn.parts.split_at_mut(*second);
            let MessagePart::Reasoning {
                trace: Some(first_trace),
                ..
            } = &mut before_second[*first]
            else {
                panic!("first reasoning card");
            };
            let MessagePart::Reasoning {
                trace: Some(second_trace),
                ..
            } = &mut second_and_after[0]
            else {
                panic!("second reasoning card");
            };
            first_trace.toggle();
            assert!(first_trace.is_expanded());
            assert!(!second_trace.is_expanded());
        });
    });
}

/// Drive the real listeners for both cards. Stable element ids are not enough:
/// each closure must also resolve the same block identity when it reaches back
/// into `ChatView` for disclosure and clipboard content.
#[gpui::test]
fn separate_reasoning_cards_toggle_and_copy_independently(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "second", cx);
            this.finish_stream_reasoning(2, "reasoning-1", None);
        });
    });

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };
    draw(cx);

    let first_trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("first reasoning trigger");
    cx.simulate_click(first_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this.messages.last().expect("assistant turn"));
        assert!(traces[0].is_expanded());
        assert!(!traces[1].is_expanded());
    });

    let second_trigger = cx
        .debug_bounds("reasoning-trigger-2")
        .expect("second reasoning trigger");
    cx.simulate_click(second_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this.messages.last().expect("assistant turn"));
        assert!(traces[0].is_expanded());
        assert!(traces[1].is_expanded());
    });

    let copy_and_read =
        |selector: &'static str, trigger: &'static str, cx: &mut gpui::VisualTestContext| {
            let trigger = cx.debug_bounds(trigger).expect("reasoning trigger");
            cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
            draw(cx);
            let copy = cx.debug_bounds(selector).expect("reasoning copy action");
            cx.simulate_click(copy.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .expect("reasoning copied")
        };

    assert_eq!(
        copy_and_read("reasoning-copy-0", "reasoning-trigger-0", cx),
        "first"
    );
    assert_eq!(
        copy_and_read("reasoning-copy-2", "reasoning-trigger-2", cx),
        "second"
    );
}

#[gpui::test]
fn a_finished_reasoning_id_cannot_be_reused(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_reasoning(0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn)[0].0, "first");
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "first"
        ));
    });
}

#[gpui::test]
fn replay_only_reasoning_is_closed_without_allocating_a_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);
    let replay = ProviderMetadata {
        chat: Some(crate::llm::ChatReplayMetadata {
            reasoning_field: Some(crate::llm::ChatReasoningField::ReasoningContent),
            reasoning_details: None,
        }),
        responses: None,
    };

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_reasoning(0, "reasoning-0".into());
            this.finish_stream_reasoning(0, "reasoning-0", Some(replay.clone()));
            this.append_stream_reasoning(0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(
            reasoning_part(turn).is_none(),
            "no visible body was streamed"
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }]
                if reasoning.display.is_empty() && reasoning.replay.as_ref() == Some(&replay)
        ));
    });
}

#[gpui::test]
fn terminal_snapshot_preserves_separate_reasoning_cards(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "partial first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "partial answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "partial second", cx);
            let first = this
                .messages
                .last_mut()
                .and_then(reasoning_part_mut)
                .expect("first reasoning card");
            first.toggle();
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "first".into(),
                                replay: None,
                            },
                        },
                        ContentBlock::Text {
                            text: "answer".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "second".into(),
                                replay: None,
                            },
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn),
            vec![("first", true), ("second", true)]
        );
        assert!(
            traces[0].is_expanded(),
            "terminal reconciliation preserves the first card's disclosure"
        );
        assert!(
            !traces[1].is_expanded(),
            "the second card retains its independent automatic disclosure"
        );
    });
}

/// Terminal canonical content may omit an unfinished tool placeholder. The
/// later reasoning block must retain its GPUI identity even though its vector
/// position changes when that placeholder disappears.
#[gpui::test]
fn terminal_filter_preserves_reasoning_identity_by_content_index(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let (ui_id, body_id) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_tool_call(0, 0, "call-0".into(), "lookup".into());
            this.append_stream_reasoning(1, "reasoning-0".into(), "partial", cx);
            this.finish_stream_reasoning(1, "reasoning-0", None);
            let turn = this.messages.last_mut().expect("assistant turn");
            let MessagePart::Reasoning {
                ui_id,
                trace: Some(trace),
                ..
            } = turn
                .parts
                .iter_mut()
                .find(|part| part.content_index() == 1)
                .expect("reasoning slot")
            else {
                panic!("reasoning part")
            };
            trace.toggle();
            (*ui_id, trace.body_entity_id())
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![IndexedContentBlock {
                        content_index: 1,
                        block: ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "authoritative".into(),
                                replay: None,
                            },
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                }),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(
            turn.parts.len(),
            1,
            "unfinished tool placeholder was filtered"
        );
        let MessagePart::Reasoning {
            ui_id: current_ui_id,
            reasoning,
            trace: Some(trace),
            ..
        } = &turn.parts[0]
        else {
            panic!("terminal reasoning part")
        };
        assert_eq!(*current_ui_id, ui_id);
        assert_eq!(trace.body_entity_id(), body_id);
        assert!(
            trace.is_expanded(),
            "manual disclosure survives reconciliation"
        );
        assert_eq!(reasoning.display, "authoritative");
    });
}

/// A late Responses `output_item.done` updates the already-closed card in place.
/// It must not restart timing, reset disclosure, or replace the markdown entity.
#[gpui::test]
fn late_reasoning_snapshot_preserves_card_state_and_identity(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let (ui_id, body_id, elapsed) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                        delta: "streamed draft".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                        replay: None,
                    },
                ],
                cx,
            );
            let turn = this.messages.last_mut().expect("assistant turn");
            let MessagePart::Reasoning {
                ui_id,
                trace: Some(trace),
                ..
            } = &mut turn.parts[0]
            else {
                panic!("reasoning part")
            };
            trace.toggle();
            (*ui_id, trace.body_entity_id(), trace.elapsed())
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![crate::llm::GenerationEvent::ReasoningSnapshotUpdated {
                    content_index: 0,
                    id: "reasoning-0-0".into(),
                    reasoning: crate::llm::ReasoningContent {
                        display: "authoritative summary".into(),
                        replay: Some(ProviderMetadata {
                            chat: None,
                            responses: Some(ResponsesReplayMetadata {
                                item_id: Some("rs_1".into()),
                                encrypted_reasoning: Some("opaque".into()),
                                ..Default::default()
                            }),
                        }),
                    },
                }],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let MessagePart::Reasoning {
            ui_id: current_ui_id,
            reasoning,
            finished,
            trace: Some(trace),
            ..
        } = &turn.parts[0]
        else {
            panic!("reasoning part")
        };
        assert_eq!(*current_ui_id, ui_id);
        assert_eq!(trace.body_entity_id(), body_id);
        assert_eq!(trace.elapsed(), elapsed);
        assert!(trace.is_expanded() && *finished);
        assert_eq!(reasoning.display, "authoritative summary");
        assert_eq!(
            reasoning
                .replay
                .as_ref()
                .and_then(|metadata| metadata.responses.as_ref())
                .and_then(|metadata| metadata.encrypted_reasoning.as_deref()),
            Some("opaque")
        );
    });
}

#[gpui::test]
fn reasoning_after_a_tool_call_creates_a_second_ordered_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let tool_call = crate::llm::ToolCall {
        id: "call-0".into(),
        name: "lookup".into(),
        arguments: serde_json::json!({"query": "Nostra"}),
        raw_arguments: r#"{"query":"Nostra"}"#.into(),
        provider_metadata: ProviderMetadata::default(),
    };
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::chat::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        delta: "before tool".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::ToolCallStarted {
                        content_index: 1,
                        index: 0,
                        id: "call-0".into(),
                        name: "lookup".into(),
                    },
                    crate::llm::GenerationEvent::ToolCallFinished {
                        content_index: 1,
                        index: 0,
                        tool_call: Box::new(tool_call.clone()),
                    },
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 2,
                        id: "reasoning-1".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 2,
                        id: "reasoning-1".into(),
                        delta: "after tool".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(reasoning_parts(turn).len(), 2);
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [
                ContentBlock::Reasoning { reasoning: first },
                ContentBlock::ToolCall { tool_call: middle },
                ContentBlock::Reasoning { reasoning: second },
            ] if first.display == "before tool"
                && middle == &tool_call
                && second.display == "after tool"
        ));
    });
}

/// Responses providers may forward empty text deltas. They carry no content and
/// cannot stand in for the separate reasoning-finished lifecycle event.
#[gpui::test]
fn empty_text_delta_does_not_finish_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.append_stream_text(1, "text-0".into(), "", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn)[0].1 && reasoning.is_expanded());
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { .. }]
        ));
    });
}

/// Direct view updates obey the same structural rule as the event bridge: a
/// text delta cannot close an independently identified reasoning block.
#[gpui::test]
fn visible_text_does_not_finish_an_independent_reasoning_block(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.append_stream_text(1, "text-0".into(), "interleaved text", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn)[0].1 && reasoning.is_expanded());
    });
}

/// Reasoning that runs to the end of a turn with no text after it — a failed
/// or cancelled turn, or a model that reasons and then stops — is closed by
/// `finish_reply` instead.
#[gpui::test]
fn terminating_a_turn_closes_an_open_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "interrupted mid-thought", cx);
            this.finish_reply(None, None, cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn)[0].1);
        assert!(!reasoning.is_expanded());
    });
}

/// As in pi's `message_end` handling, the complete terminal message is the
/// rendering authority even when it differs from the live delta projection.
#[gpui::test]
fn terminal_message_replaces_streamed_reasoning_projection(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "partial", cx);
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "authoritative terminal reasoning".into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(
            reasoning_states(turn)[0].0,
            "authoritative terminal reasoning"
        );
    });
}

/// Some providers backfill omitted intermediate events in their terminal
/// object. The terminal snapshot must still create the presentation state.
#[gpui::test]
fn terminal_message_can_create_a_reasoning_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "backfilled reasoning".into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("terminal trace");
        assert_eq!(reasoning_states(turn)[0], ("backfilled reasoning", true));
        assert!(!reasoning.is_expanded());
    });
}

/// Once the user works the toggle, the stream stops deciding for them: a
/// trace deliberately opened during streaming stays open when it ends.
#[gpui::test]
fn manual_toggle_survives_the_auto_collapse(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            let reasoning = this
                .messages
                .last_mut()
                .and_then(|turn| reasoning_part_mut(turn))
                .expect("trace");
            // Collapse it by hand mid-stream, then re-open it.
            reasoning.toggle();
            assert!(!reasoning.is_expanded());
            reasoning.toggle();
            assert!(reasoning.is_expanded());

            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn)[0].1, "the stream still ended");
        assert!(
            reasoning.is_expanded(),
            "explicit user intent outlives the auto-collapse"
        );
    });
}

/// The collapsed trigger is a chip, not a bar: it must lay out at its own
/// label width rather than stretching across the content column. Asserted
/// against the transcript's real geometry, since "does a flex child stretch"
/// depends on the container it ends up in and is easy to regress by moving
/// the element.
#[gpui::test]
fn the_collapsed_trigger_hugs_its_label(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    // Reason, then answer: that collapses the card down to its trigger.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "a thought", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "The answer.", cx);
        });
    });
    cx.run_until_parked();
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the collapsed trigger was drawn");

    assert!(
        trigger.size.width < CONTENT_MAX_WIDTH,
        "the trigger stretched to the content column ({:?}) instead of hugging its label",
        trigger.size.width
    );
    assert!(
        trigger.size.width > px(0.),
        "the trigger must still be wide enough to hit"
    );
}

/// Clicking the copy button puts the turn's complete reasoning on the
/// clipboard — the accumulated source, not the seven lines the card happens
/// to be showing.
#[gpui::test]
fn the_copy_button_copies_the_whole_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    // Use the same rooted element tree as production so click routing and
    // overlay ownership exercise the real window contract.
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    // Far more than the visible budget, so the copy would be lossy if it read
    // the rendered view instead of the source.
    let mut expected = String::new();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..30 {
                let delta = format!("Reasoning line {line}.\n\n");
                expected.push_str(&delta);
                this.append_stream_reasoning(0, "reasoning-0".into(), &delta, cx);
            }
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "The answer.", cx);
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let copy = cx
        .debug_bounds("reasoning-copy-0")
        .expect("the copy button is in the tree once there is reasoning");
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the reasoning trigger is in the tree");
    // The copy action is intentionally hidden from hit testing and keyboard
    // focus until its group is hovered. Exercise that real interaction
    // instead of clicking the hidden element by debug bounds.
    cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, expected,
        "the clipboard must carry the complete reasoning source"
    );

    // Copying is not a disclosure gesture. The button sits beside the trigger
    // rather than inside it, and `Clipboard` stops propagation, so a copy must
    // leave the card exactly as the user left it.
    assert!(
        chat.read_with(cx, |this, _| this
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .is_some_and(|reasoning| !reasoning.is_expanded())),
        "copying must not toggle the card open"
    );
}

/// Nothing to copy before the first delta lands, so the button stays out of
/// the tree rather than offering to copy an empty string.
#[gpui::test]
fn the_copy_button_appears_only_once_there_is_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };

    // An empty protocol delta is not evidence that reasoning started.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "", cx)
        });
    });
    draw(cx);
    assert!(
        chat.read_with(cx, |this, _| this
            .messages
            .last()
            .is_some_and(|turn| reasoning_part(turn).is_none())),
        "empty deltas must not allocate a trace"
    );
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_none(),
        "no copy button while there is nothing to copy"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "a thought", cx)
        });
    });
    draw(cx);
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_some(),
        "the button becomes available as soon as reasoning arrives"
    );
}

/// A non-reasoning assistant turn has no synthetic placeholder part. Its first
/// visible text event creates the exact part that render consumes.
#[gpui::test]
fn a_turn_without_reasoning_creates_only_its_text_part(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, "text-0".into(), "answer", cx);
        });
    });
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(matches!(
            turn.parts.as_slice(),
            [MessagePart::Text { text, .. }] if text == "answer"
        ));
    });
}

/// The design claim behind rendering the card inline instead of floating it:
/// once a reasoning stream saturates the height budget, the card stops
/// growing, so everything laid out below it holds still no matter how many
/// tokens still arrive. Asserted against the transcript's own content height,
/// which is what a reflow would move.
#[gpui::test]
fn a_saturated_card_stops_moving_the_content_below_it(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
    };
    let transcript_content_height = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y)
    };

    // Well past a seven-line budget, so the cap is already engaged.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..40 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    let saturated = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .expect("trace")
            .scroll_max_offset()
    });
    assert!(
        saturated > px(0.),
        "the card must be hiding content behind its own scroll, not growing to fit it"
    );
    let before = transcript_content_height(cx);

    // Another 40 paragraphs of reasoning: all of it lands inside the card.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 40..80 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    assert_eq!(
        transcript_content_height(cx),
        before,
        "a capped card must not change the transcript's layout as it streams"
    );
}

/// A terminal reasoning block is historical content, so opening it starts at
/// the beginning even though live reasoning follows the tail while streaming.
#[gpui::test]
fn completed_long_reasoning_opens_at_the_top(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    let source = (0..240)
        .map(|line| format!("Completed reasoning paragraph {line}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            chat.messages.push(Message::from_canonical(
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
            ));
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed completed reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("completed reasoning trace");
        assert!(
            trace.uses_virtualized_scroll(),
            "the fixture must exercise the long-document path"
        );
        assert_eq!(
            trace.scroll_offset(),
            point(px(0.), px(0.)),
            "opening historical reasoning must not jump to its tail"
        );
    });
}

/// The large-document path must not turn every expanded disclosure into a
/// fixed-height viewport. Short reasoning still grows only to its content.
#[gpui::test]
fn short_reasoning_keeps_its_natural_height(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));

    let source = "Check the relevant constraints.\n\nChoose the smallest valid change.";
    cx.update(|_, cx| {
        chat.update(cx, |chat, cx| {
            chat.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: source.into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("collapsed short reasoning trigger");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("expanded short reasoning body");
    let height_budget = cx.update(|window, _| window.line_height() * 7.);
    assert!(
        body.size.height < height_budget,
        "short reasoning was stretched to the {:?} height budget",
        height_budget
    );
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("short reasoning trace");
        assert!(!trace.uses_virtualized_scroll());
        assert_eq!(trace.scroll_max_offset(), px(0.));
    });
}

/// Crossing the large-document threshold must not replace an actively used
/// native scroll handle. Returning to the tail must transition immediately:
/// a completed stream may never append another delta to trigger migration.
#[gpui::test]
fn streaming_reasoning_defers_virtualization_while_the_reader_is_scrolled_up(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    let initial = (0..90)
        .map(|line| format!("Reasoning line {line} has compact source.\n\n"))
        .collect::<String>();
    assert!(
        initial.len() < 4 * 1024,
        "the first stream segment must remain below the virtualization gate"
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &initial, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the expanded reasoning body was drawn");
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(!trace.uses_virtualized_scroll());
        assert!(trace.scroll_max().y > px(80.));
    });

    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);

    let paused_offset = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(!trace.is_following());
        trace.scroll_offset()
    });

    let threshold_crossing = "Additional retained paragraph content.\n\n".repeat(80);
    assert!(
        initial.len() + threshold_crossing.len() >= 4 * 1024,
        "the second segment must cross the virtualization gate"
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), &threshold_crossing, cx);
        });
    });
    redraw(cx);
    redraw(cx);

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            !trace.uses_virtualized_scroll(),
            "virtualization must wait while the reader is away from the tail"
        );
        assert_eq!(
            trace.scroll_offset(),
            paused_offset,
            "crossing the threshold must preserve the exact native offset"
        );
    });

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the reasoning body remains visible");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    redraw(cx);
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            trace.is_following(),
            "returning to the tail must re-arm follow"
        );
        assert!(
            trace.uses_virtualized_scroll(),
            "returning to the tail must migrate even when no later delta arrives"
        );
        assert!(
            trace.scroll_max().y + trace.scroll_offset().y <= STICK_THRESHOLD,
            "the new virtualized viewport must remain anchored to the tail"
        );
    });
}

/// Streaming reasoning follows the same manual-scroll contract as the main
/// transcript: an upward gesture pauses following, and a later downward
/// gesture at the end re-arms it.
#[gpui::test]
fn streaming_reasoning_respects_manual_scroll_position(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(700.)));
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..50 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    redraw(cx);
    redraw(cx);

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the expanded reasoning body was drawn");
    let (bottom_offset, max_offset) = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .map_or((px(0.), px(0.)), |turn| {
                let trace = reasoning_part(turn).expect("reasoning trace");
                (trace.scroll_offset().y, trace.scroll_max().y)
            })
    });
    assert!(
        max_offset > px(80.),
        "the fixture must have scrollable reasoning"
    );
    assert_eq!(bottom_offset, -max_offset);

    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(80.))),
        ..Default::default()
    });
    redraw(cx);

    let paused_offset = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            !trace.is_following(),
            "upward scrolling must pause following"
        );
        trace.scroll_offset().y
    });
    assert!(paused_offset > bottom_offset, "the card should move upward");

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 50..100 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    redraw(cx);
    redraw(cx);

    let still_paused = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        reasoning_part(turn)
            .expect("reasoning trace")
            .scroll_offset()
            .y
    });
    assert_eq!(
        still_paused, paused_offset,
        "new reasoning must not force a user who scrolled up back to the end"
    );

    let body = cx
        .debug_bounds("reasoning-body-0")
        .expect("the reasoning body remains visible");
    cx.simulate_event(ScrollWheelEvent {
        position: body.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    redraw(cx);

    let rearmed = cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        assert!(
            trace.is_following(),
            "scrolling down at the end must re-arm following (offset={:?}, max={:?})",
            trace.scroll_offset(),
            trace.scroll_max()
        );
        trace.scroll_offset().y
    });
    assert!(
        cx.update(|_, cx| {
            let turn = chat.read(cx).messages.last().expect("assistant turn");
            let trace = reasoning_part(turn).expect("reasoning trace");
            trace.scroll_max().y + rearmed <= STICK_THRESHOLD
        }),
        "the downward gesture should reach the card's end"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "final line.\n\n", cx);
        });
    });
    redraw(cx);
    redraw(cx);

    assert!(cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let trace = reasoning_part(turn).expect("reasoning trace");
        trace.scroll_max().y + trace.scroll_offset().y <= STICK_THRESHOLD
    }));
}

/// The reasoning viewport owns every vertical wheel gesture inside its bounds.
/// This remains true regardless of whether transcript wheel smoothing is on.
#[gpui::test]
fn reasoning_wheel_events_never_scroll_the_transcript(cx: &mut TestAppContext) {
    init_app(cx);

    for smooth_scrolling in [false, true] {
        let (chat, cx) = add_chat_window(cx);
        cx.simulate_resize(gpui::size(px(640.), px(420.)));
        cx.update(|_, cx| {
            preferences::update_in_memory(cx, |prefs| {
                prefs.smooth_chat_scrolling = smooth_scrolling;
            });
            chat.update(cx, |this, cx| {
                for index in 0..12 {
                    this.messages.push(Message::from_canonical(
                        LlmMessage {
                            role: crate::llm::Role::Assistant,
                            content: vec![ContentBlock::Text {
                                text: format!("earlier message {index}\n\n{}", "body ".repeat(24)),
                                provider_metadata: ProviderMetadata::default(),
                            }],
                            provider_metadata: ProviderMetadata::default(),
                        },
                        cx,
                    ));
                }
                for role in [Role::User, Role::Assistant] {
                    this.messages.push(Message::empty(role));
                }
                for line in 0..60 {
                    this.append_stream_reasoning(
                        0,
                        "reasoning-0".into(),
                        &format!("Reasoning line {line}.\n\n"),
                        cx,
                    );
                }
            });
        });
        redraw(cx);
        redraw(cx);

        assert!(
            cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y > px(0.)),
            "the transcript fixture must be scrollable"
        );
        let body = cx
            .debug_bounds("reasoning-body-0")
            .expect("the latest reasoning card must be visible");
        let transcript_before = cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
        let card_before = cx.update(|_, cx| {
            chat.read(cx)
                .messages
                .last()
                .and_then(reasoning_part)
                .expect("reasoning trace")
                .scroll_offset()
                .y
        });

        // A previously queued transcript animation must be abandoned as soon
        // as the pointer enters the nested card.
        if smooth_scrolling {
            cx.simulate_event(ScrollWheelEvent {
                position: point(px(320.), px(40.)),
                delta: ScrollDelta::Lines(point(0., -3.)),
                ..Default::default()
            });
            assert!(
                cx.update(|_, cx| chat.read(cx).smooth_scroll.remaining != px(0.)),
                "the transcript fixture must have a queued smooth motion"
            );
        }

        cx.simulate_event(ScrollWheelEvent {
            position: body.center(),
            delta: ScrollDelta::Lines(point(0., 3.)),
            ..Default::default()
        });

        let (transcript_after, card_offset, pending_transcript_scroll, pending_card_scroll) = cx
            .update(|_, cx| {
                let chat = chat.read(cx);
                let trace = chat
                    .messages
                    .last()
                    .and_then(reasoning_part)
                    .expect("reasoning trace");
                (
                    chat.list_state.logical_scroll_top(),
                    trace.scroll_offset().y,
                    chat.smooth_scroll.remaining,
                    trace.smooth_scroll_remaining(),
                )
            });
        assert_eq!(transcript_after.item_ix, transcript_before.item_ix);
        assert_eq!(
            transcript_after.offset_in_item, transcript_before.offset_in_item,
            "reasoning wheel input leaked into the transcript (smooth={smooth_scrolling})"
        );
        assert!(
            card_offset < px(0.),
            "the reasoning card itself must consume the wheel input"
        );
        assert_eq!(
            pending_transcript_scroll,
            px(0.),
            "reasoning input must not queue transcript motion"
        );
        if smooth_scrolling {
            assert_eq!(
                card_offset, card_before,
                "smooth reasoning scrolling must restore the native wheel jump"
            );
            assert!(
                pending_card_scroll != px(0.),
                "reasoning wheel input must queue eased card motion"
            );
            assert!(
                cx.update(|window, cx| window.simulate_next_frame(cx)) > 0,
                "reasoning wheel input must schedule an animation frame"
            );
            redraw(cx);
            let eased_card_offset = cx.update(|_, cx| {
                chat.read(cx)
                    .messages
                    .last()
                    .and_then(reasoning_part)
                    .expect("reasoning trace")
                    .scroll_offset()
                    .y
            });
            assert!(
                eased_card_offset > card_before,
                "the animation frame must advance the reasoning card"
            );

            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(20.))),
                ..Default::default()
            });
            let (precise_offset, pending_after_precise) = cx.update(|_, cx| {
                let trace = chat
                    .read(cx)
                    .messages
                    .last()
                    .and_then(reasoning_part)
                    .expect("reasoning trace");
                (trace.scroll_offset().y, trace.smooth_scroll_remaining())
            });
            assert!(
                precise_offset > eased_card_offset,
                "precise touchpad input must keep the card's native immediate path"
            );
            assert_eq!(
                pending_after_precise,
                px(0.),
                "precise input must cancel queued discrete-wheel motion"
            );
        } else {
            assert!(
                card_offset > card_before,
                "native reasoning scrolling must remain immediate when smoothing is off"
            );
            assert_eq!(pending_card_scroll, px(0.));
        }

        // Repeated wheel ticks at either nested boundary are still contained;
        // they must not fall through just because the card cannot move further.
        for delta in [
            ScrollDelta::Lines(point(0., 1_000.)),
            ScrollDelta::Lines(point(0., 1_000.)),
            ScrollDelta::Lines(point(0., -1_000.)),
            ScrollDelta::Lines(point(0., -1_000.)),
        ] {
            let transcript_before_boundary =
                cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta,
                ..Default::default()
            });
            let transcript_after_boundary =
                cx.update(|_, cx| chat.read(cx).list_state.logical_scroll_top());
            assert_eq!(
                transcript_after_boundary.item_ix, transcript_before_boundary.item_ix,
                "boundary wheel input leaked into the transcript (smooth={smooth_scrolling})"
            );
            assert_eq!(
                transcript_after_boundary.offset_in_item, transcript_before_boundary.offset_in_item,
                "boundary wheel input changed the transcript offset (smooth={smooth_scrolling})"
            );
        }
    }
}

/// Reasoning code blocks resolve the active palette in their custom renderer,
/// so a theme change must not churn the streaming markdown entity.
#[gpui::test]
fn theme_switch_preserves_the_reasoning_body(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "```json\n{\"a\":1}\n```", cx);
        });
    });
    let before = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .expect("trace")
            .body_entity_id()
    });

    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("the trace survives a theme switch");
        assert_eq!(
            reasoning.body_entity_id(),
            before,
            "theme changes must not replace the streaming markdown state"
        );
        assert!(
            reasoning_states(turn)[0].0.contains("json"),
            "re-parsing must not lose what already streamed"
        );
    });

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}
