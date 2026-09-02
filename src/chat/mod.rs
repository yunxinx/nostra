//! Single conversation view: message list, streaming assistant turn, and
//! composer input.
//!
//! [`ChatView`] owns the transcript and the composer. The pieces a turn is made
//! of live beside it: [`assistant`] bridges gateway events into this view's
//! entities, and [`error_card`] / [`reasoning_card`] render the two turn parts
//! that need more than markdown prose.

mod assistant;
pub(crate) mod conversation_runtime;
mod error_card;
mod hover_reveal;
mod message;
mod persistence;
mod reasoning_card;
mod render;
mod scrolling;

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, ElementId, Entity, EventEmitter,
    FollowMode, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement as _, Pixels, Render, ScrollWheelEvent, SharedString, Styled as _, Subscription,
    Window, div, list, point, px,
};
use gpui_component::{
    ActiveTheme, ElementExt as _, StyledExt as _, h_flex,
    input::{InputEvent, RopeExt as _},
    scroll::ScrollableElement as _,
    text::{TextView, TextViewStyle},
    v_flex,
};
use rust_i18n::t;

use crate::llm::{
    ContentBlock, GenerationService, IndexedMessage, Message as LlmMessage, ModelSelection,
    ProviderMetadata, ReasoningContent, ToolCall, ToolResult,
};
use crate::providers;
#[cfg(test)]
use crate::session::SessionStores;
use crate::session::{ConversationContext, SessionId};
use crate::ui::{
    markdown::{MarkdownBody, MarkdownExtensionSnapshot, MarkdownPresentation},
    reference_picker::{ChatReferenceComposer, ComposerEvent, ComposerStatus},
};
#[cfg(test)]
use crate::{llm::GatewayError, session::ChatTurnTerminal};

use self::conversation_runtime::{ConversationRuntime, ConversationRuntimeSnapshot};
use self::error_card::TurnError;
use self::hover_reveal::hover_reveal_copy;
pub use self::message::{Message, MessagePart, Role};
pub(crate) use self::persistence::restore::derive_title_from_state;
use self::reasoning_card::ReasoningTrace;
use self::render::copyable_text;
pub(crate) use self::scrolling::set_smooth_scrolling;
#[cfg(test)]
use self::scrolling::{
    SMOOTH_SCROLL_FINISH_THRESHOLD, SMOOTH_SCROLL_FRAME_FRACTION, reasoning_smooth_invalidations,
    record_reasoning_smooth_invalidation, reset_reasoning_smooth_invalidations,
};
use self::scrolling::{SmoothScrollState, smooth_scroll_animation_enabled};

const CONTENT_MAX_WIDTH: Pixels = px(760.);

const MESSAGE_LIST_OVERDRAW: Pixels = px(1_000.);
const MESSAGE_HEIGHT_HINT: Pixels = px(160.);
const STICK_THRESHOLD: Pixels = px(48.);

/// First-frame fallback until the floating composer reports its actual height.
const DEFAULT_COMPOSER_HEIGHT: Pixels = px(120.);

/// Deliberately over-scrolled deferred target for the composer viewport.
/// `InputState::set_scroll_offset` applies it after the next layout pass and
/// clamps it to the fresh content size, so this lands exactly at the bottom.
const COMPOSER_SCROLL_TO_END: Pixels = px(-1_000_000.);

static NEXT_MESSAGE_UI_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MESSAGE_PART_UI_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
struct UnavailableGenerationService;

#[cfg(test)]
impl GenerationService for UnavailableGenerationService {
    fn start(
        &self,
        _: crate::llm::GenerationRequest,
    ) -> Result<crate::llm::GenerationHandle, GatewayError> {
        Err(GatewayError::configuration(
            "test generation is unavailable",
        ))
    }
}

