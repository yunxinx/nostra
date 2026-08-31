//! Scoped workspace definitions and immutable registry projections.

use std::{collections::BTreeMap, fmt, sync::Arc};

use super::{ScopeId, component::has_supported_name_characters};

/// Stable identity for a workspace definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(&'static str);

/// Stable identity of the built-in Chat workspace.
pub const CHAT_WORKSPACE_ID: WorkspaceId = WorkspaceId::new("nostra.workspace.chat");

/// Stable identity of the built-in Project workspace.
pub const PROJECT_WORKSPACE_ID: WorkspaceId = WorkspaceId::new("nostra.workspace.project");

impl WorkspaceId {
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(!value.is_empty(), "workspace ID must not be empty");
        assert!(
            has_supported_name_characters(value),
            "workspace ID contains an unsupported character"
        );
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Metadata required to order and identify one workspace contribution.
///
/// Runtime code deliberately keeps this value independent of any GUI factory;
/// the window-level host can associate an instance constructor without making
/// the registry depend on GPUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceDefinition {
    id: WorkspaceId,
    order: u32,
}

impl WorkspaceDefinition {
    #[must_use]
    pub const fn new(id: WorkspaceId, order: u32) -> Self {
        Self { id, order }
    }

    #[must_use]
    pub const fn id(&self) -> WorkspaceId {
        self.id
    }

    #[must_use]
    pub const fn order(&self) -> u32 {
        self.order
    }
}

/// A registration ownership token for one scope-local definition.
///
/// The token is intentionally not constructible from outside this module. A
/// disposer can therefore remove only the exact registration it received;
/// revoking an older token never removes a later definition with the same ID.
pub struct WorkspaceRegistration {
    scope: ScopeId,
    id: WorkspaceId,
    token: Arc<()>,
}

impl WorkspaceRegistration {
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn id(&self) -> WorkspaceId {
        self.id
    }
}

impl fmt::Debug for WorkspaceRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceRegistration")
            .field("scope", &self.scope)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceRegistryError {
    UnknownScope { scope: ScopeId },
    DuplicateScope { scope: ScopeId },
    InvalidParent { scope: ScopeId, parent: ScopeId },
    DuplicateDefinition { scope: ScopeId, id: WorkspaceId },
    RevisionExhausted,
}

impl fmt::Display for WorkspaceRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownScope { scope } => {
                write!(f, "workspace registry scope {} does not exist", scope.raw())
            }
            Self::DuplicateScope { scope } => {
                write!(
                    f,
                    "workspace registry scope {} is already registered",
                    scope.raw()
                )
            }
            Self::InvalidParent { scope, parent } => write!(
                f,
                "workspace registry scope {} cannot use missing parent scope {}",
                scope.raw(),
                parent.raw()
            ),
            Self::DuplicateDefinition { scope, id } => write!(
                f,
                "workspace `{id}` is already defined in scope {}",
                scope.raw()
            ),
            Self::RevisionExhausted => f.write_str("workspace registry revisions are exhausted"),
        }
    }
}

impl std::error::Error for WorkspaceRegistryError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRegistrySnapshot {
    revision: u64,
    scope: ScopeId,
    definitions: Arc<[WorkspaceDefinition]>,
}

impl WorkspaceRegistrySnapshot {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub fn definitions(&self) -> &[WorkspaceDefinition] {
        &self.definitions
    }

    #[must_use]
    pub fn get(&self, id: WorkspaceId) -> Option<&WorkspaceDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.id() == id)
    }
}

struct WorkspaceEntry {
    definition: WorkspaceDefinition,
    token: Arc<()>,
}

struct WorkspaceLayer {
    parent: Option<ScopeId>,
    definitions: BTreeMap<WorkspaceId, WorkspaceEntry>,
}

/// Scope-aware registry for workspace definitions.
///
/// Each scope owns its definitions. A snapshot walks from the requested scope
/// toward its ancestors, so the nearest definition shadows an ancestor with
/// the same [`WorkspaceId`]. The returned snapshot owns an immutable copy and
/// remains stable after later registry mutations.
pub struct WorkspaceRegistry {
    root: ScopeId,
    revision: u64,
    layers: BTreeMap<ScopeId, WorkspaceLayer>,
}

