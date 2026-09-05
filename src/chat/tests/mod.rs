use std::{
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use gpui::{
    App, AppContext as _, IntoElement as _, ListOffset, Modifiers, MouseButton, ParentElement as _,
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

use super::rows::{ProseRenderer, ReasoningRenderer, RowRenderer as _};
pub(crate) use super::test_support;
use super::transcript::{PartId, PartSource};
use super::{
    ChatDeleteRequest, ChatView, Role, SMOOTH_SCROLL_FINISH_THRESHOLD,
    SMOOTH_SCROLL_FRAME_FRACTION, STICK_THRESHOLD, SmoothScrollState, Turn, is_replayable,
    reasoning_smooth_invalidations, reset_reasoning_smooth_invalidations,
};
use crate::chat::projection::{Row, RowId, RowKind};

fn rows_of_kind(chat: &ChatView, kind: RowKind) -> Vec<&Row> {
    chat.view
        .projection
        .rows()
        .iter()
        .filter(|row| row.kind() == kind)
        .collect()
}

fn renderer_for_row<'a>(chat: &'a ChatView, row: &Row) -> Option<&'a dyn super::rows::RowRenderer> {
    let ix = chat.view.projection.row_index(row.id())?;
    Some(chat.view.slots[ix].renderer.as_ref())
}

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
pub(in crate::chat) fn seed_turn(chat: &gpui::Entity<ChatView>, cx: &mut gpui::VisualTestContext) {
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for role in [Role::User, Role::Assistant] {
                test_support::push_empty(this, role, cx);
            }
        });
    });
}

/// The last turn-error row's renderer.
fn last_error_renderer(chat: &ChatView) -> Option<&crate::chat::rows::TurnErrorRenderer> {
    rows_of_kind(chat, RowKind::TurnError)
        .last()
        .and_then(|row| renderer_for_row(chat, row))
        .and_then(|renderer| {
            renderer
                .as_any()
                .downcast_ref::<crate::chat::rows::TurnErrorRenderer>()
        })
}

/// The last user bubble renderer.
fn last_user_bubble_renderer(chat: &ChatView) -> Option<&crate::chat::rows::UserBubbleRenderer> {
    rows_of_kind(chat, RowKind::UserBubble)
        .last()
        .and_then(|row| renderer_for_row(chat, row))
        .and_then(|renderer| {
            renderer
                .as_any()
                .downcast_ref::<crate::chat::rows::UserBubbleRenderer>()
        })
}

/// Fold/unfold the last turn-error row's raw response body. Returns whether
/// a renderer was found.
fn toggle_error_row(chat: &mut ChatView, cx: &mut App) -> bool {
    let Some(row_id) = rows_of_kind(chat, RowKind::TurnError)
        .last()
        .map(|row| row.id())
    else {
        return false;
    };
    let Some(ix) = chat.view.projection.row_index(row_id) else {
        return false;
    };
    let Some(renderer) = chat.view.slots[ix]
        .renderer
        .as_any_mut()
        .downcast_mut::<crate::chat::rows::TurnErrorRenderer>()
    else {
        return false;
    };
    renderer.toggle_disclosure(crate::chat::rows::DisclosureTarget::ErrorBody, cx);
    let disclosure = renderer.disclosure();
    chat.view.projection.set_disclosure(row_id, disclosure);
    true
}

/// The last tool-activity row's renderer.
fn last_activity_renderer(chat: &ChatView) -> Option<&crate::chat::rows::ToolActivityRenderer> {
    rows_of_kind(chat, RowKind::ToolActivity)
        .last()
        .and_then(|row| renderer_for_row(chat, row))
        .and_then(|renderer| {
            renderer
                .as_any()
                .downcast_ref::<crate::chat::rows::ToolActivityRenderer>()
        })
}