#[derive(Clone)]
pub enum ChatEvent {
    TitleChanged(SharedString),
    SelectionChanged(ModelSelection),
    /// Emitted when the view's runtime snapshot changes in a way that affects
    /// workspace annotations such as generation state.
    StateChanged,
    /// Emitted once a durable turn begin has bound a persisted session id to
    /// this view.  Carries the session id now authoritative for the view.
    SessionBound(SessionId),
    DeleteCompleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatDeleteRequest {
    RemoveNow,
    Pending,
    Rejected,
}

pub(crate) fn create_conversation_runtime(
    scope: crate::runtime::ConversationScopeHandle,
    conversation: ConversationContext,
    generation_service: Arc<dyn GenerationService>,
    cx: &mut App,
) -> Entity<ConversationRuntime> {
    cx.new(|_| ConversationRuntime::new(scope, conversation, generation_service))
}

pub struct ChatView {
    window_handle: AnyWindowHandle,
    messages: Vec<Message>,
    composer: Entity<ChatReferenceComposer>,
    composer_status: Rc<Cell<ComposerStatus>>,
    references_enabled: bool,
    runtime: Entity<ConversationRuntime>,
    runtime_snapshot: ConversationRuntimeSnapshot,
    /// Placeholder text currently installed in the input state.  Compared
    /// against the live translation each frame so a language switch updates
    /// the composer without rebuilding it (and without notify loops).
    placeholder: SharedString,
    /// Last measured height of the complete floating composer, including its
    /// outer padding. Used as the transcript's bottom inset.
    composer_height: Pixels,
    /// Composer height measured while the input was empty, i.e. at its
    /// single-row resting size.  The empty state centers against this instead
    /// of the live height so a multi-line draft grows the composer upward
    /// *over* the greeting rather than pushing it up the panel.  Re-measured
    /// whenever the input goes back to empty, so a font or text-size change
    /// recalibrates it on its own.
    base_composer_height: Pixels,
    /// Snapshot maintained by the input subscription. Rendering hooks must not
    /// read the external input entity while recording layout bounds or deciding
    /// whether the current draft can be submitted.
    input_empty: bool,
    input_blank: bool,
    list_state: ListState,
    smooth_scroll: SmoothScrollState,
    preference_snapshot: crate::preferences::Preferences,
    catalog_handle: crate::providers::ProviderCatalogHandle,
    catalog_snapshot: crate::providers::ProviderCatalogDocument,
    markdown_presentation: MarkdownPresentation,
    selection: Option<ModelSelection>,
    selection_available: bool,
    #[cfg(test)]
    next_reply_drop_flag: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    composer_revision: u64,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    materialized_message_indices: std::collections::BTreeSet<usize>,
}

impl ChatView {
    #[cfg(test)]
    pub fn view(window: &mut Window, cx: &mut App) -> Entity<Self> {
        let preference_handle = crate::preferences::handle(cx);
        Self::view_with_session_services_and_preferences(
            SessionStores::default().chat_conversation(),
            preference_handle,
            window,
            cx,
        )
    }

    #[cfg(test)]
    pub(crate) fn view_with_session_services(
        conversation: ConversationContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let preference_handle = crate::preferences::handle(cx);
        Self::view_with_session_services_and_preferences(
            conversation,
            preference_handle,
            window,
            cx,
        )
    }

    #[cfg(test)]
    pub(crate) fn view_with_session_services_and_preferences(
        conversation: ConversationContext,
        preference_handle: crate::preferences::PreferenceHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let runtime = create_conversation_runtime(
            crate::runtime::ConversationScopeHandle::for_test(),
            conversation,
            Arc::new(UnavailableGenerationService),
            cx,
        );
        Self::view_with_generation_service_and_preferences(runtime, preference_handle, window, cx)
    }

    #[cfg(test)]
    pub(crate) fn view_with_generation_service_and_preferences(
        runtime: Entity<ConversationRuntime>,
        preference_handle: crate::preferences::PreferenceHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let markdown_extensions = crate::ui::markdown::test_extension_snapshot();
        cx.new(|cx| Self::new(runtime, preference_handle, markdown_extensions, window, cx))
    }

    pub(crate) fn view_with_runtime_services(
        runtime: Entity<ConversationRuntime>,
        services: &crate::runtime::RuntimeServices,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let preference_handle = services.preference_handle().clone();
        let markdown_extensions = services.markdown_extensions().clone();
        cx.new(|cx| Self::new(runtime, preference_handle, markdown_extensions, window, cx))
    }

