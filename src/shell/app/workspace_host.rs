//! Window-scoped ownership of the built-in workspace instances.

use gpui::{AppContext as _, Context, Entity, Window};

use crate::preferences::PreferenceHandle;
use crate::runtime::{
    CHAT_WORKSPACE_ID, PROJECT_WORKSPACE_ID, RuntimeServices, WorkspaceDefinition,
    WorkspaceRegistration, WorkspaceRegistry, WorkspaceRegistrySnapshot,
};

use super::{chat_workspace::ChatWorkspace, project_workspace::ProjectWorkspace};

const CHAT_WORKSPACE_ORDER: u32 = 10;
const PROJECT_WORKSPACE_ORDER: u32 = 20;

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
        let project_workspace = cx.new(|cx| ProjectWorkspace::new(services, preference_handle, cx));

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
}
