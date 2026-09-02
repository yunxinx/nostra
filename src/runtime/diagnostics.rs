//! Immutable runtime diagnostics, revisioned publication, and startup audit.

use std::{
    cell::{Cell, RefCell},
    fmt,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll},
    time::Duration,
    time::Instant,
};

use futures::task::AtomicWaker;

use super::{
    CapabilityId, ComponentId, DesiredRevision, ReconcileFailure, ReconcileObserver,
    ReconcileStage, ResolvedDependency, ScopeId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupPolicy {
    MustActivate,
    AllowedPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeComponentState {
    Pending,
    Preparing,
    Active,
    Failed,
    Quiescing,
    Disposed,
}

impl fmt::Display for RuntimeComponentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => f.write_str("pending"),
            Self::Preparing => f.write_str("preparing"),
            Self::Active => f.write_str("active"),
            Self::Failed => f.write_str("failed"),
            Self::Quiescing => f.write_str("quiescing"),
            Self::Disposed => f.write_str("disposed"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopedComponentId {
    component: ComponentId,
    scope: ScopeId,
}

impl ScopedComponentId {
    #[must_use]
    pub const fn new(component: ComponentId, scope: ScopeId) -> Self {
        Self { component, scope }
    }

    #[must_use]
    pub const fn component(self) -> ComponentId {
        self.component
    }

    #[must_use]
    pub const fn scope(self) -> ScopeId {
        self.scope
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MissingDependencySnapshot {
    capability: CapabilityId,
    blocking_chain: Box<[ScopedComponentId]>,
}

impl MissingDependencySnapshot {
    #[must_use]
    pub fn direct(capability: CapabilityId) -> Self {
        Self {
            capability,
            blocking_chain: Box::default(),
        }
    }

    #[must_use]
    pub fn blocked_by(
        capability: CapabilityId,
        blocking_chain: impl IntoIterator<Item = ScopedComponentId>,
    ) -> Self {
        Self {
            capability,
            blocking_chain: blocking_chain.into_iter().collect(),
        }
    }

    #[must_use]
    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    #[must_use]
    pub fn blocking_chain(&self) -> &[ScopedComponentId] {
        &self.blocking_chain
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeResourceCounts {
    effects: usize,
    tasks: usize,
    subscriptions: usize,
    quiescence_barriers: usize,
}

impl RuntimeResourceCounts {
    #[must_use]
    pub const fn new(
        effects: usize,
        tasks: usize,
        subscriptions: usize,
        quiescence_barriers: usize,
    ) -> Self {
        Self {
            effects,
            tasks,
            subscriptions,
            quiescence_barriers,
        }
    }

    #[must_use]
    pub const fn effects(self) -> usize {
        self.effects
    }

    #[must_use]
    pub const fn tasks(self) -> usize {
        self.tasks
    }

    #[must_use]
    pub const fn subscriptions(self) -> usize {
        self.subscriptions
    }

    #[must_use]
    pub const fn quiescence_barriers(self) -> usize {
        self.quiescence_barriers
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComponentSnapshotDetails {
    dependencies: Box<[ResolvedDependency]>,
    missing_dependencies: Box<[MissingDependencySnapshot]>,
    resource_counts: RuntimeResourceCounts,
}

impl ComponentSnapshotDetails {
    #[must_use]
    pub fn new(
        dependencies: impl IntoIterator<Item = ResolvedDependency>,
        missing_dependencies: impl IntoIterator<Item = MissingDependencySnapshot>,
        resource_counts: RuntimeResourceCounts,
    ) -> Self {
        let mut dependencies = dependencies.into_iter().collect::<Vec<_>>();
        dependencies.sort_unstable();
        dependencies.dedup();
        let mut missing_dependencies = missing_dependencies.into_iter().collect::<Vec<_>>();
        missing_dependencies.sort_unstable();
        missing_dependencies.dedup();
        Self {
            dependencies: dependencies.into_boxed_slice(),
            missing_dependencies: missing_dependencies.into_boxed_slice(),
            resource_counts,
        }
    }

    #[must_use]
    pub fn dependencies(&self) -> &[ResolvedDependency] {
        &self.dependencies
    }

    #[must_use]
    pub fn missing_dependencies(&self) -> &[MissingDependencySnapshot] {
        &self.missing_dependencies
    }

    #[must_use]
    pub const fn resource_counts(&self) -> RuntimeResourceCounts {
        self.resource_counts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionSnapshot {
    revision: DesiredRevision,
    stage: ReconcileStage,
    started_at: Instant,
    elapsed: Duration,
}

impl TransitionSnapshot {
    #[must_use]
    pub const fn revision(self) -> DesiredRevision {
        self.revision
    }

    #[must_use]
    pub const fn stage(self) -> ReconcileStage {
        self.stage
    }

    #[must_use]
    pub const fn started_at(self) -> Instant {
        self.started_at
    }

    #[must_use]
    pub const fn elapsed(self) -> Duration {
        self.elapsed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentSnapshot {
    component: ComponentId,
    scope: ScopeId,
    startup_policy: StartupPolicy,
    desired_revision: DesiredRevision,
    state: RuntimeComponentState,
    details: ComponentSnapshotDetails,
    transition: Option<TransitionSnapshot>,
    last_failure: Option<ReconcileFailure>,
}

pub(crate) struct ComponentTransitionState {
    component: ComponentId,
    scope: ScopeId,
    startup_policy: StartupPolicy,
    desired_revision: DesiredRevision,
    stage: ReconcileStage,
    started_at: Instant,
    last_failure: Option<ReconcileFailure>,
}

impl ComponentTransitionState {
    pub(crate) fn new(
        component: ComponentId,
        scope: ScopeId,
        startup_policy: StartupPolicy,
        desired_revision: DesiredRevision,
        stage: ReconcileStage,
        started_at: Instant,
        last_failure: Option<ReconcileFailure>,
    ) -> Self {
        Self {
            component,
            scope,
            startup_policy,
            desired_revision,
            stage,
            started_at,
            last_failure,
        }
    }
}

impl ComponentSnapshot {
    #[must_use]
    pub fn pending(
        component: ComponentId,
        scope: ScopeId,
        startup_policy: StartupPolicy,
        desired_revision: DesiredRevision,
        details: ComponentSnapshotDetails,
    ) -> Self {
        Self {
            component,
            scope,
            startup_policy,
            desired_revision,
            state: RuntimeComponentState::Pending,
            details,
            transition: None,
            last_failure: None,
        }
    }

    #[must_use]
    pub fn active(
        component: ComponentId,
        scope: ScopeId,
        startup_policy: StartupPolicy,
        desired_revision: DesiredRevision,
        details: ComponentSnapshotDetails,
    ) -> Self {
        Self {
            component,
            scope,
            startup_policy,
            desired_revision,
            state: RuntimeComponentState::Active,
            details,
            transition: None,
            last_failure: None,
        }
    }

    #[must_use]
    pub fn failed(
        startup_policy: StartupPolicy,
        failure: ReconcileFailure,
        details: ComponentSnapshotDetails,
    ) -> Self {
        Self {
            component: failure.component(),
            scope: failure.scope(),
            startup_policy,
            desired_revision: failure.revision(),
            state: RuntimeComponentState::Failed,
            details,
            transition: None,
            last_failure: Some(failure),
        }
    }

    #[must_use]
    pub fn transitioning(
        startup_policy: StartupPolicy,
        observer: &ReconcileObserver,
        observed_at: Instant,
        details: ComponentSnapshotDetails,
    ) -> Option<Self> {
        let transition = observer.transition()?;
        let state = match transition.stage() {
            ReconcileStage::Preparing | ReconcileStage::Activating => {
                RuntimeComponentState::Preparing
            }
            ReconcileStage::Stopping | ReconcileStage::RollingBack => {
                RuntimeComponentState::Quiescing
            }
        };
        Some(Self {
            component: observer.component(),
            scope: observer.scope(),
            startup_policy,
            desired_revision: transition.revision(),
            state,
            details,
            transition: Some(TransitionSnapshot {
                revision: transition.revision(),
                stage: transition.stage(),
                started_at: transition.started_at(),
                elapsed: observed_at
                    .checked_duration_since(transition.started_at())
                    .unwrap_or_default(),
            }),
            last_failure: observer.last_failure(),
        })
    }

    pub(crate) fn transitioning_explicit(
        transition: ComponentTransitionState,
        observed_at: Instant,
        details: ComponentSnapshotDetails,
    ) -> Self {
        let state = match transition.stage {
            ReconcileStage::Preparing | ReconcileStage::Activating => {
                RuntimeComponentState::Preparing
            }
            ReconcileStage::Stopping | ReconcileStage::RollingBack => {
                RuntimeComponentState::Quiescing
            }
        };
        Self {
            component: transition.component,
            scope: transition.scope,
            startup_policy: transition.startup_policy,
            desired_revision: transition.desired_revision,
            state,
            details,
            transition: Some(TransitionSnapshot {
                revision: transition.desired_revision,
                stage: transition.stage,
                started_at: transition.started_at,
                elapsed: observed_at
                    .checked_duration_since(transition.started_at)
                    .unwrap_or_default(),
            }),
            last_failure: transition.last_failure,
        }
    }

    #[must_use]
    pub fn disposed(
        component: ComponentId,
        scope: ScopeId,
        startup_policy: StartupPolicy,
        desired_revision: DesiredRevision,
        details: ComponentSnapshotDetails,
    ) -> Self {
        Self {
            component,
            scope,
            startup_policy,
            desired_revision,
            state: RuntimeComponentState::Disposed,
            details,
            transition: None,
            last_failure: None,
        }
    }

    #[must_use]
    pub const fn component(&self) -> ComponentId {
        self.component
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn startup_policy(&self) -> StartupPolicy {
        self.startup_policy
    }

    #[must_use]
    pub const fn desired_revision(&self) -> DesiredRevision {
        self.desired_revision
    }

    #[must_use]
    pub const fn state(&self) -> RuntimeComponentState {
        self.state
    }

    #[must_use]
    pub fn dependencies(&self) -> &[ResolvedDependency] {
        self.details.dependencies()
    }

    #[must_use]
    pub fn missing_dependencies(&self) -> &[MissingDependencySnapshot] {
        self.details.missing_dependencies()
    }

    #[must_use]
    pub const fn resource_counts(&self) -> RuntimeResourceCounts {
        self.details.resource_counts()
    }

    #[must_use]
    pub const fn transition(&self) -> Option<TransitionSnapshot> {
        self.transition
    }

    #[must_use]
    pub fn last_failure(&self) -> Option<&ReconcileFailure> {
        self.last_failure.as_ref()
    }

    fn validate_details(&self) -> Result<(), ComponentSnapshotViolation> {
        if let Some(capability) = self.dependencies().windows(2).find_map(|pair| {
            (pair[0].capability() == pair[1].capability()).then_some(pair[0].capability())
        }) {
            return Err(ComponentSnapshotViolation::DuplicateResolvedDependency { capability });
        }
        for dependency in self.dependencies() {
            if self
                .missing_dependencies()
                .iter()
                .any(|missing| missing.capability() == dependency.capability())
            {
                return Err(ComponentSnapshotViolation::ResolvedAndMissing {
                    capability: dependency.capability(),
                });
            }
        }

        match self.state {
            RuntimeComponentState::Active | RuntimeComponentState::Preparing => {
                if let Some(missing) = self.missing_dependencies().first() {
                    return Err(ComponentSnapshotViolation::MissingDependencyInState {
                        state: self.state,
                        capability: missing.capability(),
                    });
                }
            }
            RuntimeComponentState::Pending => {
                if self.resource_counts() != RuntimeResourceCounts::default() {
                    return Err(ComponentSnapshotViolation::OwnedResourcesInState {
                        state: self.state,
                        counts: self.resource_counts(),
                    });
                }
            }
            RuntimeComponentState::Disposed => {
                if let Some(dependency) = self.dependencies().first() {
                    return Err(ComponentSnapshotViolation::ResolvedDependencyInDisposed {
                        capability: dependency.capability(),
                    });
                }
                if let Some(missing) = self.missing_dependencies().first() {
                    return Err(ComponentSnapshotViolation::MissingDependencyInState {
                        state: self.state,
                        capability: missing.capability(),
                    });
                }
                if self.resource_counts() != RuntimeResourceCounts::default() {
                    return Err(ComponentSnapshotViolation::OwnedResourcesInState {
                        state: self.state,
                        counts: self.resource_counts(),
                    });
                }
            }
            RuntimeComponentState::Failed | RuntimeComponentState::Quiescing => {}
        }
        Ok(())
    }
}

pub struct RuntimeComponentDiagnostic<'a> {
    component: &'a ComponentSnapshot,
}

impl<'a> RuntimeComponentDiagnostic<'a> {
    #[must_use]
    pub const fn new(component: &'a ComponentSnapshot) -> Self {
        Self { component }
    }
}

impl fmt::Display for RuntimeComponentDiagnostic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let component = self.component;
        write!(
            f,
            "event=component_state component={} scope={} state={} desired_revision={} startup_policy={} effects={} tasks={} subscriptions={} quiescence_barriers={}",
            component.component(),
            component.scope().raw(),
            component.state(),
            component.desired_revision().get(),
            match component.startup_policy() {
                StartupPolicy::MustActivate => "must_activate",
                StartupPolicy::AllowedPending => "allowed_pending",
            },
            component.resource_counts().effects(),
            component.resource_counts().tasks(),
            component.resource_counts().subscriptions(),
            component.resource_counts().quiescence_barriers(),
        )?;
        for dependency in component.dependencies() {
            write!(
                f,
                " dependency={}:{}:{}:{}",
                dependency.capability().name(),
                dependency.scope().raw(),
                dependency.provider(),
                dependency.generation().get(),
            )?;
        }
        for missing in component.missing_dependencies() {
            write!(f, " missing_dependency={}", missing.capability().name())?;
            for blocker in missing.blocking_chain() {
                write!(
                    f,
                    " blocking_component={}:{}",
                    blocker.component(),
                    blocker.scope().raw(),
                )?;
            }
        }
        if let Some(transition) = component.transition() {
            write!(
                f,
                " transition_stage={:?} transition_revision={} transition_duration_ms={}",
                transition.stage(),
                transition.revision().get(),
                transition.elapsed().as_millis(),
            )?;
        }
        if let Some(failure) = component.last_failure() {
            write!(
                f,
                " last_error_stage={:?} last_error_kind={:?}",
                failure.stage(),
                failure.kind(),
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContributionRevision {
    registry: CapabilityId,
    revision: u64,
}

impl ContributionRevision {
    #[must_use]
    pub const fn new(registry: CapabilityId, revision: u64) -> Self {
        Self { registry, revision }
    }

    #[must_use]
    pub const fn registry(self) -> CapabilityId {
        self.registry
    }

    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeDiagnostic {
    LongTransition {
        component: ComponentId,
        scope: ScopeId,
        revision: DesiredRevision,
        stage: ReconcileStage,
        elapsed: Duration,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub struct StartupAuditError {
    blockers: Box<[ComponentSnapshot]>,
}

impl StartupAuditError {
    #[must_use]
    pub fn blockers(&self) -> &[ComponentSnapshot] {
        &self.blockers
    }
}

impl fmt::Debug for StartupAuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupAuditError")
            .field("blocker_count", &self.blockers.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for StartupAuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "runtime startup audit found {} required components that are not active",
            self.blockers.len()
        )?;
        for blocker in &self.blockers {
            write!(
                f,
                "; component `{}` in scope {} is {}",
                blocker.component,
                blocker.scope.raw(),
                blocker.state
            )?;
            if !blocker.missing_dependencies().is_empty() {
                f.write_str(" (missing")?;
                for missing in blocker.missing_dependencies() {
                    write!(f, " `{}`", missing.capability().name())?;
                    for blocked_by in missing.blocking_chain() {
                        write!(
                            f,
                            " via `{}` in scope {}",
                            blocked_by.component(),
                            blocked_by.scope().raw()
                        )?;
                    }
                }
                f.write_str(")")?;
            }
            if let Some(failure) = &blocker.last_failure {
                write!(
                    f,
                    " (last_error_stage={:?}, last_error_kind={:?})",
                    failure.stage(),
                    failure.kind(),
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for StartupAuditError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentSnapshotViolation {
    DuplicateResolvedDependency {
        capability: CapabilityId,
    },
    ResolvedAndMissing {
        capability: CapabilityId,
    },
    MissingDependencyInState {
        state: RuntimeComponentState,
        capability: CapabilityId,
    },
    ResolvedDependencyInDisposed {
        capability: CapabilityId,
    },
    OwnedResourcesInState {
        state: RuntimeComponentState,
        counts: RuntimeResourceCounts,
    },
}

impl fmt::Display for ComponentSnapshotViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateResolvedDependency { capability } => write!(
                f,
                "capability `{}` has multiple resolved Provider bindings",
                capability.name()
            ),
            Self::ResolvedAndMissing { capability } => write!(
                f,
                "capability `{}` is both resolved and missing",
                capability.name()
            ),
            Self::MissingDependencyInState { state, capability } => write!(
                f,
                "{state} component reports missing capability `{}`",
                capability.name()
            ),
            Self::ResolvedDependencyInDisposed { capability } => write!(
                f,
                "disposed component retains resolved capability `{}`",
                capability.name()
            ),
            Self::OwnedResourcesInState { state, counts } => write!(
                f,
                "{state} component retains {} effects, {} tasks, {} subscriptions, and {} quiescence barriers",
                counts.effects(),
                counts.tasks(),
                counts.subscriptions(),
                counts.quiescence_barriers()
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeSnapshotError {
    InvalidComponent {
        component: ComponentId,
        scope: ScopeId,
        violation: ComponentSnapshotViolation,
    },
    DuplicateComponent {
        component: ComponentId,
        scope: ScopeId,
    },
    DuplicateContributionRevision {
        registry: CapabilityId,
    },
}

impl fmt::Display for RuntimeSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidComponent {
                component,
                scope,
                violation,
            } => write!(
                f,
                "runtime snapshot component `{component}` in scope {} is inconsistent: {violation}",
                scope.raw()
            ),
            Self::DuplicateComponent { component, scope } => write!(
                f,
                "runtime snapshot contains duplicate component `{component}` in scope {}",
                scope.raw()
            ),
            Self::DuplicateContributionRevision { registry } => write!(
                f,
                "runtime snapshot contains duplicate contribution revision for `{}`",
                registry.name()
            ),
        }
    }
}

impl std::error::Error for RuntimeSnapshotError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    components: Box<[ComponentSnapshot]>,
    contribution_revisions: Box<[ContributionRevision]>,
    diagnostics: Box<[RuntimeDiagnostic]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSnapshotUpdate {
    revision: u64,
    snapshot: RuntimeSnapshot,
}

impl RuntimeSnapshotUpdate {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn snapshot(&self) -> &RuntimeSnapshot {
        &self.snapshot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeSnapshotGuard {
    revision: u64,
}

struct RuntimeSnapshotSubscriberState {
    seen_revision: Cell<u64>,
    waker: AtomicWaker,
}

struct RuntimeSnapshotSourceState {
    current: RuntimeSnapshotUpdate,
    subscribers: Vec<Weak<RuntimeSnapshotSubscriberState>>,
}

#[derive(Clone)]
pub struct RuntimeSnapshotReader {
    state: Rc<RefCell<RuntimeSnapshotSourceState>>,
}

impl RuntimeSnapshotReader {
    #[must_use]
    pub fn current(&self) -> RuntimeSnapshotUpdate {
        self.state.borrow().current.clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> RuntimeSnapshotSubscription {
        let seen_revision = self.state.borrow().current.revision;
        let subscriber = Rc::new(RuntimeSnapshotSubscriberState {
            seen_revision: Cell::new(seen_revision),
            waker: AtomicWaker::new(),
        });
        self.state
            .borrow_mut()
            .subscribers
            .push(Rc::downgrade(&subscriber));
        RuntimeSnapshotSubscription {
            source: Rc::downgrade(&self.state),
            subscriber,
        }
    }
}

#[derive(Clone)]
pub(crate) struct RuntimeSnapshotSource {
    reader: RuntimeSnapshotReader,
}

impl RuntimeSnapshotSource {
    #[must_use]
    pub(crate) fn new(snapshot: RuntimeSnapshot) -> Self {
        Self {
            reader: RuntimeSnapshotReader {
                state: Rc::new(RefCell::new(RuntimeSnapshotSourceState {
                    current: RuntimeSnapshotUpdate {
                        revision: 1,
                        snapshot,
                    },
                    subscribers: Vec::new(),
                })),
            },
        }
    }

    #[must_use]
    pub(crate) fn reader(&self) -> RuntimeSnapshotReader {
        self.reader.clone()
    }

    #[must_use]
    pub(crate) fn guard(&self) -> RuntimeSnapshotGuard {
        RuntimeSnapshotGuard {
            revision: self.reader.state.borrow().current.revision,
        }
    }

    pub(crate) fn publish(&self, snapshot: RuntimeSnapshot) -> bool {
        let guard = self.guard();
        self.publish_if_current(&guard, snapshot)
    }

    pub(crate) fn publish_if_current(
        &self,
        guard: &RuntimeSnapshotGuard,
        snapshot: RuntimeSnapshot,
    ) -> bool {
        let subscribers = {
            let mut state = self.reader.state.borrow_mut();
            if state.current.revision != guard.revision {
                return false;
            }
            let Some(revision) = state.current.revision.checked_add(1) else {
                return false;
            };
            state.current = RuntimeSnapshotUpdate { revision, snapshot };
            let mut subscribers = Vec::with_capacity(state.subscribers.len());
            state.subscribers.retain(|subscriber| {
                let Some(subscriber) = subscriber.upgrade() else {
                    return false;
                };
                subscribers.push(subscriber);
                true
            });
            subscribers
        };
        for subscriber in subscribers {
            subscriber.waker.wake();
        }
        true
    }
}

pub struct RuntimeSnapshotSubscription {
    source: Weak<RefCell<RuntimeSnapshotSourceState>>,
    subscriber: Rc<RuntimeSnapshotSubscriberState>,
}

impl RuntimeSnapshotSubscription {
    pub async fn next(&mut self) -> Option<RuntimeSnapshotUpdate> {
        RuntimeSnapshotNext { subscription: self }.await
    }
}

struct RuntimeSnapshotNext<'a> {
    subscription: &'a mut RuntimeSnapshotSubscription,
}

impl Future for RuntimeSnapshotNext<'_> {
    type Output = Option<RuntimeSnapshotUpdate>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let subscription = &mut *self.get_mut().subscription;
        let Some(source) = subscription.source.upgrade() else {
            return Poll::Ready(None);
        };
        let current = source.borrow().current.clone();
        let seen_revision = subscription.subscriber.seen_revision.get();
        if current.revision > seen_revision {
            subscription.subscriber.seen_revision.set(current.revision);
            return Poll::Ready(Some(current));
        }
        subscription.subscriber.waker.register(cx.waker());
        let current = source.borrow().current.clone();
        let seen_revision = subscription.subscriber.seen_revision.get();
        if current.revision > seen_revision {
            subscription.subscriber.seen_revision.set(current.revision);
            Poll::Ready(Some(current))
        } else {
            Poll::Pending
        }
    }
}

impl RuntimeSnapshot {
    pub fn new(
        components: impl IntoIterator<Item = ComponentSnapshot>,
        contribution_revisions: impl IntoIterator<Item = ContributionRevision>,
        long_transition_threshold: Duration,
    ) -> Result<Self, RuntimeSnapshotError> {
        let mut components = components.into_iter().collect::<Vec<_>>();
        components.sort_unstable_by_key(|component| (component.component, component.scope));
        if let Some(duplicate) = components
            .windows(2)
            .find(|pair| pair[0].component == pair[1].component && pair[0].scope == pair[1].scope)
            .map(|pair| &pair[0])
        {
            return Err(RuntimeSnapshotError::DuplicateComponent {
                component: duplicate.component,
                scope: duplicate.scope,
            });
        }
        for component in &components {
            if let Err(violation) = component.validate_details() {
                return Err(RuntimeSnapshotError::InvalidComponent {
                    component: component.component,
                    scope: component.scope,
                    violation,
                });
            }
        }

        let mut contribution_revisions = contribution_revisions.into_iter().collect::<Vec<_>>();
        contribution_revisions.sort_unstable();
        if let Some(registry) = contribution_revisions
            .windows(2)
            .find_map(|pair| (pair[0].registry == pair[1].registry).then_some(pair[0].registry))
        {
            return Err(RuntimeSnapshotError::DuplicateContributionRevision { registry });
        }

        let diagnostics = components
            .iter()
            .filter_map(|component| {
                let transition = component.transition?;
                (transition.elapsed >= long_transition_threshold).then_some(
                    RuntimeDiagnostic::LongTransition {
                        component: component.component,
                        scope: component.scope,
                        revision: transition.revision,
                        stage: transition.stage,
                        elapsed: transition.elapsed,
                    },
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            components: components.into_boxed_slice(),
            contribution_revisions: contribution_revisions.into_boxed_slice(),
            diagnostics,
        })
    }

    #[must_use]
    pub fn components(&self) -> &[ComponentSnapshot] {
        &self.components
    }

    #[must_use]
    pub fn contribution_revisions(&self) -> &[ContributionRevision] {
        &self.contribution_revisions
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[RuntimeDiagnostic] {
        &self.diagnostics
    }

    pub fn audit_startup(&self) -> Result<(), StartupAuditError> {
        let blockers = self
            .components
            .iter()
            .filter(|component| {
                component.startup_policy == StartupPolicy::MustActivate
                    && component.state != RuntimeComponentState::Active
            })
            .cloned()
            .collect::<Vec<_>>();
        if blockers.is_empty() {
            return Ok(());
        }
        Err(StartupAuditError {
            blockers: blockers.into_boxed_slice(),
        })
    }
}
