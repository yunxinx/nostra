//! Single conversation view: message list, streaming assistant turn, and
//! composer input.
//!
//! [`ChatView`] owns the transcript and the composer. The pieces a turn is made
//! of live beside it: [`assistant`] bridges gateway events into this view's
//! entities, and [`error_card`] / [`reasoning_card`] render the two turn parts
//! that need more than markdown prose.

mod assistant;
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
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, ElementId, Entity, EventEmitter,
    FollowMode, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement as _, Pixels, Render, ScrollWheelEvent, SharedString, Styled as _, Subscription,
    Task, Window, div, list, point, px,
};
use gpui_component::{
    ActiveTheme, ElementExt as _, StyledExt as _, WindowExt as _, h_flex,
    input::{InputEvent, RopeExt as _, TextareaState},
    notification::NotificationType,
    scroll::ScrollableElement as _,
    text::{TextView, TextViewStyle},
    v_flex,
};
use rust_i18n::t;

use crate::llm::{
    ContentBlock, GatewayError, GenerationService, IndexedMessage, Message as LlmMessage,
    ModelSelection, ProviderMetadata, ReasoningContent, ToolCall, ToolResult,
};
use crate::providers;
#[cfg(test)]
use crate::session::SessionStores;
use crate::session::{
    ChatSessionController, ChatSessionControllerError, ChatTurnStart, ChatTurnTerminal,
    ConversationScope, ConversationSessionServices, ProjectIdentity, SessionId,
    SessionOperationGuard, SharedSessionStore,
};
use crate::ui::{
    markdown::MarkdownBody,
    reference_picker::{ChatReferenceComposer, ComposerEvent, ComposerStatus},
};

use self::error_card::TurnError;
use self::hover_reveal::hover_reveal_copy;
pub use self::message::{Message, MessagePart, Role};
use self::reasoning_card::ReasoningTrace;
use self::render::copyable_text;
#[cfg(test)]
use self::scrolling::{
    SMOOTH_SCROLL_FINISH_THRESHOLD, SMOOTH_SCROLL_FRAME_FRACTION, reasoning_smooth_invalidations,
    record_reasoning_smooth_invalidation, reset_reasoning_smooth_invalidations,
};
use self::scrolling::{SmoothScrollState, smooth_scroll_animation_enabled};
pub(crate) use self::scrolling::{set_smooth_scrolling, smooth_scrolling_enabled};

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

static NEXT_CONVERSATION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MESSAGE_UI_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MESSAGE_PART_UI_ID: AtomicU64 = AtomicU64::new(1);

type ChatSessionControllerHandle = Arc<Mutex<ChatSessionController<SharedSessionStore>>>;

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
    /// Emitted once a durable turn begin has bound a persisted session id to
    /// this view.  Carries the session id now authoritative for the view.
    SessionBound(SessionId),
    DeleteCompleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatDeleteRequest {
    RemoveNow,
    Pending,
}

pub struct ChatView {
    window_handle: AnyWindowHandle,
    messages: Vec<Message>,
    input: Entity<TextareaState>,
    composer: Entity<ChatReferenceComposer>,
    composer_status: Rc<Cell<ComposerStatus>>,
    scope: ConversationScope,
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
    /// read the external `InputState` entity while recording layout bounds.
    input_empty: bool,
    list_state: ListState,
    smooth_scroll: SmoothScrollState,
    selection: Option<ModelSelection>,
    selection_available: bool,
    provider_catalog_revision: u64,
    generation_service: Arc<dyn GenerationService>,
    pending: bool,
    reply_task: Option<assistant::ReplyTask>,
    #[cfg(test)]
    next_reply_drop_flag: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    session_controller: Option<ChatSessionControllerHandle>,
    /// Reservation source kept separate from the controller mutex so the UI
    /// can make a queued write visible to shutdown before background work has
    /// a chance to acquire the controller.
    session_store: Option<SharedSessionStore>,
    session_unavailable: Option<String>,
    persistence_pending: bool,
    _persistence_task: Option<Task<()>>,
    /// Sticky once permanent deletion has been accepted. It prevents a
    /// durable begin that was already queued from starting provider work
    /// before the deletion callback removes the view.
    deletion_requested: bool,
    deletion_pending: bool,
    _deletion_task: Option<Task<()>>,
    /// Prevents a durable begin callback from starting provider work after the
    /// application has entered its pre-quit durability barrier.
    shutdown_requested: bool,
    composer_revision: u64,
    /// Active turn id retained until the durable terminal write settles.
    pending_turn_id: Option<String>,
    /// A detached persistence worker owns the reservation after durable begin.
    /// Releasing this view signals cancellation but cannot drop the terminal
    /// write that shutdown is waiting for.
    terminal_persistence: Option<persistence::TurnPersistenceCoordinator>,
    /// Terminal fact retained for an automatic retry on the next submit after
    /// a transient catalog/JSONL failure.
    pending_terminal: Option<(String, ChatTurnTerminal)>,
    conversation_id: String,
    next_turn_id: u64,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    materialized_message_indices: std::collections::BTreeSet<usize>,
}