/// Expand/collapse the last tool-activity row's body. Returns whether a
/// renderer was found.
fn toggle_activity_row(chat: &mut ChatView, cx: &mut App) -> bool {
    let Some(row_id) = rows_of_kind(chat, RowKind::ToolActivity)
        .last()
        .map(|row| row.id())
    else {
        return false;
    };
    let Some(ix) = chat.view.projection.row_index(row_id) else {
        return false;
    };
    let Some(renderer) = chat.view.slots[ix]
        .renderer
        .as_any_mut()
        .downcast_mut::<crate::chat::rows::ToolActivityRenderer>()
    else {
        return false;
    };
    renderer.toggle_disclosure(crate::chat::rows::DisclosureTarget::Activity, cx);
    let disclosure = renderer.disclosure();
    chat.view.projection.set_disclosure(row_id, disclosure);
    true
}

fn reasoning_row_for_part(chat: &ChatView, part_id: PartId) -> Option<RowId> {
    rows_of_kind(chat, RowKind::Reasoning)
        .into_iter()
        .map(|row| row.id())
        .find(|id| id.part == part_id)
}

fn toggle_reasoning_row_by_id(chat: &mut ChatView, row_id: RowId) -> bool {
    let Some(ix) = chat.view.projection.row_index(row_id) else {
        return false;
    };
    let Some(renderer) = chat.view.slots[ix]
        .renderer
        .as_any_mut()
        .downcast_mut::<ReasoningRenderer>()
    else {
        return false;
    };
    renderer.toggle_for_test();
    true
}

fn reasoning_renderer_by_part(chat: &ChatView, part_id: PartId) -> Option<&ReasoningRenderer> {
    let row_id = reasoning_row_for_part(chat, part_id)?;
    let ix = chat.view.projection.row_index(row_id)?;
    chat.view.slots[ix]
        .renderer
        .as_any()
        .downcast_ref::<ReasoningRenderer>()
}

fn last_llm(chat: &ChatView, cx: &App) -> LlmMessage {
    last_turn(chat, cx).to_llm()
}

fn last_reasoning_id(chat: &ChatView) -> u64 {
    reasoning_part(chat)
        .map(ReasoningRenderer::owner_id)
        .expect("reasoning part")
}

fn last_prose_id(chat: &ChatView) -> u64 {
    rows_of_kind(chat, RowKind::AssistantProse)
        .last()
        .and_then(|row| renderer_for_row(chat, row))
        .and_then(|renderer| {
            renderer
                .as_any()
                .downcast_ref::<ProseRenderer>()
                .and_then(ProseRenderer::body_owner_for_test)
        })
        .expect("prose part")
}

fn last_turn<'a>(chat: &'a ChatView, cx: &'a App) -> &'a Turn {
    chat.transcript
        .read(cx)
        .turns()
        .last()
        .expect("seeded turn")
}

fn prose_at(
    chat: &ChatView,
    turn_index: usize,
    part_index: usize,
) -> (u64, &str, &crate::ui::markdown::MarkdownBody) {
    let rows: Vec<&Row> = rows_of_kind(chat, RowKind::AssistantProse)
        .into_iter()
        .filter(|row| row.turn_index() == turn_index)
        .collect();
    let row = rows
        .get(part_index)
        .unwrap_or_else(|| panic!("no prose row at ({turn_index}, {part_index})"));
    let renderer = renderer_for_row(chat, row)
        .and_then(|renderer| renderer.as_any().downcast_ref::<ProseRenderer>())
        .expect("prose renderer");
    (
        renderer.body_owner_for_test().expect("prose owner"),
        renderer.text_for_test(),
        renderer.body_for_test().expect("materialized prose body"),
    )
}

fn prose_body_mut(
    chat: &mut ChatView,
    turn_index: usize,
    part_index: usize,
) -> &mut crate::ui::markdown::MarkdownBody {
    let row_id = {
        let rows: Vec<&Row> = rows_of_kind(chat, RowKind::AssistantProse)
            .into_iter()
            .filter(|row| row.turn_index() == turn_index)
            .collect();
        rows[part_index].id()
    };
    let ix = chat.view.projection.row_index(row_id).expect("prose row");
    chat.view.slots[ix]
        .renderer
        .as_any_mut()
        .downcast_mut::<ProseRenderer>()
        .expect("prose renderer")
        .body_for_test_mut()
        .expect("materialized prose body")
}

