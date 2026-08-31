//! Scope-aware registries for typed, independently owned contributions.

use std::{collections::BTreeMap, fmt, marker::PhantomData, sync::Arc};

use super::{ScopeId, component::has_supported_name_characters};

/// Marker for one contribution domain and its value type.
pub trait ContributionKey: 'static {
    type Value: Clone + 'static;

    const NAME: &'static str;
}

/// Stable identity of one contribution within a registry domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContributionId(&'static str);

impl ContributionId {
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(!value.is_empty(), "contribution ID must not be empty");
        assert!(
            has_supported_name_characters(value),
            "contribution ID contains an unsupported character"
        );
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ContributionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Metadata and value installed as one contribution.
pub struct ContributionDefinition<K: ContributionKey> {
    id: ContributionId,
    order: u32,
    value: K::Value,
    marker: PhantomData<K>,
}

impl<K: ContributionKey> Clone for ContributionDefinition<K> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            order: self.order,
            value: self.value.clone(),
            marker: PhantomData,
        }
    }
}

impl<K: ContributionKey> ContributionDefinition<K> {
    #[must_use]
    pub const fn new(id: ContributionId, order: u32, value: K::Value) -> Self {
        Self {
            id,
            order,
            value,
            marker: PhantomData,
        }
    }

    #[must_use]
    pub const fn id(&self) -> ContributionId {
        self.id
    }

    #[must_use]
    pub const fn order(&self) -> u32 {
        self.order
    }

    #[must_use]
    pub const fn value(&self) -> &K::Value {
        &self.value
    }
}

struct ContributionEntry<K: ContributionKey> {
    definition: ContributionDefinition<K>,
    generation: u64,
    token: Arc<()>,
}

/// Exact ownership token for one installed contribution.
pub struct ContributionRegistration<K: ContributionKey> {
    scope: ScopeId,
    id: ContributionId,
    generation: u64,
    token: Arc<()>,
    marker: PhantomData<K>,
}

