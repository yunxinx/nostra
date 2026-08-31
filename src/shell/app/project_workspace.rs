//! Project workspace instance state and conversation lifecycle.

use std::{collections::HashMap, path::PathBuf};

use gpui::{App, Context, Entity, Subscription, Task, Window};
use gpui_component::{WindowExt as _, notification::NotificationType};
use rust_i18n::t;

use crate::chat::{
    ChatDeleteRequest, ChatEvent, ChatView, create_conversation_runtime, derive_chat_title,
};
use crate::llm::ModelSelection;
use crate::preferences::{self, PreferenceHandle};
use crate::runtime::RuntimeServices;
use crate::session::{
    ProjectSessionStore as _, ResolvedSessionState, SessionId, SessionLifecycleStore as _,
    SessionStores,
};
use crate::ui::inline_delete_confirmation::InlineDeleteConfirmationHandle;

use super::{
    SidebarTarget,
    agent_workspace::{AgentLoadState, AgentWorkspace, merge_persisted_projects},
    chat_workspace::{SelectionEpoch, SelectionRequest},
    conversation_host::{
        Conversation, ConversationHost, ConversationHostSnapshot, ConversationSnapshot,
    },
};

#[derive(Clone)]
pub(super) struct ProjectConversationMetadata {
    project_id: String,
}

impl ProjectConversationMetadata {
    pub(super) fn project_id(&self) -> &str {
        &self.project_id
    }
}

pub(super) type ProjectConversationSnapshot = ConversationSnapshot<ProjectConversationMetadata>;

#[derive(Clone)]
pub(super) struct ProjectWorkspaceSnapshot {
    catalog: AgentWorkspace,
    projects: Vec<crate::session::ProjectSummary>,
    conversations: ConversationHostSnapshot<ProjectConversationMetadata>,
    deleting_projects: std::collections::HashSet<String>,
    confirming: Option<SidebarTarget>,
    delete_confirmation: InlineDeleteConfirmationHandle,
    session_load_state: AgentLoadState,
}

impl ProjectWorkspaceSnapshot {
    fn empty() -> Self {
        Self {
            catalog: AgentWorkspace::new(),
            projects: Vec::new(),
            conversations: ConversationHostSnapshot::empty(),
            deleting_projects: std::collections::HashSet::new(),
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            session_load_state: AgentLoadState::Unloaded,
        }
    }

    pub(super) fn catalog(&self) -> &AgentWorkspace {
        &self.catalog
    }

    pub(super) fn projects(&self) -> &[crate::session::ProjectSummary] {
        &self.projects
    }

    pub(super) fn conversations(&self) -> &[ProjectConversationSnapshot] {
        self.conversations.conversations()
    }

    pub(super) fn active(&self) -> Option<gpui::EntityId> {
        self.conversations.active()
    }

    pub(super) fn active_view(&self) -> Option<Entity<ChatView>> {
        self.conversations.active_view()
    }

    pub(super) fn conversation(
        &self,
        target: gpui::EntityId,
    ) -> Option<&ProjectConversationSnapshot> {
        self.conversations.conversation(target)
    }

    pub(super) fn draft_for_project(
        &self,
        project_id: &str,
    ) -> Option<&ProjectConversationSnapshot> {
        self.conversations().iter().find(|conversation| {
            conversation.metadata().project_id() == project_id
                && conversation.session_id().is_none()
        })
    }

    pub(super) fn opened_session(
        &self,
        session_id: &SessionId,
    ) -> Option<&ProjectConversationSnapshot> {
        let target = self.conversations.opened_target(session_id)?;
        self.conversations.conversation(target)
    }

    pub(super) fn is_deleting_project(&self, project_id: &str) -> bool {
        self.deleting_projects.contains(project_id)
    }

    pub(super) fn confirming(&self) -> Option<&SidebarTarget> {
        self.confirming.as_ref()
    }

    pub(super) fn delete_confirmation(&self) -> InlineDeleteConfirmationHandle {
        self.delete_confirmation.clone()
    }