/// The first reasoning renderer that owns a markdown body — the rows that
/// actually show (or can show) reasoning content. Replay-only parts allocate
/// no body and read as absent.
fn reasoning_part(chat: &ChatView) -> Option<&ReasoningRenderer> {
    rows_of_kind(chat, RowKind::Reasoning)
        .iter()
        .find_map(|row| {
            let renderer = renderer_for_row(chat, row)?
                .as_any()
                .downcast_ref::<ReasoningRenderer>()?;
            renderer.body_for_test()?;
            Some(renderer)
        })
}

/// Mutate the first reasoning renderer that owns a markdown body.
fn reasoning_part_mut(chat: &mut ChatView) -> Option<&mut ReasoningRenderer> {
    let row_id = rows_of_kind(chat, RowKind::Reasoning)
        .iter()
        .find(|row| {
            renderer_for_row(chat, row)
                .and_then(|renderer| renderer.as_any().downcast_ref::<ReasoningRenderer>())
                .is_some_and(|renderer| renderer.body_for_test().is_some())
        })?
        .id();
    let ix = chat.view.projection.row_index(row_id)?;
    let renderer = chat.view.slots[ix]
        .renderer
        .as_any_mut()
        .downcast_mut::<ReasoningRenderer>()?;
    renderer.body_for_test()?;
    Some(renderer)
}

fn reasoning_parts(chat: &ChatView) -> Vec<&ReasoningRenderer> {
    rows_of_kind(chat, RowKind::Reasoning)
        .iter()
        .filter_map(|row| {
            let renderer = renderer_for_row(chat, row)?
                .as_any()
                .downcast_ref::<ReasoningRenderer>()?;
            renderer.body_for_test()?;
            Some(renderer)
        })
        .collect()
}

fn reasoning_states<'a>(chat: &'a ChatView, cx: &'a App) -> Vec<(&'a str, bool)> {
    let turn = last_turn(chat, cx);
    turn.parts
        .iter()
        .filter_map(|part| match &part.source {
            PartSource::Reasoning { reasoning, .. } => {
                Some((reasoning.display.as_str(), part.finished))
            }
            _ => None,
        })
        .collect()
}

pub(in crate::chat) fn init_app(cx: &mut TestAppContext) {
    let prefs = preferences::Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::appearance::fonts::init(prefs.composer_font, cx);
        preferences::init_global(prefs, cx);
    });
}

pub(in crate::chat) fn add_chat_window(
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
        let transcript = cx.new(crate::chat::transcript::Transcript::new);
        let runtime = super::create_conversation_runtime(
            scope,
            conversation,
            generation_service,
            transcript.clone(),
            cx,
        );
        let chat = ChatView::view_with_generation_service_and_preferences(
            runtime,
            transcript,
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

pub(in crate::chat) fn redraw(cx: &mut gpui::VisualTestContext) {
    cx.run_until_parked();
    // A production frame runs its scheduled frame callbacks (the windowed
    // transcript's materialization sync) before painting; mirror that here.
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        let _ = window.draw(cx);
    });
}

/// Redraw until the windowed materialization settles: the first frame lets
/// the render pass request syncs, the second consumes them.
pub(in crate::chat) fn redraw_settled(cx: &mut gpui::VisualTestContext) {
    redraw(cx);
    redraw(cx);
}

fn redraw_settled_math(cx: &mut gpui::VisualTestContext) {
    redraw(cx);
    // First appearance renders immediately; later fingerprint changes inside
    // 120ms still wait for the coalesce timer. Advance it so both paths settle.
    cx.executor()
        .advance_clock(crate::ui::math::FORMULA_DEBOUNCE);
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
pub(in crate::chat) mod fixtures;
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
mod waiting;
mod windowed_layout;
