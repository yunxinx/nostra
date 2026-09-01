//! Root `ChatApp` view: hosts conversations, top bar(s), and the fixed sidebar.

mod agent_workspace;
mod chat_workspace;
mod conversation_host;
mod history_sidebar;
mod project_workspace;
mod render;
mod workspace_host;

use std::{sync::Arc, time::Duration};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, DragMoveEvent, ElementId, EmptyView, Entity,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Render, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, WindowControlArea, div, px,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, InteractiveElementExt as _, Root, Sizable as _, StyledExt as _,
    TITLE_BAR_HEIGHT,
    animation::{EffectTransition, ease_in_out_cubic},
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    sidebar::SidebarToggleButton,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::{glass, theme};
use crate::chat::ChatView;
use crate::llm::ModelSelection;
use crate::preferences::{self, PreferenceHandle, Preferences, WindowGeometry};
use crate::runtime::{
    CHAT_WORKSPACE_ID, ExitCoordinator, NORMAL_EXIT_TIMEOUT, PROJECT_WORKSPACE_ID,
    QUIT_FALLBACK_TIMEOUT, RuntimeServices, RuntimeSnapshotUpdate, WorkspaceId,
};
#[cfg(test)]
use crate::session::SessionId;
use crate::shell::actions::{OpenSettings, ToggleTheme};
use crate::ui::model_select::ModelPicker;

/// Minimum sidebar width when the user drags the right edge inward.
const SIDEBAR_MIN_WIDTH: Pixels = px(220.);
/// Maximum sidebar width when the user drags the right edge outward.
const SIDEBAR_MAX_WIDTH: Pixels = px(440.);
/// Width of the invisible drag hit-area on the sidebar's right edge.
const RESIZE_HANDLE_WIDTH: Pixels = px(6.);

/// Duration of the sidebar collapse/expand animation.
const SIDEBAR_ANIM: Duration = Duration::from_millis(220);

/// x-coordinate where the main column's floating content (model pill) should
/// start when the sidebar is fully collapsed. Equals the width of the fixed
/// top-left overlay (traffic-light padding + toggle + new-chat).
const OVERLAY_INSET: Pixels = px(148.);

/// Shared outer inset for scrollable sidebar content and its fixed footer.
const SIDEBAR_CONTENT_INSET: Pixels = px(8.);

/// Left-pad so content sits to the right of the macOS traffic lights (x=9..77).
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_PAD: Pixels = px(80.);
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_PAD: Pixels = px(12.);

use chat_workspace::{ChatWorkspace, ChatWorkspaceSnapshot};
#[cfg(test)]
use project_workspace::ProjectTarget;
use project_workspace::{ProjectWorkspace, ProjectWorkspaceSnapshot};
use workspace_host::{WorkspaceCommand, WorkspaceHost};

#[derive(Clone)]
enum WorkspaceSnapshot {
    Chat(Box<ChatWorkspaceSnapshot>),
    Project(Box<ProjectWorkspaceSnapshot>),
}

impl WorkspaceSnapshot {
    fn active_view(&self) -> Option<Entity<ChatView>> {
        match self {
            Self::Chat(snapshot) => snapshot.active_view(),
            Self::Project(snapshot) => snapshot.active_view(),
        }
    }

    fn active_selection(&self) -> Option<ModelSelection> {
        match self {
            Self::Chat(snapshot) => snapshot
                .active()
                .and_then(|target| snapshot.conversation(target))
                .and_then(|conversation| conversation.selection()),
            Self::Project(snapshot) => snapshot
                .active()
                .and_then(|target| snapshot.conversation(target))
                .and_then(|conversation| conversation.selection()),
        }
    }
}

pub struct ChatApp {
    focus_handle: FocusHandle,
    workspace_host: WorkspaceHost,
    workspace_snapshot: WorkspaceSnapshot,
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
    /// Current workspace identity selected in the window.
    workspace_id: WorkspaceId,
    _quit_task: Option<gpui::Task<()>>,
    _runtime_snapshot_task: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
    runtime_snapshot: RuntimeSnapshotUpdate,
    preference_handle: PreferenceHandle,
    preference_snapshot: Preferences,
    exit_coordinator: Arc<ExitCoordinator>,
}

