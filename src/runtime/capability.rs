//! Typed capability identities, leases, and exclusive Provider slots.

use std::{any::TypeId, fmt, marker::PhantomData, sync::Arc};

use super::component::{ComponentGeneration, ComponentId, ScopeId, has_supported_name_characters};

pub trait CapabilityKey: 'static {
    type Handle: Clone + 'static;

    const NAME: &'static str;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilityId {
    key_type: TypeId,
    name: &'static str,
}

impl CapabilityId {
    #[must_use]
    pub fn of<K: CapabilityKey>() -> Self {
        assert!(!K::NAME.is_empty(), "capability name must not be empty");
        assert!(
            has_supported_name_characters(K::NAME),
            "capability name contains an unsupported character"
        );
        Self {
            key_type: TypeId::of::<K>(),
            name: K::NAME,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub fn is<K: CapabilityKey>(self) -> bool {
        self.key_type == TypeId::of::<K>()
    }
}

impl fmt::Debug for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CapabilityId").field(&self.name).finish()
    }
}

pub struct PreparedCapability<K: CapabilityKey> {
    provider: ComponentId,
    handle: K::Handle,
}

impl<K: CapabilityKey> PreparedCapability<K> {
    #[must_use]
    pub const fn provider(&self) -> ComponentId {
        self.provider
    }
}

impl<K: CapabilityKey> fmt::Debug for PreparedCapability<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedCapability")
            .field("capability", &K::NAME)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

pub struct CapabilityLease<K: CapabilityKey> {
    scope: ScopeId,
    provider: ComponentId,
    generation: ComponentGeneration,
    registration: Arc<()>,
    handle: K::Handle,
}

impl<K: CapabilityKey> CapabilityLease<K> {
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn provider(&self) -> ComponentId {
        self.provider
    }

    #[must_use]
    pub const fn generation(&self) -> ComponentGeneration {
        self.generation
    }

    #[must_use]
    pub const fn handle(&self) -> &K::Handle {
        &self.handle
    }
}

impl<K: CapabilityKey> Clone for CapabilityLease<K> {
    fn clone(&self) -> Self {
        Self {
            scope: self.scope,
            provider: self.provider,
            generation: self.generation,
            registration: Arc::clone(&self.registration),
            handle: self.handle.clone(),
        }
    }
}

impl<K: CapabilityKey> fmt::Debug for CapabilityLease<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityLease")
            .field("capability", &K::NAME)
            .field("scope", &self.scope)
            .field("provider", &self.provider)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

pub struct ProviderRegistration<K: CapabilityKey> {
    scope: ScopeId,
    provider: ComponentId,
    generation: ComponentGeneration,
    registration: Arc<()>,
    key: PhantomData<fn() -> K>,
}

impl<K: CapabilityKey> ProviderRegistration<K> {
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn provider(&self) -> ComponentId {
        self.provider
    }

    #[must_use]
    pub const fn generation(&self) -> ComponentGeneration {
        self.generation
    }
}

impl<K: CapabilityKey> fmt::Debug for ProviderRegistration<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistration")
            .field("capability", &K::NAME)
            .field("scope", &self.scope)
            .field("provider", &self.provider)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExclusiveSlotError {
    Occupied {
        capability: CapabilityId,
        scope: ScopeId,
        current_provider: ComponentId,
        attempted_provider: ComponentId,
    },
    Vacant {
        capability: CapabilityId,
        scope: ScopeId,
        attempted_provider: ComponentId,
    },
    GenerationExhausted {
        capability: CapabilityId,
        scope: ScopeId,
        attempted_provider: ComponentId,
    },
}

impl fmt::Display for ExclusiveSlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied {
                capability,
                scope,
                current_provider,
                attempted_provider,
            } => write!(
                f,
                "capability `{}` in scope {} is provided by `{current_provider}`; `{attempted_provider}` requires explicit replacement",
                capability.name(),
                scope.raw()
            ),
            Self::Vacant {
                capability,
                scope,
                attempted_provider,
            } => write!(
                f,
                "capability `{}` in scope {} has no Provider for `{attempted_provider}` to replace",
                capability.name(),
                scope.raw()
            ),
            Self::GenerationExhausted {
                capability,
                scope,
                attempted_provider,
            } => write!(
                f,
                "capability `{}` in scope {} exhausted Provider generations before publishing `{attempted_provider}`",
                capability.name(),
                scope.raw()
            ),
        }
    }
}

impl std::error::Error for ExclusiveSlotError {}

pub struct ExclusiveCapabilitySlot<K: CapabilityKey> {
    capability: CapabilityId,
    scope: ScopeId,
    current: Option<CapabilityLease<K>>,
    last_generation: Option<ComponentGeneration>,
}

impl<K: CapabilityKey> ExclusiveCapabilitySlot<K> {
    #[must_use]
    pub fn new(scope: ScopeId) -> Self {
        Self {
            capability: CapabilityId::of::<K>(),
            scope,
            current: None,
            last_generation: None,
        }
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    pub fn prepare_candidate<E>(
        &self,
        provider: ComponentId,
        prepare: impl FnOnce() -> Result<K::Handle, E>,
    ) -> Result<PreparedCapability<K>, E> {
        prepare().map(|handle| PreparedCapability { provider, handle })
    }

    pub fn install(
        &mut self,
        candidate: PreparedCapability<K>,
    ) -> Result<ProviderRegistration<K>, ExclusiveSlotError> {
        if let Some(current) = &self.current {
            return Err(ExclusiveSlotError::Occupied {
                capability: self.capability,
                scope: self.scope,
                current_provider: current.provider,
                attempted_provider: candidate.provider,
            });
        }
        self.publish(candidate)
    }

    pub fn replace(
        &mut self,
        candidate: PreparedCapability<K>,
    ) -> Result<ProviderRegistration<K>, ExclusiveSlotError> {
        if self.current.is_none() {
            return Err(ExclusiveSlotError::Vacant {
                capability: self.capability,
                scope: self.scope,
                attempted_provider: candidate.provider,
            });
        }
        self.publish(candidate)
    }

    #[must_use]
    pub fn current(&self) -> Option<CapabilityLease<K>> {
        self.current.clone()
    }

    pub fn revoke(&mut self, registration: &ProviderRegistration<K>) -> bool {
        let is_current = self.current.as_ref().is_some_and(|current| {
            current.scope == registration.scope
                && current.provider == registration.provider
                && current.generation == registration.generation
                && Arc::ptr_eq(&current.registration, &registration.registration)
        });
        if is_current {
            self.current = None;
        }
        is_current
    }

    fn publish(
        &mut self,
        candidate: PreparedCapability<K>,
    ) -> Result<ProviderRegistration<K>, ExclusiveSlotError> {
        let generation = self
            .last_generation
            .map_or(
                Some(ComponentGeneration::INITIAL),
                ComponentGeneration::next,
            )
            .ok_or(ExclusiveSlotError::GenerationExhausted {
                capability: self.capability,
                scope: self.scope,
                attempted_provider: candidate.provider,
            })?;
        let registration = Arc::new(());
        self.current = Some(CapabilityLease {
            scope: self.scope,
            provider: candidate.provider,
            generation,
            registration: Arc::clone(&registration),
            handle: candidate.handle,
        });
        self.last_generation = Some(generation);
        Ok(ProviderRegistration {
            scope: self.scope,
            provider: candidate.provider,
            generation,
            registration,
            key: PhantomData,
        })
    }
}
