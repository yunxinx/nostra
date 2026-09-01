//! Typed Provider definitions and explicit selection.

use std::{collections::BTreeMap, fmt, rc::Rc};

use super::{AsyncStop, CapabilityId, CapabilityKey, ComponentId, EffectScope};

pub(super) type ProviderFactory<K> = Rc<dyn Fn() -> anyhow::Result<PreparedProvider<K>> + 'static>;

type ProviderActivation = Box<dyn FnOnce(&mut EffectScope) -> anyhow::Result<()> + 'static>;

/// A privately prepared Provider and every reversible effect it already owns.
///
/// The handle is not published until activation succeeds. Preparation and
/// activation effects remain in separate ordered phases, move into the active
/// Provider on success, and remain available for explicit asynchronous rollback
/// on failure.
#[must_use = "prepared Providers must be activated or rolled back"]
pub struct PreparedProvider<K: CapabilityKey> {
    handle: K::Handle,
    preparation_effects: EffectScope,
    activation_effects: EffectScope,
    activation: Option<ProviderActivation>,
}

impl<K: CapabilityKey> PreparedProvider<K> {
    pub fn new(handle: K::Handle) -> Self {
        Self {
            handle,
            preparation_effects: EffectScope::new(),
            activation_effects: EffectScope::new(),
            activation: None,
        }
    }

    pub fn on_activate(
        mut self,
        activation: impl FnOnce(&mut EffectScope) -> anyhow::Result<()> + 'static,
    ) -> Self {
        self.activation = Some(Box::new(activation));
        self
    }

    pub fn own_sync(&mut self, undo: impl FnOnce() + 'static) {
        self.preparation_effects.own_sync(undo);
    }

    pub fn own_resource<T: 'static>(&mut self, resource: T) {
        self.preparation_effects.own_resource(resource);
    }

    pub fn own_async(&mut self, stop: impl AsyncStop) {
        self.preparation_effects.own_async(stop);
    }

    pub(super) fn handle(&self) -> &K::Handle {
        &self.handle
    }

    pub(super) fn activate(&mut self) -> anyhow::Result<()> {
        let Some(activation) = self.activation.take() else {
            return Ok(());
        };
        activation(&mut self.activation_effects)
    }

    pub(super) async fn rollback(&mut self) -> anyhow::Result<()> {
        self.rollback_activation().await?;
        self.rollback_preparation().await
    }

    pub(super) async fn rollback_activation(&mut self) -> anyhow::Result<()> {
        self.activation_effects.quiesce_and_dispose().await
    }

    pub(super) async fn rollback_preparation(&mut self) -> anyhow::Result<()> {
        self.preparation_effects.quiesce_and_dispose().await
    }

    pub(super) fn effect_count(&self) -> usize {
        self.preparation_effects.effect_count() + self.activation_effects.effect_count()
    }

    pub(super) fn take_effects(&mut self) -> (EffectScope, EffectScope) {
        debug_assert!(self.activation.is_none());
        (
            std::mem::take(&mut self.preparation_effects),
            std::mem::take(&mut self.activation_effects),
        )
    }
}

impl<K: CapabilityKey> fmt::Debug for PreparedProvider<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedProvider")
            .field("capability", &K::NAME)
            .field("effect_count", &self.effect_count())
            .field("activation_pending", &self.activation.is_some())
            .finish_non_exhaustive()
    }
}

struct ProviderDefinition<K: CapabilityKey> {
    prepare: ProviderFactory<K>,
}

impl<K: CapabilityKey> Clone for ProviderDefinition<K> {
    fn clone(&self) -> Self {
        Self {
            prepare: Rc::clone(&self.prepare),
        }
    }
}

impl<K: CapabilityKey> ProviderDefinition<K> {
    fn prepare(&self) -> anyhow::Result<PreparedProvider<K>> {
        (self.prepare)()
    }
}

pub(super) fn ready_provider<K: CapabilityKey>(handle: K::Handle) -> ProviderFactory<K> {
    Rc::new(move || Ok(PreparedProvider::new(handle.clone())))
}

pub(super) fn provider_factory<K: CapabilityKey>(
    prepare: impl Fn() -> anyhow::Result<K::Handle> + 'static,
) -> ProviderFactory<K> {
    Rc::new(move || prepare().map(PreparedProvider::new))
}

pub(super) fn prepared_provider_factory<K: CapabilityKey>(
    prepare: impl Fn() -> anyhow::Result<PreparedProvider<K>> + 'static,
) -> ProviderFactory<K> {
    Rc::new(prepare)
}

