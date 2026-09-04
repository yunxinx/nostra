//! Single conversation view: transcript projection, streaming follow, and composer.
//!
//! Canonical content lives on [`transcript::Transcript`]. [`ChatView`] subscribes
//! to it and keeps a presentation mirror. [`conversation_runtime::ConversationRuntime`]
//! is the only writer.

mod assistant;
pub(crate) mod conversation_runtime;
mod error_card;
mod hover_reveal;
mod persistence;
mod reasoning_card;
mod render;
mod scrolling;
pub(crate) mod transcript;

use std::{
    cell::Cell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, FollowMode, ListState, Pixels,
    SharedString, Subscription, Window, px,
};
use gpui_component::input::{InputEvent, RopeExt as _};
use rust_i18n::t;

#[cfg(test)]
use crate::llm::{ContentBlock, IndexedMessage, Message as LlmMessage, ProviderMetadata};
use crate::llm::{GenerationService, ModelSelection};
use crate::providers;
use crate::runtime::RuntimeServices;
#[cfg(test)]
use crate::session::SessionStores;
use crate::session::{ConversationContext, SessionId};
use crate::ui::{
    markdown::{MarkdownBody, MarkdownExtensionSnapshot, MarkdownPresentation},
    reference_picker::{ChatReferenceComposer, ComposerEvent, ComposerStatus},
};
#[cfg(test)]
use crate::{llm::GatewayError, session::ChatTurnTerminal};

#[cfg(test)]
use self::conversation_runtime::ConversationStreamEvent;
use self::conversation_runtime::{ConversationRuntime, ConversationRuntimeSnapshot};
use self::error_card::TurnError;
use self::reasoning_card::ReasoningTrace;
use self::scrolling::SmoothScrollState;
pub(crate) use self::scrolling::set_smooth_scrolling;
#[cfg(test)]
use self::scrolling::{
    SMOOTH_SCROLL_FINISH_THRESHOLD, SMOOTH_SCROLL_FRAME_FRACTION, reasoning_smooth_invalidations,
    reset_reasoning_smooth_invalidations,
};
use self::transcript::{
    Part, PartChange, PartId, PartSource, Role, Transcript, TranscriptEvent, TranscriptSnapshot,
    TranscriptUpdate, Turn, TurnId, has_copyable_text, stream_ended,
};

pub(crate) use self::persistence::restore::ChatRestoreError;
#[cfg(test)]
pub(crate) use self::transcript::is_replayable;
pub(crate) use self::transcript::{derive_title, title_from_resolved_state};

const CONTENT_MAX_WIDTH: Pixels = px(760.);

const MESSAGE_LIST_OVERDRAW: Pixels = px(1_000.);
const MESSAGE_HEIGHT_HINT: Pixels = px(160.);

fn next_body_owner_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

const STICK_THRESHOLD: Pixels = px(48.);

/// First-frame fallback until the floating composer reports its actual height.
const DEFAULT_COMPOSER_HEIGHT: Pixels = px(120.);

/// Deliberately over-scrolled deferred target for the composer viewport.
const COMPOSER_SCROLL_TO_END: Pixels = px(-1_000_000.);

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
    transcript: Entity<Transcript>,
    cx: &mut App,
) -> Entity<ConversationRuntime> {
    cx.new(|_| ConversationRuntime::new(scope, conversation, generation_service, transcript))
}

pub(crate) fn create_chat_composer(
    runtime: &Entity<ConversationRuntime>,
    window: &mut Window,
    cx: &mut App,
) -> (
    Entity<ChatReferenceComposer>,
    Rc<Cell<ComposerStatus>>,
    bool,
) {
    let references_enabled = runtime.read(cx).supports_references();
    let references = runtime.read(cx).references();
    let composer_status = Rc::new(Cell::new(ComposerStatus::default()));
    let status = composer_status.clone();
    let composer = cx.new(|cx| {
        if references_enabled {
            ChatReferenceComposer::with_references(status, references, window, cx)
        } else {
            ChatReferenceComposer::chat(status, references, window, cx)
        }
    });
    (composer, composer_status, references_enabled)
}

pub(crate) struct SpawnedConversation {
    pub transcript: Entity<Transcript>,
    pub runtime: Entity<ConversationRuntime>,
    pub composer: Entity<ChatReferenceComposer>,
    pub view: Entity<ChatView>,
}

struct ChatViewHost {
    runtime: Entity<ConversationRuntime>,
    transcript: Entity<Transcript>,
    composer: Entity<ChatReferenceComposer>,
    composer_status: Rc<Cell<ComposerStatus>>,
    references_enabled: bool,
}

