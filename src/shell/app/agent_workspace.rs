//! Project catalog state, background loading, and rendering.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Pixels, Role as AriaRole, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::contrast;

use super::{
    ChatApp,
    history_sidebar::{SidebarActionIds, SidebarActionSpec, render_sidebar_actions},
    project_workspace::{ProjectTarget, ProjectWorkspace},
    workspace_host::WorkspaceCommand,
};
use crate::runtime::PROJECT_WORKSPACE_ID;
use crate::session::{
    CatalogCursor, CatalogError, CatalogPage, CatalogQuery, ProjectCatalogCursor,
    ProjectCatalogPage, ProjectCatalogQuery, ProjectSessionStore, ProjectSummary, SessionId,
    SessionSummary,
};

const AGENT_ROW_HEIGHT: Pixels = px(32.);
/// Column width cap for the Agent conversation area, matching the chat
/// transcript's content column.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AgentLoadState {
    Unloaded,
    Loading,
    Ready,
    Error(SharedString),
}

/// What the Agent main pane is showing. `None` means the empty work state
/// (projects exist, no composer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AgentOpen {
    Draft {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: SessionId,
    },
}

/// Per-project session list snapshot, loaded lazily on first expand.
#[derive(Clone, Debug)]
pub(super) struct ProjectSessionList {
    sessions: Vec<SessionSummary>,
    load_state: AgentLoadState,
    next_cursor: Option<CatalogCursor>,
    load_more_in_flight: bool,
    generation: u64,
}

impl ProjectSessionList {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            load_state: AgentLoadState::Unloaded,
            next_cursor: None,
            load_more_in_flight: false,
            generation: 0,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }
}

/// UI-independent snapshot of the Agent workspace.
#[derive(Clone)]
pub(super) struct AgentWorkspace {
    projects: Vec<ProjectSummary>,
    projects_load_state: AgentLoadState,
    projects_next_cursor: Option<ProjectCatalogCursor>,
    projects_load_more_in_flight: bool,
    projects_generation: u64,

    expanded_project_ids: HashSet<String>,
    sessions_by_project: HashMap<String, ProjectSessionList>,
    open: Option<AgentOpen>,
}

impl AgentWorkspace {
    pub(super) fn new() -> Self {
        Self {
            projects: Vec::new(),
            projects_load_state: AgentLoadState::Unloaded,
            projects_next_cursor: None,
            projects_load_more_in_flight: false,
            projects_generation: 0,
            expanded_project_ids: HashSet::new(),
            sessions_by_project: HashMap::new(),
            open: None,
        }
    }

    pub(super) fn projects(&self) -> &[ProjectSummary] {
        &self.projects
    }

    pub(super) fn projects_has_more(&self) -> bool {
        self.projects_next_cursor.is_some()
    }

    pub(super) fn projects_load_more_in_flight(&self) -> bool {
        self.projects_load_more_in_flight
    }

    pub(super) fn open(&self) -> Option<&AgentOpen> {
        self.open.as_ref()
    }

    pub(super) fn selected_session_id(&self) -> Option<&SessionId> {
        match self.open.as_ref() {
            Some(AgentOpen::Session { session_id, .. }) => Some(session_id),
            _ => None,
        }
    }

    pub(super) fn open_project_id(&self) -> Option<&str> {
        match self.open.as_ref() {
            Some(AgentOpen::Draft { project_id }) => Some(project_id.as_str()),
            Some(AgentOpen::Session { project_id, .. }) => Some(project_id.as_str()),
            None => None,
        }
    }

    pub(super) fn is_project_expanded(&self, project_id: &str) -> bool {
        self.expanded_project_ids.contains(project_id)
    }

    pub(super) fn session_list(&self, project_id: &str) -> Option<&ProjectSessionList> {
        self.sessions_by_project.get(project_id)
    }

    pub(super) fn sessions_need_load(&self, project_id: &str) -> bool {
        self.sessions_by_project.get(project_id).is_none_or(|list| {
            matches!(
                list.load_state,
                AgentLoadState::Unloaded | AgentLoadState::Error(_)
            )
        })
    }

    fn session_list_mut(&mut self, project_id: &str) -> &mut ProjectSessionList {
        self.sessions_by_project
            .entry(project_id.to_string())
            .or_insert_with(ProjectSessionList::new)
    }

    fn next_projects_generation(&mut self) -> u64 {
        self.projects_generation = self.projects_generation.wrapping_add(1);
        self.projects_generation
    }

    fn apply_projects_initial(&mut self, generation: u64, page: ProjectCatalogPage) -> bool {
        if generation != self.projects_generation {
            return false;
        }
        self.projects = dedup_projects(page.projects);
        self.projects_next_cursor = page.next_cursor;
        self.projects_load_state = AgentLoadState::Ready;
        true
    }

    fn apply_projects_load_more(&mut self, generation: u64, page: ProjectCatalogPage) -> bool {
        if generation != self.projects_generation {
            return false;
        }
        let existing: HashSet<String> =
            self.projects.iter().map(|p| p.project_id.clone()).collect();
        for project in dedup_projects(page.projects) {
            if !existing.contains(&project.project_id) {
                self.projects.push(project);
            }
        }
        self.projects_next_cursor = page.next_cursor;
        true
    }

    fn mark_projects_error(&mut self, generation: u64, message: SharedString) -> bool {
        if generation != self.projects_generation {
            return false;
        }
        self.projects_load_state = AgentLoadState::Error(message);
        true
    }

    fn apply_sessions_initial(
        &mut self,
        project_id: &str,
        generation: u64,
        page: CatalogPage,
    ) -> bool {
        let Some(list) = self.sessions_by_project.get_mut(project_id) else {
            return false;
        };
        if generation != list.generation {
            return false;
        }
        list.sessions = dedup_session_summaries(page.sessions);
        list.next_cursor = page.next_cursor;
        list.load_state = AgentLoadState::Ready;
        true
    }