pub(super) struct ProviderDefinitions<K: CapabilityKey> {
    providers: BTreeMap<ComponentId, ProviderDefinition<K>>,
    selected: ComponentId,
}

impl<K: CapabilityKey> ProviderDefinitions<K> {
    pub(super) fn with_factories(
        providers: impl IntoIterator<Item = (ComponentId, ProviderFactory<K>)>,
        selected: ComponentId,
    ) -> Result<Self, ProviderSelectionError> {
        Self::from_definitions(
            providers
                .into_iter()
                .map(|(provider, prepare)| (provider, ProviderDefinition { prepare })),
            selected,
        )
    }

    fn from_definitions(
        providers: impl IntoIterator<Item = (ComponentId, ProviderDefinition<K>)>,
        selected: ComponentId,
    ) -> Result<Self, ProviderSelectionError> {
        let capability = CapabilityId::of::<K>();
        let mut definitions = BTreeMap::new();
        for (provider, definition) in providers {
            if definitions.insert(provider, definition).is_some() {
                return Err(ProviderSelectionError::Duplicate {
                    capability,
                    provider,
                });
            }
        }
        if !definitions.contains_key(&selected) {
            return Err(ProviderSelectionError::Unknown {
                capability,
                provider: selected,
            });
        }
        Ok(Self {
            providers: definitions,
            selected,
        })
    }

    pub(super) fn prepare_selected(
        &self,
    ) -> Result<(ComponentId, PreparedProvider<K>), ProviderPrepareError> {
        self.prepare(self.selected)
    }

    pub(super) fn prepare(
        &self,
        provider: ComponentId,
    ) -> Result<(ComponentId, PreparedProvider<K>), ProviderPrepareError> {
        let definition = self.providers.get(&provider).ok_or(ProviderPrepareError {
            capability: CapabilityId::of::<K>(),
            provider,
            source: None,
        })?;
        definition
            .prepare()
            .map(|handle| (provider, handle))
            .map_err(|source| ProviderPrepareError {
                capability: CapabilityId::of::<K>(),
                provider,
                source: Some(source),
            })
    }

    pub(super) fn contains(&self, provider: ComponentId) -> bool {
        self.providers.contains_key(&provider)
    }

    pub(super) fn ids(&self) -> impl ExactSizeIterator<Item = ComponentId> + '_ {
        self.providers.keys().copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSelectionError {
    Duplicate {
        capability: CapabilityId,
        provider: ComponentId,
    },
    Unknown {
        capability: CapabilityId,
        provider: ComponentId,
    },
    ComponentCollision {
        provider: ComponentId,
        first: CapabilityId,
        second: CapabilityId,
    },
}

impl fmt::Display for ProviderSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate {
                capability,
                provider,
            } => write!(
                f,
                "capability `{}` registers Provider `{provider}` more than once",
                capability.name()
            ),
            Self::Unknown {
                capability,
                provider,
            } => write!(
                f,
                "capability `{}` selects unregistered Provider `{provider}`",
                capability.name()
            ),
            Self::ComponentCollision {
                provider,
                first,
                second,
            } => write!(
                f,
                "component `{provider}` is registered for both `{}` and `{}`",
                first.name(),
                second.name()
            ),
        }
    }
}

impl std::error::Error for ProviderSelectionError {}

pub struct ProviderPrepareError {
    capability: CapabilityId,
    provider: ComponentId,
    source: Option<anyhow::Error>,
}

impl ProviderPrepareError {
    #[must_use]
    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    #[must_use]
    pub const fn provider(&self) -> ComponentId {
        self.provider
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderPrepareFailureKind {
        if self.source.is_some() {
            ProviderPrepareFailureKind::Factory
        } else {
            ProviderPrepareFailureKind::UnknownProvider
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderPrepareFailureKind {
    UnknownProvider,
    Factory,
}

impl ProviderPrepareFailureKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownProvider => "unknown_provider",
            Self::Factory => "prepare_failed",
        }
    }
}

impl fmt::Debug for ProviderPrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderPrepareError")
            .field("capability", &self.capability.name())
            .field("provider", &self.provider)
            .field("kind", &self.kind())
            .finish()
    }
}

impl fmt::Display for ProviderPrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            ProviderPrepareFailureKind::Factory => write!(
                f,
                "Provider `{}` for capability `{}` failed to prepare",
                self.provider,
                self.capability.name()
            ),
            ProviderPrepareFailureKind::UnknownProvider => write!(
                f,
                "Provider `{}` is not registered for capability `{}`",
                self.provider,
                self.capability.name()
            ),
        }
    }
}

impl std::error::Error for ProviderPrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source.as_ref())
    }
}
