//! Typed identities and composition primitives for Nostra's in-process runtime.

mod capability;
mod component;

pub use capability::{
    CapabilityId, CapabilityKey, CapabilityLease, ExclusiveCapabilitySlot, ExclusiveSlotError,
    PreparedCapability, ProviderRegistration,
};
pub use component::{ComponentGeneration, ComponentId, ScopeId};

#[cfg(test)]
mod tests;
