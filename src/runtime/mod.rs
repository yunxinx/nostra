//! Typed identities and composition primitives for Nostra's in-process runtime.

mod capability;
mod component;
mod dependency;
mod effect;
mod reconcile;
mod scope;

pub use capability::{
    CapabilityId, CapabilityKey, CapabilityLease, ExclusiveCapabilitySlot, ExclusiveSlotError,
    PreparedCapability, ProviderRegistration,
};
pub use component::{ComponentGeneration, ComponentId, ScopeId};
pub use dependency::{
    ActivationFingerprint, DependencyDeclaration, DependencyResolution, DependencyResolver,
    DependencyResolverError, DependencySnapshot, PendingDependencies, ResolvedDependency,
};
pub use effect::{AsyncStop, DisposeError, EffectScope};
pub use reconcile::{
    ComponentLifecycle, DesiredRevision, DesiredRevisionExhausted, ReconcileFailure,
    ReconcileFailureKind, ReconcileStage, ReconcileStatus, ReconcileTarget, ScopeLocalReconciler,
};
pub use scope::{ScopeError, ScopeKind, ScopeState, ScopeTree};

#[cfg(test)]
mod tests;
