//! Root `ChatApp` view: hosts conversations, top bar(s), and the fixed sidebar.

mod history_sidebar;
mod render;

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::future;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, DragMoveEvent, ElementId, EmptyView, Entity,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, WindowControlArea, div, px,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, InteractiveElementExt as _, Root, Sizable as _, StyledExt as _,
    TITLE_BAR_HEIGHT, WindowExt as _,
    animation::{Transition, ease_in_out_cubic},
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    menu::DropdownMenu as _,
    sidebar::SidebarToggleButton,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::{glass, theme};
use crate::chat::{ChatDeleteRequest, ChatEvent, ChatView, derive_chat_title};
use crate::llm::ModelSelection;
use crate::preferences::{self, Preferences, WindowGeometry};
use crate::session::{
    ChatSessionCatalogController, ResolvedSessionState, SessionId, SessionStores,
};
use crate::shell::actions::{OpenSettings, ToggleTheme};
use crate::ui::{
    inline_delete_confirmation::InlineDeleteConfirmationHandle, model_select::ModelPicker,
};

/// Minimum sidebar width when the user drags the right edge inward.
const SIDEBAR_MIN_WIDTH: Pixels = px(220.);
/// Maximum sidebar width when the user drags the right edge outward.
const SIDEBAR_MAX_WIDTH: Pixels = px(440.);
/// Width of the invisible drag hit-area on the sidebar's right edge.
const RESIZE_HANDLE_WIDTH: Pixels = px(6.);

/// Duration of the sidebar collapse/expand animation.
const SIDEBAR_ANIM: Duration = Duration::from_millis(220);

/// GPUI waits 200 ms for quit observers. Keep the fallback storage budget
/// below that ceiling so preference persistence is not abandoned behind a
/// longer session timeout. Normal menu/window exits use the five-second
/// pre-quit path before asking GPUI to terminate.
const APP_QUIT_STORAGE_BUDGET: Duration = Duration::from_millis(150);

/// x-coordinate where the main column's floating content (model pill) should
/// start when the sidebar is fully collapsed.  Equals the width of the fixed
/// top-left overlay (traffic-light padding + toggle + new-chat button + gap).
const OVERLAY_INSET: Pixels = px(148.);

/// Left-pad so content sits to the right of the macOS traffic lights (x=9..77).
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_PAD: Pixels = px(80.);
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_PAD: Pixels = px(12.);

pub struct ChatApp {
    focus_handle: FocusHandle,
    conversations: Vec<Conversation>,
    /// Quick lookup from a persisted session id to the entity id of its opened
    /// `ChatView`.  A draft view never appears here until its first durable
    /// turn begin binds a session id.
    opened_session_index: HashMap<SessionId, gpui::EntityId>,
    /// Entity id of the currently displayed conversation, or `None` when the
    /// workspace is showing the empty detail state.
    active: Option<gpui::EntityId>,
    /// Monotonic counter guarding against stale background session selections
    /// overwriting a newer active target.
    #[allow(dead_code)]
    selection_generation: u64,
    /// Background task owning an in-flight catalog/select + hydrate cycle.
    /// Dropping it cancels the work.
    _selection_task: Option<Task<()>>,
    collapsed: bool,
    /// True once the user has toggled the sidebar at least once.  Prevents an
    /// unwanted slide-in on the very first render.
    has_toggled: bool,
    /// Current sidebar width in the expanded state.  Users can drag the
    /// sidebar's right edge to change this within `[SIDEBAR_MIN_WIDTH,
    /// SIDEBAR_MAX_WIDTH]`.  Window resizes don't touch it.
    sidebar_width: Pixels,
    /// (mouse_x, sidebar_width) captured on `mouse_down` on the resize handle.
    /// While `Some`, window-level mouse move events adjust `sidebar_width`.
    resize_start: Option<(Pixels, Pixels)>,
    /// Set on pointer-down in the empty titlebar layer. The first move hands
    /// the gesture to the platform and clears this flag.
    titlebar_move_pending: bool,
    /// Latest main-window restore bounds, kept fresh by a window-bounds
    /// observer and persisted on quit.
    window_geometry: Option<WindowGeometry>,
    model_picker: Entity<ModelPicker>,
    /// Sidebar row the pointer is over, so its actions button can appear.
    /// Matches a draft view entity or a catalog session id.
    hovered: Option<SidebarTarget>,
    /// Sidebar row awaiting inline delete confirmation.  While set, its row
    /// shows a Popover confirm card anchored to the actions button.
    confirming: Option<SidebarTarget>,
    delete_confirmation: InlineDeleteConfirmationHandle,
    /// Catalog snapshot, pagination cursor, and load state for the history
    /// sidebar.  Render only reads this; every mutation comes from a background
    /// load completion.
    history: ChatHistorySidebar,
    /// Background task owning the initial catalog page load.  Dropping it
    /// cancels the work.
    _catalog_initial_task: Option<Task<()>>,
    /// Background task owning a load-more page.  Dropping it cancels the work.
    _catalog_load_more_task: Option<Task<()>>,
    /// Background task refreshing a single session summary after a durable
    /// begin binds a new session.
    _summary_refresh_task: Option<Task<()>>,
    /// Background task permanently deleting an unopened catalog session.
    _history_delete_task: Option<Task<()>>,
    /// True once startup restore has been attempted after the first catalog
    /// frame.  Prevents repeated restore attempts on later reloads.
    startup_restore_attempted: bool,
    shutdown_completed: Arc<AtomicBool>,
    _quit_task: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
    preference_saver: PreferenceSaver,
}