    fn new(
        runtime: Entity<ConversationRuntime>,
        preference_handle: crate::preferences::PreferenceHandle,
        markdown_extensions: MarkdownExtensionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preference_snapshot = preference_handle.snapshot();
        let catalog_handle = crate::providers::ensure_global(cx);
        let catalog_snapshot = catalog_handle.snapshot();
        let preference_state = preference_handle.shared_preferences();
        let markdown_presentation =
            MarkdownPresentation::new(preference_state, markdown_extensions);
        let preferences_for_observer = preference_handle.clone();
        let catalog_for_observer = catalog_handle.clone();
        let references_enabled = runtime.read(cx).supports_references();
        let references = runtime.read(cx).references();
        let placeholder: SharedString = if references_enabled {
            t!("reference_picker.composer_placeholder").to_string()
        } else {
            t!("chat.placeholder").to_string()
        }
        .into();
        let composer_status = Rc::new(Cell::new(ComposerStatus::default()));
        let composer = cx.new(|cx| {
            if references_enabled {
                ChatReferenceComposer::with_references(
                    composer_status.clone(),
                    references.clone(),
                    window,
                    cx,
                )
            } else {
                ChatReferenceComposer::chat(composer_status.clone(), references, window, cx)
            }
        });
        let input = composer.read(cx).input();

        let composer_subscription = cx.subscribe_in(
            &composer,
            window,
            |this, _, event, window, cx| match event {
                ComposerEvent::Submit(text) => {
                    this.submit(text.clone(), window, cx);
                }
                ComposerEvent::Stop => this.cancel_reply(cx),
            },
        );

        let subscription = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                this.composer_revision = this.composer_revision.saturating_add(1);
                let (input_empty, input_blank, cursor_line, lines_len, x) = {
                    let state = input.read(cx);
                    let value = state.value();
                    (
                        value.is_empty(),
                        value.trim().is_empty(),
                        state.cursor_position().line as usize,
                        state.text().lines_len(),
                        state.scroll_offset().x,
                    )
                };
                this.input_empty = input_empty;
                this.input_blank = input_blank;
                if lines_len > 1 && cursor_line + 1 == lines_len {
                    input.update(cx, |state, cx| {
                        state.set_scroll_offset(point(x, COMPOSER_SCROLL_TO_END), cx);
                    });
                }
            }
        });

        let selection = providers::last_selection_from(&catalog_snapshot);
        let selection_available =
            providers::selection_is_available_from(selection.as_ref(), &catalog_snapshot);
        let list_state = ListState::new(0, ListAlignment::Top, MESSAGE_LIST_OVERDRAW)
            .with_uniform_item_height(MESSAGE_HEIGHT_HINT);
        list_state.set_follow_mode(FollowMode::Tail);
        let runtime_snapshot = runtime.read(cx).snapshot();
        let runtime_subscription = cx.subscribe(&runtime, |this, _, update, cx| {
            this.handle_runtime_update(update, cx);
        });
        Self {
            window_handle: window.window_handle(),
            messages: Vec::new(),
            composer,
            composer_status,
            references_enabled,
            runtime,
            runtime_snapshot,
            placeholder,
            composer_height: DEFAULT_COMPOSER_HEIGHT,
            base_composer_height: DEFAULT_COMPOSER_HEIGHT,
            input_empty: true,
            input_blank: true,
            list_state,
            smooth_scroll: SmoothScrollState::default(),
            preference_snapshot,
            catalog_handle,
            catalog_snapshot,
            markdown_presentation,
            selection,
            selection_available,
            #[cfg(test)]
            next_reply_drop_flag: None,
            composer_revision: 0,
            _subscriptions: vec![
                composer_subscription,
                subscription,
                runtime_subscription,
                cx.observe_global_in::<crate::preferences::Prefs>(window, move |this, _, cx| {
                    let snapshot = preferences_for_observer.snapshot();
                    if this.preference_snapshot == snapshot {
                        return;
                    }
                    this.preference_snapshot = snapshot;
                    cx.notify();
                }),
                cx.observe_global_in::<crate::providers::ProviderCatalog>(
                    window,
                    move |this, _, cx| {
                        let snapshot = catalog_for_observer.snapshot();
                        if this.catalog_snapshot == snapshot {
                            return;
                        }
                        this.catalog_snapshot = snapshot;
                        this.sync_selection_availability();
                        cx.notify();
                    },
                ),
            ],
            #[cfg(test)]
            materialized_message_indices: std::collections::BTreeSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn project_view_with_session_services(
        conversation: ConversationContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let preference_handle = crate::preferences::handle(cx);
        Self::project_view_with_session_services_and_preferences(
            conversation,
            preference_handle,
            window,
            cx,
        )
    }

    #[cfg(test)]
    pub(crate) fn project_view_with_session_services_and_preferences(
        conversation: ConversationContext,
        preference_handle: crate::preferences::PreferenceHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::view_with_session_services_and_preferences(
            conversation,
            preference_handle,
            window,
            cx,
        )
    }

    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.focus_input(window, cx));
    }

    #[cfg(test)]
    pub(crate) const fn markdown_extension_revision(&self) -> u64 {
        self.markdown_presentation.extension_revision()
    }

    pub(crate) fn dismiss_composer_completion(&self, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.dismiss_completion(cx));
    }

    pub(crate) fn has_in_flight_work(&self) -> bool {
        self.runtime_snapshot.has_in_flight_work()
    }

    /// Force the newest message into view.  Used right after the user sends a
    /// message, where jumping to the bottom is always the desired behavior.
    pub fn scroll_to_bottom(&self) {
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
    }

    /// Called on every streaming update.  Keeps the latest tokens in view, but
    /// only while auto-follow is armed and the user is actually reading the
    /// bottom of the transcript, so scrolling up to re-read earlier turns is
    /// never interrupted. GPUI's tail-follow state re-engages after the user
    /// scrolls back to the true bottom.
    pub fn follow_stream(&self) {
        if self.list_state.is_following_tail() {
            self.list_state.scroll_to_end();
        }
    }

    pub fn start_stream_text(&mut self, content_index: usize, id: String, cx: &mut App) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if last
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Text { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id))
        {
            return;
        }
        let ui_id = NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed);
        last.parts.push(MessagePart::Text {
            content_index,
            ui_id,
            id,
            text: String::new(),
            replay: ProviderMetadata::default(),
            finished: false,
            body: MarkdownBody::new_with_presentation("", ui_id, &self.markdown_presentation, cx),
        });
        last.parts.sort_by_key(MessagePart::content_index);
    }

    pub fn append_stream_text(
        &mut self,
        content_index: usize,
        id: String,
        delta: &str,
        cx: &mut App,
    ) {
        // OpenAI Responses, like pi's adapter, forwards text deltas as received;
        // an empty delta carries no content and must not create a canonical block
        // or alter the independent reasoning lifecycle.
        if delta.is_empty() {
            return;
        }
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        let Some(position) = last.parts.iter().position(
            |part| matches!(part, MessagePart::Text { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id),
        ) else {
            self.start_stream_text(content_index, id.clone(), cx);
            self.append_stream_text(content_index, id, delta, cx);
            return;
        };
        if let MessagePart::Text {
            text,
            finished,
            body,
            ..
        } = &mut last.parts[position]
        {
            if *finished {
                return;
            }
            text.push_str(delta);
            body.push_str(delta, cx);
        }
    }

    pub fn finish_stream_text(
        &mut self,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
    ) {
        let Some(MessagePart::Text {
            replay: current,
            finished,
            ..
        }) = self.messages.last_mut().and_then(|message| {
            message
                .parts
                .iter_mut()
                .find(|part| matches!(part, MessagePart::Text { content_index: current, id: current_id, .. } if *current == content_index && current_id == id))
        })
        else {
            return;
        };
        if let Some(replay) = replay {
            *current = replay;
        }
        *finished = true;
    }

    /// Start the reasoning content block identified by the gateway stream.
    ///
    /// Protocol adapters emit this boundary before starting a different content
    /// block, so the presentation layer never infers one block's lifecycle from
    /// another block's deltas.
    pub fn start_stream_reasoning(&mut self, content_index: usize, id: String) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if last.parts.iter().any(
            |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id),
        ) {
            return;
        }
        last.parts.push(MessagePart::Reasoning {
            content_index,
            ui_id: NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed),
            id,
            reasoning: ReasoningContent {
                display: String::new(),
                replay: None,
            },
            finished: false,
            trace: None,
        });
        last.parts.sort_by_key(MessagePart::content_index);
    }

    pub fn append_stream_reasoning(
        &mut self,
        content_index: usize,
        id: String,
        delta: &str,
        cx: &mut App,
    ) {
        // Empty protocol deltas carry no visible state and must not create a
        // card or start its duration clock.
        if delta.is_empty() {
            return;
        }
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        let Some(position) = last.parts.iter().position(
            |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id),
        ) else {
            self.start_stream_reasoning(content_index, id.clone());
            self.append_stream_reasoning(content_index, id, delta, cx);
            return;
        };
        if let MessagePart::Reasoning {
            ui_id,
            reasoning,
            finished,
            trace,
            ..
        } = &mut last.parts[position]
        {
            // A finished block is immutable. Protocol adapters must allocate a
            // new id for later reasoning so its card, timer, disclosure, and
            // canonical position remain independent.
            if *finished {
                return;
            }
            reasoning.display.push_str(delta);
            trace
                .get_or_insert_with(|| {
                    ReasoningTrace::new_with_presentation(*ui_id, &self.markdown_presentation, cx)
                })
                .push(delta, cx);
        }
    }

    pub fn finish_stream_reasoning(
        &mut self,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
    ) {
        let Some(MessagePart::Reasoning {
            reasoning,
            finished,
            trace,
            ..
        }) = self.messages.last_mut().and_then(|message| {
            message.parts.iter_mut().find(
                |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == id),
            )
        })
        else {
            return;
        };
        if let Some(replay) = replay {
            reasoning.replay = Some(replay);
        }
        *finished = true;
        if let Some(trace) = trace {
            trace.finish();
        }
    }

    /// Apply an authoritative provider snapshot without changing this block's
    /// disclosure, timer, or stable GPUI identity.
    pub fn update_stream_reasoning_snapshot(
        &mut self,
        content_index: usize,
        id: &str,
        snapshot: ReasoningContent,
        cx: &mut App,
    ) {
        let Some(MessagePart::Reasoning {
            ui_id,
            reasoning,
            finished,
            trace,
            ..
        }) = self.messages.last_mut().and_then(|message| {
            message.parts.iter_mut().find(
                |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == id),
            )
        })
        else {
            return;
        };
        if !*finished {
            return;
        }
        if snapshot.display.is_empty() {
            *trace = None;
        } else if let Some(trace) = trace.as_mut() {
            trace.set_source(&snapshot.display, cx);
        } else {
            *trace = Some(ReasoningTrace::completed_with_presentation(
                snapshot.display.clone(),
                *ui_id,
                &self.markdown_presentation,
                cx,
            ));
        }
        *reasoning = snapshot;
    }

    pub fn start_stream_tool_call(
        &mut self,
        content_index: usize,
        index: usize,
        id: String,
        name: String,
    ) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if last.parts.iter().any(
            |part| matches!(part, MessagePart::ToolCall { content_index: current, .. } if *current == content_index),
        ) {
            return;
        }
        last.parts.push(MessagePart::ToolCall {
            content_index,
            ui_id: NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed),
            index,
            id,
            name,
            tool_call: None,
        });
        last.parts.sort_by_key(MessagePart::content_index);
    }

    pub fn finish_stream_tool_call(
        &mut self,
        content_index: usize,
        index: usize,
        tool_call: ToolCall,
    ) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if let Some(MessagePart::ToolCall {
            id,
            name,
            tool_call: current,
            ..
        }) = last.parts.iter_mut().find(
            |part| matches!(part, MessagePart::ToolCall { content_index: current, index: current_index, .. } if *current == content_index && *current_index == index),
        ) {
            *id = tool_call.id.clone();
            *name = tool_call.name.clone();
            *current = Some(tool_call);
        } else {
            last.parts.push(MessagePart::ToolCall {
                content_index,
                ui_id: NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed),
                index,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                tool_call: Some(tool_call),
            });
            last.parts.sort_by_key(MessagePart::content_index);
        }
    }

    pub fn finish_stream_batch(&mut self, cx: &mut Context<Self>) {
        self.remeasure_latest_message();
        self.follow_stream();
        cx.notify();
    }

    pub fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        if !self.runtime_snapshot.is_generating() {
            return;
        }
        self.runtime.update(cx, |runtime, _| runtime.request_stop());
    }

    fn apply_runtime_snapshot(&mut self, snapshot: ConversationRuntimeSnapshot) -> bool {
        if snapshot.revision() < self.runtime_snapshot.revision() {
            return false;
        }
        self.runtime_snapshot = snapshot;
        true
    }

    /// The transcript turn with `ui_id`, or `None` once the view is dropped or
    /// the turn is replaced by a replay.
    fn message_by_ui_id(&self, ui_id: u64) -> Option<&Message> {
        self.messages.iter().find(|message| message.ui_id == ui_id)
    }

    /// The complete prose of the turn with `ui_id`, for the message-level copy
    /// button. Read from the live message at click time rather than a render
    /// snapshot, so the clipboard always reflects the latest state.
    fn copyable_message_text(&self, ui_id: u64) -> Option<SharedString> {
        self.message_by_ui_id(ui_id).map(copyable_text)
    }

    /// The reasoning source of the block `reasoning_ui_id` inside the turn
    /// `message_ui_id`, for the reasoning card's copy button.
    fn reasoning_copy_source(
        &self,
        message_ui_id: u64,
        reasoning_ui_id: u64,
    ) -> Option<SharedString> {
        self.message_by_ui_id(message_ui_id).and_then(|message| {
            message.parts.iter().find_map(|part| match part {
                MessagePart::Reasoning {
                    ui_id,
                    reasoning,
                    trace: Some(_),
                    ..
                } if *ui_id == reasoning_ui_id => Some(reasoning.display.clone().into()),
                _ => None,
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn start_pending_reply_for_test(
        &mut self,
        dropped: std::rc::Rc<std::cell::Cell<bool>>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.runtime.update(cx, |runtime, cx| {
            runtime.request_generation = runtime
                .current_generation()
                .next()
                .expect("test request generation");
            runtime.generating = true;
            runtime.publish_state(cx);
            runtime.snapshot()
        });
        self.apply_runtime_snapshot(snapshot);
        self.runtime.update(cx, |runtime, cx| {
            runtime.reply_task = Some(assistant::ReplyTask::pending_for_test(dropped, cx));
        });
    }

    #[cfg(test)]
    pub(crate) fn seed_pending_turn_for_test(
        &mut self,
        user_message: LlmMessage,
        selection: ModelSelection,
        turn_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> crate::session::ChatTurnStart {
        let turn_id = turn_id.into();
        let start = self
            .runtime
            .read(cx)
            .session_controller_for_test()
            .lock()
            .expect("test controller lock")
            .begin_turn(user_message.clone(), selection, turn_id.clone())
            .expect("persist test turn begin");
        self.mark_turn_pending_for_test(start.session_id.clone(), turn_id, cx);
        self.messages
            .push(Message::from_canonical_with_presentation(
                user_message,
                &self.markdown_presentation,
                cx,
            ));
        self.messages.push(Message::empty(Role::Assistant));
        start
    }

    #[cfg(test)]
    pub(crate) fn mark_turn_pending_for_test(
        &mut self,
        session_id: SessionId,
        turn_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.runtime.update(cx, |runtime, cx| {
            runtime.mark_turn_pending_for_test(session_id, turn_id, cx);
            runtime.snapshot()
        });
        self.apply_runtime_snapshot(snapshot);
    }

    #[cfg(test)]
    pub(crate) fn mark_generating_for_test(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.runtime.update(cx, |runtime, cx| {
            runtime.mark_generating_for_test(cx);
            runtime.snapshot()
        });
        self.apply_runtime_snapshot(snapshot);
    }

    #[cfg(test)]
    pub(in crate::chat) fn runtime_snapshot_for_test(&self) -> ConversationRuntimeSnapshot {
        self.runtime_snapshot.clone()
    }

    #[cfg(test)]
    pub(in crate::chat) fn finish_current_reply_with_terminal_for_test(
        &mut self,
        message: Option<IndexedMessage>,
        terminal: ChatTurnTerminal,
        error: Option<GatewayError>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.runtime_snapshot.request_generation();
        self.finish_reply_with_terminal(generation, message, terminal, error, cx);
    }

    #[cfg(test)]
    pub(crate) fn start_durable_pending_reply_for_test(
        &mut self,
        dropped: std::rc::Rc<std::cell::Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.selection = Some(ModelSelection {
            profile_id: "fixture-profile".into(),
            model_id: "fixture-model".into(),
        });
        self.selection_available = true;
        self.next_reply_drop_flag = Some(dropped);
        self.submit("close during generation".to_string(), window, cx)
    }

    #[cfg(test)]
    pub(crate) fn durable_session_id_for_test(&self) -> Option<crate::session::SessionId> {
        self.runtime_snapshot.session_id().cloned()
    }

    #[cfg(test)]
    pub(crate) fn persist_session_for_test(
        &mut self,
        cx: &mut Context<Self>,
    ) -> crate::session::SessionId {
        let user_message = LlmMessage {
            role: crate::llm::Role::User,
            content: vec![ContentBlock::Text {
                text: "persisted fixture".into(),
                provider_metadata: ProviderMetadata::default(),
            }],
            provider_metadata: ProviderMetadata::default(),
        };
        let selection = ModelSelection {
            profile_id: "fixture-profile".into(),
            model_id: "fixture-model".into(),
        };
        let (start, snapshot) = self.runtime.update(cx, |runtime, cx| {
            let controller = runtime
                .session_controller
                .as_ref()
                .expect("test Chat store should be available");
            let mut controller = controller.lock().expect("test controller lock");
            let start = controller
                .begin_turn(user_message, selection, "fixture-turn")
                .expect("persist test turn");
            controller
                .finish_turn("fixture-turn", &ChatTurnTerminal::cancelled())
                .expect("persist test terminal");
            drop(controller);
            runtime.session_id = Some(start.session_id.clone());
            runtime.publish_state(cx);
            (start, runtime.snapshot())
        });
        self.apply_runtime_snapshot(snapshot);
        cx.emit(ChatEvent::SessionBound(start.session_id.clone()));
        start.session_id
    }

    pub fn select_model(&mut self, selection: ModelSelection, cx: &mut Context<Self>) {
        if !self.update_selection(selection.clone()) {
            return;
        }
        providers::select_model(selection.clone(), &self.catalog_handle, cx);
        cx.emit(ChatEvent::SelectionChanged(selection));
        cx.notify();
    }

    fn update_selection(&mut self, selection: ModelSelection) -> bool {
        if self.selection.as_ref() == Some(&selection) {
            return false;
        }
        self.selection = Some(selection);
        self.selection_available = true;
        true
    }

    pub fn selection(&self) -> Option<ModelSelection> {
        self.selection.clone()
    }

    /// Whether this view is currently streaming a provider reply.  Used by the
    /// workspace sidebar to annotate the row without deriving other row data
    /// from the view.
    pub(crate) fn is_generating(&self) -> bool {
        self.runtime_snapshot.is_generating()
    }

    fn sync_selection_availability(&mut self) {
        self.selection_available =
            providers::selection_is_available_from(self.selection.as_ref(), &self.catalog_snapshot);
    }
}

impl EventEmitter<ChatEvent> for ChatView {}

fn is_replayable(message: &LlmMessage) -> bool {
    message.role != crate::llm::Role::Assistant || !message.content.is_empty()
}

fn derive_title(text: &str) -> SharedString {
    let mut cleaned = text.replace('\n', " ");
    if cleaned.chars().count() > 40 {
        cleaned = cleaned.chars().take(37).collect::<String>() + "...";
    }
    if cleaned.trim().is_empty() {
        t!("chat.default_title").to_string().into()
    } else {
        cleaned.into()
    }
}

/// Derive a sidebar title from the first user message text.  Exported so the
/// workspace can compute a title from a [`ResolvedSessionState`] before the
/// hydrated view's first event callback fires.
#[allow(dead_code)]
pub(crate) fn derive_chat_title(text: &str) -> SharedString {
    derive_title(text)
}

#[cfg(test)]
mod tests;