struct ExitWork {
    snapshot: Preferences,
}

impl ChatApp {
    /// Build the root app view from persisted preferences.  Sidebar width is
    /// clamped into the allowed range, and a save-on-quit hook is registered
    /// so the current UI state survives across restarts.
    pub fn new(
        prefs: Preferences,
        services: RuntimeServices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(prefs, services, window, cx)
    }

    fn build(
        prefs: Preferences,
        services: RuntimeServices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preference_handle = services.preference_handle().clone();
        let exit_coordinator = services.exit_coordinator();
        let runtime_snapshots = services.runtime_snapshots();
        let runtime_snapshot = runtime_snapshots.current();
        let mut runtime_snapshot_subscription = runtime_snapshots.subscribe();
        let sidebar_width = px(prefs.sidebar_width)
            .max(SIDEBAR_MIN_WIDTH)
            .min(SIDEBAR_MAX_WIDTH);
        let parent = cx.weak_entity();
        let workspace_host = WorkspaceHost::new(services, preference_handle.clone(), window, cx);
        let chat_workspace = workspace_host.chat_workspace();
        let project_workspace = workspace_host.project_workspace();
        let requested_workspace_id = if prefs.restore_last_workspace_on_start {
            prefs.last_workspace_id
        } else {
            CHAT_WORKSPACE_ID
        };
        let workspace_id = workspace_host
            .registry_snapshot()
            .get(requested_workspace_id)
            .map_or(CHAT_WORKSPACE_ID, |_| requested_workspace_id);
        let workspace_snapshot = if workspace_id == PROJECT_WORKSPACE_ID {
            WorkspaceSnapshot::Project(Box::new(project_workspace.read(cx).snapshot().clone()))
        } else {
            WorkspaceSnapshot::Chat(Box::new(chat_workspace.read(cx).snapshot().clone()))
        };
        let model_picker = cx.new(|cx| {
            ModelPicker::new(
                prefs.last_model_selection.clone(),
                preference_handle.clone(),
                move |selection, cx| {
                    parent
                        .update(cx, |app, cx| app.select_model_from_picker(selection, cx))
                        .unwrap_or(false)
                },
                window,
                cx,
            )
        });

        let workspace_subscription =
            cx.observe_in(&chat_workspace, window, |this, workspace, window, cx| {
                if this.workspace_id == CHAT_WORKSPACE_ID {
                    this.workspace_snapshot =
                        WorkspaceSnapshot::Chat(Box::new(workspace.read(cx).snapshot().clone()));
                    this.sync_model_picker_to_active(window, cx);
                    cx.notify();
                }
            });
        let project_workspace_subscription =
            cx.observe_in(&project_workspace, window, |this, workspace, window, cx| {
                if this.workspace_id == PROJECT_WORKSPACE_ID {
                    this.workspace_snapshot =
                        WorkspaceSnapshot::Project(Box::new(workspace.read(cx).snapshot().clone()));
                    this.sync_model_picker_to_active(window, cx);
                    cx.notify();
                }
            });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace_host,
            workspace_snapshot,
            collapsed: prefs.sidebar_collapsed,
            has_toggled: false,
            sidebar_width,
            resize_start: None,
            titlebar_move_pending: false,
            window_geometry: Some(WindowGeometry::from_window(window)),
            model_picker,
            workspace_id,
            _quit_task: None,
            _runtime_snapshot_task: None,
            _subscriptions: vec![workspace_subscription, project_workspace_subscription],
            runtime_snapshot,
            preference_handle,
            preference_snapshot: prefs.clone(),
            exit_coordinator,
        };
        this.track_window_geometry(window, cx);
        this._runtime_snapshot_task = Some(cx.spawn(async move |this, cx| {
            while let Some(update) = runtime_snapshot_subscription.next().await {
                if this
                    .update(cx, |this, cx| {
                        if update.revision() <= this.runtime_snapshot.revision() {
                            return;
                        }
                        this.runtime_snapshot = update;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        this.track_preferences(window, cx);
        this.track_system_appearance(window, cx);
        this.register_save_on_quit(cx);
        this.register_window_close(window, cx);
        if this.workspace_id == PROJECT_WORKSPACE_ID {
            this.project_workspace()
                .update(cx, |workspace, cx| workspace.start_agent_projects_load(cx));
        }
        this
    }

    #[cfg(test)]
    fn runtime_snapshot_for_test(&self) -> &RuntimeSnapshotUpdate {
        &self.runtime_snapshot
    }

    fn chat_workspace(&self) -> Entity<ChatWorkspace> {
        self.workspace_host.chat_workspace()
    }

    fn project_workspace(&self) -> Entity<ProjectWorkspace> {
        self.workspace_host.project_workspace()
    }

    fn chat_snapshot(&self) -> &ChatWorkspaceSnapshot {
        match &self.workspace_snapshot {
            WorkspaceSnapshot::Chat(snapshot) => snapshot,
            WorkspaceSnapshot::Project(_) => unreachable!("active workspace snapshot is Chat"),
        }
    }

    fn project_snapshot(&self) -> &ProjectWorkspaceSnapshot {
        match &self.workspace_snapshot {
            WorkspaceSnapshot::Project(snapshot) => snapshot,
            WorkspaceSnapshot::Chat(_) => unreachable!("active workspace snapshot is Project"),
        }
    }

    fn sync_workspace_snapshot(&mut self, cx: &App) {
        self.workspace_snapshot = if self.workspace_id == PROJECT_WORKSPACE_ID {
            WorkspaceSnapshot::Project(Box::new(
                self.project_workspace().read(cx).snapshot().clone(),
            ))
        } else {
            WorkspaceSnapshot::Chat(Box::new(self.chat_workspace().read(cx).snapshot().clone()))
        };
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
        let sub = cx.observe_window_appearance(window, |this, window, cx| {
            theme::sync_system_appearance(this.preference_snapshot.theme_mode, window, cx);
        });
        self._subscriptions.push(sub);
    }

    fn track_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preference_handle = self.preference_handle.clone();
        let sub = cx.observe_global_in::<preferences::Prefs>(window, move |this, _, cx| {
            let snapshot = preference_handle.snapshot();
            if this.preference_snapshot == snapshot {
                return;
            }
            this.preference_snapshot = snapshot;
            this.project_workspace()
                .update(cx, |workspace, cx| workspace.refresh_preferences(cx));
            cx.notify();
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
        let exit_coordinator = self.exit_coordinator.clone();
        // Register on App itself, not Context<ChatApp>. The Context helper
        // captures only a weak entity and native main-window close can release
        // that entity before the platform dispatches application shutdown.
        App::on_app_quit(cx, move |cx| {
            // GPUI removes the observer closure immediately after invoking it.
            // Keep the coordinator entity alive in the returned future while
            // detached terminal workers and the final store barrier finish.
            let keepalive = app.clone();
            let exit_coordinator = exit_coordinator.clone();
            let work = app.update(cx, |this, cx| this.prepare_exit_work(cx));
            let executor = cx.background_executor().clone();
            let ExitWork { snapshot } = work;
            let task = executor.spawn(exit_coordinator.run(snapshot, QUIT_FALLBACK_TIMEOUT));
            async move {
                let _keepalive = keepalive;
                let report = task.await;
                log_exit_results(&report);
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
        self.workspace_host.prepare_for_shutdown(cx);
        let sidebar_width = self.sidebar_width.as_f32();
        let sidebar_collapsed = self.collapsed;
        let window = self.window_geometry;
        let snapshot = preferences::snapshot_with(cx, |prefs| {
            prefs.sidebar_width = sidebar_width;
            prefs.sidebar_collapsed = sidebar_collapsed;
            prefs.window = window;
        });
        ExitWork { snapshot }
    }

    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self._quit_task.is_some() {
            return;
        }
        let ExitWork { snapshot } = self.prepare_exit_work(cx);
        let exit_coordinator = self.exit_coordinator.clone();
        let executor = cx.background_executor().clone();
        self._quit_task = Some(cx.spawn(async move |_, cx| {
            let report = executor
                .spawn(exit_coordinator.run(snapshot, NORMAL_EXIT_TIMEOUT))
                .await;
            log_exit_results(&report);
            cx.update(|cx| cx.quit());
        }));
    }

    fn sync_model_picker_to_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = self.workspace_snapshot.active_selection();
        self.model_picker.update(cx, |picker, cx| {
            picker.set_conversation(selection, window, cx)
        });
    }

    fn select_model_from_picker(
        &mut self,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        let handled = self.workspace_host.dispatch(
            self.workspace_id,
            WorkspaceCommand::SelectModel(selection),
            None,
            cx,
        );
        self.sync_workspace_snapshot(cx);
        handled
    }

    fn dispatch_workspace_command(
        &mut self,
        workspace_id: WorkspaceId,
        command: WorkspaceCommand,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let handled = self
            .workspace_host
            .dispatch(workspace_id, command, window, cx);
        self.sync_workspace_snapshot(cx);
        handled
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        self.has_toggled = true;
        cx.notify();
    }

    pub(crate) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_id == CHAT_WORKSPACE_ID {
            self.model_picker
                .update(cx, |picker, cx| picker.dismiss(window, cx));
        }
        self.dispatch_workspace_command(self.workspace_id, WorkspaceCommand::New, Some(window), cx);
    }

    #[cfg(test)]
    fn spawn_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        self.dispatch_workspace_command(CHAT_WORKSPACE_ID, WorkspaceCommand::New, Some(window), cx);
        self.sync_workspace_snapshot(cx);
    }

    #[cfg(test)]
    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        let target = self
            .chat_workspace()
            .read(cx)
            .conversations
            .conversations()
            .get(index)
            .map(|conversation| conversation.view.entity_id());
        if let Some(target) = target {
            self.dispatch_workspace_command(
                CHAT_WORKSPACE_ID,
                WorkspaceCommand::SelectView(target),
                Some(window),
                cx,
            );
        }
        self.sync_workspace_snapshot(cx);
    }

    #[cfg(test)]
    fn select_target(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        self.dispatch_workspace_command(
            CHAT_WORKSPACE_ID,
            WorkspaceCommand::SelectView(target),
            Some(window),
            cx,
        );
        self.sync_workspace_snapshot(cx);
    }

    #[cfg(test)]
    fn select_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        self.dispatch_workspace_command(
            CHAT_WORKSPACE_ID,
            WorkspaceCommand::RestoreChatSession(session_id),
            Some(window),
            cx,
        );
        self.sync_workspace_snapshot(cx);
    }

    #[cfg(test)]
    fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_workspace_command(
            CHAT_WORKSPACE_ID,
            WorkspaceCommand::DeleteView(target),
            Some(window),
            cx,
        );
        self.sync_workspace_snapshot(cx);
    }

    pub(crate) fn request_delete_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let revealed = self.workspace_host.dispatch(
            self.workspace_id,
            WorkspaceCommand::DeleteActive,
            Some(window),
            cx,
        );
        if revealed && self.collapsed {
            self.collapsed = false;
            self.has_toggled = true;
            cx.notify();
        }
    }

    fn active_view(&self) -> Option<Entity<ChatView>> {
        self.workspace_snapshot.active_view()
    }
}

fn log_exit_results(report: &crate::runtime::ExitReport) {
    if let Err(error) = &report.session {
        crate::logging::error(
            "session.shutdown",
            format_args!("failed to shut down session stores before exit: {error}"),
        );
    }
    if let Err(error) = &report.preferences {
        crate::logging::error(
            "preferences",
            format_args!("failed to save preferences during exit: {error}"),
        );
    }
}

#[cfg(test)]
mod tests;