type PreferenceSaver = Arc<dyn Fn(Preferences) -> anyhow::Result<()> + Send + Sync>;

/// Identity of a sidebar row.  Drafts (unbound views) are addressed by their
/// view entity; persisted catalog rows are addressed by their session id so the
/// row stays stable whether or not the session is currently opened.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum SidebarTarget {
    View(gpui::EntityId),
    Session(SessionId),
}

use history_sidebar::ChatHistorySidebar;

struct ExitWork {
    stores: Option<SessionStores>,
    snapshot: Preferences,
    preference_saver: PreferenceSaver,
}

struct Conversation {
    view: Entity<ChatView>,
    title: SharedString,
    selection: Option<ModelSelection>,
    /// `None` while this view is an unbound draft; `Some` once a durable turn
    /// begin or a successful restore has bound it to a persisted session.
    session_id: Option<SessionId>,
    _subscription: Subscription,
}

impl ChatApp {
    /// Build the root app view from persisted preferences.  Sidebar width is
    /// clamped into the allowed range, and a save-on-quit hook is registered
    /// so the current UI state survives across restarts.
    pub fn new(prefs: Preferences, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with_preference_saver(
            prefs,
            window,
            cx,
            Arc::new(|prefs| preferences::save(&prefs)),
        )
    }

    pub(super) fn new_with_preference_saver(
        prefs: Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
        preference_saver: PreferenceSaver,
    ) -> Self {
        Self::build(prefs, window, cx, preference_saver)
    }

