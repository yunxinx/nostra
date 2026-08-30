//! Runtime scope hierarchy, inherited capabilities, and ordered closure.

use std::{any::Any, collections::BTreeMap, fmt, mem};

use super::{
    AsyncStop, CapabilityId, CapabilityKey, CapabilityLease, DisposeError, EffectScope,
    ExclusiveCapabilitySlot, ScopeId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Application,
    Window,
    Conversation,
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Application => f.write_str("application"),
            Self::Window => f.write_str("window"),
            Self::Conversation => f.write_str("conversation"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeState {
    Open,
    Closing,
    Closed,
}

#[derive(Debug)]
pub enum ScopeError {
    Unknown {
        scope: ScopeId,
    },
    NotOpen {
        scope: ScopeId,
        state: ScopeState,
    },
    InvalidParent {
        scope: ScopeId,
        expected: ScopeKind,
        actual: ScopeKind,
    },
    IdExhausted,
    Dispose {
        scope: ScopeId,
        source: DisposeError,
    },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { scope } => write!(f, "runtime scope {} does not exist", scope.raw()),
            Self::NotOpen { scope, state } => {
                write!(f, "runtime scope {} is {state:?}", scope.raw())
            }
            Self::InvalidParent {
                scope,
                expected,
                actual,
            } => write!(
                f,
                "runtime scope {} is {actual}; a {expected} scope is required",
                scope.raw()
            ),
            Self::IdExhausted => f.write_str("runtime scope IDs are exhausted"),
            Self::Dispose { scope, source } => write!(
                f,
                "runtime scope {} could not finish closing: {source}",
                scope.raw()
            ),
        }
    }
}

impl std::error::Error for ScopeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Dispose { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

struct ScopeNode {
    kind: ScopeKind,
    parent: Option<ScopeId>,
    children: Vec<ScopeId>,
    state: ScopeState,
    effects: EffectScope,
    capabilities: BTreeMap<CapabilityId, Box<dyn Any>>,
}

impl ScopeNode {
    fn new(kind: ScopeKind, parent: Option<ScopeId>) -> Self {
        Self {
            kind,
            parent,
            children: Vec::new(),
            state: ScopeState::Open,
            effects: EffectScope::new(),
            capabilities: BTreeMap::new(),
        }
    }
}

#[must_use = "scope trees must be explicitly closed to establish quiescence"]
pub struct ScopeTree {
    application: ScopeId,
    next_id: Option<u64>,
    nodes: BTreeMap<ScopeId, ScopeNode>,
}

impl ScopeTree {
    pub fn new() -> Self {
        let application = ScopeId::new(0);
        Self {
            application,
            next_id: Some(1),
            nodes: BTreeMap::from([(application, ScopeNode::new(ScopeKind::Application, None))]),
        }
    }

    #[must_use]
    pub const fn application(&self) -> ScopeId {
        self.application
    }

    pub fn create_window(&mut self) -> Result<ScopeId, ScopeError> {
        self.create_child(self.application, ScopeKind::Application, ScopeKind::Window)
    }

    pub fn create_conversation(&mut self, window: ScopeId) -> Result<ScopeId, ScopeError> {
        self.create_child(window, ScopeKind::Window, ScopeKind::Conversation)
    }

    #[must_use]
    pub fn state(&self, scope: ScopeId) -> Option<ScopeState> {
        self.nodes.get(&scope).map(|node| node.state)
    }

    #[must_use]
    pub fn kind(&self, scope: ScopeId) -> Option<ScopeKind> {
        self.nodes.get(&scope).map(|node| node.kind)
    }

    #[must_use]
    pub fn parent(&self, scope: ScopeId) -> Option<ScopeId> {
        self.nodes.get(&scope).and_then(|node| node.parent)
    }

    pub fn capability_slot<K: CapabilityKey>(
        &mut self,
        scope: ScopeId,
    ) -> Result<&mut ExclusiveCapabilitySlot<K>, ScopeError> {
        let node = self.open_node_mut(scope)?;
        let capability = CapabilityId::of::<K>();
        let entry = node
            .capabilities
            .entry(capability)
            .or_insert_with(|| Box::new(ExclusiveCapabilitySlot::<K>::new(scope)) as Box<dyn Any>);
        let Some(slot) = entry.downcast_mut::<ExclusiveCapabilitySlot<K>>() else {
            unreachable!("capability identity preserves its marker type");
        };
        Ok(slot)
    }

    pub fn resolve<K: CapabilityKey>(
        &self,
        scope: ScopeId,
    ) -> Result<Option<CapabilityLease<K>>, ScopeError> {
        let capability = CapabilityId::of::<K>();
        let mut current = Some(scope);
        while let Some(scope) = current {
            let node = self.open_node(scope)?;
            if let Some(entry) = node.capabilities.get(&capability) {
                let Some(slot) = entry.downcast_ref::<ExclusiveCapabilitySlot<K>>() else {
                    unreachable!("capability identity preserves its marker type");
                };
                if let Some(lease) = slot.current() {
                    return Ok(Some(lease));
                }
            }
            current = node.parent;
        }
        Ok(None)
    }

    pub fn own_sync(
        &mut self,
        scope: ScopeId,
        undo: impl FnOnce() + 'static,
    ) -> Result<(), ScopeError> {
        self.open_node_mut(scope)?.effects.own_sync(undo);
        Ok(())
    }