impl ChatViewHost {
    fn from_runtime(
        runtime: Entity<ConversationRuntime>,
        transcript: Entity<Transcript>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let (composer, composer_status, references_enabled) =
            create_chat_composer(&runtime, window, cx);
        Self {
            runtime,
            transcript,
            composer,
            composer_status,
            references_enabled,
        }
    }
}

pub(crate) fn spawn_conversation(
    scope: crate::runtime::ConversationScopeHandle,
    conversation: ConversationContext,
    generation_service: Arc<dyn GenerationService>,
    services: &RuntimeServices,
    window: &mut Window,
    cx: &mut App,
) -> SpawnedConversation {
    let transcript = cx.new(Transcript::new);
    let runtime = create_conversation_runtime(
        scope,
        conversation,
        generation_service,
        transcript.clone(),
        cx,
    );
    let host = ChatViewHost::from_runtime(runtime.clone(), transcript.clone(), window, cx);
    let composer = host.composer.clone();
    let view = ChatView::view_with_runtime_services(host, services, window, cx);
    SpawnedConversation {
        transcript,
        runtime,
        composer,
        view,
    }
}

/// Presentation-only projection of one transcript turn. Carries everything
/// render needs; rendering must not lock the [`Transcript`] entity.
pub(crate) struct TurnMirror {
    pub(in crate::chat) turn_id: TurnId,
    pub(in crate::chat) role: Role,
    pub(in crate::chat) parts: Vec<PartMirror>,
    pub(in crate::chat) error: Option<TurnError>,
    /// `stream_ended` and non-empty prose, computed on the model side at
    /// mirror update time so render never reads the canonical turn.
    pub(in crate::chat) copyable: bool,
}

pub(crate) enum PartMirror {
    Prose {
        part_id: PartId,
        text: String,
        body: MarkdownBody,
    },
    Reasoning {
        part_id: PartId,
        content_index: usize,
        display: String,
        finished: bool,
        trace: Option<ReasoningTrace>,
    },
    ToolCall {
        part_id: PartId,
        name: String,
    },
    ToolResult {
        part_id: PartId,
        body: MarkdownBody,
    },
}

impl PartMirror {
    fn part_id(&self) -> PartId {
        match self {
            Self::Prose { part_id, .. }
            | Self::Reasoning { part_id, .. }
            | Self::ToolCall { part_id, .. }
            | Self::ToolResult { part_id, .. } => *part_id,
        }
    }
}