impl<K: ContributionKey> ContributionRegistration<K> {
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn id(&self) -> ContributionId {
        self.id
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl<K: ContributionKey> fmt::Debug for ContributionRegistration<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContributionRegistration")
            .field("scope", &self.scope)
            .field("id", &self.id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ContributionSnapshotEntry<K: ContributionKey> {
    definition: ContributionDefinition<K>,
}

impl<K: ContributionKey> ContributionSnapshotEntry<K> {
    #[must_use]
    pub const fn id(&self) -> ContributionId {
        self.definition.id
    }

    #[must_use]
    pub const fn order(&self) -> u32 {
        self.definition.order
    }

    #[must_use]
    pub const fn value(&self) -> &K::Value {
        &self.definition.value
    }
}

/// Immutable projection of all visible contributions at one scope revision.
#[derive(Clone)]
pub struct ContributionSnapshot<K: ContributionKey> {
    revision: u64,
    scope: ScopeId,
    contributions: Arc<[ContributionSnapshotEntry<K>]>,
}

impl<K: ContributionKey> ContributionSnapshot<K> {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub fn contributions(&self) -> &[ContributionSnapshotEntry<K>] {
        &self.contributions
    }

    #[must_use]
    pub fn get(&self, id: ContributionId) -> Option<&ContributionSnapshotEntry<K>> {
        self.contributions
            .iter()
            .find(|contribution| contribution.id() == id)
    }
}

struct ContributionLayer<K: ContributionKey> {
    parent: Option<ScopeId>,
    contributions: BTreeMap<ContributionId, ContributionEntry<K>>,
}

/// Scope-aware registry for one typed contribution domain.
pub struct ContributionRegistry<K: ContributionKey> {
    root: ScopeId,
    revision: u64,
    next_generation: u64,
    layers: BTreeMap<ScopeId, ContributionLayer<K>>,
}

impl<K: ContributionKey> ContributionRegistry<K> {
    #[must_use]
    pub fn new(root: ScopeId) -> Self {
        assert!(
            !K::NAME.is_empty(),
            "contribution registry name must not be empty"
        );
        assert!(
            has_supported_name_characters(K::NAME),
            "contribution registry name contains an unsupported character"
        );
        Self {
            root,
            revision: 0,
            next_generation: 0,
            layers: BTreeMap::from([(
                root,
                ContributionLayer {
                    parent: None,
                    contributions: BTreeMap::new(),
                },
            )]),
        }
    }

    #[must_use]
    pub const fn root(&self) -> ScopeId {
        self.root
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        K::NAME
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn add_scope(
        &mut self,
        scope: ScopeId,
        parent: ScopeId,
    ) -> Result<(), ContributionRegistryError> {
        if self.layers.contains_key(&scope) {
            return Err(ContributionRegistryError::DuplicateScope { scope });
        }
        if !self.layers.contains_key(&parent) {
            return Err(ContributionRegistryError::InvalidParent { scope, parent });
        }
        self.advance_revision()?;
        self.layers.insert(
            scope,
            ContributionLayer {
                parent: Some(parent),
                contributions: BTreeMap::new(),
            },
        );
        Ok(())
    }

    pub fn register(
        &mut self,
        scope: ScopeId,
        definition: ContributionDefinition<K>,
    ) -> Result<ContributionRegistration<K>, ContributionRegistryError> {
        let mut registrations = self.register_batch(scope, [definition])?;
        match registrations.pop() {
            Some(registration) => Ok(registration),
            None => unreachable!("a non-empty contribution batch returns one registration"),
        }
    }

    /// Validate every candidate before changing the registry or its revision.
    pub fn register_batch<I>(
        &mut self,
        scope: ScopeId,
        definitions: I,
    ) -> Result<Vec<ContributionRegistration<K>>, ContributionRegistryError>
    where
        I: IntoIterator<Item = ContributionDefinition<K>>,
    {
        let definitions = definitions.into_iter().collect::<Vec<_>>();
        if definitions.is_empty() {
            return Ok(Vec::new());
        }
        let Some(layer) = self.layers.get(&scope) else {
            return Err(ContributionRegistryError::UnknownScope { scope });
        };

        let mut candidate_ids = std::collections::BTreeSet::new();
        for definition in &definitions {
            if !candidate_ids.insert(definition.id())
                || layer.contributions.contains_key(&definition.id())
            {
                return Err(ContributionRegistryError::DuplicateContribution {
                    scope,
                    id: definition.id(),
                });
            }
        }
        let count = u64::try_from(definitions.len())
            .map_err(|_| ContributionRegistryError::GenerationExhausted)?;
        self.next_generation
            .checked_add(count)
            .ok_or(ContributionRegistryError::GenerationExhausted)?;
        self.advance_revision()?;

        let mut registrations = Vec::with_capacity(definitions.len());
        let layer = self
            .layers
            .get_mut(&scope)
            .expect("scope was validated before contribution registration");
        for definition in definitions {
            self.next_generation = self
                .next_generation
                .checked_add(1)
                .expect("generation capacity was validated before registration");
            let generation = self.next_generation;
            let id = definition.id();
            let token = Arc::new(());
            layer.contributions.insert(
                id,
                ContributionEntry {
                    definition,
                    generation,
                    token: Arc::clone(&token),
                },
            );
            registrations.push(ContributionRegistration {
                scope,
                id,
                generation,
                token,
                marker: PhantomData,
            });
        }
        Ok(registrations)
    }

    /// Revoke only the exact generation represented by `registration`.
    pub fn revoke(
        &mut self,
        registration: &ContributionRegistration<K>,
    ) -> Result<bool, ContributionRegistryError> {
        let Some(layer) = self.layers.get(&registration.scope) else {
            return Ok(false);
        };
        let is_current = layer
            .contributions
            .get(&registration.id)
            .is_some_and(|entry| {
                entry.generation == registration.generation
                    && Arc::ptr_eq(&entry.token, &registration.token)
            });
        if !is_current {
            return Ok(false);
        }
        self.advance_revision()?;
        self.layers
            .get_mut(&registration.scope)
            .expect("scope remains registered while revoking")
            .contributions
            .remove(&registration.id);
        Ok(true)
    }

    pub fn snapshot(
        &self,
        scope: ScopeId,
    ) -> Result<ContributionSnapshot<K>, ContributionRegistryError> {
        let mut current = Some(
            self.layers
                .get(&scope)
                .ok_or(ContributionRegistryError::UnknownScope { scope })?,
        );
        let mut contributions = BTreeMap::<ContributionId, ContributionSnapshotEntry<K>>::new();
        while let Some(layer) = current {
            for (id, entry) in &layer.contributions {
                contributions
                    .entry(*id)
                    .or_insert_with(|| ContributionSnapshotEntry {
                        definition: ContributionDefinition {
                            id: entry.definition.id,
                            order: entry.definition.order,
                            value: entry.definition.value.clone(),
                            marker: PhantomData,
                        },
                    });
            }
            current = layer.parent.and_then(|parent| self.layers.get(&parent));
        }

        let mut contributions = contributions.into_values().collect::<Vec<_>>();
        contributions.sort_unstable_by(|left, right| {
            left.order()
                .cmp(&right.order())
                .then_with(|| left.id().cmp(&right.id()))
        });
        Ok(ContributionSnapshot {
            revision: self.revision,
            scope,
            contributions: contributions.into(),
        })
    }

    fn advance_revision(&mut self) -> Result<(), ContributionRegistryError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(ContributionRegistryError::RevisionExhausted)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContributionRegistryError {
    UnknownScope { scope: ScopeId },
    DuplicateScope { scope: ScopeId },
    InvalidParent { scope: ScopeId, parent: ScopeId },
    DuplicateContribution { scope: ScopeId, id: ContributionId },
    RevisionExhausted,
    GenerationExhausted,
}

impl fmt::Display for ContributionRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownScope { scope } => {
                write!(
                    f,
                    "contribution registry scope {} does not exist",
                    scope.raw()
                )
            }
            Self::DuplicateScope { scope } => write!(
                f,
                "contribution registry scope {} is already registered",
                scope.raw()
            ),
            Self::InvalidParent { scope, parent } => write!(
                f,
                "contribution registry scope {} cannot use missing parent scope {}",
                scope.raw(),
                parent.raw()
            ),
            Self::DuplicateContribution { scope, id } => write!(
                f,
                "contribution `{id}` is already defined in scope {}",
                scope.raw()
            ),
            Self::RevisionExhausted => f.write_str("contribution registry revisions are exhausted"),
            Self::GenerationExhausted => {
                f.write_str("contribution registry generations are exhausted")
            }
        }
    }
}

impl std::error::Error for ContributionRegistryError {}