    pub(super) fn session_load_state(&self) -> &AgentLoadState {
        &self.session_load_state
    }
}

struct ProjectSessionRestore {
    request: SelectionRequest,
    project_id: String,
    session_id: SessionId,
    project: crate::session::ProjectIdentity,
    state: ResolvedSessionState,
}

pub(super) struct ProjectWorkspace {
    pub(super) catalog: AgentWorkspace,
    pub(super) conversations: ConversationHost<ProjectConversationMetadata>,
    pub(super) selection_epoch: SelectionEpoch,
    pub(super) _projects_task: Option<Task<()>>,
    pub(super) _session_list_tasks: HashMap<String, Task<()>>,
    pub(super) _open_task: Option<Task<()>>,
    pub(super) _delete_task: Option<Task<()>>,
    pub(super) deleting_projects: std::collections::HashSet<String>,
    pub(super) _folder_task: Option<Task<()>>,
    pub(super) confirming: Option<SidebarTarget>,
    pub(super) delete_confirmation: InlineDeleteConfirmationHandle,
    pub(super) session_load_state: AgentLoadState,
    pub(super) session_services: SessionStores,
    pub(super) runtime_services: RuntimeServices,
    pub(super) preference_handle: PreferenceHandle,
    snapshot: ProjectWorkspaceSnapshot,
}

impl ProjectWorkspace {
    pub(super) fn new(
        services: RuntimeServices,
        preference_handle: PreferenceHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut workspace = Self {
            catalog: AgentWorkspace::new(),
            conversations: ConversationHost::new(),
            selection_epoch: SelectionEpoch::default(),
            _projects_task: None,
            _session_list_tasks: HashMap::new(),
            _open_task: None,
            _delete_task: None,
            deleting_projects: std::collections::HashSet::new(),
            _folder_task: None,
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            session_load_state: AgentLoadState::Unloaded,
            session_services: services.session_services().clone(),
            runtime_services: services,
            preference_handle,
            snapshot: ProjectWorkspaceSnapshot::empty(),
        };
        workspace.publish_snapshot_with(cx);
        workspace
    }

    pub(super) fn snapshot(&self) -> &ProjectWorkspaceSnapshot {
        &self.snapshot
    }

    fn publish_snapshot_with(&mut self, cx: &App) {
        let persisted = self.preference_handle.snapshot().agent_projects;
        self.snapshot = ProjectWorkspaceSnapshot {
            catalog: self.catalog.clone(),
            projects: merge_persisted_projects(self.catalog.projects(), &persisted),
            conversations: self.conversations.snapshot(cx),
            deleting_projects: self.deleting_projects.clone(),
            confirming: self.confirming.clone(),
            delete_confirmation: self.delete_confirmation.clone(),
            session_load_state: self.session_load_state.clone(),
        };
    }

    pub(super) fn notify_changed(&mut self, cx: &mut Context<Self>) {
        self.publish_snapshot_with(cx);
        cx.notify();
    }

    pub(super) fn refresh_preferences(&mut self, cx: &mut Context<Self>) {
        self.notify_changed(cx);
    }