pub struct ChatView {
    window_handle: AnyWindowHandle,
    pub(in crate::chat) mirrors: Vec<TurnMirror>,
    composer: Entity<ChatReferenceComposer>,
    composer_status: Rc<Cell<ComposerStatus>>,
    references_enabled: bool,
    pub(in crate::chat) runtime: Entity<ConversationRuntime>,
    runtime_snapshot: ConversationRuntimeSnapshot,
    pub(in crate::chat) transcript: Entity<Transcript>,
    transcript_snapshot: TranscriptSnapshot,
    placeholder: SharedString,
    composer_height: Pixels,
    base_composer_height: Pixels,
    input_empty: bool,
    input_blank: bool,
    pub(in crate::chat) list_state: ListState,
    smooth_scroll: SmoothScrollState,
    preference_snapshot: crate::preferences::Preferences,
    catalog_handle: crate::providers::ProviderCatalogHandle,
    catalog_snapshot: crate::providers::ProviderCatalogDocument,
    markdown_presentation: MarkdownPresentation,
    selection: Option<ModelSelection>,
    selection_available: bool,
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
        let transcript = cx.new(Transcript::new);
        let runtime = create_conversation_runtime(
            crate::runtime::ConversationScopeHandle::for_test(),
            conversation,
            Arc::new(UnavailableGenerationService),
            transcript.clone(),
            cx,
        );
        Self::view_with_generation_service_and_preferences(
            runtime,
            transcript,
            preference_handle,
            window,
            cx,
        )
    }

    #[cfg(test)]
    pub(crate) fn view_with_generation_service_and_preferences(
        runtime: Entity<ConversationRuntime>,
        transcript: Entity<Transcript>,
        preference_handle: crate::preferences::PreferenceHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let markdown_extensions = crate::ui::markdown::test_extension_snapshot();
        let host = ChatViewHost::from_runtime(runtime, transcript, window, cx);
        cx.new(|cx| Self::new(host, preference_handle, markdown_extensions, window, cx))
    }

    fn view_with_runtime_services(
        host: ChatViewHost,
        services: &crate::runtime::RuntimeServices,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let preference_handle = services.preference_handle().clone();
        let markdown_extensions = services.markdown_extensions().clone();
        cx.new(|cx| Self::new(host, preference_handle, markdown_extensions, window, cx))
    }

    fn new(
        host: ChatViewHost,
        preference_handle: crate::preferences::PreferenceHandle,
        markdown_extensions: MarkdownExtensionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ChatViewHost {
            runtime,
            transcript,
            composer,
            composer_status,
            references_enabled,
        } = host;
        let preference_snapshot = preference_handle.snapshot();
        let catalog_handle = crate::providers::ensure_global(cx);
        let catalog_snapshot = catalog_handle.snapshot();
        let preference_state = preference_handle.shared_preferences();
        let markdown_presentation =
            MarkdownPresentation::new(preference_state, markdown_extensions);
        let preferences_for_observer = preference_handle.clone();
        let catalog_for_observer = catalog_handle.clone();
        let placeholder: SharedString = if references_enabled {
            t!("reference_picker.composer_placeholder").to_string()
        } else {
            t!("chat.placeholder").to_string()
        }
        .into();
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
                        state.set_scroll_offset(gpui::point(x, COMPOSER_SCROLL_TO_END), cx);
                    });
                }
            }
        });

        let selection = providers::last_selection_from(&catalog_snapshot);
        let selection_available =
            providers::selection_is_available_from(selection.as_ref(), &catalog_snapshot);
        let list_state = ListState::new(0, gpui::ListAlignment::Top, MESSAGE_LIST_OVERDRAW)
            .with_uniform_item_height(MESSAGE_HEIGHT_HINT);
        list_state.set_follow_mode(FollowMode::Tail);
        let runtime_snapshot = runtime.read(cx).snapshot();
        let transcript_snapshot = transcript.read(cx).snapshot();
        let runtime_subscription = cx.subscribe(&runtime, |this, _, update, cx| {
            this.handle_runtime_update(update, cx);
        });
        let transcript_subscription = cx.subscribe(&transcript, |this, _, update, cx| {
            this.handle_transcript_update(update, cx);
        });
        let mut this = Self {
            window_handle: window.window_handle(),
            mirrors: Vec::new(),
            composer,
            composer_status,
            references_enabled,
            runtime,
            runtime_snapshot,
            transcript,
            transcript_snapshot,
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
            composer_revision: 0,
            _subscriptions: vec![
                composer_subscription,
                subscription,
                runtime_subscription,
                transcript_subscription,
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
        };
        this.sync_from_transcript(cx);
        this
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

    pub fn scroll_to_bottom(&self) {
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
    }

    pub fn follow_stream(&self) {
        if self.list_state.is_following_tail() {
            self.list_state.scroll_to_end();
        }
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

    fn handle_transcript_update(&mut self, update: &TranscriptUpdate, cx: &mut Context<Self>) {
        let revision = update.snapshot().revision();
        if revision <= self.transcript_snapshot.revision() {
            return;
        }
        if revision > self.transcript_snapshot.revision().saturating_add(1) {
            self.sync_from_transcript(cx);
            self.transcript_snapshot = update.snapshot().clone();
            cx.notify();
            return;
        }
        match update.event() {
            TranscriptEvent::Reset => {
                self.sync_from_transcript(cx);
            }
            TranscriptEvent::TailAppended { turn_ids } => {
                self.append_mirrors(turn_ids, cx);
            }
            TranscriptEvent::PartInserted { turn_id, part_id } => {
                self.insert_part_mirror(*turn_id, *part_id, cx);
                self.refresh_turn_flags(*turn_id, cx);
            }
            TranscriptEvent::PartChanged {
                turn_id,
                part_id,
                change,
                delta,
            } => {
                self.apply_part_change(*turn_id, *part_id, *change, delta, cx);
                self.refresh_turn_flags(*turn_id, cx);
            }
            TranscriptEvent::TurnReplaced { turn_id } => {
                self.replace_turn_mirror(*turn_id, cx);
                self.refresh_turn_flags(*turn_id, cx);
            }
        }
        self.transcript_snapshot = update.snapshot().clone();
        debug_assert_eq!(self.transcript_snapshot.turn_count(), self.mirrors.len());
        debug_assert_eq!(
            self.transcript_snapshot.is_streaming(),
            self.transcript_snapshot.streaming().is_some()
        );
        if self.transcript_snapshot.streaming().is_some() {
            self.follow_stream();
        }
        self.sync_message_list_count();
        cx.notify();
    }

    fn sync_from_transcript(&mut self, cx: &mut App) {
        let turn_ids: Vec<TurnId> = self
            .transcript
            .read(cx)
            .turns()
            .iter()
            .map(|turn| turn.turn_id)
            .collect();
        self.mirrors = turn_ids
            .iter()
            .filter_map(|turn_id| {
                let turn = self.transcript.read(cx).turn(*turn_id)?.clone();
                Some(self.mirror_from_turn(&turn, cx))
            })
            .collect();
        self.sync_message_list_count();
    }

    fn append_mirrors(&mut self, turn_ids: &[TurnId], cx: &mut App) {
        for turn_id in turn_ids {
            if self.mirrors.iter().any(|mirror| mirror.turn_id == *turn_id) {
                continue;
            }
            let Some(turn) = self.transcript.read(cx).turn(*turn_id).cloned() else {
                continue;
            };
            self.mirrors.push(self.mirror_from_turn(&turn, cx));
        }
    }

    /// Recompute the presentation flags derived from the canonical turn
    /// (`copyable`) after the transcript changed under an existing mirror.
    fn refresh_turn_flags(&mut self, turn_id: TurnId, cx: &App) {
        let Some(turn) = self.transcript.read(cx).turn(turn_id) else {
            return;
        };
        let copyable = stream_ended(turn) && has_copyable_text(turn);
        let Some(mirror) = self
            .mirrors
            .iter_mut()
            .find(|mirror| mirror.turn_id == turn_id)
        else {
            return;
        };
        mirror.copyable = copyable;
    }

    fn insert_part_mirror(&mut self, turn_id: TurnId, part_id: PartId, cx: &mut App) {
        let Some((part, order)) = ({
            let transcript = self.transcript.read(cx);
            transcript.turn(turn_id).map(|turn| {
                (
                    turn.parts
                        .iter()
                        .find(|part| part.part_id == part_id)
                        .cloned(),
                    turn.parts
                        .iter()
                        .map(|part| part.part_id)
                        .collect::<Vec<_>>(),
                )
            })
        }) else {
            return;
        };
        let Some(part) = part else {
            return;
        };
        let Some(mirror) = self
            .mirrors
            .iter_mut()
            .find(|mirror| mirror.turn_id == turn_id)
        else {
            return;
        };
        if mirror
            .parts
            .iter()
            .any(|existing| existing.part_id() == part_id)
        {
            return;
        }
        mirror
            .parts
            .push(inserted_part_mirror(&part, &self.markdown_presentation, cx));
        mirror.parts.sort_by_key(|existing| {
            order
                .iter()
                .position(|id| *id == existing.part_id())
                .unwrap_or(usize::MAX)
        });
    }

    fn apply_part_change(
        &mut self,
        turn_id: TurnId,
        part_id: PartId,
        change: PartChange,
        delta: &SharedString,
        cx: &mut App,
    ) {
        let replace_prose = (change == PartChange::Replace).then(|| {
            self.transcript.read(cx).turn(turn_id).and_then(|turn| {
                turn.parts
                    .iter()
                    .find(|part| part.part_id == part_id)
                    .and_then(|part| part.source.prose_text().map(str::to_string))
            })
        });
        let replace_reasoning = (change == PartChange::Replace).then(|| {
            self.transcript.read(cx).turn(turn_id).and_then(|turn| {
                turn.parts.iter().find_map(|part| match &part.source {
                    PartSource::Reasoning { reasoning, .. } if part.part_id == part_id => {
                        Some(reasoning.display.clone())
                    }
                    _ => None,
                })
            })
        });
        let Some(mirror) = self
            .mirrors
            .iter_mut()
            .find(|mirror| mirror.turn_id == turn_id)
        else {
            return;
        };
        let Some(part) = mirror
            .parts
            .iter_mut()
            .find(|part| part.part_id() == part_id)
        else {
            return;
        };
        match (part, change) {
            (PartMirror::Prose { text, body, .. }, PartChange::Append) => {
                text.push_str(delta);
                body.push_str(delta, cx);
            }
            (PartMirror::Prose { text, body, .. }, PartChange::Replace) => {
                if let Some(source) = replace_prose.flatten() {
                    *text = source.clone();
                    body.set_text(&source, cx);
                }
            }
            (PartMirror::Prose { body, .. }, PartChange::Finished) => body.finish(cx),
            (PartMirror::Reasoning { display, trace, .. }, PartChange::Append) => {
                display.push_str(delta);
                trace
                    .get_or_insert_with(|| {
                        ReasoningTrace::new_with_presentation(
                            next_body_owner_id(),
                            &self.markdown_presentation,
                            cx,
                        )
                    })
                    .push(delta, cx);
            }
            (PartMirror::Reasoning { display, trace, .. }, PartChange::Replace) => {
                let source = replace_reasoning.flatten().unwrap_or_default();
                *display = source.clone();
                if source.is_empty() {
                    *trace = None;
                } else if let Some(trace) = trace.as_mut() {
                    trace.set_source(&source, cx);
                } else {
                    *trace = Some(ReasoningTrace::completed_with_presentation(
                        source,
                        next_body_owner_id(),
                        &self.markdown_presentation,
                        cx,
                    ));
                }
            }
            (
                PartMirror::Reasoning {
                    finished,
                    trace: Some(trace),
                    ..
                },
                PartChange::Finished,
            ) => {
                *finished = true;
                trace.finish(cx);
            }
            _ => {}
        }
    }

    fn replace_turn_mirror(&mut self, turn_id: TurnId, cx: &mut App) {
        let Some(turn) = self.transcript.read(cx).turn(turn_id).cloned() else {
            return;
        };
        let Some(mirror) = self
            .mirrors
            .iter_mut()
            .find(|mirror| mirror.turn_id == turn_id)
        else {
            self.mirrors.push(self.mirror_from_turn(&turn, cx));
            return;
        };
        let mut previous = std::mem::take(&mut mirror.parts)
            .into_iter()
            .map(|part| (part.part_id(), part))
            .collect::<BTreeMap<_, _>>();
        mirror.parts = turn
            .parts
            .iter()
            .map(|part| match previous.remove(&part.part_id) {
                Some(existing) => {
                    reuse_part_mirror(existing, part, &self.markdown_presentation, cx)
                }
                None => part_mirror(part, &self.markdown_presentation, cx),
            })
            .collect();
        mirror.role = turn.role;
        mirror.error = turn.error.clone().map(|error| TurnError::new(error, cx));
    }

    fn mirror_from_turn(&self, turn: &Turn, cx: &mut App) -> TurnMirror {
        TurnMirror {
            turn_id: turn.turn_id,
            role: turn.role,
            parts: turn
                .parts
                .iter()
                .map(|part| part_mirror(part, &self.markdown_presentation, cx))
                .collect(),
            error: turn.error.clone().map(|error| TurnError::new(error, cx)),
            copyable: stream_ended(turn) && has_copyable_text(turn),
        }
    }

    fn copyable_message_text(&self, turn_id: TurnId, cx: &App) -> Option<SharedString> {
        self.transcript.read(cx).copyable_text(turn_id)
    }

    fn reasoning_copy_source(
        &self,
        turn_id: TurnId,
        part_id: PartId,
        cx: &App,
    ) -> Option<SharedString> {
        self.transcript.read(cx).turn(turn_id).and_then(|turn| {
            turn.parts.iter().find_map(|part| match &part.source {
                PartSource::Reasoning { reasoning, .. } if part.part_id == part_id => {
                    Some(reasoning.display.clone().into())
                }
                _ => None,
            })
        })
    }

    #[cfg(test)]
    pub(in crate::chat) fn runtime_snapshot_for_test(&self) -> ConversationRuntimeSnapshot {
        self.runtime_snapshot.clone()
    }

    #[cfg(test)]
    pub(crate) fn durable_session_id_for_test(&self) -> Option<crate::session::SessionId> {
        self.runtime_snapshot.session_id().cloned()
    }

    pub fn select_model(&mut self, selection: ModelSelection, cx: &mut Context<Self>) {
        if self.selection.as_ref() == Some(&selection) {
            return;
        }
        providers::select_model(selection.clone(), &self.catalog_handle, cx);
        self.set_selection(selection, cx);
    }

    pub(crate) fn set_selection(&mut self, selection: ModelSelection, cx: &mut Context<Self>) {
        if !self.update_selection(selection) {
            return;
        }
        cx.notify();
    }

    pub(crate) fn restore_session(
        &mut self,
        session_id: &SessionId,
        state: &crate::session::ResolvedSessionState,
        cx: &mut Context<Self>,
    ) -> Result<Option<ModelSelection>, ChatRestoreError> {
        let (result, snapshot) = self.runtime.update(cx, |runtime, cx| {
            let result = runtime.restore_session(session_id, state, cx);
            (result, runtime.snapshot())
        });
        self.apply_runtime_snapshot(snapshot);
        // Turns arrive through the deferred `TranscriptEvent::Reset`. Advancing
        // `transcript_snapshot` here would make the view skip that event.
        if let Ok(Some(selection)) = &result {
            self.update_selection(selection.clone());
        }
        cx.notify();
        result
    }

    fn update_selection(&mut self, selection: ModelSelection) -> bool {
        if self.selection.as_ref() == Some(&selection) {
            return false;
        }
        self.selection = Some(selection);
        self.selection_available = true;
        true
    }

    fn sync_selection_availability(&mut self) {
        self.selection_available =
            providers::selection_is_available_from(self.selection.as_ref(), &self.catalog_snapshot);
    }
}

fn inserted_part_mirror(
    part: &Part,
    presentation: &MarkdownPresentation,
    cx: &mut App,
) -> PartMirror {
    if part.finished {
        return part_mirror(part, presentation, cx);
    }
    match &part.source {
        // Unfinished inserts start empty. Stream batches publish Insert then
        // Append after the model already contains the delta; seeding the body
        // from `part` here would double the text on Append.
        PartSource::Prose { .. } => PartMirror::Prose {
            part_id: part.part_id,
            text: String::new(),
            body: MarkdownBody::new_streaming_with_presentation(
                "",
                next_body_owner_id(),
                presentation,
                cx,
            ),
        },
        PartSource::Reasoning { .. } => PartMirror::Reasoning {
            part_id: part.part_id,
            content_index: part.content_index,
            display: String::new(),
            finished: false,
            trace: None,
        },
        PartSource::ToolCall { .. } | PartSource::ToolResult(_) => {
            part_mirror(part, presentation, cx)
        }
    }
}

fn part_mirror(part: &Part, presentation: &MarkdownPresentation, cx: &mut App) -> PartMirror {
    let owner_id = next_body_owner_id();
    match &part.source {
        PartSource::Prose { text, .. } => {
            let body = if part.finished {
                MarkdownBody::new_with_presentation(text, owner_id, presentation, cx)
            } else {
                MarkdownBody::new_streaming_with_presentation(text, owner_id, presentation, cx)
            };
            PartMirror::Prose {
                part_id: part.part_id,
                text: text.clone(),
                body,
            }
        }
        PartSource::Reasoning { reasoning, .. } => {
            let trace = if reasoning.display.is_empty() {
                None
            } else if part.finished {
                Some(ReasoningTrace::completed_with_presentation(
                    reasoning.display.clone(),
                    owner_id,
                    presentation,
                    cx,
                ))
            } else {
                let mut trace = ReasoningTrace::new_with_presentation(owner_id, presentation, cx);
                trace.push(&reasoning.display, cx);
                Some(trace)
            };
            PartMirror::Reasoning {
                part_id: part.part_id,
                content_index: part.content_index,
                display: reasoning.display.clone(),
                finished: part.finished,
                trace,
            }
        }
        PartSource::ToolCall { name, .. } => PartMirror::ToolCall {
            part_id: part.part_id,
            name: name.clone(),
        },
        PartSource::ToolResult(tool_result) => PartMirror::ToolResult {
            part_id: part.part_id,
            body: MarkdownBody::new_with_presentation(
                &tool_result.content,
                owner_id,
                presentation,
                cx,
            ),
        },
    }
}

fn reuse_part_mirror(
    existing: PartMirror,
    part: &Part,
    presentation: &MarkdownPresentation,
    cx: &mut App,
) -> PartMirror {
    match (existing, &part.source) {
        (
            PartMirror::Prose {
                part_id,
                text: _,
                mut body,
            },
            PartSource::Prose { text: source, .. },
        ) => {
            let text = source.clone();
            body.set_text(source, cx);
            if part.finished {
                body.finish(cx);
            }
            PartMirror::Prose {
                part_id,
                text,
                body,
            }
        }
        (
            PartMirror::Reasoning {
                part_id,
                display: _,
                trace: Some(mut trace),
                ..
            },
            PartSource::Reasoning { reasoning, .. },
        ) if !reasoning.display.is_empty() => {
            let display = reasoning.display.clone();
            if trace.source_len() != reasoning.display.len() {
                trace.set_source(&reasoning.display, cx);
            }
            trace.finish(cx);
            PartMirror::Reasoning {
                part_id,
                content_index: part.content_index,
                display,
                finished: true,
                trace: Some(trace),
            }
        }
        (PartMirror::ToolCall { part_id, .. }, PartSource::ToolCall { name, .. }) => {
            PartMirror::ToolCall {
                part_id,
                name: name.clone(),
            }
        }
        (PartMirror::ToolResult { part_id, mut body }, PartSource::ToolResult(tool_result)) => {
            body.set_text(&tool_result.content, cx);
            PartMirror::ToolResult { part_id, body }
        }
        (_, _) => part_mirror(part, presentation, cx),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test-only drivers for the conversation behind a [`ChatView`].
    //!
    //! [`ChatView`] itself owns no write methods: every helper here writes
    //! through [`Transcript`] or [`ConversationRuntime`] and then drives the
    //! presentation mirror with the same update path production events take.

    use self::assistant::ReplyTask;
    use self::conversation_runtime::ConversationRequestGeneration;
    use super::*;
    use crate::session::ChatTurnStart;

    fn apply_transcript_updates(
        chat: &mut ChatView,
        updates: Vec<TranscriptUpdate>,
        cx: &mut Context<ChatView>,
    ) {
        for update in updates {
            chat.handle_transcript_update(&update, cx);
        }
    }

    pub(crate) fn apply_stream(
        chat: &mut ChatView,
        events: &[ConversationStreamEvent],
        cx: &mut Context<ChatView>,
    ) {
        let updates = chat.transcript.update(cx, |transcript, cx| {
            transcript.apply_stream_batch(events, cx)
        });
        apply_transcript_updates(chat, updates, cx);
        chat.remeasure_latest_message();
        chat.follow_stream();
    }

    pub(crate) fn push_canonical(
        chat: &mut ChatView,
        message: LlmMessage,
        cx: &mut Context<ChatView>,
    ) {
        let update = chat.transcript.update(cx, |transcript, cx| {
            transcript.push_canonical_turn(message, cx)
        });
        apply_transcript_updates(chat, vec![update], cx);
    }

    pub(crate) fn push_empty(chat: &mut ChatView, role: Role, cx: &mut Context<ChatView>) {
        let update = chat
            .transcript
            .update(cx, |transcript, cx| transcript.push_empty_turn(role, cx));
        apply_transcript_updates(chat, vec![update], cx);
    }

    pub(crate) fn append_text(
        chat: &mut ChatView,
        content_index: usize,
        id: String,
        delta: &str,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::TextDelta {
                content_index,
                id,
                delta: delta.to_string(),
            }],
            cx,
        );
    }

    pub(crate) fn start_text(
        chat: &mut ChatView,
        content_index: usize,
        id: String,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::TextStarted { content_index, id }],
            cx,
        );
    }

    pub(crate) fn finish_text(
        chat: &mut ChatView,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::TextFinished {
                content_index,
                id: id.to_string(),
                replay,
            }],
            cx,
        );
    }

    pub(crate) fn append_reasoning(
        chat: &mut ChatView,
        content_index: usize,
        id: String,
        delta: &str,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::ReasoningDelta {
                content_index,
                id,
                delta: delta.to_string(),
            }],
            cx,
        );
    }

    pub(crate) fn finish_reasoning(
        chat: &mut ChatView,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::ReasoningFinished {
                content_index,
                id: id.to_string(),
                replay,
            }],
            cx,
        );
    }

    pub(crate) fn start_tool_call(
        chat: &mut ChatView,
        content_index: usize,
        index: usize,
        id: String,
        name: String,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::ToolCallStarted {
                content_index,
                index,
                id,
                name,
            }],
            cx,
        );
    }

    pub(crate) fn start_reasoning(
        chat: &mut ChatView,
        content_index: usize,
        id: String,
        cx: &mut Context<ChatView>,
    ) {
        apply_stream(
            chat,
            &[ConversationStreamEvent::ReasoningStarted { content_index, id }],
            cx,
        );
    }

    pub(crate) fn finish_stream_batch(chat: &mut ChatView) {
        chat.remeasure_latest_message();
        chat.follow_stream();
    }

    pub(crate) fn finish_reply(
        chat: &mut ChatView,
        message: Option<IndexedMessage>,
        error: Option<GatewayError>,
        cx: &mut Context<ChatView>,
    ) {
        let update = chat.transcript.update(cx, |transcript, cx| {
            transcript.finish_turn(message, error, cx)
        });
        if let Some(update) = update {
            chat.handle_transcript_update(&update, cx);
        }
        chat.follow_stream();
        chat.remeasure_latest_message();
        let snapshot = chat.runtime.update(cx, |runtime, cx| {
            runtime.generating = false;
            runtime.pending_turn_id = None;
            runtime.terminal_persistence = None;
            runtime.publish_state(cx);
            runtime.snapshot()
        });
        chat.apply_runtime_snapshot(snapshot);
    }

    pub(crate) fn finish_reply_with_terminal(
        chat: &mut ChatView,
        generation: ConversationRequestGeneration,
        message: Option<IndexedMessage>,
        terminal: ChatTurnTerminal,
        error: Option<GatewayError>,
        cx: &mut Context<ChatView>,
    ) {
        if generation != chat.runtime_snapshot.request_generation() {
            return;
        }
        let update = chat.transcript.update(cx, |transcript, cx| {
            transcript.finish_turn(message, error, cx)
        });
        if let Some(update) = update {
            chat.handle_transcript_update(&update, cx);
        }
        chat.follow_stream();
        chat.remeasure_latest_message();
        let snapshot = chat.runtime.update(cx, |runtime, cx| {
            runtime.finish_terminal(generation, terminal, cx);
            runtime.snapshot()
        });
        chat.apply_runtime_snapshot(snapshot);
    }

    pub(crate) fn finish_current_reply_with_terminal(
        chat: &mut ChatView,
        message: Option<IndexedMessage>,
        terminal: ChatTurnTerminal,
        error: Option<GatewayError>,
        cx: &mut Context<ChatView>,
    ) {
        let generation = chat.runtime_snapshot.request_generation();
        finish_reply_with_terminal(chat, generation, message, terminal, error, cx);
    }

    pub(crate) fn start_pending_reply(
        chat: &mut ChatView,
        dropped: std::rc::Rc<std::cell::Cell<bool>>,
        cx: &mut Context<ChatView>,
    ) {
        let snapshot = chat.runtime.update(cx, |runtime, cx| {
            runtime.request_generation = runtime
                .current_generation()
                .next()
                .expect("test request generation");
            runtime.generating = true;
            runtime.publish_state(cx);
            runtime.snapshot()
        });
        chat.apply_runtime_snapshot(snapshot);
        chat.runtime.update(cx, |runtime, cx| {
            runtime.reply_task = Some(ReplyTask::pending_for_test(dropped, cx));
        });
    }

    pub(crate) fn seed_pending_turn(
        chat: &mut ChatView,
        user_message: LlmMessage,
        selection: ModelSelection,
        turn_id: impl Into<String>,
        cx: &mut Context<ChatView>,
    ) -> ChatTurnStart {
        let turn_id = turn_id.into();
        let start = chat
            .runtime
            .read(cx)
            .session_controller_for_test()
            .lock()
            .expect("test controller lock")
            .begin_turn(user_message.clone(), selection, turn_id.clone())
            .expect("persist test turn begin");
        mark_turn_pending(chat, start.session_id.clone(), turn_id, cx);
        let updates = chat.transcript.update(cx, |transcript, cx| {
            let (_, update) = transcript.begin_turn(user_message, cx);
            vec![update]
        });
        apply_transcript_updates(chat, updates, cx);
        start
    }

    pub(crate) fn mark_turn_pending(
        chat: &mut ChatView,
        session_id: SessionId,
        turn_id: impl Into<String>,
        cx: &mut Context<ChatView>,
    ) {
        let snapshot = chat.runtime.update(cx, |runtime, cx| {
            runtime.mark_turn_pending_for_test(session_id, turn_id, cx);
            runtime.snapshot()
        });
        chat.apply_runtime_snapshot(snapshot);
    }

    pub(crate) fn mark_generating(chat: &mut ChatView, cx: &mut Context<ChatView>) {
        let snapshot = chat.runtime.update(cx, |runtime, cx| {
            runtime.mark_generating_for_test(cx);
            runtime.snapshot()
        });
        chat.apply_runtime_snapshot(snapshot);
    }

    pub(crate) fn start_durable_pending_reply(
        chat: &mut ChatView,
        dropped: std::rc::Rc<std::cell::Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) -> bool {
        chat.selection = Some(ModelSelection {
            profile_id: "fixture-profile".into(),
            model_id: "fixture-model".into(),
        });
        chat.selection_available = true;
        chat.runtime.update(cx, |runtime, _| {
            runtime.next_reply_drop_flag = Some(dropped);
        });
        chat.submit("close during generation".to_string(), window, cx)
    }

    pub(crate) fn persist_session(chat: &mut ChatView, cx: &mut Context<ChatView>) -> SessionId {
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
        let (start, snapshot) = chat.runtime.update(cx, |runtime, cx| {
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
        chat.apply_runtime_snapshot(snapshot);
        start.session_id
    }
}

#[cfg(test)]
mod tests;
