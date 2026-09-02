//! Typed identities and composition primitives for Nostra's in-process runtime.

mod capability;
mod component;
mod composition;
mod contribution;
/// Crate-private until RuntimeHost lands. See `.trellis/tasks/09-01-runtime-host`.
#[allow(dead_code)]
mod dependency;
mod diagnostics;
mod effect;
mod exit;
mod generation_mount;
mod provider;
/// Crate-private until RuntimeHost lands. See `.trellis/tasks/09-01-runtime-host`.
#[allow(dead_code)]
mod reconcile;
mod scope;
mod workspace;

pub use capability::{
    CapabilityId, CapabilityKey, CapabilityLease, ExclusiveCapabilitySlot, ExclusiveSlotError,
    PreparedCapability, ProviderRegistration,
};
pub use component::{ComponentGeneration, ComponentId, ScopeId};
pub use composition::{
    CompositionBuildError, CompositionReplaceError, CompositionRoot, CompositionRootBuilder,
    ConversationScopeHandle, GenerationCapability, PreferenceCapability, ProviderActivationError,
    ProviderRollbackError, RuntimeServices, SessionServicesCapability, StartupCleanupError,
    StartupCleanupStage,
};
pub use contribution::{
    ContributionDefinition, ContributionId, ContributionKey, ContributionRegistration,
    ContributionRegistry, ContributionRegistryError, ContributionSnapshot,
    ContributionSnapshotEntry,
};
pub(crate) use dependency::ResolvedDependency;
#[cfg(test)]
pub(crate) use dependency::{
    ActivationFingerprint, DependencyDeclaration, DependencyResolution, DependencyResolver,
    DependencyResolverError, DependencySnapshot,
};
pub(crate) use diagnostics::RuntimeSnapshotSource;
pub use diagnostics::{
    ComponentSnapshot, ComponentSnapshotDetails, ComponentSnapshotViolation, ContributionRevision,
    MissingDependencySnapshot, RuntimeComponentDiagnostic, RuntimeComponentState,
    RuntimeDiagnostic, RuntimeResourceCounts, RuntimeSnapshot, RuntimeSnapshotError,
    RuntimeSnapshotReader, RuntimeSnapshotSubscription, RuntimeSnapshotUpdate, ScopedComponentId,
    StartupAuditError, StartupPolicy, TransitionSnapshot,
};
pub use effect::{AsyncStop, DisposeError, EffectScope};
pub use exit::{ExitCoordinator, ExitReport, NORMAL_EXIT_TIMEOUT, QUIT_FALLBACK_TIMEOUT};
pub use provider::{
    PreparedProvider, ProviderPrepareError, ProviderPrepareFailureKind, ProviderSelectionError,
};
#[cfg(test)]
pub(crate) use reconcile::{
    ComponentLifecycle, ReconcileFailureKind, ReconcileStatus, ScopeLocalReconciler,
};
pub(crate) use reconcile::{DesiredRevision, ReconcileFailure, ReconcileObserver, ReconcileStage};
pub use scope::{ScopeError, ScopeKind, ScopeState, ScopeTree};
pub use workspace::{
    CHAT_WORKSPACE_ID, PROJECT_WORKSPACE_ID, WorkspaceDefinition, WorkspaceId,
    WorkspaceRegistration, WorkspaceRegistry, WorkspaceRegistryError, WorkspaceRegistrySnapshot,
};

#[cfg(test)]
mod tests;