    fn build(
        prefs: Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
        preference_saver: PreferenceSaver,
    ) -> Self {
        let sidebar_width = px(prefs.sidebar_width)
            .max(SIDEBAR_MIN_WIDTH)
            .min(SIDEBAR_MAX_WIDTH);

        let parent = cx.weak_entity();
        let model_picker = cx.new(|cx| {
            ModelPicker::new(
                prefs.last_model_selection.clone(),
                move |selection, cx| {
                    parent
                        .update(cx, |app, cx| app.select_model_from_picker(selection, cx))
                        .unwrap_or(false)
                },
                window,
                cx,
            )
        });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            conversations: Vec::new(),
            opened_session_index: HashMap::new(),
            active: None,
            selection_generation: 0,
            _selection_task: None,
            collapsed: prefs.sidebar_collapsed,
            has_toggled: false,
            sidebar_width,
            resize_start: None,
            titlebar_move_pending: false,
            window_geometry: Some(WindowGeometry::from_window(window)),
            model_picker,
            hovered: None,
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            history: ChatHistorySidebar::new(),
            _catalog_initial_task: None,
            _catalog_load_more_task: None,
            _summary_refresh_task: None,
            _history_delete_task: None,
            startup_restore_attempted: false,
            shutdown_completed: Arc::new(AtomicBool::new(false)),
            _quit_task: None,
            _subscriptions: Vec::new(),
            preference_saver,
        };
        this.track_window_geometry(window, cx);
        this.track_system_appearance(window, cx);
        this.register_save_on_quit(cx);
        this.register_window_close(window, cx);
        this.start_catalog_initial_load(window, cx);
        this
    }

    /// Keep `window_geometry` current across moves and resizes.  The
    /// observer fires for both, because the platform window reports either
    /// as a bounds change.
    fn track_window_geometry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.observe_window_bounds(window, |this, window, _| {
            this.window_geometry = Some(WindowGeometry::from_window(window));
        });
        self._subscriptions.push(sub);
    }

    /// Keep a "follow system" theme live after startup. The subscription is
    /// window-scoped and drops with the root view.
    fn track_system_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.observe_window_appearance(window, |_, window, cx| {
            theme::sync_system_appearance(window, cx);
        });
        self._subscriptions.push(sub);
    }

    /// Persist current preferences (sidebar state, window geometry) when the
    /// app quits.  Settings-window changes are already live in the `Prefs`
    /// global, so folding in the exit-only state completes the snapshot.
    /// File I/O happens on the background executor so it doesn't stall
    /// shutdown; gpui awaits the returned task before exiting.
    fn register_save_on_quit(&self, cx: &mut Context<Self>) {
        let app = cx.entity();
        let shutdown_completed = Arc::clone(&self.shutdown_completed);
        // Register on App itself, not Context<ChatApp>. The Context helper
        // captures only a weak entity and native main-window close can release
        // that entity before the platform dispatches application shutdown.
        App::on_app_quit(cx, move |cx| {
            // GPUI removes the observer closure immediately after invoking it.
            // Keep the coordinator entity alive in the returned future while
            // detached terminal workers and the final store barrier finish.
            let keepalive = app.clone();
            let shutdown_completed = Arc::clone(&shutdown_completed);
            let work = (!shutdown_completed.load(Ordering::Acquire))
                .then(|| app.update(cx, |this, cx| this.prepare_exit_work(cx)));
            let executor = cx.background_executor().clone();
            async move {
                let _keepalive = keepalive;
                let Some(ExitWork {
                    stores,
                    snapshot,
                    preference_saver,
                }) = work
                else {
                    return;
                };
                let session_task = executor.spawn(async move {
                    match stores {
                        Some(stores) => stores.shutdown_with_timeout(APP_QUIT_STORAGE_BUDGET),
                        None => Ok(()),
                    }
                });
                let preferences_task = executor.spawn(async move { preference_saver(snapshot) });
                let (sessions, preferences) = future::join(session_task, preferences_task).await;
                log_exit_results(sessions, preferences);
                shutdown_completed.store(true, Ordering::Release);
            }
        })
        .detach();
    }

    fn register_window_close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let app = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            // Refuse the first native close request so the same pre-quit
            // coordinator used by the menu can finish exact terminal writes
            // before GPUI releases the window-owned entities.
            app.update(cx, |this, cx| this.request_quit(cx)).is_err()
        });
    }

    fn prepare_exit_work(&mut self, cx: &mut Context<Self>) -> ExitWork {
        for conversation in &self.conversations {
            conversation
                .view
                .update(cx, |chat, cx| chat.prepare_for_shutdown(cx));
        }
        let stores = cx.try_global::<SessionStores>().cloned();
        let sidebar_width = self.sidebar_width.as_f32();
        let sidebar_collapsed = self.collapsed;
        let window = self.window_geometry;
        let snapshot = preferences::snapshot_with(cx, |prefs| {
            prefs.sidebar_width = sidebar_width;
            prefs.sidebar_collapsed = sidebar_collapsed;
            prefs.window = window;
        });
        ExitWork {
            stores,
            snapshot,
            preference_saver: Arc::clone(&self.preference_saver),
        }
    }

    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self._quit_task.is_some() {
            return;
        }
        let ExitWork {
            stores,
            snapshot,
            preference_saver,
        } = self.prepare_exit_work(cx);
        let executor = cx.background_executor().clone();
        let session_task = executor.spawn(async move {
            match stores {
                Some(stores) => stores.shutdown(),
                None => Ok(()),
            }
        });
        let preferences_task = executor.spawn(async move { preference_saver(snapshot) });
        let shutdown_completed = Arc::clone(&self.shutdown_completed);
        self._quit_task = Some(cx.spawn(async move |_, cx| {
            let (sessions, preferences) = future::join(session_task, preferences_task).await;
            log_exit_results(sessions, preferences);
            shutdown_completed.store(true, Ordering::Release);
            cx.update(|cx| cx.quit());
        }));
    }

    /// Create a fresh draft conversation with no persisted session id.  The
    /// session id is bound only after the first durable turn begin succeeds
    /// (see [`Self::handle_session_bound`]).
    fn spawn_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        let title: SharedString = t!("chat.default_title").to_string().into();
        let view = ChatView::view(window, cx);
        let sub = self.subscribe_conversation(&view, window, cx);
        let selection = view.read(cx).selection();
        self.conversations.push(Conversation {
            view,
            title,
            selection,
            session_id: None,
            _subscription: sub,
        });
        self.active = self.conversations.last().map(|c| c.view.entity_id());
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    /// Wire up the [`ChatEvent`] subscription for a conversation view.  Kept as
    /// a helper so both draft creation and restore use the same wiring.
    fn subscribe_conversation(
        &mut self,
        view: &Entity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |this, view, event, window, cx| {
            let target = view.entity_id();
            let Some(index) = this.conversation_index(target) else {
                return;
            };
            match event {
                ChatEvent::TitleChanged(title) => {
                    let conversation = &mut this.conversations[index];
                    if conversation.title != *title {
                        conversation.title = title.clone();
                        cx.notify();
                    }
                }
                ChatEvent::SelectionChanged(selection) => {
                    let conversation = &mut this.conversations[index];
                    if conversation.selection.as_ref() != Some(selection) {
                        conversation.selection = Some(selection.clone());
                        if this.active == Some(target) {
                            this.sync_model_picker_to_active(window, cx);
                        }
                        cx.notify();
                    }
                }
                ChatEvent::SessionBound(session_id) => {
                    this.handle_session_bound(target, session_id.clone(), window, cx);
                }
                ChatEvent::DeleteCompleted => {
                    // The subscription belongs to the conversation being
                    // removed. Defer its destruction until this event callback
                    // has returned instead of dropping the active callback in
                    // place.
                    cx.defer_in(window, move |this, window, cx| {
                        this.remove_conversation(target, window, cx)
                    });
                }
            }
        })
    }

    /// Record that a conversation view has bound a persisted session id, either
    /// via a durable turn begin or via [`ChatView::restore_from_session`].
    fn handle_session_bound(
        &mut self,
        target: gpui::EntityId,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.conversation_index(target) else {
            return;
        };
        let conversation = &mut self.conversations[index];
        if conversation.session_id.as_ref() == Some(&session_id) {
            return;
        }
        conversation.session_id = Some(session_id.clone());
        self.opened_session_index.insert(session_id.clone(), target);
        // A brand-new durable session is not yet in the catalog snapshot; a
        // restored session already is.  Refreshing in the background covers
        // both without a full reload and without blocking the UI.
        self.refresh_history_summary(session_id.clone(), cx);
        self.record_active_session(&session_id, cx);
        cx.notify();
        let _ = window;
    }

    /// Select the conversation at `index`.  Only switches the active target;
    /// never drops or cancels another conversation's streaming task.
    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.conversations.len() {
            let target = self.conversations[ix].view.entity_id();
            if self.active != Some(target) {
                self.model_picker
                    .update(cx, |picker, cx| picker.dismiss(window, cx));
                self.active = Some(target);
                self.sync_model_picker_to_active(window, cx);
                cx.notify();
            }
        }
    }

    /// Select a conversation by its view entity id.  Used by render callbacks
    /// that already carry the entity id.
    #[allow(dead_code)]
    fn select_target(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active == Some(target) {
            return;
        }
        if !self
            .conversations
            .iter()
            .any(|c| c.view.entity_id() == target)
        {
            return;
        }
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        self.active = Some(target);
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    /// Lazily select a persisted Chat session.  If the session is already
    /// opened, just switch the active target.  Otherwise spawn a background
    /// catalog/select + hydrate cycle guarded by a monotonic generation so a
    /// stale result cannot overwrite a newer active target.
    fn select_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&target) = self.opened_session_index.get(&session_id) {
            self.select_target(target, window, cx);
            return;
        }

        let Some(stores) = cx.try_global::<SessionStores>().cloned() else {
            return;
        };
        let catalog_store = match stores.chat_catalog() {
            Ok(store) => store,
            Err(error) => {
                crate::logging::error(
                    "chat.workspace",
                    format_args!("cannot select session: {error}"),
                );
                return;
            }
        };

        self.selection_generation = self.selection_generation.wrapping_add(1);
        let generation = self.selection_generation;
        let app = cx.entity();
        let window_handle = window.window_handle();
        let task = cx.spawn(async move |_this, cx| {
            let select_session_id = session_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut controller = ChatSessionCatalogController::new(catalog_store);
                    controller.load_initial().and_then(|_| {
                        controller
                            .select(&select_session_id)
                            .map(|selected| selected.state)
                    })
                })
                .await;
            let state = match result {
                Ok(state) => state,
                Err(error) => {
                    crate::logging::error(
                        "chat.workspace",
                        format_args!("failed to load session {session_id}: {error}"),
                    );
                    return;
                }
            };
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    this.apply_session_restore(generation, session_id, state, window, cx);
                });
            });
        });
        self._selection_task = Some(task);
        cx.notify();
    }

    /// Apply a background-resolved session state to the workspace.  Stale
    /// generations are discarded so a slow load cannot replace a newer active
    /// target the user has since chosen.
    fn apply_session_restore(
        &mut self,
        generation: u64,
        session_id: SessionId,
        state: ResolvedSessionState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.selection_generation {
            return;
        }
        if let Some(&target) = self.opened_session_index.get(&session_id) {
            self.select_target(target, window, cx);
            return;
        }

        let title: SharedString = state
            .messages
            .iter()
            .find(|message| message.message.role == crate::llm::Role::User)
            .and_then(|message| {
                message
                    .message
                    .content
                    .iter()
                    .find_map(|block| match block {
                        crate::llm::ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            Some(derive_chat_title(text))
                        }
                        _ => None,
                    })
            })
            .unwrap_or_else(|| t!("chat.default_title").to_string().into());
        let view = ChatView::view(window, cx);
        let restore_result = view.update(cx, |chat, cx| {
            chat.restore_from_session(&session_id, &state, cx)
        });
        if let Err(error) = restore_result {
            crate::logging::error(
                "chat.workspace",
                format_args!("failed to hydrate session {session_id}: {error}"),
            );
            return;
        }
        let sub = self.subscribe_conversation(&view, window, cx);
        let selection = view.read(cx).selection();
        let target = view.entity_id();
        self.conversations.push(Conversation {
            view,
            title,
            selection,
            session_id: Some(session_id.clone()),
            _subscription: sub,
        });
        self.opened_session_index.insert(session_id.clone(), target);
        self.active = Some(target);
        self.sync_model_picker_to_active(window, cx);
        self.record_active_session(&session_id, cx);
        cx.notify();
    }

    fn sync_model_picker_to_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.active else {
            return;
        };
        let Some(conversation) = self
            .conversations
            .iter()
            .find(|c| c.view.entity_id() == target)
        else {
            return;
        };
        let selection = conversation.selection.clone();
        self.model_picker.update(cx, |picker, cx| {
            picker.set_conversation(selection, window, cx)
        });
    }

    fn select_model_from_picker(
        &mut self,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target) = self.active else {
            return false;
        };
        let Some(conversation) = self
            .conversations
            .iter()
            .find(|c| c.view.entity_id() == target)
        else {
            return false;
        };
        let view = conversation.view.clone();
        view.update(cx, |chat, cx| chat.select_model(selection, cx));
        true
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        self.has_toggled = true;
        cx.notify();
    }

    pub(crate) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_draft(window, cx);
    }

    pub(crate) fn request_delete_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.active else {
            return;
        };
        let Some(conversation) = self
            .conversations
            .iter()
            .find(|c| c.view.entity_id() == target)
        else {
            return;
        };
        let title = conversation.title.clone();
        self.request_delete_conversation(target, title, window, cx);
    }

    fn request_delete_conversation(
        &self,
        target: gpui::EntityId,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app = app.clone();
            alert
                .confirm()
                .title(t!("sidebar.delete_chat_title").to_string())
                .description(
                    t!("sidebar.delete_chat_description", title = title.as_ref()).to_string(),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(t!("sidebar.delete_chat_confirm").to_string())
                        .ok_variant(gpui_component::button::ButtonVariant::Danger)
                        .cancel_text(t!("sidebar.delete_chat_cancel").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    app.update(cx, |this, cx| {
                        this.delete_conversation(target, window, cx);
                    })
                    .is_ok()
                })
        });
    }

    /// Arm inline delete confirmation for a sidebar row.  The row's actions
    /// button becomes a Popover trigger showing a confirm card anchored to it.
    fn begin_delete_confirmation(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_confirmation.dismiss_for_unmount(window, cx);
        self.confirming = Some(target);
        cx.notify();
    }

    /// Resolve a confirmed inline deletion.  Drafts and opened catalog rows go
    /// through the existing view durability path; unopened catalog rows are
    /// deleted directly from the store.
    fn confirm_delete_target(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            SidebarTarget::View(entity) => self.delete_conversation(entity, window, cx),
            SidebarTarget::Session(session_id) => {
                if let Some(&entity) = self.opened_session_index.get(&session_id) {
                    self.delete_conversation(entity, window, cx);
                } else {
                    self.delete_unopened_session(session_id, window, cx);
                }
            }
        }
    }

    fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_confirming = self.confirming == Some(SidebarTarget::View(target));
        if was_confirming {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
        }

        let Some(index) = self.conversation_index(target) else {
            if was_confirming {
                cx.notify();
            }
            return;
        };

        let request = self.conversations[index]
            .view
            .update(cx, |chat, cx| chat.request_delete(cx));
        if request == ChatDeleteRequest::Pending {
            if was_confirming {
                cx.notify();
            }
            return;
        }

        self.remove_conversation(target, window, cx);
    }

    fn remove_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.conversation_index(target) else {
            return;
        };

        let removed = self.conversations.remove(index);
        if let Some(session_id) = &removed.session_id {
            self.opened_session_index.remove(session_id);
            // Keep the catalog snapshot in sync so the row disappears even when
            // the deletion came from the opened-view durability path.
            self.history.remove(session_id);
        }
        if self.hovered == Some(SidebarTarget::View(target)) {
            self.hovered = None;
        }
        if self.confirming == Some(SidebarTarget::View(target)) {
            self.confirming = None;
        }
        if self.active == Some(target) {
            // Keep the active position stable: prefer the conversation now at
            // the same index, fall back to the previous one, then to none.
            self.active = self
                .conversations
                .get(index)
                .map(|c| c.view.entity_id())
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|i| self.conversations.get(i).map(|c| c.view.entity_id()))
                });
        }

        if self.conversations.is_empty() {
            self.active = None;
            self.sync_model_picker_to_active(window, cx);
            cx.notify();
            return;
        }

        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    fn conversation_index(&self, target: gpui::EntityId) -> Option<usize> {
        self.conversations
            .iter()
            .position(|conversation| conversation.view.entity_id() == target)
    }

    fn active_view(&self) -> Option<Entity<ChatView>> {
        self.active.and_then(|target| {
            self.conversations
                .iter()
                .find(|c| c.view.entity_id() == target)
                .map(|c| c.view.clone())
        })
    }
}

fn log_exit_results(
    sessions: Result<(), crate::session::SessionStoresError>,
    preferences: anyhow::Result<()>,
) {
    if let Err(error) = sessions {
        crate::logging::error(
            "session.shutdown",
            format_args!("failed to shut down session stores before exit: {error:?}"),
        );
    }
    if let Err(error) = preferences {
        crate::logging::error(
            "preferences",
            format_args!("failed to save preferences during exit: {error:?}"),
        );
    }
}

#[cfg(test)]
mod tests;
