//! Root `ChatApp` view: hosts conversations, top bar(s), and the fixed sidebar.

mod agent_workspace;
mod chat_workspace;
mod history_sidebar;
mod render;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

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
    animation::{EffectTransition, ease_in_out_cubic},
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    notification::NotificationType,
    sidebar::SidebarToggleButton,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::{glass, theme};
use crate::chat::{
    ChatDeleteRequest, ChatEvent, ChatView, create_conversation_runtime, derive_chat_title,
};
use crate::llm::ModelSelection;
use crate::preferences::{self, PreferenceHandle, Preferences, WindowGeometry, WorkspaceMode};
use crate::runtime::{
    ExitCoordinator, NORMAL_EXIT_TIMEOUT, QUIT_FALLBACK_TIMEOUT, RuntimeServices,
};
use crate::session::{
    ProjectSessionStore as _, ResolvedSessionState, SessionId, SessionLifecycleStore as _,
    SessionStores,
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

use chat_workspace::{ChatWorkspace, ChatWorkspaceSnapshot, SelectionEpoch, SelectionRequest};

struct AgentSessionRestore {
    request: SelectionRequest,
    project_id: String,
    session_id: SessionId,
    project: crate::session::ProjectIdentity,
    state: ResolvedSessionState,
}

pub struct ChatApp {
    focus_handle: FocusHandle,
    chat_workspace: Entity<ChatWorkspace>,
    chat_workspace_snapshot: ChatWorkspaceSnapshot,
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
    /// Agent sidebar row awaiting inline delete confirmation.
    confirming: Option<SidebarTarget>,
    delete_confirmation: InlineDeleteConfirmationHandle,
    /// Current workspace mode (Chat or Agent).
    workspace_mode: WorkspaceMode,
    /// Agent project workspace snapshot.  Render only reads this; every
    /// mutation comes from a background load completion.
    agent: AgentWorkspace,
    /// Project conversations use the same ChatView/composer as Chat mode.
    agent_conversations: Vec<AgentConversation>,
    /// Entity id of the currently displayed project conversation.
    agent_active: Option<gpui::EntityId>,
    /// Request identity guarding project-session materialization independently
    /// from the older read-only Agent snapshot loader.
    agent_selection_epoch: SelectionEpoch,
    /// Background task owning the Agent project catalog load.
    _agent_projects_task: Option<Task<()>>,
    /// Background tasks owning per-project Agent session-list loads.
    _agent_session_list_tasks: HashMap<String, Task<()>>,
    /// Background task owning the Agent session detail load.
    _agent_session_task: Option<Task<()>>,
    /// Background task opening a project session into a retained `ChatView`.
    _agent_open_task: Option<Task<()>>,
    /// Background task owning a direct Agent session or project deletion.
    _agent_delete_task: Option<Task<()>>,
    /// Projects hidden from interaction while their durable deletion runs.
    agent_deleting_projects: HashSet<String>,
    /// Background task resolving the native folder picker for a new Agent
    /// work project.
    _folder_task: Option<Task<()>>,
    _quit_task: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
    session_services: SessionStores,
    runtime_services: RuntimeServices,
    preference_handle: PreferenceHandle,
    preference_snapshot: Preferences,
    exit_coordinator: Arc<ExitCoordinator>,
}

/// Identity of a sidebar row.  Drafts (unbound views) are addressed by their
/// view entity; persisted catalog rows are addressed by their session id so the
/// row stays stable whether or not the session is currently opened.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum SidebarTarget {
    View(gpui::EntityId),
    Session(SessionId),
    AgentView(gpui::EntityId),
    AgentSession {
        project_id: String,
        session_id: SessionId,
    },
    AgentProject(String),
}

use agent_workspace::AgentWorkspace;

struct ExitWork {
    snapshot: Preferences,
}

