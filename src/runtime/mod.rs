//! Typed identities and composition primitives for Nostra's in-process runtime.

mod capability;
mod component;
mod composition;
mod contribution;
mod dependency;
mod diagnostics;
mod effect;
mod exit;
mod generation_mount;
mod provider;
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
pub use dependency::{
    ActivationFingerprint, DependencyDeclaration, DependencyResolution, DependencyResolver,
    DependencyResolverError, DependencySnapshot, PendingDependencies, ResolvedDependency,
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
pub use reconcile::{
    ComponentLifecycle, DesiredRevision, DesiredRevisionExhausted, ReconcileFailure,
    ReconcileFailureKind, ReconcileObserver, ReconcileStage, ReconcileStatus, ReconcileTarget,
    ReconcileTransition, ScopeLocalReconciler,
};
pub use scope::{ScopeError, ScopeKind, ScopeState, ScopeTree};
pub use workspace::{
    CHAT_WORKSPACE_ID, PROJECT_WORKSPACE_ID, WorkspaceDefinition, WorkspaceId,
    WorkspaceRegistration, WorkspaceRegistry, WorkspaceRegistryError, WorkspaceRegistrySnapshot,
};

#[cfg(test)]
mod tests;
