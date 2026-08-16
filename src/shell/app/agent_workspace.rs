//! Agent project workspace state, background loading, and rendering.
//!
//! The workspace is read-only: it browses persisted Agent projects, their
//! sessions, and the resolved transcript for a selected session. No tools are
//! executed and no runtime is impersonated. Every catalog and session read runs
//! on the background executor; render only reads the snapshot.

use std::collections::HashSet;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    tab::{Tab, TabBar},
    v_flex,
};
use rust_i18n::t;

use crate::llm::{ContentBlock, Role};
use crate::session::{
    CatalogCursor, CatalogError, CatalogPage, CatalogQuery, ProjectCatalogCursor,
    ProjectCatalogPage, ProjectCatalogQuery, ProjectSessionStore, ProjectSummary, SessionId,
    SessionStores, SessionSummary,
};

use super::ChatApp;

const AGENT_ROW_HEIGHT: Pixels = px(32.);
/// Column width cap for the Agent conversation area, matching the chat
/// transcript's content column.
const AGENT_CONTENT_MAX_WIDTH: Pixels = px(760.);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AgentLoadState {
    Unloaded,
    Loading,
    Ready,
    Error(SharedString),
}

/// UI-independent snapshot of the Agent workspace.
pub(super) struct AgentWorkspace {
    projects: Vec<ProjectSummary>,
    projects_load_state: AgentLoadState,
    projects_next_cursor: Option<ProjectCatalogCursor>,
    projects_load_more_in_flight: bool,
    projects_generation: u64,

    selected_project_id: Option<String>,

    sessions: Vec<SessionSummary>,
    sessions_load_state: AgentLoadState,
    sessions_next_cursor: Option<CatalogCursor>,
    sessions_load_more_in_flight: bool,
    sessions_generation: u64,

    selected_session_id: Option<SessionId>,

    session_state: Option<crate::session::ResolvedSessionState>,
    session_load_state: AgentLoadState,
    session_generation: u64,
}

impl AgentWorkspace {
    pub(super) fn new() -> Self {
        Self {
            projects: Vec::new(),
            projects_load_state: AgentLoadState::Unloaded,
            projects_next_cursor: None,
            projects_load_more_in_flight: false,
            projects_generation: 0,
            selected_project_id: None,
            sessions: Vec::new(),
            sessions_load_state: AgentLoadState::Unloaded,
            sessions_next_cursor: None,
            sessions_load_more_in_flight: false,
            sessions_generation: 0,
            selected_session_id: None,
            session_state: None,
            session_load_state: AgentLoadState::Unloaded,
            session_generation: 0,
        }
    }

    pub(super) fn projects(&self) -> &[ProjectSummary] {
        &self.projects
    }

    #[allow(dead_code)]
    pub(super) fn projects_load_state(&self) -> &AgentLoadState {
        &self.projects_load_state
    }

    pub(super) fn projects_has_more(&self) -> bool {
        self.projects_next_cursor.is_some()
    }

    pub(super) fn projects_load_more_in_flight(&self) -> bool {
        self.projects_load_more_in_flight
    }

    pub(super) fn selected_project_id(&self) -> Option<&str> {
        self.selected_project_id.as_deref()
    }

    pub(super) fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    #[allow(dead_code)]
    pub(super) fn sessions_load_state(&self) -> &AgentLoadState {
        &self.sessions_load_state
    }

    pub(super) fn sessions_has_more(&self) -> bool {
        self.sessions_next_cursor.is_some()
    }

    pub(super) fn sessions_load_more_in_flight(&self) -> bool {
        self.sessions_load_more_in_flight
    }

    pub(super) fn selected_session_id(&self) -> Option<&SessionId> {
        self.selected_session_id.as_ref()
    }

    pub(super) fn session_state(&self) -> Option<&crate::session::ResolvedSessionState> {
        self.session_state.as_ref()
    }

    #[allow(dead_code)]
    pub(super) fn session_load_state(&self) -> &AgentLoadState {
        &self.session_load_state
    }

    fn next_projects_generation(&mut self) -> u64 {
        self.projects_generation = self.projects_generation.wrapping_add(1);
        self.projects_generation
    }