struct AgentConversation {
    view: Entity<ChatView>,
    project_id: String,
    title: SharedString,
    selection: Option<ModelSelection>,
    session_id: Option<SessionId>,
    _subscription: Subscription,
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
        let session_services = services.session_services().clone();
        let preference_handle = services.preference_handle().clone();
        let exit_coordinator = services.exit_coordinator();
        let sidebar_width = px(prefs.sidebar_width)
            .max(SIDEBAR_MIN_WIDTH)
            .min(SIDEBAR_MAX_WIDTH);
        let workspace_mode = if prefs.restore_last_workspace_on_start {
            WorkspaceMode::from_workspace_id(prefs.last_workspace_id)
        } else {
            WorkspaceMode::Chat
        };

        let parent = cx.weak_entity();
        let chat_workspace = cx
            .new(|cx| ChatWorkspace::new(services.clone(), preference_handle.clone(), window, cx));
        let chat_workspace_snapshot = chat_workspace.read(cx).snapshot().clone();
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
                this.chat_workspace_snapshot = workspace.read(cx).snapshot().clone();
                this.sync_model_picker_to_active(window, cx);
                cx.notify();
            });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            chat_workspace,
            chat_workspace_snapshot,
            collapsed: prefs.sidebar_collapsed,
            has_toggled: false,
            sidebar_width,
            resize_start: None,
            titlebar_move_pending: false,
            window_geometry: Some(WindowGeometry::from_window(window)),
            model_picker,
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            workspace_mode,
            agent: AgentWorkspace::new(),
            agent_conversations: Vec::new(),
            agent_active: None,
            agent_selection_epoch: SelectionEpoch::default(),
            _agent_projects_task: None,
            _agent_session_list_tasks: HashMap::new(),
            _agent_session_task: None,
            _agent_open_task: None,
            _agent_delete_task: None,
            agent_deleting_projects: HashSet::new(),
            _folder_task: None,
            _quit_task: None,
            _subscriptions: vec![workspace_subscription],
            session_services,
            runtime_services: services,
            preference_handle,
            preference_snapshot: prefs.clone(),
            exit_coordinator,
        };
        this.track_window_geometry(window, cx);
        this.track_preferences(window, cx);
        this.track_system_appearance(window, cx);
        this.register_save_on_quit(cx);
        this.register_window_close(window, cx);
        if matches!(this.workspace_mode, WorkspaceMode::Project) {
            this.start_agent_projects_load(cx);
        }
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
        self.chat_workspace
            .update(cx, |workspace, cx| workspace.prepare_for_shutdown(cx));
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

    fn begin_agent_selection_request(&mut self) -> SelectionRequest {
        self._agent_open_task = None;
        self.agent_selection_epoch.begin()
    }

    fn invalidate_agent_selection_request(&mut self) {
        self._agent_open_task = None;
        self._agent_session_task = None;
        self.agent.invalidate_session_load();
        self.agent_selection_epoch.invalidate();
    }

    fn agent_selection_request_is_current(
        &self,
        request: SelectionRequest,
        project_id: &str,
        session_id: &SessionId,
    ) -> bool {
        self.agent_selection_epoch.is_current(request)
            && self.agent.open_project_id() == Some(project_id)
            && self.agent.selected_session_id() == Some(session_id)
    }

    fn create_conversation_scope(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<crate::runtime::ConversationScopeHandle> {
        match self.runtime_services.create_conversation_scope() {
            Ok(scope) => Some(scope),
            Err(error) => {
                crate::logging::error(
                    "runtime.scope",
                    format_args!("failed to create conversation scope: {error}"),
                );
                window.push_notification(
                    (
                        NotificationType::Error,
                        t!("chat.error.runtime_unavailable").to_string(),
                    ),
                    cx,
                );
                None
            }
        }
    }

    fn agent_conversation_index(&self, target: gpui::EntityId) -> Option<usize> {
        self.agent_conversations
            .iter()
            .position(|conversation| conversation.view.entity_id() == target)
    }

    fn project_identity(
        &self,
        project_id: &str,
        _cx: &App,
    ) -> Option<crate::session::ProjectIdentity> {
        self.agent
            .projects()
            .iter()
            .find(|project| project.project_id == project_id)
            .and_then(|project| {
                crate::session::ProjectIdentity::from_parts(
                    project.project_id.clone(),
                    project.canonical_path.clone(),
                    project.display_name.clone(),
                )
                .ok()
            })
            .or_else(|| {
                self.preference_handle
                    .snapshot()
                    .agent_projects
                    .into_iter()
                    .find(|project| project.project_id == project_id)
                    .and_then(|project| {
                        crate::session::ProjectIdentity::from_parts(
                            project.project_id,
                            project.canonical_path,
                            project.display_name,
                        )
                        .ok()
                    })
            })
    }

    fn subscribe_agent_conversation(
        &mut self,
        view: &Entity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |this, view, event, window, cx| {
            let target = view.entity_id();
            let Some(index) = this.agent_conversation_index(target) else {
                return;
            };
            match event {
                ChatEvent::TitleChanged(title) => {
                    this.agent_conversations[index].title = title.clone();
                }
                ChatEvent::SelectionChanged(selection) => {
                    this.agent_conversations[index].selection = Some(selection.clone());
                    if this.agent_active == Some(target) {
                        this.sync_model_picker_to_active(window, cx);
                    }
                }
                ChatEvent::StateChanged => {}
                ChatEvent::SessionBound(session_id) => {
                    let project_id = this.agent_conversations[index].project_id.clone();
                    this.agent_conversations[index].session_id = Some(session_id.clone());
                    this.agent
                        .bind_draft_session(project_id.clone(), session_id.clone());
                    this.refresh_agent_sessions(project_id, cx);
                }
                ChatEvent::DeleteCompleted => {
                    let conversation = &this.agent_conversations[index];
                    let project_id = conversation.project_id.clone();
                    let session_id = conversation.session_id.clone();
                    cx.defer_in(window, move |this, window, cx| {
                        this.remove_agent_conversation(target, project_id, session_id, window, cx);
                    });
                }
            }
            cx.notify();
        })
    }

    fn open_agent_draft(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing_view) = self
            .agent_conversations
            .iter()
            .find(|c| c.project_id == project_id && c.session_id.is_none())
            .map(|conversation| conversation.view.clone())
        {
            self.invalidate_agent_selection_request();
            self.agent.open_draft(project_id);
            existing_view.update(cx, |view, cx| view.focus_composer(window, cx));
            self.agent_active = Some(existing_view.entity_id());
            self.sync_model_picker_to_active(window, cx);
            cx.notify();
            return;
        }
        let Some(identity) = self.project_identity(&project_id, cx) else {
            return;
        };
        let Some(scope) = self.create_conversation_scope(window, cx) else {
            return;
        };
        self.invalidate_agent_selection_request();
        let runtime = create_conversation_runtime(
            scope,
            self.session_services.project_conversation(identity),
            self.runtime_services.generation_service(),
            cx,
        );
        let view = ChatView::project_view_with_generation_service_and_preferences(
            runtime,
            self.preference_handle.clone(),
            window,
            cx,
        );
        let selection = view.read(cx).selection();
        let subscription = self.subscribe_agent_conversation(&view, window, cx);
        let target = view.entity_id();
        self.agent_conversations.push(AgentConversation {
            view: view.clone(),
            project_id: project_id.clone(),
            title: t!("agent.new_draft").to_string().into(),
            selection,
            session_id: None,
            _subscription: subscription,
        });
        self.agent.new_project_draft(project_id.clone());
        self.start_agent_sessions_load(project_id, cx);
        self.agent_active = Some(target);
        view.update(cx, |view, cx| view.focus_composer(window, cx));
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    fn open_agent_session(
        &mut self,
        project_id: String,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing_target) = self
            .agent_conversations
            .iter()
            .find(|c| c.session_id.as_ref() == Some(&session_id))
            .map(|conversation| conversation.view.entity_id())
        {
            self.invalidate_agent_selection_request();
            self.agent.select_session(project_id, session_id);
            self.agent_active = Some(existing_target);
            self.sync_model_picker_to_active(window, cx);
            cx.notify();
            return;
        }
        let Some(identity) = self.project_identity(&project_id, cx) else {
            return;
        };
        let stores = self.session_services.clone();
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(_) => return,
        };
        let request = self.begin_agent_selection_request();
        let app = cx.entity();
        let window_handle = window.window_handle();
        self.agent
            .select_session(project_id.clone(), session_id.clone());
        let load_project_id = project_id.clone();
        let load_session_id = session_id.clone();
        let task = cx.spawn(async move |_, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    project_store.load_project_session(&load_project_id, &load_session_id, None)
                })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    let Ok(state) = loaded else {
                        return;
                    };
                    this.apply_agent_session_restore(
                        AgentSessionRestore {
                            request,
                            project_id,
                            session_id,
                            project: identity,
                            state,
                        },
                        window,
                        cx,
                    );
                });
            });
        });
        self._agent_open_task = Some(task);
        cx.notify();
    }

    fn apply_agent_session_restore(
        &mut self,
        restore: AgentSessionRestore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.agent_selection_request_is_current(
            restore.request,
            &restore.project_id,
            &restore.session_id,
        ) {
            return;
        }
        if let Some(existing_target) = self
            .agent_conversations
            .iter()
            .find(|conversation| conversation.session_id.as_ref() == Some(&restore.session_id))
            .map(|conversation| conversation.view.entity_id())
        {
            self.agent_active = Some(existing_target);
            self.sync_model_picker_to_active(window, cx);
            cx.notify();
            return;
        }

        let Some(scope) = self.create_conversation_scope(window, cx) else {
            return;
        };
        let runtime = create_conversation_runtime(
            scope,
            self.session_services
                .project_conversation(restore.project.clone()),
            self.runtime_services.generation_service(),
            cx,
        );
        let view = ChatView::project_view_with_generation_service_and_preferences(
            runtime,
            self.preference_handle.clone(),
            window,
            cx,
        );

        let title = restore
            .state
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
            .unwrap_or_else(|| t!("agent.untitled_session").to_string().into());
        if view
            .update(cx, |chat, cx| {
                chat.restore_from_session(&restore.session_id, &restore.state, cx)
            })
            .is_err()
        {
            view.update(cx, |view, cx| view.close_scope(cx));
            return;
        }
        let subscription = self.subscribe_agent_conversation(&view, window, cx);
        let target = view.entity_id();
        self.agent_conversations.push(AgentConversation {
            view: view.clone(),
            project_id: restore.project_id,
            title,
            selection: view.read(cx).selection(),
            session_id: Some(restore.session_id),
            _subscription: subscription,
        });
        self.agent_active = Some(target);
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    fn remove_agent_conversation(
        &mut self,
        target: gpui::EntityId,
        project_id: String,
        session_id: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.agent_conversation_index(target) else {
            return;
        };
        let removed = self.agent_conversations.remove(index);
        if session_id.is_none() {
            removed.view.update(cx, |view, cx| view.close_scope(cx));
        }
        if let Some(session_id) = session_id {
            self.agent.remove_session(&project_id, &session_id);
        }
        self.agent.discard_draft(&project_id);
        if self.agent_active == Some(target) {
            self.agent_active = None;
        }
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }

    fn delete_unopened_agent_session(
        &mut self,
        project_id: String,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stores = self.session_services.clone();
        let Ok(mut store) = stores.agent() else {
            return;
        };
        let app = cx.weak_entity();
        let window_handle = window.window_handle();
        self._agent_delete_task = Some(cx.spawn(async move |_, cx| {
            let delete_id = session_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { store.delete_session(&delete_id) })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    match result {
                        Ok(()) => this.agent.remove_session(&project_id, &session_id),
                        Err(error) => {
                            crate::logging::error(
                                "agent.delete",
                                format_args!("failed to delete Agent session: {error}"),
                            );
                            window.push_notification(
                                (
                                    NotificationType::Error,
                                    t!("agent.delete_failed").to_string(),
                                ),
                                cx,
                            );
                        }
                    }
                    this._agent_delete_task = None;
                    cx.notify();
                })
                .ok();
            });
        }));
    }

    fn delete_agent_project(
        &mut self,
        project: crate::session::ProjectSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_in_flight_work = self
            .agent_conversations
            .iter()
            .filter(|conversation| conversation.project_id == project.project_id)
            .any(|conversation| conversation.view.read(cx).has_in_flight_work());
        if has_in_flight_work {
            window.push_notification(
                (NotificationType::Error, t!("agent.delete_busy").to_string()),
                cx,
            );
            return;
        }
        let stores = self.session_services.clone();
        let Ok(project_store) = stores.agent_projects() else {
            return;
        };
        let Ok(mut lifecycle) = stores.agent() else {
            return;
        };
        let project_id = project.project_id.clone();
        let apply_project_id = project_id.clone();
        let app = cx.weak_entity();
        let window_handle = window.window_handle();
        self.agent_deleting_projects
            .insert(apply_project_id.clone());
        cx.notify();
        self._agent_delete_task = Some(cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut cursor = None;
                    loop {
                        let page = project_store.list_project_sessions(
                            &project_id,
                            crate::session::CatalogQuery {
                                cursor,
                                ..crate::session::CatalogQuery::first_page()
                            },
                        )?;
                        for summary in page.sessions {
                            lifecycle.delete_session(&summary.session_id)?;
                        }
                        let Some(next) = page.next_cursor else {
                            break;
                        };
                        cursor = Some(next);
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            preferences::remove_agent_project(&apply_project_id, cx);
                            if this.agent.open_project_id() == Some(apply_project_id.as_str()) {
                                this.invalidate_agent_selection_request();
                            }
                            let removed_targets = this
                                .agent_conversations
                                .iter()
                                .filter(|conversation| conversation.project_id == apply_project_id)
                                .map(|conversation| conversation.view.entity_id())
                                .collect::<Vec<_>>();
                            this.agent_conversations
                                .retain(|conversation| conversation.project_id != apply_project_id);
                            if this
                                .agent_active
                                .is_some_and(|target| removed_targets.contains(&target))
                            {
                                this.agent_active = None;
                            }
                            this.agent.remove_project(&apply_project_id);
                        }
                        Err(error) => {
                            crate::logging::error(
                                "agent.delete",
                                format_args!("failed to delete Agent project: {error}"),
                            );
                            window.push_notification(
                                (
                                    NotificationType::Error,
                                    t!("agent.delete_failed").to_string(),
                                ),
                                cx,
                            );
                        }
                    }
                    this.agent_deleting_projects.remove(&apply_project_id);
                    this._agent_delete_task = None;
                    cx.notify();
                })
                .ok();
            });
        }));
    }

    fn sync_model_picker_to_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match self.workspace_mode {
            WorkspaceMode::Chat => self.chat_workspace_snapshot.active(),
            WorkspaceMode::Project => self.agent_active,
        };
        let Some(target) = target else {
            return;
        };
        let selection = match self.workspace_mode {
            WorkspaceMode::Chat => self
                .chat_workspace_snapshot
                .conversation(target)
                .and_then(|conversation| conversation.selection()),
            WorkspaceMode::Project => self
                .agent_conversations
                .iter()
                .find(|c| c.view.entity_id() == target)
                .and_then(|conversation| conversation.selection.clone()),
        };
        self.model_picker.update(cx, |picker, cx| {
            picker.set_conversation(selection, window, cx)
        });
    }

    fn select_model_from_picker(
        &mut self,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        let target = match self.workspace_mode {
            WorkspaceMode::Chat => self.chat_workspace_snapshot.active(),
            WorkspaceMode::Project => self.agent_active,
        };
        let Some(target) = target else {
            return false;
        };
        let view = match self.workspace_mode {
            WorkspaceMode::Chat => self
                .chat_workspace_snapshot
                .conversation(target)
                .map(|conversation| conversation.view()),
            WorkspaceMode::Project => self
                .agent_conversations
                .iter()
                .find(|c| c.view.entity_id() == target)
                .map(|conversation| conversation.view.clone()),
        };
        let Some(view) = view else {
            return false;
        };
        if matches!(self.workspace_mode, WorkspaceMode::Chat) {
            return self
                .chat_workspace
                .update(cx, |workspace, cx| workspace.select_model(selection, cx));
        }
        view.update(cx, |chat, cx| chat.select_model(selection, cx));
        true
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        self.has_toggled = true;
        cx.notify();
    }

    pub(crate) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.workspace_mode {
            WorkspaceMode::Chat => {
                self.model_picker
                    .update(cx, |picker, cx| picker.dismiss(window, cx));
                self.chat_workspace
                    .update(cx, |workspace, cx| workspace.spawn_draft(window, cx));
            }
            WorkspaceMode::Project => self.open_project_folder(cx),
        }
    }

    #[cfg(test)]
    fn spawn_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        self.chat_workspace
            .update(cx, |workspace, cx| workspace.spawn_draft(window, cx));
        self.chat_workspace_snapshot = self.chat_workspace.read(cx).snapshot().clone();
    }

    #[cfg(test)]
    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker
            .update(cx, |picker, cx| picker.dismiss(window, cx));
        self.chat_workspace
            .update(cx, |workspace, cx| workspace.select(index, cx));
        self.chat_workspace_snapshot = self.chat_workspace.read(cx).snapshot().clone();
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
        self.chat_workspace
            .update(cx, |workspace, cx| workspace.select_target(target, cx));
        self.chat_workspace_snapshot = self.chat_workspace.read(cx).snapshot().clone();
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
        self.chat_workspace.update(cx, |workspace, cx| {
            workspace.select_session(session_id, window, cx)
        });
        self.chat_workspace_snapshot = self.chat_workspace.read(cx).snapshot().clone();
    }

    #[cfg(test)]
    fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chat_workspace.update(cx, |workspace, cx| {
            workspace.delete_conversation(target, window, cx)
        });
        self.chat_workspace_snapshot = self.chat_workspace.read(cx).snapshot().clone();
    }

    /// Open the native folder picker and register the chosen folder as the
    /// active Agent work project.
    fn open_project_folder(&mut self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("agent.open_folder_prompt").to_string().into()),
        });
        let task = cx.spawn(async move |this, cx| {
            let picked = match prompt.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = picked else {
                return;
            };
            // The dialog returns the displayed path; canonicalize when the
            // entry still resolves so identity survives symlinked mounts.
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            this.update(cx, |state, cx| {
                state.register_agent_project(canonical, cx);
            })
            .ok();
        });
        self._folder_task = Some(task);
    }

    /// Adopt `canonical` as the active project: reuse an existing project
    /// identity for the same path (store row or persisted record), otherwise
    /// mint one and persist it so the folder survives restarts.
    fn register_agent_project(&mut self, canonical: std::path::PathBuf, cx: &mut Context<Self>) {
        let existing_id = self
            .agent
            .projects()
            .iter()
            .find(|project| project.canonical_path == canonical)
            .map(|project| project.project_id.clone())
            .or_else(|| {
                self.preference_handle
                    .snapshot()
                    .agent_projects
                    .into_iter()
                    .find(|record| record.canonical_path == canonical)
                    .map(|record| record.project_id)
            });

        let project_id = existing_id.unwrap_or_else(|| {
            let display_name = canonical
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
            let identity =
                crate::session::ProjectIdentity::new(canonical.clone(), display_name.clone());
            preferences::update_with(cx, &self.preference_handle, |prefs| {
                prefs
                    .agent_projects
                    .push(crate::preferences::AgentProjectRecord {
                        project_id: identity.project_id.clone(),
                        canonical_path: canonical.clone(),
                        display_name,
                    });
            });
            identity.project_id
        });

        self.agent.expand_project(project_id.clone());
        self.start_agent_sessions_load(project_id, cx);
        cx.notify();
    }

    pub(crate) fn request_delete_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.workspace_mode, WorkspaceMode::Project) {
            let Some(entity) = self.agent_active else {
                return;
            };
            let Some((project_id, session_id)) = self
                .agent_conversations
                .iter()
                .find(|conversation| conversation.view.entity_id() == entity)
                .map(|conversation| {
                    (
                        conversation.project_id.clone(),
                        conversation.session_id.clone(),
                    )
                })
            else {
                return;
            };
            let target = session_id.map_or_else(
                || SidebarTarget::AgentView(entity),
                |session_id| SidebarTarget::AgentSession {
                    project_id: project_id.clone(),
                    session_id,
                },
            );
            if self.collapsed {
                self.collapsed = false;
                self.has_toggled = true;
            }
            self.agent.expand_project(project_id);
            self.begin_delete_confirmation(target, window, cx);
            return;
        }
        let Some(target) = self.chat_workspace_snapshot.active() else {
            return;
        };
        if !self
            .chat_workspace_snapshot
            .conversations()
            .iter()
            .any(|conversation| conversation.target() == target)
        {
            return;
        }
        if self.collapsed {
            self.collapsed = false;
            self.has_toggled = true;
        }
        self.chat_workspace.update(cx, |workspace, cx| {
            workspace.begin_delete_confirmation(SidebarTarget::View(target), window, cx)
        });
    }

    fn delete_agent_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.agent_conversation_index(target) else {
            return;
        };
        let project_id = self.agent_conversations[index].project_id.clone();
        let session_id = self.agent_conversations[index].session_id.clone();
        if session_id.is_none() {
            self.remove_agent_conversation(target, project_id, None, window, cx);
            return;
        }
        let request = self.agent_conversations[index]
            .view
            .update(cx, |chat, cx| chat.request_delete(cx));
        if request == ChatDeleteRequest::RemoveNow {
            self.remove_agent_conversation(target, project_id, session_id, window, cx);
        }
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
            SidebarTarget::View(entity) => self.chat_workspace.update(cx, |workspace, cx| {
                workspace.delete_conversation(entity, window, cx)
            }),
            SidebarTarget::Session(session_id) => {
                self.chat_workspace.update(cx, |workspace, cx| {
                    workspace.confirm_delete_target(SidebarTarget::Session(session_id), window, cx)
                });
            }
            SidebarTarget::AgentView(entity) => self.delete_agent_conversation(entity, window, cx),
            SidebarTarget::AgentSession {
                project_id,
                session_id,
            } => {
                if let Some(entity) = self
                    .agent_conversations
                    .iter()
                    .find(|conversation| conversation.session_id.as_ref() == Some(&session_id))
                    .map(|conversation| conversation.view.entity_id())
                {
                    self.delete_agent_conversation(entity, window, cx);
                } else {
                    self.delete_unopened_agent_session(project_id, session_id, window, cx);
                }
            }
            SidebarTarget::AgentProject(project_id) => {
                if let Some(project) = self
                    .merged_agent_projects(cx)
                    .into_iter()
                    .find(|project| project.project_id == project_id)
                {
                    self.delete_agent_project(project, window, cx);
                }
            }
        }
    }

    fn active_view(&self) -> Option<Entity<ChatView>> {
        self.chat_workspace_snapshot.active_view()
    }

    fn active_agent_view(&self) -> Option<Entity<ChatView>> {
        self.agent_active.and_then(|target| {
            self.agent_conversations
                .iter()
                .find(|conversation| conversation.view.entity_id() == target)
                .map(|conversation| conversation.view.clone())
        })
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
