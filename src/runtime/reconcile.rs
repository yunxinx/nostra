//! Serialized component reconciliation within one runtime scope.
//!
//! Production assembly still lives in [`super::composition`]. This loop is
//! crate-private and exercised by fake-component tests; it is not the live
//! composition driver. `RuntimeHost` is not landed.

use std::{cell::RefCell, fmt, future::Future, pin::Pin, rc::Rc, time::Instant};

use super::{ComponentId, EffectScope, ScopeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DesiredRevision(u64);

impl DesiredRevision {
    pub const INITIAL: Self = Self(1);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredRevisionExhausted;

impl fmt::Display for DesiredRevisionExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("desired revisions are exhausted")
    }
}

impl std::error::Error for DesiredRevisionExhausted {}

struct DesiredState<D> {
    revision: DesiredRevision,
    desired: Option<D>,
}

#[derive(Clone)]
#[must_use = "the target handle keeps reconciliation requests observable"]
pub struct ReconcileTarget<D> {
    state: Rc<RefCell<DesiredState<D>>>,
}

impl<D: PartialEq> ReconcileTarget<D> {
    pub fn set_desired(
        &self,
        desired: Option<D>,
    ) -> Result<DesiredRevision, DesiredRevisionExhausted> {
        let mut state = self.state.borrow_mut();
        if state.desired == desired {
            return Ok(state.revision);
        }
        state.revision = state.revision.next().ok_or(DesiredRevisionExhausted)?;
        state.desired = desired;
        Ok(state.revision)
    }
}

impl<D> ReconcileTarget<D> {
    pub fn retry(&self) -> Result<DesiredRevision, DesiredRevisionExhausted> {
        let mut state = self.state.borrow_mut();
        state.revision = state.revision.next().ok_or(DesiredRevisionExhausted)?;
        Ok(state.revision)
    }

    #[must_use]
    pub fn revision(&self) -> DesiredRevision {
        self.state.borrow().revision
    }

    fn snapshot(&self) -> DesiredSnapshot<D>
    where
        D: Clone,
    {
        let state = self.state.borrow();
        DesiredSnapshot {
            revision: state.revision,
            desired: state.desired.clone(),
        }
    }
}

impl<D> fmt::Debug for ReconcileTarget<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconcileTarget")
            .field("revision", &self.revision())
            .finish_non_exhaustive()
    }
}

