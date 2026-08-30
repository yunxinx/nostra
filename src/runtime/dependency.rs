//! Required dependency resolution and activation identity snapshots.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    rc::Rc,
};

use super::{
    CapabilityId, CapabilityKey, CapabilityLease, ComponentGeneration, ComponentId,
    ExclusiveCapabilitySlot, ScopeId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DependencyDeclaration {
    capability: CapabilityId,
}

impl DependencyDeclaration {
    #[must_use]
    pub fn required<K: CapabilityKey>() -> Self {
        Self {
            capability: CapabilityId::of::<K>(),
        }
    }

    #[must_use]
    pub const fn capability(self) -> CapabilityId {
        self.capability
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResolvedDependency {
    capability: CapabilityId,
    scope: ScopeId,
    provider: ComponentId,
    generation: ComponentGeneration,
}

impl ResolvedDependency {
    #[must_use]
    pub const fn capability(self) -> CapabilityId {
        self.capability
    }

    #[must_use]
    pub const fn scope(self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn provider(self) -> ComponentId {
        self.provider
    }

    #[must_use]
    pub const fn generation(self) -> ComponentGeneration {
        self.generation
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActivationFingerprint(Box<[ResolvedDependency]>);

impl ActivationFingerprint {
    #[must_use]
    pub fn bindings(&self) -> &[ResolvedDependency] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDependencies {
    missing: Box<[CapabilityId]>,
}

impl PendingDependencies {
    #[must_use]
    pub fn missing(&self) -> &[CapabilityId] {
        &self.missing
    }
}

#[derive(Clone)]
struct ErasedCapabilityLease {
    binding: ResolvedDependency,
    lease: Rc<dyn Any>,
}

impl ErasedCapabilityLease {
    fn new<K: CapabilityKey>(lease: CapabilityLease<K>) -> Self {
        Self {
            binding: ResolvedDependency {
                capability: CapabilityId::of::<K>(),
                scope: lease.scope(),
                provider: lease.provider(),
                generation: lease.generation(),
            },
            lease: Rc::new(lease),
        }
    }

    fn typed<K: CapabilityKey>(&self) -> Option<CapabilityLease<K>> {
        self.lease
            .as_ref()
            .downcast_ref::<CapabilityLease<K>>()
            .cloned()
    }
}

trait ErasedCapabilitySource {
    fn scope(&self) -> ScopeId;
    fn current(&self) -> Option<ErasedCapabilityLease>;
}

impl<K: CapabilityKey> ErasedCapabilitySource for ExclusiveCapabilitySlot<K> {
    fn scope(&self) -> ScopeId {
        ExclusiveCapabilitySlot::scope(self)
    }

    fn current(&self) -> Option<ErasedCapabilityLease> {
        ExclusiveCapabilitySlot::current(self).map(ErasedCapabilityLease::new)
    }
}

#[derive(Clone)]
pub struct DependencySnapshot {
    leases: BTreeMap<CapabilityId, ErasedCapabilityLease>,
    activation_fingerprint: ActivationFingerprint,
}

impl DependencySnapshot {
    #[must_use]
    pub fn activation_fingerprint(&self) -> &ActivationFingerprint {
        &self.activation_fingerprint
    }

    #[must_use]
    pub fn lease<K: CapabilityKey>(&self) -> Option<CapabilityLease<K>> {
        self.leases
            .get(&CapabilityId::of::<K>())
            .and_then(ErasedCapabilityLease::typed::<K>)
    }
}

impl fmt::Debug for DependencySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DependencySnapshot")
            .field("bindings", &self.activation_fingerprint.bindings())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum DependencyResolution {
    Pending(PendingDependencies),
    Ready(DependencySnapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyResolverError {
    DuplicateSource {
        capability: CapabilityId,
        existing_scope: ScopeId,
        attempted_scope: ScopeId,
    },
}

impl fmt::Display for DependencyResolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSource {
                capability,
                existing_scope,
                attempted_scope,
            } => write!(
                f,
                "capability `{}` already has a dependency source in scope {}; scope {} must be selected explicitly",
                capability.name(),
                existing_scope.raw(),
                attempted_scope.raw()
            ),
        }
    }
}

impl std::error::Error for DependencyResolverError {}

#[derive(Default)]
pub struct DependencyResolver<'a> {
    sources: BTreeMap<CapabilityId, &'a dyn ErasedCapabilitySource>,
}

impl<'a> DependencyResolver<'a> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn include<K: CapabilityKey>(
        &mut self,
        slot: &'a ExclusiveCapabilitySlot<K>,
    ) -> Result<(), DependencyResolverError> {
        let capability = CapabilityId::of::<K>();
        if let Some(existing) = self.sources.get(&capability) {
            return Err(DependencyResolverError::DuplicateSource {
                capability,
                existing_scope: existing.scope(),
                attempted_scope: slot.scope(),
            });
        }
        self.sources.insert(capability, slot);
        Ok(())
    }

    #[must_use]
    pub fn resolve(self, declarations: &[DependencyDeclaration]) -> DependencyResolution {
        let required = declarations
            .iter()
            .map(|declaration| declaration.capability)
            .collect::<BTreeSet<_>>();
        let mut missing = Vec::new();
        let mut leases = BTreeMap::new();
        for capability in required {
            match self
                .sources
                .get(&capability)
                .and_then(|source| source.current())
            {
                Some(lease) => {
                    leases.insert(capability, lease);
                }
                None => missing.push(capability),
            }
        }
        if !missing.is_empty() {
            return DependencyResolution::Pending(PendingDependencies {
                missing: missing.into_boxed_slice(),
            });
        }
        let activation_fingerprint = ActivationFingerprint(
            leases
                .values()
                .map(|lease| lease.binding)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        DependencyResolution::Ready(DependencySnapshot {
            leases,
            activation_fingerprint,
        })
    }
}

impl fmt::Debug for DependencyResolver<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DependencyResolver")
            .field(
                "sources",
                &self
                    .sources
                    .iter()
                    .map(|(capability, source)| (*capability, source.scope()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
