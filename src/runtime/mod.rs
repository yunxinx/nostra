//! Typed identities and composition primitives for Nostra's in-process runtime.

mod capability;
mod component;
mod composition;
mod dependency;
mod diagnostics;
mod effect;
mod exit;
mod reconcile;
mod scope;
mod workspace;

pub use capability::{
    CapabilityId, CapabilityKey, CapabilityLease, ExclusiveCapabilitySlot, ExclusiveSlotError,
    PreparedCapability, ProviderRegistration,
};
pub use component::{ComponentGeneration, ComponentId, ScopeId};
pub(crate) use composition::default_generation_service;
pub use composition::{
    CompositionBuildError, CompositionRoot, CompositionRootBuilder, ConversationScopeHandle,
    GenerationCapability, PreferenceCapability, RuntimeServices, SessionServicesCapability,
};
pub use dependency::{
    ActivationFingerprint, DependencyDeclaration, DependencyResolution, DependencyResolver,
    DependencyResolverError, DependencySnapshot, PendingDependencies, ResolvedDependency,
};
pub use diagnostics::{
    ComponentSnapshot, ComponentSnapshotDetails, ComponentSnapshotViolation, ContributionRevision,
    MissingDependencySnapshot, RuntimeComponentState, RuntimeDiagnostic, RuntimeResourceCounts,
    RuntimeSnapshot, RuntimeSnapshotError, ScopedComponentId, StartupAuditError, StartupPolicy,
    TransitionSnapshot,
};
pub use effect::{AsyncStop, DisposeError, EffectScope};
pub use exit::{ExitCoordinator, ExitReport, NORMAL_EXIT_TIMEOUT, QUIT_FALLBACK_TIMEOUT};
pub use reconcile::{
    ComponentLifecycle, DesiredRevision, DesiredRevisionExhausted, ReconcileFailure,
    ReconcileFailureKind, ReconcileObserver, ReconcileStage, ReconcileStatus, ReconcileTarget,
    ReconcileTransition, ScopeLocalReconciler,
};
pub use scope::{ScopeError, ScopeKind, ScopeState, ScopeTree};
pub use workspace::{
    WorkspaceDefinition, WorkspaceId, WorkspaceRegistration, WorkspaceRegistry,
    WorkspaceRegistryError, WorkspaceRegistrySnapshot,
};

#[cfg(test)]
mod tests;
