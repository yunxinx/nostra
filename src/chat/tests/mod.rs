use std::{
    cell::RefCell,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use gpui::{
    IntoElement as _, ListOffset, Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent,
    TestAppContext, point, px,
};
use gpui_component::input::InputEvent;

use crate::llm::{
    ContentBlock, IndexedContentBlock, IndexedMessage, Message as LlmMessage, ModelSelection,
    ProviderMetadata, ResponsesReplayMetadata,
};
use crate::preferences;
use crate::session::{
    CatalogQuery, ChatTurnTerminal, InMemorySessionStore, LocalSessionStore, LocalStoreConfig,
    SessionCatalogStore, SessionDomain, SessionReadStore, SessionStores, TurnStatus,
};

use super::reasoning_card::VIRTUALIZED_SOURCE_BYTES;
use super::{
    CONTENT_MAX_WIDTH, ChatDeleteRequest, ChatEvent, ChatView, Message, MessagePart,
    ReasoningTrace, Role, SMOOTH_SCROLL_FINISH_THRESHOLD, SMOOTH_SCROLL_FRAME_FRACTION,
    STICK_THRESHOLD, SmoothScrollState, is_replayable, reasoning_smooth_invalidations,
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
    // Test windows start inactive even though a real main window is activated
    // during startup. Keep the helper's default aligned with production so
    // smooth-scroll tests exercise the active path unless they explicitly
    // call `deactivate_window`.
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
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

mod code_blocks;
mod markdown_streaming;
mod math_interaction;
mod math_rendering;
mod performance;
/// Agent-runnable feedback loop for the user-visible long-content stall.
///
/// Keep this test deterministic: elapsed time is diagnostic, while the bound
/// on continuous code-text elements is the pass/fail signal. A long block must
/// not be rebuilt line-by-line for disclosure, wrap, and every smooth-scroll frame.
mod persistence;
mod presentation;
mod reasoning_actions;
mod reasoning_layout;
mod reasoning_lifecycle;
mod reasoning_scrolling;
mod scrolling;
