//! Window-scoped ownership of the built-in workspace instances.

use gpui::{App, AppContext as _, Context, Entity, EntityId, Window};

use crate::llm::ModelSelection;
use crate::preferences::PreferenceHandle;
use crate::runtime::{
    CHAT_WORKSPACE_ID, PROJECT_WORKSPACE_ID, RuntimeServices, WorkspaceDefinition,
    WorkspaceRegistration, WorkspaceRegistry, WorkspaceRegistrySnapshot,
};
use crate::session::SessionId;

use super::{chat_workspace::ChatWorkspace, project_workspace::ProjectWorkspace};

const CHAT_WORKSPACE_ORDER: u32 = 10;
const PROJECT_WORKSPACE_ORDER: u32 = 20;

/// Commands shared by the window-level workspace router.
///
/// The payload describes an operation, not a sidebar row identity. Row
/// targets remain private to their owning workspace so a new workspace does
/// not have to extend a central target enum.
pub(super) enum WorkspaceCommand {
    New,
    OpenProjectFolder,
    OpenProjectDraft(String),
    SelectView(EntityId),
    RestoreChatSession(SessionId),
    RestoreProjectSession {
        project_id: String,
        session_id: SessionId,
    },
    SelectModel(ModelSelection),
    #[cfg(test)]
    DeleteView(EntityId),
    DeleteActive,
}

/// Owns the workspace instances and their window-scoped definition registry.
///
/// The registry contains only stable metadata. The host associates those
/// definitions with the GPUI entities that implement the built-in workspaces,
/// while each workspace continues to own its conversations and their child
/// conversation scopes.
pub(super) struct WorkspaceHost {
    _registry: WorkspaceRegistry,
    _registry_snapshot: WorkspaceRegistrySnapshot,
    _registrations: [WorkspaceRegistration; 2],
    chat_workspace: Entity<ChatWorkspace>,
    project_workspace: Entity<ProjectWorkspace>,
}

impl WorkspaceHost {
    pub(super) fn new(
        services: RuntimeServices,
        preference_handle: PreferenceHandle,
        window: &mut Window,
        cx: &mut Context<super::ChatApp>,
    ) -> Self {
        let application_scope = services.application_scope();
        let window_scope = services.window_scope();
        let mut registry = WorkspaceRegistry::new(application_scope);
        let chat_registration = registry
            .register(
                application_scope,
                WorkspaceDefinition::new(CHAT_WORKSPACE_ID, CHAT_WORKSPACE_ORDER),
            )
            .expect("built-in Chat workspace definition must register once");
        let project_registration = registry
            .register(
                application_scope,
                WorkspaceDefinition::new(PROJECT_WORKSPACE_ID, PROJECT_WORKSPACE_ORDER),
            )
            .expect("built-in Project workspace definition must register once");
        registry
            .add_scope(window_scope, application_scope)
            .expect("runtime window scope must be a valid registry child");
        let registry_snapshot = registry
            .snapshot(window_scope)
            .expect("built-in workspace snapshot must resolve");

        let chat_workspace = cx
            .new(|cx| ChatWorkspace::new(services.clone(), preference_handle.clone(), window, cx));
        let project_workspace = cx.new(|_| ProjectWorkspace::new(services, preference_handle));

        Self {
            _registry: registry,
            _registry_snapshot: registry_snapshot,
            _registrations: [chat_registration, project_registration],
            chat_workspace,
            project_workspace,
        }
    }

    pub(super) fn chat_workspace(&self) -> Entity<ChatWorkspace> {
        self.chat_workspace.clone()
    }

    pub(super) fn project_workspace(&self) -> Entity<ProjectWorkspace> {
        self.project_workspace.clone()
    }

    pub(super) fn registry_snapshot(&self) -> &WorkspaceRegistrySnapshot {
        &self._registry_snapshot
    }

