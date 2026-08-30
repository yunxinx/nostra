//! Default application composition and typed root capabilities.

use std::{
    convert::Infallible,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use crate::session::SessionStores;

use super::{
    AsyncStop, CapabilityKey, CapabilityLease, ComponentId, ComponentSnapshot,
    ComponentSnapshotDetails, DesiredRevision, DisposeError, RuntimeSnapshot, RuntimeSnapshotError,
    ScopeError, ScopeId, ScopeTree, StartupAuditError, StartupPolicy,
};

const LOCAL_SESSION_PROVIDER: ComponentId = ComponentId::new("nostra.session.local");
const LONG_TRANSITION_THRESHOLD: Duration = Duration::from_secs(30);

pub struct SessionServicesCapability;

impl CapabilityKey for SessionServicesCapability {
    type Handle = SessionStores;

    const NAME: &'static str = "nostra.session.services";
}

#[derive(Debug)]
pub enum CompositionBuildError {
    Snapshot(RuntimeSnapshotError),
    Startup(StartupAuditError),
}

struct SessionShutdownOwner {
    stores: SessionStores,
    state: Option<Arc<SessionShutdownState>>,
}

#[derive(Default)]
struct SessionShutdownState {
    status: Mutex<SessionShutdownStatus>,
}

#[derive(Default)]
struct SessionShutdownStatus {
    result: Option<Result<(), Arc<str>>>,
    waker: Option<Waker>,
}

impl SessionShutdownState {
    fn finish(&self, result: Result<(), Arc<str>>) {
        let waker = {
            let mut status = match self.status.lock() {
                Ok(status) => status,
                Err(poisoned) => poisoned.into_inner(),
            };
            status.result = Some(result);
            status.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct SessionShutdownFuture {
    state: Arc<SessionShutdownState>,
}

impl Future for SessionShutdownFuture {
    type Output = Result<(), DisposeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut status = match self.state.status.lock() {
            Ok(status) => status,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &status.result {
            Some(Ok(())) => Poll::Ready(Ok(())),
            Some(Err(error)) => Poll::Ready(Err(DisposeError::msg(error.to_string()))),
            None => {
                status.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl AsyncStop for SessionShutdownOwner {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        let state = if let Some(state) = &self.state {
            Arc::clone(state)
        } else {
            let state = Arc::new(SessionShutdownState::default());
            let worker_state = Arc::clone(&state);
            let stores = self.stores.clone();
            let spawn_result = thread::Builder::new()
                .name("nostra-runtime-session-shutdown".to_string())
                .spawn(move || {
                    let result = stores
                        .shutdown()
                        .map_err(|error| Arc::<str>::from(error.to_string()));
                    worker_state.finish(result);
                });
            if let Err(error) = spawn_result {
                state.finish(Err(Arc::from(error.to_string())));
            }
            self.state = Some(Arc::clone(&state));
            state
        };
        Box::pin(SessionShutdownFuture { state })
    }
}

impl fmt::Display for CompositionBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "default composition snapshot is invalid: {error}"),
            Self::Startup(error) => write!(f, "default composition failed startup audit: {error}"),
        }
    }
}

impl std::error::Error for CompositionBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Startup(error) => Some(error),
        }
    }
}

#[must_use = "composition builders must be built to install their capabilities"]
pub struct CompositionRootBuilder {
    session_services: SessionStores,
}

impl CompositionRootBuilder {
    pub fn build(self) -> Result<CompositionRoot, CompositionBuildError> {
        let application = ScopeTree::APPLICATION_SCOPE;
        let mut scopes = ScopeTree::new();
        let session_services = {
            let slot = match scopes.capability_slot::<SessionServicesCapability>(application) {
                Ok(slot) => slot,
                Err(error) => unreachable!("new application scope is open: {error}"),
            };
            let candidate = match slot.prepare_candidate(LOCAL_SESSION_PROVIDER, || {
                Ok::<_, Infallible>(self.session_services)
            }) {
                Ok(candidate) => candidate,
                Err(error) => match error {},
            };
            if let Err(error) = slot.install(candidate) {
                unreachable!("new capability slot accepts its initial Provider: {error}");
            }
            match slot.current() {
                Some(lease) => lease,
                None => unreachable!("installed session capability remains available"),
            }
        };

        let snapshot = RuntimeSnapshot::new(
            [ComponentSnapshot::active(
                LOCAL_SESSION_PROVIDER,
                application,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::default(),
            )],
            [],
            LONG_TRANSITION_THRESHOLD,
        )
        .map_err(CompositionBuildError::Snapshot)?;
        snapshot
            .audit_startup()
            .map_err(CompositionBuildError::Startup)?;

        let shutdown_owner = SessionShutdownOwner {
            stores: session_services.handle().clone(),
            state: None,
        };
        if let Err(error) = scopes.own_async(application, shutdown_owner) {
            unreachable!("new application scope is open: {error}");
        }

        Ok(CompositionRoot {
            scopes,
            snapshot,
            session_services: Some(session_services),
        })
    }
}

#[must_use = "composition roots own application-scoped capabilities and must be closed"]
pub struct CompositionRoot {
    scopes: ScopeTree,
    snapshot: RuntimeSnapshot,
    session_services: Option<CapabilityLease<SessionServicesCapability>>,
}

impl CompositionRoot {
    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn builder(session_services: SessionStores) -> CompositionRootBuilder {
        CompositionRootBuilder { session_services }
    }

    #[must_use]
    pub const fn application_scope(&self) -> ScopeId {
        self.scopes.application()
    }

    #[must_use]
    pub fn session_services(&self) -> Option<&CapabilityLease<SessionServicesCapability>> {
        self.session_services.as_ref()
    }

    #[must_use]
    pub const fn snapshot(&self) -> &RuntimeSnapshot {
        &self.snapshot
    }

    pub async fn close(&mut self) -> Result<(), ScopeError> {
        let application = self.scopes.application();
        self.scopes.close(application).await?;
        self.session_services = None;
        self.snapshot = RuntimeSnapshot::new(
            [ComponentSnapshot::disposed(
                LOCAL_SESSION_PROVIDER,
                application,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::default(),
            )],
            [],
            LONG_TRANSITION_THRESHOLD,
        )
        .expect("disposed default composition snapshot remains valid");
        Ok(())
    }
}
