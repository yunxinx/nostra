//! Single conversation view: transcript projection, streaming follow, and composer.
//!
//! Canonical content lives on [`transcript::Transcript`]. [`ChatView`]
//! subscribes to it and folds each update into a [`view::TranscriptView`]:
//! a row projection, a height-cached retained list, and windowed renderer
//! materialization. [`conversation_runtime::ConversationRuntime`] is the only
//! writer.

mod assistant;
pub(crate) mod conversation_runtime;
mod persistence;
pub(crate) mod projection;
pub(crate) mod rows;
pub(crate) mod transcript;
pub(crate) mod view;

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, ListOffset, Pixels, SharedString,
    Subscription, Window, px,
};
use gpui_component::input::{InputEvent, RopeExt as _};
use rust_i18n::t;

use crate::llm::{GenerationService, ModelSelection};
use crate::providers;
use crate::runtime::RuntimeServices;
use crate::session::{ConversationContext, SessionId};
use crate::ui::{
    markdown::{MarkdownExtensionSnapshot, MarkdownPresentation},
    reference_picker::{ChatReferenceComposer, ComposerEvent, ComposerStatus},
};

#[cfg(test)]
use crate::llm::{
    ContentBlock, GatewayError, IndexedMessage, Message as LlmMessage, ProviderMetadata,
};
#[cfg(test)]
use crate::session::ChatTurnTerminal;
#[cfg(test)]
use crate::session::SessionStores;

#[cfg(test)]
use self::conversation_runtime::ConversationStreamEvent;
use self::conversation_runtime::{ConversationRuntime, ConversationRuntimeSnapshot};
use self::projection::{RowId, RowProjection, TypographySnapshot};
#[cfg(test)]
use self::transcript::Role;
use self::transcript::{Transcript, TranscriptEvent, TranscriptSnapshot, TranscriptUpdate};
pub(crate) use self::view::scrolling::SmoothScrollState;
pub(crate) use self::view::scrolling::set_smooth_scrolling;
#[cfg(test)]
pub(crate) use self::view::scrolling::{
    SMOOTH_SCROLL_FINISH_THRESHOLD, SMOOTH_SCROLL_FRAME_FRACTION, reasoning_smooth_invalidations,
    reset_reasoning_smooth_invalidations,
};

pub(crate) use self::persistence::restore::ChatRestoreError;
#[cfg(test)]
pub(crate) use self::transcript::Turn;
#[cfg(test)]
pub(crate) use self::transcript::is_replayable;
pub(crate) use self::transcript::{derive_title, title_from_resolved_state};

pub(in crate::chat) const CONTENT_MAX_WIDTH: Pixels = px(760.);

pub(in crate::chat) const MESSAGE_LIST_OVERDRAW: Pixels = px(1_000.);
pub(in crate::chat) const MESSAGE_HEIGHT_HINT: Pixels = px(160.);

fn next_body_owner_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(in crate::chat) const STICK_THRESHOLD: Pixels = px(48.);

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

/// Everything a workspace needs to (re)build a conversation's view: kept on
/// the host `Conversation` so a cold conversation can be warmed again.
#[derive(Clone)]
pub(crate) struct ChatViewParts {
    pub(crate) runtime: Entity<ConversationRuntime>,
    pub(crate) transcript: Entity<Transcript>,
    pub(crate) composer: Entity<ChatReferenceComposer>,
    pub(crate) composer_status: Rc<Cell<ComposerStatus>>,
    pub(crate) references_enabled: bool,
}

pub(crate) struct SpawnedConversation {
    pub(crate) parts: ChatViewParts,
    pub(crate) view: Entity<ChatView>,
}