impl ChatView {
    #[cfg(test)]
    pub fn view(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::view_with_session_services(SessionStores::default().chat_conversation(), window, cx)
    }

    #[cfg(test)]
    pub(crate) fn view_with_session_services(
        session_services: ConversationSessionServices,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            Self::new(
                ConversationScope::Chat,
                session_services,
                Arc::new(UnavailableGenerationService),
                window,
                cx,
            )
        })
    }

    pub(crate) fn view_with_generation_service(
        session_services: ConversationSessionServices,
        generation_service: Arc<dyn GenerationService>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            Self::new(
                ConversationScope::Chat,
                session_services,
                generation_service,
                window,
                cx,
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn project_view_with_session_services(
        project: ProjectIdentity,
        session_services: ConversationSessionServices,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            Self::new(
                ConversationScope::Project(project),
                session_services,
                Arc::new(UnavailableGenerationService),
                window,
                cx,
            )
        })
    }

    pub(crate) fn project_view_with_generation_service(
        project: ProjectIdentity,
        session_services: ConversationSessionServices,
        generation_service: Arc<dyn GenerationService>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            Self::new(
                ConversationScope::Project(project),
                session_services,
                generation_service,
                window,
                cx,
            )
        })
    }

    fn new(
        scope: ConversationScope,
        session_services: ConversationSessionServices,
        generation_service: Arc<dyn GenerationService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder: SharedString = match &scope {
            ConversationScope::Chat => t!("chat.placeholder").to_string(),
            ConversationScope::Project(_) => {
                t!("reference_picker.composer_placeholder").to_string()
            }
        }
        .into();
        let composer_status = Rc::new(Cell::new(ComposerStatus::default()));
        let composer = cx.new(|cx| match &scope {
            ConversationScope::Chat => ChatReferenceComposer::chat(
                composer_status.clone(),
                session_services.references(),
                window,
                cx,
            ),
            ConversationScope::Project(_) => ChatReferenceComposer::with_references(
                composer_status.clone(),
                session_services.references(),
                window,
                cx,
            ),
        });
        let input = composer.read(cx).input();

        let composer_subscription = cx.subscribe_in(
            &composer,
            window,
            |this, _, event, window, cx| match event {
                ComposerEvent::Submit(text) => {
                    this.submit(text.clone(), window, cx);
                }
                ComposerEvent::Stop => this.cancel_reply(),
            },
        );

        let subscription = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                this.composer_revision = this.composer_revision.saturating_add(1);
                // After an edit that lands the caret on the last line
                // (typing at the end, or pasting a block of text), snap
                // the composer viewport to the bottom.  The component's
                // own paste follow-scroll computes against the previous
                // frame's layout, so pasting many lines into a shorter
                // document clamps its scroll target to zero and the view
                // stays stuck at the top.
                let (input_empty, cursor_line, lines_len, x) = {
                    let state = input.read(cx);
                    (
                        state.value().is_empty(),
                        state.cursor_position().line as usize,
                        state.text().lines_len(),
                        state.scroll_offset().x,
                    )
                };
                this.input_empty = input_empty;
                if lines_len > 1 && cursor_line + 1 == lines_len {
                    input.update(cx, |state, cx| {
                        state.set_scroll_offset(point(x, COMPOSER_SCROLL_TO_END), cx);
                    });
                }
            }
        });

        let selection = providers::last_selection(cx);
        let selection_available = providers::selection_is_available(selection.as_ref(), cx);
        let list_state = ListState::new(0, ListAlignment::Top, MESSAGE_LIST_OVERDRAW)
            .with_uniform_item_height(MESSAGE_HEIGHT_HINT);
        list_state.set_follow_mode(FollowMode::Tail);
        let (session_controller, session_store, session_unavailable) =
            match session_services.lifecycle() {
                Ok(store) => {
                    let controller = match &scope {
                        ConversationScope::Chat => ChatSessionController::new(store.clone()),
                        ConversationScope::Project(project) => {
                            ChatSessionController::for_project(store.clone(), project.clone())
                        }
                    };
                    (Some(Arc::new(Mutex::new(controller))), Some(store), None)
                }
                Err(error) => (None, None, Some(error.to_string())),
            };
        Self {
            window_handle: window.window_handle(),
            messages: Vec::new(),
            input,
            composer,
            composer_status,
            scope,
            placeholder,
            composer_height: DEFAULT_COMPOSER_HEIGHT,
            base_composer_height: DEFAULT_COMPOSER_HEIGHT,
            input_empty: true,
            list_state,
            smooth_scroll: SmoothScrollState::default(),
            selection,
            selection_available,
            provider_catalog_revision: providers::catalog_revision(),
            generation_service,
            pending: false,
            reply_task: None,
            #[cfg(test)]
            next_reply_drop_flag: None,
            session_controller,
            session_store,
            session_unavailable,
            persistence_pending: false,
            _persistence_task: None,
            deletion_requested: false,
            deletion_pending: false,
            _deletion_task: None,
            shutdown_requested: false,
            composer_revision: 0,
            pending_turn_id: None,
            terminal_persistence: None,
            pending_terminal: None,
            conversation_id: format!(
                "conversation-{}",
                NEXT_CONVERSATION_ID.fetch_add(1, Ordering::Relaxed)
            ),
            next_turn_id: 1,
            _subscriptions: vec![composer_subscription, subscription],
            #[cfg(test)]
            materialized_message_indices: std::collections::BTreeSet::new(),
        }
    }

    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.focus_input(window, cx));
    }

    pub(crate) fn dismiss_composer_completion(&self, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.dismiss_completion(cx));
    }

    pub(crate) fn has_in_flight_work(&self) -> bool {
        self.pending
            || self.persistence_pending
            || self.deletion_requested
            || self.deletion_pending
            || self.pending_turn_id.is_some()
            || self.terminal_persistence.is_some()
            || self.pending_terminal.is_some()
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
            body: MarkdownBody::new("", ui_id, cx),
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
                .get_or_insert_with(|| ReasoningTrace::new(*ui_id, cx))
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
            *trace = Some(ReasoningTrace::completed(
                snapshot.display.clone(),
                *ui_id,
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

    pub fn cancel_reply(&mut self) {
        if !self.pending {
            return;
        }
        if let Some(reply) = &self.reply_task {
            reply.cancel();
        }
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
        self.pending = true;
        self.reply_task = Some(assistant::ReplyTask::pending_for_test(dropped, cx));
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
        self.provider_catalog_revision = crate::providers::catalog_revision();
        self.next_reply_drop_flag = Some(dropped);
        self.submit("close during generation".to_string(), window, cx)
    }

    #[cfg(test)]
    pub(crate) fn durable_session_id_for_test(&self) -> Option<crate::session::SessionId> {
        self.conversation_id.parse().ok()
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
        let controller = self
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
        self.conversation_id = start.session_id.to_string();
        cx.emit(ChatEvent::SessionBound(start.session_id.clone()));
        start.session_id
    }

    pub fn select_model(&mut self, selection: ModelSelection, cx: &mut Context<Self>) {
        if !self.update_selection(selection.clone()) {
            return;
        }
        providers::select_model(selection.clone(), cx);
        cx.emit(ChatEvent::SelectionChanged(selection));
        cx.notify();
    }

    fn update_selection(&mut self, selection: ModelSelection) -> bool {
        if self.selection.as_ref() == Some(&selection) {
            return false;
        }
        self.selection = Some(selection);
        self.selection_available = true;
        self.provider_catalog_revision = providers::catalog_revision();
        true
    }

    pub fn selection(&self) -> Option<ModelSelection> {
        self.selection.clone()
    }

    /// Whether this view is currently streaming a provider reply.  Used by the
    /// workspace sidebar to annotate the row without deriving other row data
    /// from the view.
    pub(crate) fn is_generating(&self) -> bool {
        self.pending
    }

    fn sync_selection_availability(&mut self, cx: &App) {
        let revision = providers::catalog_revision();
        if self.provider_catalog_revision == revision {
            return;
        }
        self.selection_available = providers::selection_is_available(self.selection.as_ref(), cx);
        self.provider_catalog_revision = revision;
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
