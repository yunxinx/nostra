//! Root `ChatApp` view: hosts conversations, top bar(s), and the fixed sidebar.

mod render;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::future;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, DragMoveEvent, ElementId, EmptyView, Entity,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, WindowControlArea, div, px,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, InteractiveElementExt as _, Root, Sizable as _, StyledExt as _,
    TITLE_BAR_HEIGHT, WindowExt as _,
    animation::{Transition, ease_in_out_cubic},
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    sidebar::SidebarToggleButton,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::{glass, theme};
use crate::chat::{ChatDeleteRequest, ChatEvent, ChatView};
use crate::llm::ModelSelection;
use crate::preferences::{self, Preferences, WindowGeometry};
use crate::session::SessionStores;
use crate::shell::actions::{OpenSettings, ToggleTheme};
use crate::ui::{
    self,
    inline_delete_confirmation::{InlineDeleteConfirmation, InlineDeleteConfirmationHandle},
    model_select::ModelPicker,
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
    active: usize,
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
    /// Conversation whose row the pointer is over, so its actions button can
    /// appear.  Matches `Conversation::view.entity_id()`.
    hovered: Option<gpui::EntityId>,
    /// Conversation awaiting inline delete confirmation.  While set, its row
    /// shows a Popover confirm card anchored to the actions button.
    confirming: Option<gpui::EntityId>,
    delete_confirmation: InlineDeleteConfirmationHandle,
    shutdown_completed: Arc<AtomicBool>,
    _quit_task: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
    preference_saver: PreferenceSaver,
}

type PreferenceSaver = Arc<dyn Fn(Preferences) -> anyhow::Result<()> + Send + Sync>;

struct ExitWork {
    stores: Option<SessionStores>,
    snapshot: Preferences,
    preference_saver: PreferenceSaver,
}

struct Conversation {
    view: Entity<ChatView>,
    title: SharedString,
    selection: Option<ModelSelection>,
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
            active: 0,
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
            shutdown_completed: Arc::new(AtomicBool::new(false)),
            _quit_task: None,
            _subscriptions: Vec::new(),
            preference_saver,
        };
        this.track_window_geometry(window, cx);
        this.track_system_appearance(window, cx);
        this.spawn_conversation(window, cx);
        this.register_save_on_quit(cx);
        this.register_window_close(window, cx);
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

    fn spawn_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        let title: SharedString = t!("chat.default_title").to_string().into();
        let view = ChatView::view(window, cx);
        let sub = cx.subscribe_in(&view, window, |this, view, event, window, cx| {
            let target = view.entity_id();
            let Some(index) = this
                .conversations
                .iter()
                .position(|conversation| conversation.view.entity_id() == target)
            else {
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
                        if index == this.active {
                            this.sync_model_picker_to_active(window, cx);
                        }
                        cx.notify();
                    }
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
        });
        let selection = view.read(cx).selection();
        self.conversations.push(Conversation {
            view,
            title,
            selection,
            _subscription: sub,
        });
        self.active = self.conversations.len() - 1;
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.conversations.len() && ix != self.active {
            self.model_picker
                .update(cx, |picker, cx| picker.dismiss(window, cx));
            self.active = ix;
            self.sync_model_picker_to_active(window, cx);
            cx.notify();
        }
    }

    fn sync_model_picker_to_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversations.get(self.active) else {
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
        let Some(conversation) = self.conversations.get(self.active) else {
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
        self.spawn_conversation(window, cx);
    }

    pub(crate) fn request_delete_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversations.get(self.active) else {
            return;
        };
        let target = conversation.view.entity_id();
        self.request_delete_conversation(target, conversation.title.clone(), window, cx);
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

    /// Arm inline delete confirmation for a conversation.  The row's actions
    /// button becomes a Popover trigger showing a confirm card anchored to it.
    fn begin_delete_confirmation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_confirmation.dismiss_for_unmount(window, cx);
        self.confirming = Some(target);
        cx.notify();
    }

    fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_confirming = self.confirming == Some(target);
        if was_confirming {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
        }

        let Some(index) = self
            .conversations
            .iter()
            .position(|conversation| conversation.view.entity_id() == target)
        else {
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
        let Some(index) = self
            .conversations
            .iter()
            .position(|conversation| conversation.view.entity_id() == target)
        else {
            return;
        };

        self.conversations.remove(index);
        if self.hovered == Some(target) {
            self.hovered = None;
        }
        if self.conversations.is_empty() {
            self.active = 0;
            self.spawn_conversation(window, cx);
            return;
        }

        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = index.min(self.conversations.len() - 1);
        }
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
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