/// Build a `ChatView` for an existing conversation host. `restore` carries
/// the projection and scroll anchor saved when the conversation went cold.
pub(crate) fn build_chat_view(
    parts: ChatViewParts,
    preference_handle: crate::preferences::PreferenceHandle,
    markdown_extensions: MarkdownExtensionSnapshot,
    restore: Option<(RowProjection, Option<ListOffset>)>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ChatView> {
    cx.new(|cx| {
        ChatView::new(
            parts,
            preference_handle,
            markdown_extensions,
            restore,
            window,
            cx,
        )
    })
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
    let (composer, composer_status, references_enabled) =
        create_chat_composer(&runtime, window, cx);
    let parts = ChatViewParts {
        runtime: runtime.clone(),
        transcript: transcript.clone(),
        composer,
        composer_status,
        references_enabled,
    };
    let view = build_chat_view(
        parts.clone(),
        services.preference_handle().clone(),
        services.markdown_extensions().clone(),
        None,
        window,
        cx,
    );
    SpawnedConversation { parts, view }
}

pub struct ChatView {
    window_handle: AnyWindowHandle,
    pub(in crate::chat) view: view::TranscriptView,
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
    preference_snapshot: crate::preferences::Preferences,
    catalog_handle: crate::providers::ProviderCatalogHandle,
    catalog_snapshot: crate::providers::ProviderCatalogDocument,
    markdown_presentation: MarkdownPresentation,
    selection: Option<ModelSelection>,
    selection_available: bool,
    composer_revision: u64,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    prepend_pages: Vec<crate::chat::transcript::TranscriptPage>,
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
        let (composer, composer_status, references_enabled) =
            create_chat_composer(&runtime, window, cx);
        cx.new(|cx| {
            Self::new(
                ChatViewParts {
                    runtime,
                    transcript,
                    composer,
                    composer_status,
                    references_enabled,
                },
                preference_handle,
                markdown_extensions,
                None,
                window,
                cx,
            )
        })
    }

    fn new(
        parts: ChatViewParts,
        preference_handle: crate::preferences::PreferenceHandle,
        markdown_extensions: MarkdownExtensionSnapshot,
        restore: Option<(RowProjection, Option<ListOffset>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ChatViewParts {
            runtime,
            transcript,
            composer,
            composer_status,
            references_enabled,
        } = parts;
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
        let runtime_snapshot = runtime.read(cx).snapshot();
        let transcript_snapshot = transcript.read(cx).snapshot();
        let typography = TypographySnapshot {
            line_height: window.line_height(),
            font_size: window.rem_size(),
            typography_revision: 0,
            theme_revision: self::projection::current_theme_revision(),
        };
        let is_restore = restore.is_some();
        let mut view =
            view::TranscriptView::new(&transcript, &transcript_snapshot, typography, restore, cx);
        if is_restore {
            // A cold restore rebuilds rows at the saved anchor, which can
            // slide them under a stationary pointer; freeze hover until the
            // pointer moves (R4).
            view.park_pointer(window);
        }
        // Scroll events drive the jump-to-latest affordance and the
        // materialization window.
        let view_weak = cx.weak_entity();
        view.list_state.set_scroll_handler(move |_, _window, cx| {
            let _ = view_weak.update(cx, |this, cx| this.view.note_scrolled(cx));
        });
        let runtime_subscription = cx.subscribe(&runtime, |this, _, update, cx| {
            this.handle_runtime_update(update, cx);
        });
        let transcript_subscription = cx.subscribe(&transcript, |this, _, update, cx| {
            this.handle_transcript_update(update, cx);
        });
        let mut this = Self {
            window_handle: window.window_handle(),
            view,
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
                    let user_markdown_changed = this.preference_snapshot.user_message_markdown
                        != snapshot.user_message_markdown;
                    this.preference_snapshot = snapshot;
                    if user_markdown_changed {
                        // The markdown/plain choice happens at materialize
                        // time; user rows re-materialize with the new one.
                        this.view.release_user_bubble_rows(cx);
                    }
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
            prepend_pages: Vec::new(),
        };
        this.view
            .set_generating(this.runtime_snapshot.is_generating());
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

    pub fn scroll_to_bottom(&mut self) {
        // Rows slide under a stationary pointer; freeze hover first (R4).
        self.view.follow_stream();
    }

    pub fn follow_stream(&mut self) {
        self.view.follow_stream();
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
        let generating_changed = self.runtime_snapshot.is_generating() != snapshot.is_generating();
        self.runtime_snapshot = snapshot;
        if generating_changed {
            self.view
                .set_generating(self.runtime_snapshot.is_generating());
        }
        true
    }

    fn handle_transcript_update(&mut self, update: &TranscriptUpdate, cx: &mut Context<Self>) {
        let revision = update.snapshot().revision();
        if revision <= self.transcript_snapshot.revision() {
            return;
        }
        let out_of_band = revision > self.transcript_snapshot.revision().saturating_add(1);
        let event = if out_of_band {
            TranscriptEvent::Reset
        } else {
            update.event().clone()
        };
        if matches!(event, TranscriptEvent::PagePrepended { .. }) {
            // Capture the reading anchor while rows still sit at the old
            // offsets (AC3). Parking uses the last observed pointer, so no
            // window is required here.
            self.view.capture_prepend_anchor();
        }
        self.view.handle_transcript_event(
            &event,
            update.snapshot(),
            &self.transcript,
            &self.markdown_presentation,
            self.preference_snapshot.user_message_markdown,
            cx,
        );
        if matches!(event, TranscriptEvent::PagePrepended { .. }) {
            self.view.restore_prepend_anchor();
        }
        self.transcript_snapshot = update.snapshot().clone();
        if self.transcript_snapshot.is_streaming() {
            self.follow_stream();
        }
        if self.view.wants_prepend(&self.transcript_snapshot) {
            self.load_before(cx);
        }
        cx.notify();
    }

    /// Load one earlier page behind the current window (R7). In this phase
    /// the resolved-state source returns everything, so the queue only has
    /// entries under test; production transcripts report `has_earlier` false.
    pub(in crate::chat) fn load_before(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.transcript_snapshot.has_earlier() {
            return false;
        }
        #[cfg(test)]
        {
            let Some(page) = self.prepend_pages.pop() else {
                return false;
            };
            let update = self
                .transcript
                .update(cx, |transcript, cx| transcript.prepend(page, cx));
            self.handle_transcript_update(&update, cx);
            true
        }
        #[cfg(not(test))]
        {
            let _ = cx;
            false
        }
    }

    pub(crate) fn copy_source_for(&self, row_id: RowId, cx: &App) -> Option<SharedString> {
        let ix = self.view.projection.row_index(row_id)?;
        let renderer = &self.view.slots[ix].renderer;
        renderer.copy_source(self.transcript.read(cx))
    }

    /// Cold transition (R8): release every renderer and hand the projection
    /// (heights + disclosure) plus the scroll anchor to the host.
    pub(crate) fn cool_down(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (RowProjection, Option<ListOffset>) {
        self.view.cool_down(cx)
    }

    pub(crate) fn select_model(&mut self, selection: ModelSelection, cx: &mut Context<Self>) {
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

    /// Fold a fresh composer measurement into the two tracked heights, and
    /// report whether either moved (i.e. whether a re-render is needed).
    ///
    /// The live height follows every frame, but the resting height only
    /// records while the input is empty — and an empty input is exactly one
    /// row tall. That keeps the greeting anchored when a draft grows the
    /// composer, without hard-coding what one row measures.
    pub(super) fn record_composer_height(&mut self, height: Pixels) -> bool {
        let mut changed = false;
        if self.composer_height != height {
            self.composer_height = height;
            changed = true;
        }
        if self.input_empty && self.base_composer_height != height {
            self.base_composer_height = height;
            changed = true;
        }
        changed
    }

    /// Test accessors for host-level (shell) tests, which sit outside the
    /// chat module tree and cannot reach the private view fields.
    #[cfg(test)]
    pub(crate) fn composer_text_for_test(&self, cx: &App) -> String {
        self.composer.read(cx).input().read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn projection_len_for_test(&self) -> usize {
        self.view.projection.len()
    }

    #[cfg(test)]
    pub(crate) fn row_id_at_for_test(&self, ix: usize) -> Option<RowId> {
        self.view.projection.row(ix).map(|row| row.id())
    }

    #[cfg(test)]
    pub(crate) fn logical_top_item_for_test(&self) -> usize {
        self.view.list_state.logical_scroll_top().item_ix
    }

    #[cfg(test)]
    pub(crate) fn scroll_rows_to_for_test(&mut self, ix: usize) {
        self.view.list_state.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: gpui::px(0.),
        });
    }

    #[cfg(test)]
    pub(in crate::chat) fn runtime_snapshot_for_test(&self) -> ConversationRuntimeSnapshot {
        self.runtime_snapshot.clone()
    }

    #[cfg(test)]
    pub(crate) fn durable_session_id_for_test(&self) -> Option<crate::session::SessionId> {
        self.runtime_snapshot.session_id().cloned()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test-only drivers for the conversation behind a [`ChatView`].
    //!
    //! [`ChatView`] itself owns no write methods: every helper here writes
    //! through [`Transcript`] or [`ConversationRuntime`] and then drives the
    //! row view with the same update path production events take.

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
        chat.follow_stream();
        apply_transcript_updates(chat, updates, cx);
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

    pub(crate) fn finish_stream_batch(_chat: &mut ChatView) {}

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
            apply_transcript_updates(chat, vec![update], cx);
        }
        chat.follow_stream();
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
            apply_transcript_updates(chat, vec![update], cx);
        }
        chat.follow_stream();
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

    pub(crate) fn queue_prepend_page(
        chat: &mut ChatView,
        page: crate::chat::transcript::TranscriptPage,
    ) {
        chat.prepend_pages.push(page);
    }
}

#[cfg(test)]
mod tests;