    fn apply_sessions_load_more(
        &mut self,
        project_id: &str,
        generation: u64,
        page: CatalogPage,
    ) -> bool {
        let Some(list) = self.sessions_by_project.get_mut(project_id) else {
            return false;
        };
        if generation != list.generation {
            return false;
        }
        let existing: HashSet<SessionId> =
            list.sessions.iter().map(|s| s.session_id.clone()).collect();
        for summary in dedup_session_summaries(page.sessions) {
            if !existing.contains(&summary.session_id) {
                list.sessions.push(summary);
            }
        }
        list.next_cursor = page.next_cursor;
        true
    }

    fn mark_sessions_error(
        &mut self,
        project_id: &str,
        generation: u64,
        message: SharedString,
    ) -> bool {
        let Some(list) = self.sessions_by_project.get_mut(project_id) else {
            return false;
        };
        if generation != list.generation {
            return false;
        }
        list.load_state = AgentLoadState::Error(message);
        true
    }

    /// Expand a project without changing the open conversation.
    pub(super) fn expand_project(&mut self, project_id: String) {
        self.expanded_project_ids.insert(project_id);
    }

    /// Toggle a project's expanded state. Returns whether it is now expanded.
    pub(super) fn toggle_project_expanded(&mut self, project_id: String) -> bool {
        if self.expanded_project_ids.contains(&project_id) {
            self.expanded_project_ids.remove(&project_id);
            false
        } else {
            self.expanded_project_ids.insert(project_id);
            true
        }
    }

    /// Open a session in the main pane without leaving the grouped tree.
    pub(super) fn select_session(&mut self, project_id: String, session_id: SessionId) {
        self.open = Some(AgentOpen::Session {
            project_id,
            session_id,
        });
    }

    /// Start a fresh conversation draft under `project_id`, expand that
    /// project, and open the composer. No session id exists until a future
    /// send runtime persists the first turn.
    pub(super) fn new_project_draft(&mut self, project_id: String) {
        self.expanded_project_ids.insert(project_id.clone());
        self.open = Some(AgentOpen::Draft { project_id });
    }

    pub(super) fn open_draft(&mut self, project_id: String) {
        self.new_project_draft(project_id);
    }

    pub(super) fn bind_draft_session(&mut self, project_id: String, session_id: SessionId) {
        self.open = Some(AgentOpen::Session {
            project_id,
            session_id,
        });
    }

    pub(super) fn discard_draft(&mut self, project_id: &str) {
        if matches!(self.open, Some(AgentOpen::Draft { project_id: ref id }) if id == project_id) {
            self.open = None;
        }
    }

    pub(super) fn remove_session(&mut self, project_id: &str, session_id: &SessionId) {
        if let Some(list) = self.sessions_by_project.get_mut(project_id) {
            list.sessions
                .retain(|session| &session.session_id != session_id);
        }
        if matches!(self.open, Some(AgentOpen::Session { project_id: ref pid, session_id: ref sid }) if pid == project_id && sid == session_id)
        {
            self.open = None;
        }
    }

    pub(super) fn remove_project(&mut self, project_id: &str) {
        self.projects
            .retain(|project| project.project_id != project_id);
        self.sessions_by_project.remove(project_id);
        self.expanded_project_ids.remove(project_id);
        if self.open_project_id() == Some(project_id) {
            self.open = None;
        }
    }
}

/// Merge persisted (folder-opened, still session-less) projects into the
/// store catalog snapshot.  A persisted record is skipped once any merged
/// project — store or earlier record — already owns its id or its canonical
/// path; the store row stays authoritative.
pub(super) fn merge_persisted_projects(
    store_projects: &[ProjectSummary],
    persisted: &[crate::preferences::AgentProjectRecord],
) -> Vec<ProjectSummary> {
    let mut merged = store_projects.to_vec();
    for record in persisted {
        let covered = merged.iter().any(|project| {
            project.project_id == record.project_id
                || project.canonical_path == record.canonical_path
        });
        if !covered {
            merged.push(ProjectSummary {
                project_id: record.project_id.clone(),
                display_name: record.display_name.clone(),
                canonical_path: record.canonical_path.clone(),
                session_count: 0,
                last_updated_at: 0,
            });
        }
    }
    merged
}

fn dedup_projects(projects: Vec<ProjectSummary>) -> Vec<ProjectSummary> {
    let mut seen = HashSet::with_capacity(projects.len());
    projects
        .into_iter()
        .filter(|p| seen.insert(p.project_id.clone()))
        .collect()
}

fn dedup_session_summaries(sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
    let mut seen = HashSet::with_capacity(sessions.len());
    sessions
        .into_iter()
        .filter(|s| seen.insert(s.session_id.clone()))
        .collect()
}

/// Compact relative time for sidebar session rows (hours/days).
fn format_sidebar_relative_time(now_millis: i64, timestamp_millis: i64) -> String {
    let delta = now_millis.saturating_sub(timestamp_millis).max(0);
    let hours = delta / 3_600_000;
    if hours < 1 {
        t!("agent.time_just_now").to_string()
    } else if hours < 24 {
        t!("agent.time_hours_ago", n = hours).to_string()
    } else {
        t!("agent.time_days_ago", n = hours / 24).to_string()
    }
}

impl ProjectWorkspace {
    // ---------- Background project catalog loading ----------