struct DesiredSnapshot<D> {
    revision: DesiredRevision,
    desired: Option<D>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileStage {
    Preparing,
    Activating,
    Stopping,
    RollingBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileTransition {
    scope: ScopeId,
    component: ComponentId,
    revision: DesiredRevision,
    stage: ReconcileStage,
    started_at: Instant,
}

impl ReconcileTransition {
    #[must_use]
    pub const fn scope(self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn component(self) -> ComponentId {
        self.component
    }

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileFailureKind {
    Error,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileFailure {
    scope: ScopeId,
    component: ComponentId,
    revision: DesiredRevision,
    stage: ReconcileStage,
    kind: ReconcileFailureKind,
    message: String,
}

impl ReconcileFailure {
    pub(crate) fn error(
        scope: ScopeId,
        component: ComponentId,
        revision: DesiredRevision,
        stage: ReconcileStage,
        message: impl Into<String>,
    ) -> Self {
        Self {
            scope,
            component,
            revision,
            stage,
            kind: ReconcileFailureKind::Error,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn component(&self) -> ComponentId {
        self.component
    }

    #[must_use]
    pub const fn revision(&self) -> DesiredRevision {
        self.revision
    }

    #[must_use]
    pub const fn stage(&self) -> ReconcileStage {
        self.stage
    }

    #[must_use]
    pub const fn kind(&self) -> ReconcileFailureKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ReconcileFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "component `{}` in scope {} failed during {:?} at desired revision {}: {}",
            self.component,
            self.scope.raw(),
            self.stage,
            self.revision.get(),
            self.message
        )
    }
}

impl std::error::Error for ReconcileFailure {}

#[derive(Clone)]
#[must_use = "the observer keeps in-flight reconciliation diagnostics accessible"]
pub struct ReconcileObserver {
    scope: ScopeId,
    component: ComponentId,
    transition: Rc<RefCell<Option<ReconcileTransition>>>,
    last_failure: Rc<RefCell<Option<ReconcileFailure>>>,
}

impl ReconcileObserver {
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn component(&self) -> ComponentId {
        self.component
    }

    #[must_use]
    pub fn transition(&self) -> Option<ReconcileTransition> {
        *self.transition.borrow()
    }

    #[must_use]
    pub fn last_failure(&self) -> Option<ReconcileFailure> {
        self.last_failure.borrow().clone()
    }
}

impl fmt::Debug for ReconcileObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconcileObserver")
            .field("scope", &self.scope)
            .field("component", &self.component)
            .field("transition", &self.transition())
            .field("last_failure", &self.last_failure())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "reconciliation failures must remain visible to the runtime"]
pub enum ReconcileStatus {
    Settled {
        revision: DesiredRevision,
        active: bool,
    },
    Failed(ReconcileFailure),
}

/// Prepares a private candidate before replacement and publishes it during activation.
pub trait ComponentLifecycle<D> {
    /// Resources prepared for one desired revision without externally publishing it.
    type Prepared: 'static;

    /// Acquires candidate resources and registers every reversible side effect eagerly.
    fn prepare<'a>(
        &'a mut self,
        revision: DesiredRevision,
        desired: &'a D,
        effects: &'a mut EffectScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Self::Prepared>> + 'a>>;

    /// Publishes a prepared candidate after the previous active scope is quiescent.
    fn activate<'a>(
        &'a mut self,
        revision: DesiredRevision,
        desired: &'a D,
        prepared: Self::Prepared,
        effects: &'a mut EffectScope,
    ) -> anyhow::Result<()>;
}

struct ActiveMount<D> {
    desired: D,
    effects: EffectScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidatePhaseKind {
    Preparing,
    Prepared,
    RollingBack,
}

enum CandidatePhase {
    Preparing,
    Prepared,
    RollingBack { failure: Option<ReconcileFailure> },
}

impl CandidatePhase {
    const fn kind(&self) -> CandidatePhaseKind {
        match self {
            Self::Preparing => CandidatePhaseKind::Preparing,
            Self::Prepared => CandidatePhaseKind::Prepared,
            Self::RollingBack { .. } => CandidatePhaseKind::RollingBack,
        }
    }
}

struct CandidateMount<D, P> {
    revision: DesiredRevision,
    desired: D,
    effects: EffectScope,
    prepared: Option<P>,
    phase: CandidatePhase,
}

struct TransitionGuard {
    transition_state: Rc<RefCell<Option<ReconcileTransition>>>,
    transition: ReconcileTransition,
    last_failure: Rc<RefCell<Option<ReconcileFailure>>>,
    interrupted: Option<ReconcileFailure>,
}

impl TransitionGuard {
    fn new(
        transition_state: Rc<RefCell<Option<ReconcileTransition>>>,
        transition: ReconcileTransition,
        last_failure: Rc<RefCell<Option<ReconcileFailure>>>,
        interrupted: ReconcileFailure,
    ) -> Self {
        let mut current = transition_state.borrow_mut();
        assert!(
            current.is_none(),
            "one reconciler cannot start concurrent transitions"
        );
        *current = Some(transition);
        drop(current);
        Self {
            transition_state,
            transition,
            last_failure,
            interrupted: Some(interrupted),
        }
    }

    fn complete(mut self) {
        self.interrupted = None;
    }
}

impl Drop for TransitionGuard {
    fn drop(&mut self) {
        let mut current = self.transition_state.borrow_mut();
        if current.as_ref() == Some(&self.transition) {
            *current = None;
        }
        drop(current);
        if let Some(failure) = self.interrupted.take() {
            *self.last_failure.borrow_mut() = Some(failure);
        }
    }
}

#[must_use = "a reconciler owns live and partially disposed component effects"]
pub struct ScopeLocalReconciler<D, P> {
    scope: ScopeId,
    component: ComponentId,
    target: ReconcileTarget<D>,
    active: Option<ActiveMount<D>>,
    candidate: Option<CandidateMount<D, P>>,
    stop_attempt: Option<DesiredRevision>,
    current_transition: Rc<RefCell<Option<ReconcileTransition>>>,
    last_failure: Rc<RefCell<Option<ReconcileFailure>>>,
}

impl<D: Clone + Eq, P: 'static> ScopeLocalReconciler<D, P> {
    pub fn new(scope: ScopeId, component: ComponentId, desired: Option<D>) -> Self {
        Self {
            scope,
            component,
            target: ReconcileTarget {
                state: Rc::new(RefCell::new(DesiredState {
                    revision: DesiredRevision::INITIAL,
                    desired,
                })),
            },
            active: None,
            candidate: None,
            stop_attempt: None,
            current_transition: Rc::new(RefCell::new(None)),
            last_failure: Rc::new(RefCell::new(None)),
        }
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn component(&self) -> ComponentId {
        self.component
    }

    pub fn target(&self) -> ReconcileTarget<D> {
        self.target.clone()
    }

    pub fn observer(&self) -> ReconcileObserver {
        ReconcileObserver {
            scope: self.scope,
            component: self.component,
            transition: Rc::clone(&self.current_transition),
            last_failure: Rc::clone(&self.last_failure),
        }
    }

    #[must_use]
    pub fn active_desired(&self) -> Option<&D> {
        self.active.as_ref().map(|active| &active.desired)
    }

    #[must_use]
    pub fn last_failure(&self) -> Option<ReconcileFailure> {
        self.last_failure.borrow().clone()
    }

    pub async fn reconcile<L>(&mut self, lifecycle: &mut L) -> ReconcileStatus
    where
        L: ComponentLifecycle<D, Prepared = P>,
    {
        loop {
            let target = self.target.snapshot();
            if let Some(failure) = self.last_failure() {
                if failure.revision == target.revision {
                    return ReconcileStatus::Failed(failure);
                }
                *self.last_failure.borrow_mut() = None;
            }

            if self.stop_attempt.is_some() {
                self.stop_attempt = Some(target.revision);
                let guard = self.transition_guard(target.revision, ReconcileStage::Stopping);
                let stop_result = {
                    let Some(active) = self.active.as_mut() else {
                        unreachable!("a stop attempt retains active effect ownership");
                    };
                    active.effects.quiesce_and_dispose().await
                };
                guard.complete();
                match stop_result {
                    Ok(()) => {
                        self.active = None;
                        self.stop_attempt = None;
                        continue;
                    }
                    Err(error) => {
                        let failure = self.error_failure(
                            target.revision,
                            ReconcileStage::Stopping,
                            error.to_string(),
                        );
                        let status = self.record_failure(failure);
                        if self.target.revision() == target.revision {
                            return status;
                        }
                        continue;
                    }
                }
            }

            if let Some(phase) = self
                .candidate
                .as_ref()
                .map(|candidate| candidate.phase.kind())
            {
                match phase {
                    CandidatePhaseKind::Preparing => {
                        let Some(candidate) = self.candidate.as_mut() else {
                            unreachable!("candidate ownership was checked above");
                        };
                        candidate.phase = CandidatePhase::RollingBack { failure: None };
                        continue;
                    }
                    CandidatePhaseKind::RollingBack => {
                        let failure =
                            match self.candidate.as_ref().map(|candidate| &candidate.phase) {
                                Some(CandidatePhase::RollingBack { failure }) => failure.clone(),
                                _ => unreachable!("candidate phase was checked above"),
                            };
                        let guard =
                            self.transition_guard(target.revision, ReconcileStage::RollingBack);
                        let rollback_result = {
                            let Some(candidate) = self.candidate.as_mut() else {
                                unreachable!("rollback retains candidate effect ownership");
                            };
                            candidate.effects.quiesce_and_dispose().await
                        };
                        guard.complete();
                        match rollback_result {
                            Ok(()) => {
                                self.candidate = None;
                                if let Some(failure) = failure
                                    && self.target.revision() == failure.revision
                                {
                                    return self.record_failure(failure);
                                }
                                continue;
                            }
                            Err(error) => {
                                let message = failure.as_ref().map_or_else(
                                    || error.to_string(),
                                    |failure| {
                                        format!("{}; rollback failed: {}", failure.message, error)
                                    },
                                );
                                let rollback_failure = self.error_failure(
                                    target.revision,
                                    ReconcileStage::RollingBack,
                                    message,
                                );
                                let Some(candidate) = self.candidate.as_mut() else {
                                    unreachable!("failed rollback retains its candidate");
                                };
                                candidate.phase = CandidatePhase::RollingBack {
                                    failure: Some(rollback_failure.clone()),
                                };
                                let status = self.record_failure(rollback_failure);
                                if self.target.revision() == target.revision {
                                    return status;
                                }
                                continue;
                            }
                        }
                    }
                    CandidatePhaseKind::Prepared => {
                        let Some(candidate) = self.candidate.as_ref() else {
                            unreachable!("candidate ownership was checked above");
                        };
                        if candidate.revision != target.revision {
                            let Some(candidate) = self.candidate.as_mut() else {
                                unreachable!("candidate ownership was checked above");
                            };
                            candidate.phase = CandidatePhase::RollingBack { failure: None };
                            continue;
                        }
                        if self.active.is_some() {
                            self.stop_attempt = Some(target.revision);
                            continue;
                        }

                        let candidate_revision = candidate.revision;
                        let guard =
                            self.transition_guard(candidate_revision, ReconcileStage::Activating);
                        let activation_result = {
                            let Some(candidate) = self.candidate.as_mut() else {
                                unreachable!("candidate ownership was checked above");
                            };
                            let Some(prepared) = candidate.prepared.take() else {
                                unreachable!("a prepared candidate retains its prepared value");
                            };
                            lifecycle.activate(
                                candidate_revision,
                                &candidate.desired,
                                prepared,
                                &mut candidate.effects,
                            )
                        };
                        guard.complete();
                        match activation_result {
                            Ok(()) => {
                                let Some(candidate) = self.candidate.take() else {
                                    unreachable!("activation retains candidate ownership");
                                };
                                self.active = Some(ActiveMount {
                                    desired: candidate.desired,
                                    effects: candidate.effects,
                                });
                            }
                            Err(error) => {
                                let failure = self.error_failure(
                                    candidate_revision,
                                    ReconcileStage::Activating,
                                    error.to_string(),
                                );
                                let Some(candidate) = self.candidate.as_mut() else {
                                    unreachable!("failed activation retains its candidate");
                                };
                                candidate.phase = CandidatePhase::RollingBack {
                                    failure: Some(failure),
                                };
                            }
                        }
                        continue;
                    }
                }
            }

            if self
                .active
                .as_ref()
                .is_some_and(|active| target.desired.as_ref() == Some(&active.desired))
            {
                return ReconcileStatus::Settled {
                    revision: target.revision,
                    active: true,
                };
            }

            let Some(desired) = target.desired else {
                if self.active.is_some() {
                    self.stop_attempt = Some(target.revision);
                    continue;
                }
                return ReconcileStatus::Settled {
                    revision: target.revision,
                    active: false,
                };
            };

            self.candidate = Some(CandidateMount {
                revision: target.revision,
                desired,
                effects: EffectScope::new(),
                prepared: None,
                phase: CandidatePhase::Preparing,
            });
            let guard = self.transition_guard(target.revision, ReconcileStage::Preparing);
            let prepare_result = {
                let Some(candidate) = self.candidate.as_mut() else {
                    unreachable!("candidate ownership was just created");
                };
                lifecycle
                    .prepare(target.revision, &candidate.desired, &mut candidate.effects)
                    .await
            };
            guard.complete();
            match prepare_result {
                Ok(prepared) => {
                    let Some(candidate) = self.candidate.as_mut() else {
                        unreachable!("successful preparation retains its candidate");
                    };
                    candidate.prepared = Some(prepared);
                    candidate.phase = CandidatePhase::Prepared;
                }
                Err(error) => {
                    let failure = self.error_failure(
                        target.revision,
                        ReconcileStage::Preparing,
                        error.to_string(),
                    );
                    let Some(candidate) = self.candidate.as_mut() else {
                        unreachable!("failed preparation retains its candidate");
                    };
                    candidate.phase = CandidatePhase::RollingBack {
                        failure: Some(failure),
                    };
                }
            }
        }
    }

    fn transition_guard(
        &self,
        revision: DesiredRevision,
        stage: ReconcileStage,
    ) -> TransitionGuard {
        let transition = ReconcileTransition {
            scope: self.scope,
            component: self.component,
            revision,
            stage,
            started_at: Instant::now(),
        };
        TransitionGuard::new(
            Rc::clone(&self.current_transition),
            transition,
            Rc::clone(&self.last_failure),
            self.failure(
                revision,
                stage,
                ReconcileFailureKind::Interrupted,
                "the transition future ended before the lifecycle barrier settled".to_owned(),
            ),
        )
    }

    fn error_failure(
        &self,
        revision: DesiredRevision,
        stage: ReconcileStage,
        message: String,
    ) -> ReconcileFailure {
        ReconcileFailure::error(self.scope, self.component, revision, stage, message)
    }

    fn failure(
        &self,
        revision: DesiredRevision,
        stage: ReconcileStage,
        kind: ReconcileFailureKind,
        message: String,
    ) -> ReconcileFailure {
        ReconcileFailure {
            scope: self.scope,
            component: self.component,
            revision,
            stage,
            kind,
            message,
        }
    }

    fn record_failure(&self, failure: ReconcileFailure) -> ReconcileStatus {
        *self.last_failure.borrow_mut() = Some(failure.clone());
        ReconcileStatus::Failed(failure)
    }
}