    /// Dispatch a command to the workspace selected by the window.
    ///
    /// Each branch only adapts the command to the workspace's typed API;
    /// persistence, selection guards, and lifecycle effects stay owned by the
    /// workspace entity.
    pub(super) fn dispatch(
        &self,
        workspace_id: crate::runtime::WorkspaceId,
        command: WorkspaceCommand,
        mut window: Option<&mut Window>,
        cx: &mut App,
    ) -> bool {
        match command {
            WorkspaceCommand::New => match workspace_id {
                CHAT_WORKSPACE_ID => {
                    let Some(window) = window.take() else {
                        return false;
                    };
                    self.chat_workspace
                        .update(cx, |workspace, cx| workspace.spawn_draft(window, cx));
                    true
                }
                PROJECT_WORKSPACE_ID => {
                    let Some(_window) = window.take() else {
                        return false;
                    };
                    self.project_workspace
                        .update(cx, |workspace, cx| workspace.open_project_folder(cx));
                    true
                }
                _ => false,
            },
            WorkspaceCommand::OpenProjectFolder => {
                if workspace_id != PROJECT_WORKSPACE_ID {
                    return false;
                }
                self.project_workspace
                    .update(cx, |workspace, cx| workspace.open_project_folder(cx));
                true
            }
            WorkspaceCommand::OpenProjectDraft(project_id) => {
                if workspace_id != PROJECT_WORKSPACE_ID {
                    return false;
                }
                let Some(window) = window.take() else {
                    return false;
                };
                self.project_workspace.update(cx, |workspace, cx| {
                    workspace.open_draft(project_id, window, cx)
                });
                true
            }
            WorkspaceCommand::SelectView(target) => {
                if workspace_id != CHAT_WORKSPACE_ID {
                    return false;
                }
                self.chat_workspace
                    .update(cx, |workspace, cx| workspace.select_target(target, cx));
                true
            }
            WorkspaceCommand::RestoreChatSession(session_id) => {
                if workspace_id != CHAT_WORKSPACE_ID {
                    return false;
                }
                let Some(window) = window.take() else {
                    return false;
                };
                self.chat_workspace.update(cx, |workspace, cx| {
                    workspace.select_session(session_id, window, cx)
                });
                true
            }
            WorkspaceCommand::RestoreProjectSession {
                project_id,
                session_id,
            } => {
                if workspace_id != PROJECT_WORKSPACE_ID {
                    return false;
                }
                let Some(window) = window.take() else {
                    return false;
                };
                self.project_workspace.update(cx, |workspace, cx| {
                    workspace.open_session(project_id, session_id, window, cx)
                });
                true
            }
            WorkspaceCommand::SelectModel(selection) => match workspace_id {
                CHAT_WORKSPACE_ID => self
                    .chat_workspace
                    .update(cx, |workspace, cx| workspace.select_model(selection, cx)),
                PROJECT_WORKSPACE_ID => self
                    .project_workspace
                    .update(cx, |workspace, cx| workspace.select_model(selection, cx)),
                _ => false,
            },
            #[cfg(test)]
            WorkspaceCommand::DeleteView(target) => {
                if workspace_id != CHAT_WORKSPACE_ID {
                    return false;
                }
                let Some(window) = window.take() else {
                    return false;
                };
                self.chat_workspace.update(cx, |workspace, cx| {
                    workspace.delete_conversation(target, window, cx)
                });
                true
            }
            WorkspaceCommand::DeleteActive => match workspace_id {
                CHAT_WORKSPACE_ID => {
                    let Some(window) = window.take() else {
                        return false;
                    };
                    self.chat_workspace.update(cx, |workspace, cx| {
                        workspace.request_delete_active(window, cx)
                    })
                }
                PROJECT_WORKSPACE_ID => {
                    let Some(window) = window.take() else {
                        return false;
                    };
                    self.project_workspace.update(cx, |workspace, cx| {
                        workspace.request_delete_active(window, cx)
                    })
                }
                _ => false,
            },
        }
    }

    /// Prepare every workspace conversation for shutdown before the runtime
    /// exit coordinator flushes session durability.
    pub(super) fn prepare_for_shutdown(&self, cx: &mut App) {
        self.chat_workspace
            .update(cx, |workspace, cx| workspace.prepare_for_shutdown(cx));
        self.project_workspace
            .update(cx, |workspace, cx| workspace.prepare_for_shutdown(cx));
    }
}
