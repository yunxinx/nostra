//! Typed identities and composition primitives for Nostra's in-process runtime.

mod capability;
mod component;
mod dependency;

pub use capability::{
    CapabilityId, CapabilityKey, CapabilityLease, ExclusiveCapabilitySlot, ExclusiveSlotError,
    PreparedCapability, ProviderRegistration,
};
pub use component::{ComponentGeneration, ComponentId, ScopeId};
pub use dependency::{
    ActivationFingerprint, DependencyDeclaration, DependencyResolution, DependencyResolver,
    DependencyResolverError, DependencySnapshot, PendingDependencies, ResolvedDependency,
};

#[cfg(test)]
mod tests;
