use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use gpui::{
    AppContext as _, IntoElement as _, ListOffset, Modifiers, MouseButton, ParentElement as _,
    ScrollDelta, ScrollWheelEvent, TestAppContext, point, px,
};
use gpui_component::input::InputEvent;

use crate::llm::{
    ContentBlock, FinishReason, GatewayError, GenerationEvent, GenerationHandle, GenerationOutcome,
    GenerationRequest, GenerationRunner, GenerationService, IndexedContentBlock, IndexedMessage,
    Message as LlmMessage, ModelSelection, OutcomeStatus, Protocol, ProviderMetadata,
    ResponsesReplayMetadata, Usage,
};
use crate::preferences;
use crate::session::{
    CatalogQuery, ChatTurnTerminal, ConversationContext, InMemorySessionStore, LocalSessionStore,
    LocalStoreConfig, ProjectIdentity, ResolvedSessionState, SessionCatalogStore, SessionDomain,
    SessionId, SessionReadStore, SessionStores, TurnStatus,
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

#[gpui::test]
fn delayed_runtime_snapshots_cannot_revert_the_chat_view_projection(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);

    let (older, latest) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let older = this.runtime.update(cx, |runtime, cx| {
                runtime.generating = true;
                runtime.publish_state(cx);
                runtime.snapshot()
            });
            let latest = this.runtime.update(cx, |runtime, cx| {
                runtime.generating = false;
                runtime.publish_state(cx);
                runtime.snapshot()
            });

            assert!(older.revision() < latest.revision());
            assert!(this.apply_runtime_snapshot(latest.clone()));
            (older, latest)
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            assert!(
                !this.apply_runtime_snapshot(older),
                "a delayed runtime snapshot must not replace a newer projection"
            );
            let current = this.runtime_snapshot_for_test();
            assert_eq!(current.revision(), latest.revision());
            assert!(!current.is_generating());
        });
    });

    cx.run_until_parked();

    cx.update(|_, cx| {
        chat.read_with(cx, |this, _| {
            let current = this.runtime_snapshot_for_test();
            assert_eq!(current.revision(), latest.revision());
            assert!(!current.is_generating());
        });
    });
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
    add_chat_window_with_session_services(cx, SessionStores::default().chat_conversation())
}

fn add_chat_window_with_stores(
    cx: &mut TestAppContext,
    stores: SessionStores,
) -> (gpui::Entity<ChatView>, &mut gpui::VisualTestContext) {
    add_chat_window_with_session_services(cx, stores.chat_conversation())
}