    pub(super) fn start_agent_projects_load(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            self.catalog.projects_load_state,
            AgentLoadState::Unloaded | AgentLoadState::Error(_)
        ) {
            return;
        }
        let stores = self.runtime_services.session_services().clone();
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(error) => {
                self.catalog.projects_load_state = AgentLoadState::Error(error.to_string().into());
                self.notify_changed(cx);
                return;
            }
        };

        let generation = self.catalog.next_projects_generation();
        self.catalog.projects_load_state = AgentLoadState::Loading;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { project_store.list_projects(ProjectCatalogQuery::first_page()) },
                )
                .await;
            this.update(cx, |state, cx| {
                state.apply_agent_projects_initial(generation, result, cx);
            })
            .ok();
        });
        self._projects_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_agent_projects_initial(
        &mut self,
        generation: u64,
        result: Result<ProjectCatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(page) => {
                self.catalog.apply_projects_initial(generation, page);
            }
            Err(error) => {
                let message = error.to_string().into();
                self.catalog.mark_projects_error(generation, message);
            }
        }
        self._projects_task = None;
        self.notify_changed(cx);
    }

    pub(super) fn start_agent_projects_load_more(&mut self, cx: &mut Context<Self>) {
        if self.catalog.projects_load_more_in_flight() || !self.catalog.projects_has_more() {
            return;
        }
        let Some(cursor) = self.catalog.projects_next_cursor.clone() else {
            return;
        };
        let stores = self.runtime_services.session_services().clone();
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(_) => return,
        };

        self.catalog.projects_load_more_in_flight = true;
        let generation = self.catalog.projects_generation;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_store.list_projects(ProjectCatalogQuery {
                        cursor: Some(cursor),
                        limit: ProjectCatalogQuery::first_page().limit,
                    })
                })
                .await;
            this.update(cx, |state, cx| {
                state.apply_agent_projects_load_more(generation, result, cx);
            })
            .ok();
        });
        self._projects_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_agent_projects_load_more(
        &mut self,
        generation: u64,
        result: Result<ProjectCatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        if let Ok(page) = result {
            self.catalog.apply_projects_load_more(generation, page);
        }
        self.catalog.projects_load_more_in_flight = false;
        self._projects_task = None;
        self.notify_changed(cx);
    }

    // ---------- Background session list loading ----------

    pub(super) fn refresh_agent_sessions(&mut self, project_id: String, cx: &mut Context<Self>) {
        self._session_list_tasks.remove(&project_id);
        self.catalog.sessions_by_project.remove(&project_id);
        self.start_agent_sessions_load(project_id, cx);
    }

    pub(super) fn start_agent_sessions_load(&mut self, project_id: String, cx: &mut Context<Self>) {
        if !self.catalog.sessions_need_load(&project_id) {
            return;
        }
        let stores = self.runtime_services.session_services().clone();
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(error) => {
                self.catalog.session_list_mut(&project_id).load_state =
                    AgentLoadState::Error(error.to_string().into());
                self.notify_changed(cx);
                return;
            }
        };
        let generation = {
            let list = self.catalog.session_list_mut(&project_id);
            let generation = list.next_generation();
            list.load_state = AgentLoadState::Loading;
            generation
        };
        let load_project_id = project_id.clone();
        let apply_project_id = project_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_store
                        .list_project_sessions(&load_project_id, CatalogQuery::first_page())
                })
                .await;
            this.update(cx, |state, cx| {
                state.apply_agent_sessions_initial(&apply_project_id, generation, result, cx);
            })
            .ok();
        });
        self._session_list_tasks.insert(project_id, task);
        self.notify_changed(cx);
    }

    fn apply_agent_sessions_initial(
        &mut self,
        project_id: &str,
        generation: u64,
        result: Result<CatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(page) => {
                self.catalog
                    .apply_sessions_initial(project_id, generation, page);
            }
            Err(error) => {
                let message = error.to_string().into();
                self.catalog
                    .mark_sessions_error(project_id, generation, message);
            }
        }
        self._session_list_tasks.remove(project_id);
        self.notify_changed(cx);
    }

    pub(super) fn start_agent_sessions_load_more(
        &mut self,
        project_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = self.catalog.sessions_by_project.get(&project_id) else {
            return;
        };
        if list.load_more_in_flight || list.next_cursor.is_none() {
            return;
        }
        let Some(cursor) = list.next_cursor.clone() else {
            return;
        };
        let generation = list.generation;
        let stores = self.runtime_services.session_services().clone();
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(_) => return,
        };

        if let Some(list) = self.catalog.sessions_by_project.get_mut(&project_id) {
            list.load_more_in_flight = true;
        }
        let load_project_id = project_id.clone();
        let apply_project_id = project_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_store.list_project_sessions(
                        &load_project_id,
                        CatalogQuery {
                            cursor: Some(cursor),
                            ..CatalogQuery::first_page()
                        },
                    )
                })
                .await;
            this.update(cx, |state, cx| {
                state.apply_agent_sessions_load_more(&apply_project_id, generation, result, cx);
            })
            .ok();
        });
        self._session_list_tasks.insert(project_id, task);
        self.notify_changed(cx);
    }

    fn apply_agent_sessions_load_more(
        &mut self,
        project_id: &str,
        generation: u64,
        result: Result<CatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        if let Ok(page) = result {
            self.catalog
                .apply_sessions_load_more(project_id, generation, page);
        }
        if let Some(list) = self.catalog.sessions_by_project.get_mut(project_id) {
            list.load_more_in_flight = false;
        }
        self._session_list_tasks.remove(project_id);
        self.notify_changed(cx);
    }
}

impl ChatApp {
    // ---------- Agent sidebar rendering ----------

    pub(super) fn render_agent_content(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut children = vec![self.render_agent_projects_header(cx)];
        children.extend(self.render_agent_project_tree(window, cx));

        v_flex()
            .id("agent-projects")
            .debug_selector(|| "sidebar-list-surface".to_string())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_0p5()
            .children(children)
            .into_any_element()
    }