    fn next_sessions_generation(&mut self) -> u64 {
        self.sessions_generation = self.sessions_generation.wrapping_add(1);
        self.sessions_generation
    }

    fn next_session_generation(&mut self) -> u64 {
        self.session_generation = self.session_generation.wrapping_add(1);
        self.session_generation
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

    fn apply_sessions_initial(&mut self, generation: u64, page: CatalogPage) -> bool {
        if generation != self.sessions_generation {
            return false;
        }
        self.sessions = dedup_session_summaries(page.sessions);
        self.sessions_next_cursor = page.next_cursor;
        self.sessions_load_state = AgentLoadState::Ready;
        true
    }

    fn apply_sessions_load_more(&mut self, generation: u64, page: CatalogPage) -> bool {
        if generation != self.sessions_generation {
            return false;
        }
        let existing: HashSet<SessionId> =
            self.sessions.iter().map(|s| s.session_id.clone()).collect();
        for summary in dedup_session_summaries(page.sessions) {
            if !existing.contains(&summary.session_id) {
                self.sessions.push(summary);
            }
        }
        self.sessions_next_cursor = page.next_cursor;
        true
    }

    fn mark_sessions_error(&mut self, generation: u64, message: SharedString) -> bool {
        if generation != self.sessions_generation {
            return false;
        }
        self.sessions_load_state = AgentLoadState::Error(message);
        true
    }

    fn apply_session_state(
        &mut self,
        generation: u64,
        state: crate::session::ResolvedSessionState,
    ) -> bool {
        if generation != self.session_generation {
            return false;
        }
        self.session_state = Some(state);
        self.session_load_state = AgentLoadState::Ready;
        true
    }

    fn mark_session_error(&mut self, generation: u64, message: SharedString) -> bool {
        if generation != self.session_generation {
            return false;
        }
        self.session_load_state = AgentLoadState::Error(message);
        true
    }

    /// Select a project, clearing any previous session list and detail.
    pub(super) fn select_project(&mut self, project_id: String) {
        self.selected_project_id = Some(project_id);
        self.sessions.clear();
        self.sessions_next_cursor = None;
        self.sessions_load_state = AgentLoadState::Unloaded;
        self.selected_session_id = None;
        self.session_state = None;
        self.session_load_state = AgentLoadState::Unloaded;
    }

    /// Go back to the project list, clearing the session list and detail.
    pub(super) fn clear_project_selection(&mut self) {
        self.selected_project_id = None;
        self.sessions.clear();
        self.sessions_next_cursor = None;
        self.sessions_load_state = AgentLoadState::Unloaded;
        self.selected_session_id = None;
        self.session_state = None;
        self.session_load_state = AgentLoadState::Unloaded;
    }

    /// Select a session for reading, clearing any previous detail.
    pub(super) fn select_session(&mut self, session_id: SessionId) {
        self.selected_session_id = Some(session_id);
        self.session_state = None;
        self.session_load_state = AgentLoadState::Unloaded;
    }

    /// Start a fresh Agent conversation draft for the selected project.  No
    /// session id exists until a future send runtime persists the first turn,
    /// so this only clears the session selection.
    pub(super) fn new_agent_draft(&mut self) {
        self.selected_session_id = None;
        self.session_state = None;
        self.session_load_state = AgentLoadState::Unloaded;
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

impl ChatApp {
    // ---------- Background project catalog loading ----------

    pub(super) fn start_agent_projects_load(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            self.agent.projects_load_state,
            AgentLoadState::Unloaded | AgentLoadState::Error(_)
        ) {
            return;
        }
        let Some(stores) = cx.try_global::<SessionStores>().cloned() else {
            self.agent.projects_load_state =
                AgentLoadState::Error(t!("agent.load_failed").to_string().into());
            return;
        };
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(error) => {
                self.agent.projects_load_state = AgentLoadState::Error(error.to_string().into());
                return;
            }
        };

        let generation = self.agent.next_projects_generation();
        self.agent.projects_load_state = AgentLoadState::Loading;
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { project_store.list_projects(ProjectCatalogQuery::first_page()) },
                )
                .await;
            app.update(cx, |this, cx| {
                this.apply_agent_projects_initial(generation, result, cx);
            });
        });
        self._agent_projects_task = Some(task);
        cx.notify();
    }

    fn apply_agent_projects_initial(
        &mut self,
        generation: u64,
        result: Result<ProjectCatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(page) => {
                self.agent.apply_projects_initial(generation, page);
            }
            Err(error) => {
                let message = error.to_string().into();
                self.agent.mark_projects_error(generation, message);
            }
        }
        self._agent_projects_task = None;
        cx.notify();
    }

    pub(super) fn start_agent_projects_load_more(&mut self, cx: &mut Context<Self>) {
        if self.agent.projects_load_more_in_flight() || !self.agent.projects_has_more() {
            return;
        }
        let Some(cursor) = self.agent.projects_next_cursor.clone() else {
            return;
        };
        let Some(stores) = cx.try_global::<SessionStores>().cloned() else {
            return;
        };
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(_) => return,
        };

        self.agent.projects_load_more_in_flight = true;
        let generation = self.agent.projects_generation;
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_store.list_projects(ProjectCatalogQuery {
                        cursor: Some(cursor),
                        limit: ProjectCatalogQuery::first_page().limit,
                    })
                })
                .await;
            app.update(cx, |this, cx| {
                this.apply_agent_projects_load_more(generation, result, cx);
            });
        });
        self._agent_projects_task = Some(task);
        cx.notify();
    }

    fn apply_agent_projects_load_more(
        &mut self,
        generation: u64,
        result: Result<ProjectCatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        if let Ok(page) = result {
            self.agent.apply_projects_load_more(generation, page);
        }
        self.agent.projects_load_more_in_flight = false;
        self._agent_projects_task = None;
        cx.notify();
    }

    // ---------- Background session list loading ----------

    pub(super) fn start_agent_sessions_load(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.agent.selected_project_id().map(str::to_string) else {
            return;
        };
        if !matches!(
            self.agent.sessions_load_state,
            AgentLoadState::Unloaded | AgentLoadState::Error(_)
        ) {
            return;
        }
        let Some(stores) = cx.try_global::<SessionStores>().cloned() else {
            self.agent.sessions_load_state =
                AgentLoadState::Error(t!("agent.load_failed").to_string().into());
            return;
        };
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(error) => {
                self.agent.sessions_load_state = AgentLoadState::Error(error.to_string().into());
                return;
            }
        };

        let generation = self.agent.next_sessions_generation();
        self.agent.sessions_load_state = AgentLoadState::Loading;
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_store.list_project_sessions(&project_id, CatalogQuery::first_page())
                })
                .await;
            app.update(cx, |this, cx| {
                this.apply_agent_sessions_initial(generation, result, cx);
            });
        });
        self._agent_sessions_task = Some(task);
        cx.notify();
    }

    fn apply_agent_sessions_initial(
        &mut self,
        generation: u64,
        result: Result<CatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(page) => {
                self.agent.apply_sessions_initial(generation, page);
            }
            Err(error) => {
                let message = error.to_string().into();
                self.agent.mark_sessions_error(generation, message);
            }
        }
        self._agent_sessions_task = None;
        cx.notify();
    }

    pub(super) fn start_agent_sessions_load_more(&mut self, cx: &mut Context<Self>) {
        if self.agent.sessions_load_more_in_flight() || !self.agent.sessions_has_more() {
            return;
        }
        let Some(cursor) = self.agent.sessions_next_cursor.clone() else {
            return;
        };
        let Some(project_id) = self.agent.selected_project_id().map(str::to_string) else {
            return;
        };
        let Some(stores) = cx.try_global::<SessionStores>().cloned() else {
            return;
        };
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(_) => return,
        };

        self.agent.sessions_load_more_in_flight = true;
        let generation = self.agent.sessions_generation;
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_store.list_project_sessions(
                        &project_id,
                        CatalogQuery {
                            cursor: Some(cursor),
                            ..CatalogQuery::first_page()
                        },
                    )
                })
                .await;
            app.update(cx, |this, cx| {
                this.apply_agent_sessions_load_more(generation, result, cx);
            });
        });
        self._agent_sessions_task = Some(task);
        cx.notify();
    }

    fn apply_agent_sessions_load_more(
        &mut self,
        generation: u64,
        result: Result<CatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        if let Ok(page) = result {
            self.agent.apply_sessions_load_more(generation, page);
        }
        self.agent.sessions_load_more_in_flight = false;
        self._agent_sessions_task = None;
        cx.notify();
    }

    // ---------- Background session detail loading ----------

    pub(super) fn start_agent_session_load(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.agent.selected_session_id().cloned() else {
            return;
        };
        let Some(project_id) = self.agent.selected_project_id().map(str::to_string) else {
            return;
        };
        if !matches!(
            self.agent.session_load_state,
            AgentLoadState::Unloaded | AgentLoadState::Error(_)
        ) {
            return;
        }
        let Some(stores) = cx.try_global::<SessionStores>().cloned() else {
            self.agent.session_load_state =
                AgentLoadState::Error(t!("agent.load_failed").to_string().into());
            return;
        };
        let project_store = match stores.agent_projects() {
            Ok(store) => store,
            Err(error) => {
                self.agent.session_load_state = AgentLoadState::Error(error.to_string().into());
                return;
            }
        };

        let generation = self.agent.next_session_generation();
        self.agent.session_load_state = AgentLoadState::Loading;
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        project_store.load_project_session(&project_id, &session_id, None)
                    })
                    .await;
            app.update(cx, |this, cx| {
                this.apply_agent_session_state(generation, result, cx);
            });
        });
        self._agent_session_task = Some(task);
        cx.notify();
    }

    fn apply_agent_session_state(
        &mut self,
        generation: u64,
        result: Result<crate::session::ResolvedSessionState, crate::session::SessionError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(state) => {
                self.agent.apply_session_state(generation, state);
            }
            Err(error) => {
                let message = error.to_string().into();
                self.agent.mark_session_error(generation, message);
            }
        }
        self._agent_session_task = None;
        cx.notify();
    }

    // ---------- Agent sidebar rendering ----------

    pub(super) fn render_agent_content(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.agent.selected_project_id().is_some() {
            self.render_agent_sessions_list(cx)
        } else {
            self.render_agent_projects_list(cx)
        }
    }

    fn render_agent_projects_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut children: Vec<AnyElement> = Vec::new();

        // Header doubles as the open-folder affordance: a local project can
        // exist before the store registers one with its first Agent session.
        children.push(
            h_flex()
                .px_2()
                .h(px(24.))
                .items_center()
                .gap_1()
                .text_xs()
                .text_color(cx.theme().sidebar_foreground.opacity(0.6))
                .child(div().flex_1().child(t!("agent.projects").to_string()))
                .child(
                    Button::new("agent-open-folder")
                        .ghost()
                        .small()
                        .icon(IconName::FolderOpen)
                        .tooltip(t!("agent.open_folder").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_project_folder(cx);
                        })),
                )
                .into_any_element(),
        );

        // Folder-opened projects without a session yet live only in
        // preferences; merge them under the store catalog rows.
        let projects = merge_persisted_projects(
            self.agent.projects(),
            &crate::preferences::get(cx).agent_projects,
        );

        let loading_and_empty = matches!(self.agent.projects_load_state, AgentLoadState::Loading)
            && projects.is_empty();

        if loading_and_empty {
            children.push(self.render_agent_loading_state(cx).into_any_element());
        }

        let selected_project = self.agent.selected_project_id();
        for project in &projects {
            children.push(
                self.render_agent_project_row(project, selected_project, cx)
                    .into_any_element(),
            );
        }

        let ready = matches!(self.agent.projects_load_state, AgentLoadState::Ready);
        let error_and_empty = matches!(self.agent.projects_load_state, AgentLoadState::Error(_))
            && projects.is_empty();
        let no_rows = projects.is_empty();

        if error_and_empty {
            children.push(self.render_agent_error_state(cx).into_any_element());
        } else if ready && no_rows {
            children.push(
                self.render_agent_projects_empty_state(cx)
                    .into_any_element(),
            );
        } else if self.agent.projects_has_more() {
            children.push(self.render_agent_load_more_row(cx).into_any_element());
        }

        v_flex()
            .id("agent-projects")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .pt_2()
            .gap_1()
            .children(children)
            .into_any_element()
    }

    fn render_agent_sessions_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut children: Vec<AnyElement> = Vec::new();

        children.push(
            h_flex()
                .px_2()
                .h(px(24.))
                .items_center()
                .gap_1()
                .text_xs()
                .child(
                    Button::new("agent-back")
                        .ghost()
                        .small()
                        .icon(IconName::ArrowLeft)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.agent.clear_project_selection();
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_color(cx.theme().sidebar_foreground.opacity(0.6))
                        .child(t!("agent.sessions").to_string()),
                )
                .into_any_element(),
        );

        let loading_and_empty = matches!(self.agent.sessions_load_state, AgentLoadState::Loading)
            && self.agent.sessions.is_empty();

        if loading_and_empty {
            children.push(self.render_agent_loading_state(cx).into_any_element());
        }

        let selected_session = self.agent.selected_session_id();
        for summary in self.agent.sessions() {
            children.push(
                self.render_agent_session_row(summary, selected_session, cx)
                    .into_any_element(),
            );
        }

        let ready = matches!(self.agent.sessions_load_state, AgentLoadState::Ready);
        let error_and_empty = matches!(self.agent.sessions_load_state, AgentLoadState::Error(_))
            && self.agent.sessions.is_empty();
        let no_rows = self.agent.sessions.is_empty();

        if error_and_empty {
            children.push(self.render_agent_error_state(cx).into_any_element());
        } else if ready && no_rows {
            children.push(
                self.render_agent_sessions_empty_state(cx)
                    .into_any_element(),
            );
        } else if self.agent.sessions_has_more() {
            children.push(self.render_agent_load_more_row(cx).into_any_element());
        }

        v_flex()
            .id("agent-sessions")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .pt_2()
            .gap_1()
            .children(children)
            .into_any_element()
    }

    fn render_agent_project_row(
        &self,
        project: &ProjectSummary,
        _selected: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_id = project.project_id.clone();
        let display_name = project.display_name.clone();
        let session_count = project.session_count;
        let is_selected = self.agent.selected_project_id() == Some(project.project_id.as_str());

        div()
            .id(format!("agent-project-{project_id}"))
            .h(AGENT_ROW_HEIGHT)
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |this| this.bg(cx.theme().accent.opacity(0.15)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.agent.select_project(project_id.clone());
                this.start_agent_sessions_load(cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().sidebar_foreground)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(display_name),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().sidebar_foreground.opacity(0.5))
                    .child(format!("{session_count}")),
            )
            .into_any_element()
    }

    fn render_agent_session_row(
        &self,
        summary: &SessionSummary,
        selected: Option<&SessionId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session_id = summary.session_id.clone();
        let title: SharedString = summary
            .title
            .clone()
            .unwrap_or_else(|| t!("agent.untitled_session").to_string())
            .into();
        let is_selected = selected == Some(&summary.session_id);

        div()
            .id(format!("agent-session-{session_id}"))
            .h(AGENT_ROW_HEIGHT)
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |this| this.bg(cx.theme().accent.opacity(0.15)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.agent.select_session(session_id.clone());
                this.start_agent_session_load(cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().sidebar_foreground)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(title),
            )
            .into_any_element()
    }

    fn render_agent_loading_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .h(AGENT_ROW_HEIGHT)
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(Spinner::new().small())
            .child(t!("agent.loading").to_string())
    }

    fn render_agent_projects_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_2()
            .py_3()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(div().child(t!("agent.no_projects").to_string()))
            .child(
                div()
                    .text_xs()
                    .child(t!("agent.no_projects_hint").to_string()),
            )
    }

    fn render_agent_sessions_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_2()
            .py_3()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(div().child(t!("agent.no_sessions").to_string()))
    }

    fn render_agent_error_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_2()
            .py_3()
            .gap_2()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(div().child(t!("agent.load_failed").to_string()))
            .child(
                Button::new("agent-retry")
                    .ghost()
                    .small()
                    .label(t!("agent.retry").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.agent.selected_project_id().is_some() {
                            this.start_agent_sessions_load(cx);
                        } else {
                            this.start_agent_projects_load(cx);
                        }
                    })),
            )
    }

    fn render_agent_load_more_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("agent-load-more")
            .h(AGENT_ROW_HEIGHT)
            .flex()
            .items_center()
            .justify_center()
            .px_2()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(t!("agent.load_more").to_string())
            .on_click(cx.listener(|this, _, _, cx| {
                if this.agent.selected_project_id().is_some() {
                    this.start_agent_sessions_load_more(cx);
                } else {
                    this.start_agent_projects_load_more(cx);
                }
            }))
    }

    // ---------- Agent main area rendering ----------

    pub(super) fn render_agent_main(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Without a project there is nothing to converse about yet: show the
        // folder guide instead of a composer.
        if self.agent.selected_project_id().is_none() {
            return self.render_agent_folder_guide(cx).into_any_element();
        }

        let transcript = self.render_agent_transcript(cx);
        // Chat-style column: transcript scrolls on top, the draft composer
        // card sits below. The composer owns the `$` Chat reference
        // completion but no send or tool runtime.
        v_flex()
            .flex_1()
            .min_h_0()
            .child(div().flex_1().min_h_0().child(transcript))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .justify_center()
                    .px_6()
                    .pt_2()
                    .pb_3()
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(AGENT_CONTENT_MAX_WIDTH)
                            .child(self.agent_composer.clone()),
                    ),
            )
            .into_any_element()
    }

    /// Full-area guide shown in Agent mode before any folder is opened.
    fn render_agent_folder_guide(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
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
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_project_folder(cx);
                    })),
            )
            .into_any_element()
    }

    fn render_agent_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.agent.session_state() else {
            return self.render_agent_main_empty(cx).into_any_element();
        };

        let mut children: Vec<AnyElement> = Vec::new();

        if !state.transcript_replays.is_empty() {
            children.push(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} replay fact(s)", state.transcript_replays.len()))
                    .into_any_element(),
            );
        }

        if state.latest_compaction.is_some() {
            children.push(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("agent.compacted").to_string())
                    .into_any_element(),
            );
        }

        for message in &state.messages {
            children.push(self.render_agent_message(message, cx).into_any_element());
        }

        if state.messages.is_empty()
            && state.transcript_replays.is_empty()
            && state.latest_compaction.is_none()
        {
            children.push(
                div()
                    .px_4()
                    .py_8()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("agent.empty_session").to_string())
                    .into_any_element(),
            );
        }

        v_flex()
            .id("agent-transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_4()
            .gap_2()
            .children(children)
            .into_any_element()
    }

    fn render_agent_main_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let loading = matches!(self.agent.session_load_state, AgentLoadState::Loading);

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

        // A selected project without a selected session is a fresh draft
        // conversation; greet it like the chat workspace greets a new chat.
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

    fn render_agent_message(
        &self,
        message: &crate::session::ResolvedMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let role = &message.message.role;
        let (role_label, role_color) = match role {
            Role::User => (t!("agent.role_user").to_string(), theme.primary),
            Role::Assistant => (
                t!("agent.role_assistant").to_string(),
                theme.muted_foreground,
            ),
            Role::System => (t!("agent.role_system").to_string(), theme.muted_foreground),
            Role::Developer => (
                t!("agent.role_developer").to_string(),
                theme.muted_foreground,
            ),
            Role::Tool => (t!("agent.role_tool").to_string(), theme.muted_foreground),
        };

        let mut text_parts: Vec<String> = Vec::new();
        for block in &message.message.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    text_parts.push(text.clone());
                }
                ContentBlock::Reasoning { .. } => {
                    text_parts.push(t!("agent.reasoning_block").to_string());
                }
                ContentBlock::ToolCall { tool_call } => {
                    text_parts.push(format!("{}: {}", t!("agent.tool_call"), tool_call.name));
                }
                ContentBlock::ToolResult { .. } => {
                    text_parts.push(t!("agent.tool_result").to_string());
                }
            }
        }
        let body = text_parts.join("\n\n");

        v_flex()
            .px_4()
            .py_2()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(role_color)
                    .child(role_label),
            )
            .child(div().text_sm().text_color(theme.foreground).child(body))
            .into_any_element()
    }

    // ---------- Workspace mode tabs ----------

    pub(super) fn render_workspace_mode_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.workspace_mode {
            super::WorkspaceMode::Chat => 0,
            super::WorkspaceMode::Agent => 1,
        };

        TabBar::new("workspace-mode")
            .segmented()
            .w_full()
            .child(Tab::new().flex_1().label(t!("sidebar.chats").to_string()))
            .child(Tab::new().flex_1().label(t!("agent.mode").to_string()))
            .selected_index(selected)
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                let mode = match index {
                    0 => super::WorkspaceMode::Chat,
                    _ => super::WorkspaceMode::Agent,
                };
                this.switch_workspace_mode(mode, window, cx);
            }))
    }

    pub(super) fn switch_workspace_mode(
        &mut self,
        mode: super::WorkspaceMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_mode == mode {
            return;
        }
        if matches!(self.workspace_mode, super::WorkspaceMode::Agent) {
            // Leaving Agent mode unmounts the composer; close any open
            // completion popup so it cannot linger over Chat.
            self.agent_composer
                .update(cx, |composer, cx| composer.dismiss_completion(cx));
        }
        self.workspace_mode = mode;
        if matches!(mode, super::WorkspaceMode::Agent)
            && matches!(
                self.agent.projects_load_state,
                AgentLoadState::Unloaded | AgentLoadState::Error(_)
            )
        {
            self.start_agent_projects_load(cx);
        }
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
            jsonl_path: std::path::PathBuf::from("/tmp/session.jsonl"),
        }
    }

    #[test]
    fn select_project_clears_session_list_and_detail() {
        let mut workspace = AgentWorkspace::new();
        let project = ProjectIdentity::new("/tmp/test", "Test");
        let session = sample_session(&project);
        workspace.sessions = vec![session];
        workspace.sessions_load_state = AgentLoadState::Ready;
        workspace.selected_session_id = Some(SessionId::new(SessionDomain::Agent));
        workspace.session_load_state = AgentLoadState::Ready;

        workspace.select_project("new-project-id".to_string());

        assert_eq!(
            workspace.selected_project_id,
            Some("new-project-id".to_string())
        );
        assert!(workspace.sessions.is_empty());
        assert!(matches!(
            workspace.sessions_load_state,
            AgentLoadState::Unloaded
        ));
        assert!(workspace.selected_session_id.is_none());
        assert!(workspace.session_state.is_none());
        assert!(matches!(
            workspace.session_load_state,
            AgentLoadState::Unloaded
        ));
    }

    #[test]
    fn clear_project_selection_resets_everything() {
        let mut workspace = AgentWorkspace::new();
        workspace.selected_project_id = Some("test-project".to_string());
        workspace.sessions = vec![sample_session(&ProjectIdentity::new("/tmp/test", "Test"))];
        workspace.sessions_load_state = AgentLoadState::Ready;
        workspace.selected_session_id = Some(SessionId::new(SessionDomain::Agent));
        workspace.session_load_state = AgentLoadState::Ready;

        workspace.clear_project_selection();

        assert!(workspace.selected_project_id.is_none());
        assert!(workspace.sessions.is_empty());
        assert!(matches!(
            workspace.sessions_load_state,
            AgentLoadState::Unloaded
        ));
        assert!(workspace.selected_session_id.is_none());
        assert!(workspace.session_state.is_none());
    }

    #[test]
    fn select_session_clears_previous_state() {
        let mut workspace = AgentWorkspace::new();
        let session_id = SessionId::new(SessionDomain::Agent);
        workspace.session_load_state = AgentLoadState::Ready;

        workspace.select_session(session_id.clone());

        assert_eq!(workspace.selected_session_id, Some(session_id));
        assert!(workspace.session_state.is_none());
        assert!(matches!(
            workspace.session_load_state,
            AgentLoadState::Unloaded
        ));
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
        let generation = workspace.next_sessions_generation();
        workspace.next_sessions_generation();

        let page = CatalogPage {
            sessions: vec![sample_session(&ProjectIdentity::new("/tmp/x", "X"))],
            next_cursor: None,
        };
        assert!(!workspace.apply_sessions_initial(generation, page));
        assert!(workspace.sessions.is_empty());
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
        let generation = workspace.next_sessions_generation();
        workspace.apply_sessions_initial(
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
        assert!(workspace.apply_sessions_load_more(generation, page));
        assert_eq!(workspace.sessions.len(), 1);
    }
}