fn add_chat_window_with_session_services(
    cx: &mut TestAppContext,
    conversation: ConversationContext,
) -> (gpui::Entity<ChatView>, &mut gpui::VisualTestContext) {
    let (root, cx) = cx.add_window_view(|window, cx| {
        let chat = ChatView::view_with_session_services(conversation, window, cx);
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

fn add_chat_window_with_generation_service(
    cx: &mut TestAppContext,
    stores: SessionStores,
    generation_service: Arc<dyn GenerationService>,
) -> (gpui::Entity<ChatView>, &mut gpui::VisualTestContext) {
    let conversation = stores.chat_conversation();
    let preference_handle = cx.update(|cx| preferences::handle(cx));
    let scope = crate::runtime::ConversationScopeHandle::for_test();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let runtime =
            super::create_conversation_runtime(scope, conversation, generation_service, cx);
        let chat = ChatView::view_with_generation_service_and_preferences(
            runtime,
            preference_handle,
            window,
            cx,
        );
        gpui_component::Root::new(chat, window, cx)
    });
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

struct ScriptedGenerationService {
    events: Arc<Vec<GenerationEvent>>,
    pending: bool,
}

impl ScriptedGenerationService {
    fn completed(events: Vec<GenerationEvent>) -> Self {
        Self {
            events: Arc::new(events),
            pending: false,
        }
    }

    fn pending() -> Self {
        Self {
            events: Arc::new(Vec::new()),
            pending: true,
        }
    }
}

impl GenerationService for ScriptedGenerationService {
    fn start(&self, _: GenerationRequest) -> Result<GenerationHandle, GatewayError> {
        Ok(GenerationHandle::from_runner(ScriptedGenerationRunner {
            events: self.events.iter().cloned().collect(),
            pending: self.pending,
        }))
    }
}

struct ScriptedGenerationRunner {
    events: Vec<GenerationEvent>,
    pending: bool,
}

impl GenerationRunner for ScriptedGenerationRunner {
    fn run<'a>(
        &'a mut self,
        on_event: &'a mut dyn FnMut(GenerationEvent) -> bool,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        let events = std::mem::take(&mut self.events);
        let pending = self.pending;
        Box::pin(async move {
            if pending {
                std::future::pending::<()>().await;
                return;
            }
            for event in events {
                if !on_event(event) {
                    break;
                }
            }
        })
    }

    fn cancel(&mut self) -> Option<GenerationEvent> {
        Some(GenerationEvent::Finished(Box::new(GenerationOutcome {
            request_id: "scripted-cancel".into(),
            profile_id: "profile".into(),
            model_id: "model".into(),
            protocol: Protocol::Responses,
            status: OutcomeStatus::Cancelled,
            finish_reason: None,
            usage: Usage::default(),
            response_id: None,
            upstream_model: None,
            time_to_first_event: None,
            latency: Duration::ZERO,
            message: None,
            error: None,
        })))
    }
}

fn scripted_completed_events() -> Vec<GenerationEvent> {
    vec![
        GenerationEvent::TextStarted {
            content_index: 0,
            id: "text-0".into(),
        },
        GenerationEvent::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "scripted".into(),
        },
        GenerationEvent::TextFinished {
            content_index: 0,
            id: "text-0".into(),
            replay: None,
        },
        GenerationEvent::Finished(Box::new(GenerationOutcome {
            request_id: "scripted-complete".into(),
            profile_id: "profile".into(),
            model_id: "model".into(),
            protocol: Protocol::Responses,
            status: OutcomeStatus::Completed,
            finish_reason: Some(FinishReason::Stop),
            usage: Usage::default(),
            response_id: Some("response".into()),
            upstream_model: Some("model".into()),
            time_to_first_event: None,
            latency: Duration::ZERO,
            message: Some(IndexedMessage {
                role: crate::llm::Role::Assistant,
                content: vec![IndexedContentBlock {
                    content_index: 0,
                    block: ContentBlock::Text {
                        text: "scripted".into(),
                        provider_metadata: ProviderMetadata::default(),
                    },
                }],
                provider_metadata: ProviderMetadata::default(),
            }),
            error: None,
        })),
    ]
}

#[gpui::test]
fn chat_and_project_modes_share_the_composer_entity_with_project_references_enabled(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let stores =
        SessionStores::with_stores(InMemorySessionStore::new(), InMemorySessionStore::new());
    let project = ProjectIdentity::new("/tmp/nostra-shared-composer", "Shared composer");
    let chat_conversation = stores.chat_conversation();
    let project_conversation = stores.project_conversation(project.clone());
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let chat = ChatView::view_with_session_services(chat_conversation, window, cx);
        let project =
            ChatView::project_view_with_session_services(project_conversation, window, cx);
        let pair = cx.new(|_| ComposerPair { chat, project });
        gpui_component::Root::new(pair, window, cx)
    });
    let pair = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<ComposerPair>()
            .expect("Root must contain the composer pair")
    });
    pair.read_with(cx, |pair, cx| {
        assert!(!pair.chat.read(cx).composer.read(cx).references_enabled());
        assert!(pair.project.read(cx).composer.read(cx).references_enabled());
    });
}

struct ComposerPair {
    chat: gpui::Entity<ChatView>,
    project: gpui::Entity<ChatView>,
}

impl gpui::Render for ComposerPair {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
            .child(self.chat.clone())
            .child(self.project.clone())
    }
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