    pub(super) fn toggle_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        let expanded = self.catalog.toggle_project_expanded(project_id.clone());
        if expanded && self.catalog.sessions_need_load(&project_id) {
            self.start_agent_sessions_load(project_id, cx);
        } else {
            self.notify_changed(cx);
        }
    }

    pub(super) fn prepare_for_shutdown(&mut self, cx: &mut Context<Self>) {
        for conversation in self.conversations.conversations() {
            conversation
                .view
                .update(cx, |chat, cx| chat.prepare_for_shutdown(cx));
        }
    }

    pub(super) fn dismiss_active_completion(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.conversations.active() else {
            return;
        };
        if let Some(conversation) = self.conversations.conversation(target) {
            conversation
                .view
                .update(cx, |view, cx| view.dismiss_composer_completion(cx));
        }
    }

    fn begin_selection_request(&mut self) -> SelectionRequest {
        self._open_task = None;
        self.selection_epoch.begin()
    }

    fn invalidate_selection_request(&mut self) {
        self._open_task = None;
        self.selection_epoch.invalidate();
        self.session_load_state = AgentLoadState::Unloaded;
    }

    fn selection_request_is_current(
        &self,
        request: SelectionRequest,
        project_id: &str,
        session_id: &SessionId,
    ) -> bool {
        self.selection_epoch.is_current(request)
            && self.catalog.open_project_id() == Some(project_id)
            && self.catalog.selected_session_id() == Some(session_id)
    }

    fn create_scope(
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

    fn project_identity(&self, project_id: &str) -> Option<crate::session::ProjectIdentity> {
        self.catalog
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

    fn subscribe_conversation(
        &mut self,
        view: &Entity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |workspace, view, event, window, cx| {
            let target = view.entity_id();
            let Some(index) = workspace.conversations.conversation_index(target) else {
                return;
            };
            match event {
                ChatEvent::TitleChanged(title) => {
                    workspace.conversations.conversations_mut()[index].title = title.clone();
                }
                ChatEvent::SelectionChanged(selection) => {
                    workspace.conversations.conversations_mut()[index].selection =
                        Some(selection.clone());
                }
                ChatEvent::StateChanged => {}
                ChatEvent::SessionBound(session_id) => {
                    let project_id = workspace.conversations.conversations()[index]
                        .metadata
                        .project_id
                        .clone();
                    if workspace
                        .conversations
                        .bind_session(target, session_id.clone())
                    {
                        workspace
                            .catalog
                            .bind_draft_session(project_id.clone(), session_id.clone());
                        workspace.refresh_agent_sessions(project_id, cx);
                    }
                }
                ChatEvent::DeleteCompleted => {
                    cx.defer_in(window, move |workspace, window, cx| {
                        workspace.remove_conversation(target, window, cx);
                    });
                    return;
                }
            }
            workspace.notify_changed(cx);
        })
    }

    pub(super) fn open_draft(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing_target) = self
            .conversations
            .conversations()
            .iter()
            .find(|conversation| {
                conversation.metadata.project_id == project_id && conversation.session_id.is_none()
            })
            .map(|conversation| conversation.view.entity_id())
        {
            self.invalidate_selection_request();
            self.catalog.open_draft(project_id);
            self.conversations.set_active(existing_target);
            if let Some(conversation) = self.conversations.conversation(existing_target) {
                conversation
                    .view
                    .update(cx, |view, cx| view.focus_composer(window, cx));
            }
            self.notify_changed(cx);
            return;
        }
        let Some(identity) = self.project_identity(&project_id) else {
            return;
        };
        let Some(scope) = self.create_scope(window, cx) else {
            return;
        };
        self.invalidate_selection_request();
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
        let subscription = self.subscribe_conversation(&view, window, cx);
        self.conversations.push_and_activate(Conversation {
            view: view.clone(),
            metadata: ProjectConversationMetadata {
                project_id: project_id.clone(),
            },
            title: t!("agent.new_draft").to_string().into(),
            selection,
            session_id: None,
            _subscription: subscription,
        });
        self.catalog.new_project_draft(project_id.clone());
        self.start_agent_sessions_load(project_id, cx);
        view.update(cx, |view, cx| view.focus_composer(window, cx));
        self.notify_changed(cx);
    }

    pub(super) fn open_session(
        &mut self,
        project_id: String,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing_target) = self.conversations.opened_target(&session_id) {
            self.invalidate_selection_request();
            self.catalog.select_session(project_id, session_id);
            self.conversations.set_active(existing_target);
            self.notify_changed(cx);
            return;
        }
        let Some(identity) = self.project_identity(&project_id) else {
            return;
        };
        let Ok(project_store) = self.session_services.agent_projects() else {
            return;
        };
        let request = self.begin_selection_request();
        let workspace = cx.entity();
        let window_handle = window.window_handle();
        self.catalog
            .select_session(project_id.clone(), session_id.clone());
        self.session_load_state = AgentLoadState::Loading;
        let load_project_id = project_id.clone();
        let load_session_id = session_id.clone();
        let task = cx.spawn(async move |_this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    project_store.load_project_session(&load_project_id, &load_session_id, None)
                })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| match loaded {
                    Ok(state) => workspace.apply_session_restore(
                        ProjectSessionRestore {
                            request,
                            project_id,
                            session_id,
                            project: identity,
                            state,
                        },
                        window,
                        cx,
                    ),
                    Err(error) => {
                        workspace.session_load_state =
                            AgentLoadState::Error(error.to_string().into());
                        workspace._open_task = None;
                        workspace.notify_changed(cx);
                    }
                });
            });
        });
        self._open_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_session_restore(
        &mut self,
        restore: ProjectSessionRestore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selection_request_is_current(
            restore.request,
            &restore.project_id,
            &restore.session_id,
        ) {
            return;
        }
        self.session_load_state = AgentLoadState::Ready;
        self._open_task = None;
        if let Some(existing_target) = self.conversations.opened_target(&restore.session_id) {
            self.conversations.set_active(existing_target);
            self.notify_changed(cx);
            return;
        }

        let Some(scope) = self.create_scope(window, cx) else {
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
        let subscription = self.subscribe_conversation(&view, window, cx);
        self.conversations.push_and_activate(Conversation {
            selection: view.read(cx).selection(),
            view,
            metadata: ProjectConversationMetadata {
                project_id: restore.project_id,
            },
            title,
            session_id: Some(restore.session_id),
            _subscription: subscription,
        });
        self.notify_changed(cx);
    }

    pub(super) fn select_model(
        &mut self,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target) = self.conversations.active() else {
            return false;
        };
        let Some(conversation) = self.conversations.conversation(target) else {
            return false;
        };
        conversation
            .view
            .update(cx, |chat, cx| chat.select_model(selection, cx));
        true
    }

    pub(super) fn retry_open_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project_id, session_id)) = self.catalog.open().and_then(|open| match open {
            crate::shell::app::agent_workspace::AgentOpen::Session {
                project_id,
                session_id,
            } => Some((project_id.clone(), session_id.clone())),
            crate::shell::app::agent_workspace::AgentOpen::Draft { .. } => None,
        }) else {
            return;
        };
        self.open_session(project_id, session_id, window, cx);
    }

    fn remove_conversation(
        &mut self,
        target: gpui::EntityId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(removed) = self.conversations.remove(target) else {
            return;
        };
        let project_id = removed.conversation.metadata.project_id;
        if removed.conversation.session_id.is_none() {
            removed
                .conversation
                .view
                .update(cx, |view, cx| view.close_scope(cx));
        }
        if let Some(session_id) = removed.conversation.session_id {
            self.catalog.remove_session(&project_id, &session_id);
        }
        self.catalog.discard_draft(&project_id);
        self.notify_changed(cx);
    }

    fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conversation) = self.conversations.conversation(target) else {
            return;
        };
        if conversation.session_id.is_none() {
            self.remove_conversation(target, window, cx);
            return;
        }
        let request = conversation
            .view
            .update(cx, |chat, cx| chat.request_delete(cx));
        if request == ChatDeleteRequest::RemoveNow {
            self.remove_conversation(target, window, cx);
        }
    }

    fn delete_unopened_session(
        &mut self,
        project_id: String,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut store) = self.session_services.agent() else {
            return;
        };
        let workspace = cx.entity();
        let window_handle = window.window_handle();
        self._delete_task = Some(cx.spawn(async move |_, cx| {
            let delete_id = session_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { store.delete_session(&delete_id) })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    match result {
                        Ok(()) => workspace.catalog.remove_session(&project_id, &session_id),
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
                    workspace._delete_task = None;
                    workspace.notify_changed(cx);
                });
            });
        }));
    }

    fn delete_project(
        &mut self,
        project: crate::session::ProjectSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_in_flight_work = self
            .conversations
            .conversations()
            .iter()
            .filter(|conversation| conversation.metadata.project_id == project.project_id)
            .any(|conversation| conversation.view.read(cx).has_in_flight_work());
        if has_in_flight_work {
            window.push_notification(
                (NotificationType::Error, t!("agent.delete_busy").to_string()),
                cx,
            );
            return;
        }
        let Ok(project_store) = self.session_services.agent_projects() else {
            return;
        };
        let Ok(mut lifecycle) = self.session_services.agent() else {
            return;
        };
        let project_id = project.project_id.clone();
        let apply_project_id = project_id.clone();
        let workspace = cx.entity();
        let window_handle = window.window_handle();
        self.deleting_projects.insert(apply_project_id.clone());
        self.notify_changed(cx);
        self._delete_task = Some(cx.spawn(async move |_, cx| {
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
                workspace.update(cx, |workspace, cx| {
                    match result {
                        Ok(()) => workspace.finish_project_delete(&apply_project_id, cx),
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
                    workspace.deleting_projects.remove(&apply_project_id);
                    workspace._delete_task = None;
                    workspace.notify_changed(cx);
                });
            });
        }));
    }

    fn finish_project_delete(&mut self, project_id: &str, cx: &mut Context<Self>) {
        preferences::remove_agent_project(project_id, cx);
        if self.catalog.open_project_id() == Some(project_id) {
            self.invalidate_selection_request();
        }
        self._session_list_tasks.remove(project_id);
        let removed = self
            .conversations
            .retain(|conversation| conversation.metadata.project_id != project_id);
        for conversation in removed {
            conversation
                .view
                .update(cx, |view, cx| view.close_scope(cx));
        }
        self.catalog.remove_project(project_id);
    }

    pub(super) fn begin_delete_confirmation(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_confirmation.dismiss_for_unmount(window, cx);
        self.confirming = Some(target);
        self.notify_changed(cx);
    }

    pub(super) fn clear_delete_confirmation(
        &mut self,
        target: &SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming.as_ref() == Some(target) {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
            self.notify_changed(cx);
        }
    }

    pub(super) fn request_delete_active(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target) = self.conversations.active() else {
            return false;
        };
        let Some(conversation) = self.conversations.conversation(target) else {
            return false;
        };
        let project_id = conversation.metadata.project_id.clone();
        let sidebar_target = conversation.session_id.clone().map_or_else(
            || SidebarTarget::AgentView(target),
            |session_id| SidebarTarget::AgentSession {
                project_id: project_id.clone(),
                session_id,
            },
        );
        self.catalog.expand_project(project_id);
        self.begin_delete_confirmation(sidebar_target, window, cx);
        true
    }

    pub(super) fn confirm_delete_target(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            SidebarTarget::AgentView(entity) => self.delete_conversation(entity, window, cx),
            SidebarTarget::AgentSession {
                project_id,
                session_id,
            } => {
                if let Some(conversation) = self.snapshot.opened_session(&session_id) {
                    self.delete_conversation(conversation.target(), window, cx);
                } else {
                    self.delete_unopened_session(project_id, session_id, window, cx);
                }
            }
            SidebarTarget::AgentProject(project_id) => {
                if let Some(project) = self
                    .snapshot
                    .projects()
                    .iter()
                    .find(|project| project.project_id == project_id)
                    .cloned()
                {
                    self.delete_project(project, window, cx);
                }
            }
            _ => {}
        }
    }

    pub(super) fn open_project_folder(&mut self, cx: &mut Context<Self>) {
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
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            this.update(cx, |workspace, cx| {
                workspace.register_project(canonical, cx);
            })
            .ok();
        });
        self._folder_task = Some(task);
    }

    fn register_project(&mut self, canonical: PathBuf, cx: &mut Context<Self>) {
        let existing_id = self
            .catalog
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

        self.catalog.expand_project(project_id.clone());
        self.start_agent_sessions_load(project_id, cx);
        self.notify_changed(cx);
    }
}