impl WorkspaceRegistry {
    #[must_use]
    pub fn new(root: ScopeId) -> Self {
        Self {
            root,
            revision: 0,
            layers: BTreeMap::from([(
                root,
                WorkspaceLayer {
                    parent: None,
                    definitions: BTreeMap::new(),
                },
            )]),
        }
    }

    #[must_use]
    pub const fn root(&self) -> ScopeId {
        self.root
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Add a scope layer whose definitions inherit from `parent`.
    pub fn add_scope(
        &mut self,
        scope: ScopeId,
        parent: ScopeId,
    ) -> Result<(), WorkspaceRegistryError> {
        if self.layers.contains_key(&scope) {
            return Err(WorkspaceRegistryError::DuplicateScope { scope });
        }
        if !self.layers.contains_key(&parent) {
            return Err(WorkspaceRegistryError::InvalidParent { scope, parent });
        }
        self.advance_revision()?;
        self.layers.insert(
            scope,
            WorkspaceLayer {
                parent: Some(parent),
                definitions: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// Register one definition in a scope and return its exact ownership token.
    pub fn register(
        &mut self,
        scope: ScopeId,
        definition: WorkspaceDefinition,
    ) -> Result<WorkspaceRegistration, WorkspaceRegistryError> {
        let layer = self
            .layers
            .get(&scope)
            .ok_or(WorkspaceRegistryError::UnknownScope { scope })?;
        if layer.definitions.contains_key(&definition.id()) {
            return Err(WorkspaceRegistryError::DuplicateDefinition {
                scope,
                id: definition.id(),
            });
        }
        self.advance_revision()?;
        let id = definition.id();
        let token = Arc::new(());
        self.layers
            .get_mut(&scope)
            .expect("scope was validated before registration")
            .definitions
            .insert(
                id,
                WorkspaceEntry {
                    definition,
                    token: Arc::clone(&token),
                },
            );
        Ok(WorkspaceRegistration { scope, id, token })
    }

    /// Revoke exactly the registration represented by `registration`.
    ///
    /// Repeated revocation is an idempotent no-op. A stale token cannot revoke
    /// a successor registration created after the original was removed.
    pub fn revoke(
        &mut self,
        registration: &WorkspaceRegistration,
    ) -> Result<bool, WorkspaceRegistryError> {
        let Some(layer) = self.layers.get(&registration.scope) else {
            return Ok(false);
        };
        let is_current = layer
            .definitions
            .get(&registration.id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.token, &registration.token));
        if !is_current {
            return Ok(false);
        }
        self.advance_revision()?;
        self.layers
            .get_mut(&registration.scope)
            .expect("scope remains registered while revoking")
            .definitions
            .remove(&registration.id);
        Ok(true)
    }

    /// Build an immutable, inheritance-resolved snapshot for `scope`.
    pub fn snapshot(
        &self,
        scope: ScopeId,
    ) -> Result<WorkspaceRegistrySnapshot, WorkspaceRegistryError> {
        let mut current = Some(
            self.layers
                .get(&scope)
                .ok_or(WorkspaceRegistryError::UnknownScope { scope })?,
        );
        let mut definitions = BTreeMap::<WorkspaceId, WorkspaceDefinition>::new();
        while let Some(layer) = current {
            for (id, entry) in &layer.definitions {
                definitions
                    .entry(*id)
                    .or_insert_with(|| entry.definition.clone());
            }
            current = layer.parent.and_then(|parent| self.layers.get(&parent));
        }

        let mut definitions = definitions.into_values().collect::<Vec<_>>();
        definitions.sort_unstable_by(|left, right| {
            left.order()
                .cmp(&right.order())
                .then_with(|| left.id().cmp(&right.id()))
        });
        Ok(WorkspaceRegistrySnapshot {
            revision: self.revision,
            scope,
            definitions: definitions.into(),
        })
    }

    fn advance_revision(&mut self) -> Result<(), WorkspaceRegistryError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(WorkspaceRegistryError::RevisionExhausted)?;
        Ok(())
    }
}