    pub fn own_resource<T: 'static>(
        &mut self,
        scope: ScopeId,
        resource: T,
    ) -> Result<(), ScopeError> {
        self.open_node_mut(scope)?.effects.own_resource(resource);
        Ok(())
    }

    pub fn own_async(&mut self, scope: ScopeId, stop: impl AsyncStop) -> Result<(), ScopeError> {
        self.open_node_mut(scope)?.effects.own_async(stop);
        Ok(())
    }

    pub async fn close(&mut self, scope: ScopeId) -> Result<(), ScopeError> {
        if self.state(scope).is_none() {
            return Err(ScopeError::Unknown { scope });
        }
        if self.state(scope) == Some(ScopeState::Closed) {
            return Ok(());
        }

        let mut order = Vec::new();
        self.collect_close_order(scope, &mut order);
        for current in order {
            if self.state(current) == Some(ScopeState::Closed) {
                continue;
            }
            {
                let Some(node) = self.nodes.get_mut(&current) else {
                    unreachable!("close order contains only registered scopes");
                };
                node.state = ScopeState::Closing;
                node.capabilities.clear();
            }
            let dispose_result = {
                let Some(node) = self.nodes.get_mut(&current) else {
                    unreachable!("closing scope remains registered");
                };
                node.effects.quiesce_and_dispose().await
            };
            if let Err(source) = dispose_result {
                return Err(ScopeError::Dispose {
                    scope: current,
                    source,
                });
            }
            let Some(node) = self.nodes.get_mut(&current) else {
                unreachable!("closed scope remains registered for idempotence");
            };
            node.state = ScopeState::Closed;
        }
        Ok(())
    }

    fn create_child(
        &mut self,
        parent: ScopeId,
        expected_parent: ScopeKind,
        kind: ScopeKind,
    ) -> Result<ScopeId, ScopeError> {
        let parent_node = self.open_node(parent)?;
        if parent_node.kind != expected_parent {
            return Err(ScopeError::InvalidParent {
                scope: parent,
                expected: expected_parent,
                actual: parent_node.kind,
            });
        }
        let raw = self.next_id.ok_or(ScopeError::IdExhausted)?;
        let scope = ScopeId::new(raw);
        self.next_id = raw.checked_add(1);
        self.nodes.insert(scope, ScopeNode::new(kind, Some(parent)));
        let Some(parent_node) = self.nodes.get_mut(&parent) else {
            unreachable!("validated parent remains registered");
        };
        parent_node.children.push(scope);
        Ok(scope)
    }

    fn open_node(&self, scope: ScopeId) -> Result<&ScopeNode, ScopeError> {
        let node = self
            .nodes
            .get(&scope)
            .ok_or(ScopeError::Unknown { scope })?;
        if node.state != ScopeState::Open {
            return Err(ScopeError::NotOpen {
                scope,
                state: node.state,
            });
        }
        Ok(node)
    }

    fn open_node_mut(&mut self, scope: ScopeId) -> Result<&mut ScopeNode, ScopeError> {
        let node = self
            .nodes
            .get_mut(&scope)
            .ok_or(ScopeError::Unknown { scope })?;
        if node.state != ScopeState::Open {
            return Err(ScopeError::NotOpen {
                scope,
                state: node.state,
            });
        }
        Ok(node)
    }

    fn collect_close_order(&self, scope: ScopeId, order: &mut Vec<ScopeId>) {
        let Some(node) = self.nodes.get(&scope) else {
            return;
        };
        for child in node.children.iter().rev().copied() {
            if self.state(child) != Some(ScopeState::Closed) {
                self.collect_close_order(child, order);
            }
        }
        if node.state != ScopeState::Closed {
            order.push(scope);
        }
    }

    fn release_for_drop(&mut self, scope: ScopeId) -> bool {
        let children = self
            .nodes
            .get(&scope)
            .map(|node| node.children.clone())
            .unwrap_or_default();
        let mut children_released = true;
        for child in children.into_iter().rev() {
            if self.state(child) != Some(ScopeState::Closed) && !self.release_for_drop(child) {
                children_released = false;
            }
        }
        if !children_released {
            return false;
        }

        let Some(node) = self.nodes.get_mut(&scope) else {
            return true;
        };
        if node.state == ScopeState::Closed {
            return true;
        }
        node.state = ScopeState::Closing;
        node.capabilities.clear();
        if !node.effects.release_for_drop() {
            return false;
        }
        node.state = ScopeState::Closed;
        true
    }
}

impl Default for ScopeTree {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScopeTree {
    fn drop(&mut self) {
        let unfinished_before_drop = self
            .nodes
            .values()
            .filter(|node| node.state != ScopeState::Closed)
            .count();
        if unfinished_before_drop == 0 {
            return;
        }
        self.release_for_drop(self.application);
        let retained = self
            .nodes
            .values()
            .filter(|node| node.state != ScopeState::Closed)
            .count();
        crate::logging::error(
            "runtime.scope",
            format_args!(
                "scope tree dropped before explicit closure; retaining {retained} of {unfinished_before_drop} unfinished scopes"
            ),
        );
        if retained > 0 {
            self.nodes
                .retain(|_, node| node.state != ScopeState::Closed);
            mem::forget(mem::take(&mut self.nodes));
        }
    }
}