    fn render_agent_projects_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = contrast::sidebar_muted_text(cx, 0.6);
        let app = cx.entity().downgrade();
        h_flex()
            .id("agent-projects-header")
            .h(px(28.))
            .items_center()
            .pl_2()
            .pr_1()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .font_medium()
                    .text_color(muted)
                    .child(t!("agent.projects").to_string()),
            )
            .child(
                Button::new("agent-add-project")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Plus)
                    .tooltip(t!("agent.open_folder").to_string())
                    .on_click(move |_, _, cx| {
                        app.update(cx, |app, cx| {
                            app.dispatch_workspace_command(
                                PROJECT_WORKSPACE_ID,
                                WorkspaceCommand::OpenProjectFolder,
                                None,
                                cx,
                            );
                        })
                        .ok();
                    }),
            )
            .into_any_element()
    }

    fn render_agent_project_tree(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut children: Vec<AnyElement> = Vec::new();
        let catalog = self.project_snapshot().catalog();
        let projects = self.project_snapshot().projects();

        let loading_and_empty =
            matches!(catalog.projects_load_state, AgentLoadState::Loading) && projects.is_empty();
        if loading_and_empty {
            children.push(
                self.render_agent_loading_state(false, cx)
                    .into_any_element(),
            );
        }

        for project in projects {
            children.push(self.render_agent_project_block(project, window, cx));
        }

        let ready = matches!(catalog.projects_load_state, AgentLoadState::Ready);
        let error_and_empty =
            matches!(catalog.projects_load_state, AgentLoadState::Error(_)) && projects.is_empty();
        if error_and_empty {
            children.push(
                self.render_agent_projects_error_state(cx)
                    .into_any_element(),
            );
        } else if ready && projects.is_empty() {
            children.push(
                self.render_agent_projects_empty_state(cx)
                    .into_any_element(),
            );
        } else if catalog.projects_has_more() {
            children.push(
                self.render_agent_projects_load_more_row(window, cx)
                    .into_any_element(),
            );
        }
        children
    }

    fn render_agent_project_block(
        &self,
        project: &ProjectSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_id = project.project_id.clone();
        let catalog = self.project_snapshot().catalog();
        let expanded = catalog.is_project_expanded(&project_id);
        let list = catalog.session_list(&project_id);
        let draft_here = self
            .project_snapshot()
            .draft_for_project(&project_id)
            .is_some();

        let mut block = v_flex().w_full().gap_0p5();
        block = block.child(self.render_agent_project_row(project, expanded, window, cx));

        if expanded {
            if let Some(list) = list {
                if matches!(list.load_state, AgentLoadState::Loading) && list.sessions.is_empty() {
                    block = block.child(self.render_agent_loading_state(true, cx));
                }
                if draft_here {
                    block = block.child(self.render_agent_draft_row(&project_id, window, cx));
                }
                let selected = catalog.selected_session_id();
                for summary in &list.sessions {
                    block = block.child(self.render_agent_session_row(
                        &project_id,
                        summary,
                        selected,
                        window,
                        cx,
                    ));
                }
                if matches!(list.load_state, AgentLoadState::Error(_)) && list.sessions.is_empty() {
                    block = block.child(self.render_agent_sessions_error_state(&project_id, cx));
                } else if matches!(list.load_state, AgentLoadState::Ready)
                    && list.sessions.is_empty()
                    && !draft_here
                {
                    block = block.child(self.render_agent_project_empty_row(cx));
                } else if list.next_cursor.is_some() {
                    block =
                        block.child(self.render_agent_sessions_show_more(&project_id, window, cx));
                }
            } else if draft_here {
                block = block.child(self.render_agent_draft_row(&project_id, window, cx));
            }
        }

        block.into_any_element()
    }

    fn render_agent_project_row(
        &self,
        project: &ProjectSummary,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_id = project.project_id.clone();
        let display_name = project.display_name.clone();
        let chevron = if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        let is_open_project = self.project_snapshot().catalog().open_project_id()
            == Some(project.project_id.as_str());
        let toggle_id = format!("agent-project-{project_id}");
        let hover_group: SharedString = format!("agent-project-hover-{project_id}").into();
        let focus_handle = window
            .use_keyed_state(toggle_id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let name_for_key = project_id.clone();
        let target = ProjectTarget::Project(project_id.clone());
        let is_confirming = self.project_snapshot().confirming() == Some(&target);
        let workspace = self.project_workspace().downgrade();
        let workspace_for_key = workspace.clone();
        let app = cx.entity().downgrade();
        let tints = contrast::sidebar_row_tints(cx);

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(toggle_id)
                    .group(hover_group.clone())
                    .h(AGENT_ROW_HEIGHT)
                    .items_center()
                    .gap_1()
                    .px_1()
                    .rounded_md()
                    .when(is_open_project, |this| {
                        this.bg(tints.selected).text_color(tints.selected_text)
                    })
                    .when(!is_open_project, |this| {
                        this.hover(|this| this.bg(tints.hover).text_color(tints.hover_text))
                    })
                    .child(
                        div()
                            .id(format!("agent-project-name-{project_id}"))
                            .role(AriaRole::Button)
                            .aria_label(display_name.clone())
                            .track_focus(&focus_handle.tab_stop(true))
                            .focus_visible(|this| this.border_1().border_color(focus_ring))
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_1()
                            .cursor_default()
                            .on_click({
                                let project_id = project_id.clone();
                                move |_: &ClickEvent, _, cx| {
                                    workspace
                                        .update(cx, |workspace, cx| {
                                            workspace.toggle_project(project_id.clone(), cx)
                                        })
                                        .ok();
                                }
                            })
                            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                                if crate::ui::consume_button_key(event, window, cx) {
                                    workspace_for_key
                                        .update(cx, |workspace, cx| {
                                            workspace.toggle_project(name_for_key.clone(), cx)
                                        })
                                        .ok();
                                }
                            })
                            .child(
                                Icon::new(chevron)
                                    .size_3p5()
                                    .text_color(contrast::sidebar_muted_text(cx, 0.6)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(display_name),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .when(!is_confirming, |this| this.invisible())
                            .group_hover(hover_group, |this| this.visible())
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_0p5()
                                    .child(
                                        Button::new(format!("agent-project-new-{project_id}"))
                                            .ghost()
                                            .small()
                                            .compact()
                                            .icon(IconName::Plus)
                                            .tooltip(t!("agent.new_in_project").to_string())
                                            .on_click({
                                                let app = app.clone();
                                                let project_id = project_id.clone();
                                                move |_, window, cx| {
                                                    app.update(cx, |app, cx| {
                                                        app.dispatch_workspace_command(
                                                            PROJECT_WORKSPACE_ID,
                                                            WorkspaceCommand::OpenProjectDraft(
                                                                project_id.clone(),
                                                            ),
                                                            Some(window),
                                                            cx,
                                                        );
                                                    })
                                                    .ok();
                                                }
                                            }),
                                    )
                                    .child(self.render_project_sidebar_actions(
                                        target,
                                        true,
                                        is_confirming,
                                        cx,
                                    )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_agent_draft_row(
        &self,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title: SharedString = t!("agent.new_draft").to_string().into();
        let is_selected = matches!(
            self.project_snapshot().catalog().open(),
            Some(AgentOpen::Draft { project_id: open_id }) if open_id == project_id
        );
        let app = cx.entity().downgrade();
        let project_id = project_id.to_string();
        let target = self
            .project_snapshot()
            .draft_for_project(&project_id)
            .map(|conversation| ProjectTarget::Conversation(conversation.id()));
        self.render_indented_session_row(
            format!("agent-draft-{project_id}"),
            title,
            None,
            is_selected,
            window,
            cx,
            move |window, cx| {
                app.update(cx, |app, cx| {
                    app.dispatch_workspace_command(
                        PROJECT_WORKSPACE_ID,
                        WorkspaceCommand::OpenProjectDraft(project_id.clone()),
                        Some(window),
                        cx,
                    );
                })
                .ok();
            },
            target,
        )
    }

    fn render_agent_session_row(
        &self,
        project_id: &str,
        summary: &SessionSummary,
        selected: Option<&SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session_id = summary.session_id.clone();
        let title: SharedString = summary
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| t!("agent.untitled_session").to_string())
            .into();
        let is_selected = selected == Some(&summary.session_id);
        let relative = format_sidebar_relative_time(
            chrono::Local::now().timestamp_millis(),
            summary.updated_at,
        );
        let project_id = project_id.to_string();
        let app = cx.entity().downgrade();
        let target = ProjectTarget::Session {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        };
        self.render_indented_session_row(
            format!("agent-session-{session_id}"),
            title,
            Some(relative),
            is_selected,
            window,
            cx,
            move |window, cx| {
                app.update(cx, |app, cx| {
                    app.dispatch_workspace_command(
                        PROJECT_WORKSPACE_ID,
                        WorkspaceCommand::RestoreProjectSession {
                            project_id: project_id.clone(),
                            session_id: session_id.clone(),
                        },
                        Some(window),
                        cx,
                    );
                })
                .ok();
            },
            Some(target),
        )
    }

    fn render_project_sidebar_actions(
        &self,
        target: ProjectTarget,
        visible: bool,
        confirming: bool,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let ids = match &target {
            ProjectTarget::Conversation(entity) => SidebarActionIds {
                trigger_id: ("agent-conversation-actions", entity.as_u64()).into(),
                confirm_id: ("agent-conversation-delete-confirm", entity.as_u64()).into(),
                trigger_debug_selector: format!("agent-conversation-actions-{}", entity.as_u64()),
                delete_label: t!("agent.delete_session").to_string(),
                confirm_title: t!("agent.delete_session_title").to_string(),
            },
            ProjectTarget::Session {
                project_id,
                session_id,
            } => SidebarActionIds {
                trigger_id: format!("agent-session-actions-{project_id}-{session_id}").into(),
                confirm_id: format!("agent-session-delete-confirm-{project_id}-{session_id}")
                    .into(),
                trigger_debug_selector: format!("agent-session-actions-{project_id}-{session_id}"),
                delete_label: t!("agent.delete_session").to_string(),
                confirm_title: t!("agent.delete_session_title").to_string(),
            },
            ProjectTarget::Project(project_id) => SidebarActionIds {
                trigger_id: format!("agent-project-actions-{project_id}").into(),
                confirm_id: format!("agent-project-delete-confirm-{project_id}").into(),
                trigger_debug_selector: format!("agent-project-actions-{project_id}"),
                delete_label: t!("agent.delete_project").to_string(),
                confirm_title: t!("agent.delete_project_title").to_string(),
            },
        };
        let workspace = self.project_workspace().downgrade();
        render_sidebar_actions(
            SidebarActionSpec {
                target,
                visible,
                confirming,
                ids,
                handle: self.project_snapshot().delete_confirmation(),
            },
            {
                let workspace = workspace.clone();
                move |target, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.clear_delete_confirmation(&target, window, cx)
                        })
                        .ok();
                }
            },
            {
                let workspace = workspace.clone();
                move |target, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.confirm_delete_target(target, window, cx)
                        })
                        .ok();
                }
            },
            move |target, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.begin_delete_confirmation(target, window, cx)
                    })
                    .ok();
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_indented_session_row(
        &self,
        row_id: String,
        title: SharedString,
        relative: Option<String>,
        is_selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
        target: Option<ProjectTarget>,
    ) -> AnyElement {
        let focus_handle = window
            .use_keyed_state(row_id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let on_activate = Rc::new(on_activate);
        let hover_group: SharedString = format!("{row_id}-hover").into();
        let is_confirming = target
            .as_ref()
            .is_some_and(|target| self.project_snapshot().confirming() == Some(target));
        let tints = contrast::sidebar_row_tints(cx);

        div()
            .id(row_id)
            .group(hover_group.clone())
            .role(AriaRole::Button)
            .aria_label(title.clone())
            .aria_selected(is_selected)
            .track_focus(&focus_handle.tab_stop(true))
            .focus_visible(|this| this.border_1().border_color(focus_ring))
            .h(AGENT_ROW_HEIGHT)
            .ml_4()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .rounded_md()
            .cursor_default()
            .when(is_selected, |this| {
                this.bg(tints.selected).text_color(tints.selected_text)
            })
            .when(!is_selected, |this| {
                this.hover(|this| this.bg(tints.hover).text_color(tints.hover_text))
            })
            .on_click({
                let on_activate = on_activate.clone();
                move |_, window, cx| on_activate(window, cx)
            })
            .on_key_down({
                let on_activate = on_activate.clone();
                move |event: &KeyDownEvent, window, cx| {
                    if crate::ui::consume_button_key(event, window, cx) {
                        on_activate(window, cx);
                    }
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(title.clone()),
            )
            .when_some(relative, |this, time| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(contrast::sidebar_muted_text(cx, 0.5))
                        .child(time),
                )
            })
            .child(
                div()
                    .flex_none()
                    .when(!is_selected && !is_confirming, |this| this.invisible())
                    .group_hover(hover_group, |this| this.visible())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when_some(target, |this, target| {
                        this.child(self.render_project_sidebar_actions(
                            target,
                            true,
                            is_confirming,
                            cx,
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_agent_loading_state(
        &self,
        indented: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .when(indented, |this| this.ml_4())
            .px_2()
            .h(AGENT_ROW_HEIGHT)
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .child(Spinner::new().small())
            .child(t!("agent.loading").to_string())
    }

    fn render_agent_projects_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_2()
            .py_3()
            .gap_1()
            .text_sm()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .child(div().child(t!("agent.no_projects").to_string()))
            .child(
                div()
                    .text_xs()
                    .child(t!("agent.no_projects_hint").to_string()),
            )
    }

    fn render_agent_project_empty_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .ml_4()
            .px_2()
            .py_2()
            .text_xs()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .child(t!("agent.no_sessions").to_string())
    }

    fn render_agent_projects_error_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.project_workspace().downgrade();
        v_flex()
            .px_2()
            .py_3()
            .gap_2()
            .text_sm()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .child(div().child(t!("agent.load_failed").to_string()))
            .child(
                Button::new("agent-retry-projects")
                    .ghost()
                    .small()
                    .label(t!("agent.retry").to_string())
                    .on_click(move |_, _, cx| {
                        workspace
                            .update(cx, |workspace, cx| workspace.start_agent_projects_load(cx))
                            .ok();
                    }),
            )
    }

    fn render_agent_sessions_error_state(
        &self,
        project_id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let project_id = project_id.to_string();
        let workspace = self.project_workspace().downgrade();
        v_flex()
            .ml_4()
            .px_2()
            .py_2()
            .gap_1()
            .text_xs()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .child(div().child(t!("agent.load_failed").to_string()))
            .child(
                Button::new(format!("agent-retry-sessions-{project_id}"))
                    .ghost()
                    .small()
                    .label(t!("agent.retry").to_string())
                    .on_click(move |_, _, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.start_agent_sessions_load(project_id.clone(), cx)
                            })
                            .ok();
                    }),
            )
    }

    fn render_agent_projects_load_more_row(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = t!("agent.load_more").to_string();
        let workspace = self.project_workspace().downgrade();
        self.render_sidebar_action_row(
            "agent-load-more-projects",
            label,
            false,
            window,
            cx,
            move |cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.start_agent_projects_load_more(cx)
                    })
                    .ok();
            },
        )
    }

    fn render_agent_sessions_show_more(
        &self,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let project_id = project_id.to_string();
        let label = t!("agent.show_more").to_string();
        let workspace = self.project_workspace().downgrade();
        self.render_sidebar_action_row(
            format!("agent-show-more-{project_id}"),
            label,
            true,
            window,
            cx,
            move |cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.start_agent_sessions_load_more(project_id.clone(), cx)
                    })
                    .ok();
            },
        )
    }

    fn render_sidebar_action_row(
        &self,
        row_id: impl Into<SharedString>,
        label: String,
        indented: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_activate: impl Fn(&mut App) + 'static,
    ) -> impl IntoElement {
        let row_id = row_id.into();
        let focus_handle = window
            .use_keyed_state(row_id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let on_activate = Rc::new(on_activate);
        div()
            .id(row_id)
            .role(AriaRole::Button)
            .aria_label(label.clone())
            .track_focus(&focus_handle.tab_stop(true))
            .focus_visible(|this| this.border_1().border_color(focus_ring))
            .h(AGENT_ROW_HEIGHT)
            .when(indented, |this| this.ml_4())
            .flex()
            .items_center()
            .px_2()
            .text_xs()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .cursor_default()
            .hover(|this| this.bg(contrast::sidebar_row_tints(cx).hover))
            .on_click({
                let on_activate = on_activate.clone();
                move |_, _, cx| on_activate(cx)
            })
            .on_key_down({
                let on_activate = on_activate.clone();
                move |event: &KeyDownEvent, window, cx| {
                    if crate::ui::consume_button_key(event, window, cx) {
                        on_activate(cx);
                    }
                }
            })
            .child(label)
    }

    // ---------- Agent main area rendering ----------

    pub(super) fn render_agent_main(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let catalog = self.project_snapshot().catalog();
        let has_projects = !self.project_snapshot().projects().is_empty();
        if !has_projects {
            return self.render_agent_folder_guide(cx).into_any_element();
        }
        if catalog.open().is_none() {
            return self.render_agent_idle_workspace(cx).into_any_element();
        }
        if catalog
            .open_project_id()
            .is_some_and(|project_id| self.project_snapshot().is_deleting_project(project_id))
        {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new())
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("agent.deleting_project").to_string()),
                )
                .into_any_element();
        }

        self.project_snapshot()
            .active_view()
            .map(IntoElement::into_any_element)
            .unwrap_or_else(|| self.render_agent_main_empty(cx).into_any_element())
    }

    fn render_agent_idle_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_2xl()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(t!("agent.idle_title").to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("agent.idle_hint").to_string()),
            )
            .into_any_element()
    }

    /// Full-area guide shown in Agent mode before any folder is opened.
    fn render_agent_folder_guide(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let app = cx.entity().downgrade();
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::FolderOpen)
                    .size_10()
                    .text_color(theme.muted_foreground.opacity(0.5)),
            )
            .child(
                div()
                    .text_xl()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(t!("agent.guide_title").to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("agent.guide_hint").to_string()),
            )
            .child(
                Button::new("agent-guide-open-folder")
                    .primary()
                    .label(t!("agent.open_folder").to_string())
                    .on_click(move |_, _, cx| {
                        app.update(cx, |app, cx| {
                            app.dispatch_workspace_command(
                                PROJECT_WORKSPACE_ID,
                                WorkspaceCommand::OpenProjectFolder,
                                None,
                                cx,
                            );
                        })
                        .ok();
                    }),
            )
            .into_any_element()
    }

    fn render_agent_main_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let loading = matches!(
            self.project_snapshot().session_load_state(),
            AgentLoadState::Loading
        );

        if loading {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new())
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("agent.loading_session").to_string()),
                )
                .into_any_element();
        }

        if matches!(
            self.project_snapshot().session_load_state(),
            AgentLoadState::Error(_)
        ) {
            let workspace = self.project_workspace().downgrade();
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("agent.load_failed").to_string()),
                )
                .child(
                    Button::new("agent-retry-session")
                        .ghost()
                        .small()
                        .label(t!("agent.retry").to_string())
                        .on_click(move |_, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.retry_open_session(window, cx)
                                })
                                .ok();
                        }),
                )
                .into_any_element();
        }

        // A draft (or a session whose transcript has not loaded yet) greets
        // like the chat workspace greets a new chat.
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_2xl()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(t!("agent.welcome_title").to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("agent.welcome_hint").to_string()),
            )
            .into_any_element()
    }

    pub(super) fn switch_workspace(
        &mut self,
        workspace_id: crate::runtime::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_id == workspace_id {
            return;
        }
        if self.workspace_id == crate::runtime::PROJECT_WORKSPACE_ID {
            self.project_workspace()
                .update(cx, |workspace, cx| workspace.dismiss_active_completion(cx));
        }
        self.workspace_id = workspace_id;
        crate::preferences::set_last_workspace_id(workspace_id, cx);
        self.sync_workspace_snapshot(cx);
        if workspace_id == crate::runtime::PROJECT_WORKSPACE_ID
            && matches!(
                self.project_snapshot().catalog().projects_load_state,
                AgentLoadState::Unloaded | AgentLoadState::Error(_)
            )
        {
            self.project_workspace()
                .update(cx, |workspace, cx| workspace.start_agent_projects_load(cx));
        }
        self.sync_model_picker_to_active(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ProjectIdentity, SessionDomain, SessionHeader};

    fn sample_project(id: &str) -> ProjectSummary {
        ProjectSummary {
            project_id: id.to_string(),
            display_name: format!("Project {id}"),
            canonical_path: std::path::PathBuf::from(format!("/tmp/{id}")),
            session_count: 1,
            last_updated_at: 100,
        }
    }

    fn sample_session(project: &ProjectIdentity) -> SessionSummary {
        let header = SessionHeader::new(SessionDomain::Agent, Some(project.clone()));
        SessionSummary {
            session_id: header.session_id,
            domain: SessionDomain::Agent,
            project: Some(project.clone()),
            title: Some("Test session".to_string()),
            preview: None,
            model: None,
            total_tokens: 0,
            created_at: 100,
            updated_at: 100,
            favorited: false,
            jsonl_path: std::path::PathBuf::from("/tmp/session.jsonl"),
        }
    }

    #[test]
    fn expanding_one_project_keeps_another_projects_sessions() {
        let mut workspace = AgentWorkspace::new();
        let project_a = ProjectIdentity::new("/tmp/a", "A");
        let session_a = sample_session(&project_a);
        let generation = workspace.session_list_mut("project-a").next_generation();
        workspace.apply_sessions_initial(
            "project-a",
            generation,
            CatalogPage {
                sessions: vec![session_a],
                next_cursor: None,
            },
        );
        workspace.expand_project("project-a".into());
        workspace.expand_project("project-b".into());

        assert!(workspace.is_project_expanded("project-a"));
        assert!(workspace.is_project_expanded("project-b"));
        assert_eq!(
            workspace
                .session_list("project-a")
                .map(|list| list.sessions.len()),
            Some(1)
        );
        assert!(
            workspace
                .session_list("project-b")
                .map(|list| list.sessions.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn collapsing_a_project_does_not_drop_its_session_snapshot() {
        let mut workspace = AgentWorkspace::new();
        let project = ProjectIdentity::new("/tmp/a", "A");
        let session = sample_session(&project);
        let generation = workspace.session_list_mut("project-a").next_generation();
        workspace.apply_sessions_initial(
            "project-a",
            generation,
            CatalogPage {
                sessions: vec![session],
                next_cursor: None,
            },
        );
        workspace.expand_project("project-a".into());
        assert!(!workspace.toggle_project_expanded("project-a".into()));
        assert!(!workspace.is_project_expanded("project-a"));
        assert_eq!(
            workspace
                .session_list("project-a")
                .map(|list| list.sessions.len()),
            Some(1)
        );
    }

    #[test]
    fn new_project_draft_expands_and_opens_composer_state() {
        let mut workspace = AgentWorkspace::new();
        workspace.new_project_draft("project-a".into());
        assert!(workspace.is_project_expanded("project-a"));
        assert_eq!(
            workspace.open(),
            Some(&AgentOpen::Draft {
                project_id: "project-a".into()
            })
        );
        assert!(workspace.selected_session_id().is_none());
    }

    #[test]
    fn select_session_updates_the_open_target_without_leaving_the_tree() {
        let mut workspace = AgentWorkspace::new();
        workspace.expand_project("project-a".into());
        workspace.new_project_draft("project-a".into());
        let session_id = SessionId::new(SessionDomain::Agent);
        workspace.select_session("project-a".into(), session_id.clone());

        assert_eq!(workspace.selected_session_id(), Some(&session_id));
        assert!(workspace.is_project_expanded("project-a"));
        assert_eq!(
            workspace.open(),
            Some(&AgentOpen::Session {
                project_id: "project-a".into(),
                session_id,
            })
        );
    }

    fn record(id: &str, path: &str, name: &str) -> crate::preferences::AgentProjectRecord {
        crate::preferences::AgentProjectRecord {
            project_id: id.to_string(),
            canonical_path: std::path::PathBuf::from(path),
            display_name: name.to_string(),
        }
    }

    #[test]
    fn persisted_projects_merge_below_store_rows() {
        let store = vec![sample_project("project-a")];
        let persisted = vec![
            // Same id as a store row: the store row wins, no duplicate.
            record("project-a", "/tmp/dup", "Dup"),
            // Same canonical path as a store row under a different id: the
            // store stays authoritative for that path.
            record("project-b", "/tmp/project-a", "Shadow"),
            // A genuinely new folder: appears with zero sessions.
            record("project-c", "/tmp/c", "New Folder"),
        ];
        let merged = merge_persisted_projects(&store, &persisted);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].project_id, "project-a");
        assert_eq!(merged[1].project_id, "project-c");
        assert_eq!(merged[1].session_count, 0);
        assert_eq!(merged[1].display_name, "New Folder");
    }

    #[test]
    fn persisted_records_deduplicate_each_other_by_path() {
        let persisted = vec![
            record("project-x", "/tmp/x", "First"),
            record("project-y", "/tmp/x", "Second"),
        ];
        let merged = merge_persisted_projects(&[], &persisted);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].project_id, "project-x");
    }

    #[test]
    fn generation_guard_rejects_stale_project_load() {
        let mut workspace = AgentWorkspace::new();
        let generation = workspace.next_projects_generation();
        workspace.next_projects_generation();

        let page = ProjectCatalogPage {
            projects: vec![sample_project("a")],
            next_cursor: None,
        };
        assert!(!workspace.apply_projects_initial(generation, page));
        assert!(workspace.projects.is_empty());
    }

    #[test]
    fn generation_guard_rejects_stale_session_load() {
        let mut workspace = AgentWorkspace::new();
        let generation = workspace.session_list_mut("project-x").next_generation();
        workspace.session_list_mut("project-x").next_generation();

        let page = CatalogPage {
            sessions: vec![sample_session(&ProjectIdentity::new("/tmp/x", "X"))],
            next_cursor: None,
        };
        assert!(!workspace.apply_sessions_initial("project-x", generation, page));
        assert!(
            workspace
                .session_list("project-x")
                .map(|list| list.sessions.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn stale_session_load_does_not_recreate_removed_project() {
        let mut workspace = AgentWorkspace::new();
        let generation = workspace.session_list_mut("project-x").next_generation();
        workspace.remove_project("project-x");

        assert!(!workspace.apply_sessions_initial(
            "project-x",
            generation,
            CatalogPage {
                sessions: vec![sample_session(&ProjectIdentity::new("/tmp/x", "X"))],
                next_cursor: None,
            },
        ));
        assert!(workspace.session_list("project-x").is_none());
    }

    #[test]
    fn apply_projects_initial_replaces_snapshot() {
        let mut workspace = AgentWorkspace::new();
        let generation = workspace.next_projects_generation();

        let page = ProjectCatalogPage {
            projects: vec![sample_project("a"), sample_project("b")],
            next_cursor: None,
        };
        assert!(workspace.apply_projects_initial(generation, page));
        assert_eq!(workspace.projects.len(), 2);
        assert!(matches!(
            workspace.projects_load_state,
            AgentLoadState::Ready
        ));
        assert!(!workspace.projects_has_more());
    }

    #[test]
    fn apply_projects_load_more_appends_without_duplicates() {
        let mut workspace = AgentWorkspace::new();
        let generation = workspace.next_projects_generation();
        workspace.apply_projects_initial(
            generation,
            ProjectCatalogPage {
                projects: vec![sample_project("a")],
                next_cursor: Some(ProjectCatalogCursor {
                    updated_at: 50,
                    project_id: "a".to_string(),
                }),
            },
        );

        let page = ProjectCatalogPage {
            projects: vec![sample_project("a"), sample_project("b")],
            next_cursor: None,
        };
        assert!(workspace.apply_projects_load_more(generation, page));
        assert_eq!(workspace.projects.len(), 2);
    }

    #[test]
    fn apply_sessions_load_more_appends_without_duplicates() {
        let mut workspace = AgentWorkspace::new();
        let project = ProjectIdentity::new("/tmp/x", "X");
        let session = sample_session(&project);
        let generation = workspace.session_list_mut("project-x").next_generation();
        workspace.apply_sessions_initial(
            "project-x",
            generation,
            CatalogPage {
                sessions: vec![session.clone()],
                next_cursor: Some(CatalogCursor {
                    created_at: 50,
                    session_id: session.session_id.clone(),
                }),
            },
        );

        let page = CatalogPage {
            sessions: vec![session],
            next_cursor: None,
        };
        assert!(workspace.apply_sessions_load_more("project-x", generation, page));
        assert_eq!(
            workspace
                .session_list("project-x")
                .map(|list| list.sessions.len()),
            Some(1)
        );
    }

    #[test]
    fn sidebar_relative_time_uses_hours_and_days() {
        let now = 1_700_000_000_000_i64;
        assert_eq!(
            format_sidebar_relative_time(now, now),
            t!("agent.time_just_now").to_string()
        );
        assert_eq!(
            format_sidebar_relative_time(now, now - 3_600_000),
            t!("agent.time_hours_ago", n = 1).to_string()
        );
        assert_eq!(
            format_sidebar_relative_time(now, now - 86_400_000 * 2),
            t!("agent.time_days_ago", n = 2).to_string()
        );
    }

    #[test]
    fn project_sidebar_labels_resolve_in_every_locale() {
        for locale in ["en", "zh-CN"] {
            for key in [
                "agent.mode",
                "agent.projects",
                "agent.no_sessions",
                "agent.show_more",
                "agent.new_in_project",
                "agent.new_draft",
                "agent.idle_title",
                "agent.idle_hint",
                "sidebar.chats",
                "account.work_mode",
            ] {
                let resolved = t!(key, locale = locale).to_string();
                assert_ne!(resolved, key, "{key} unresolved for {locale}");
                assert!(!resolved.is_empty(), "{key} empty for {locale}");
            }
        }
    }
}
