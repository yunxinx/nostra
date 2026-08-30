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

use http_client::HttpClient;
use reqwest_client::ReqwestClient;

use crate::{
    llm::{
        GatewayGenerationService, GenerationService, HttpTransport, InMemoryMetrics,
        ProviderCatalogSnapshot,
    },
    preferences::{JSON_PROVIDER_NAME, PreferenceHandle, Preferences},
    session::SessionStores,
};

use super::{
    AsyncStop, CapabilityKey, CapabilityLease, ComponentId, ComponentSnapshot,
    ComponentSnapshotDetails, DesiredRevision, DisposeError, RuntimeSnapshot, RuntimeSnapshotError,
    ScopeError, ScopeId, ScopeTree, StartupAuditError, StartupPolicy,
};

const LOCAL_SESSION_PROVIDER: ComponentId = ComponentId::new("nostra.session.local");
const JSON_PREFERENCE_PROVIDER: ComponentId = ComponentId::new(JSON_PROVIDER_NAME);
const GATEWAY_GENERATION_PROVIDER: ComponentId = ComponentId::new("nostra.generation.gateway");
const LONG_TRANSITION_THRESHOLD: Duration = Duration::from_secs(30);

/// Build the first-party generation Provider from an explicit catalog
/// snapshot. The catalog is captured before the service is installed so every
/// Consumer observes one validated routing view for its lifetime.
pub(crate) fn default_generation_service(
    catalog: ProviderCatalogSnapshot,
    http_client: Arc<dyn HttpClient>,
) -> Arc<dyn GenerationService> {
    let metrics = Arc::new(InMemoryMetrics::new(256));
    Arc::new(GatewayGenerationService::new(
        catalog,
        HttpTransport::new(http_client),
        Some(metrics),
    ))
}

pub struct SessionServicesCapability;

impl CapabilityKey for SessionServicesCapability {
    type Handle = SessionStores;

    const NAME: &'static str = "nostra.session.services";
}

/// Application-scoped preference storage capability.
pub struct PreferenceCapability;

impl CapabilityKey for PreferenceCapability {
    type Handle = PreferenceHandle;

    const NAME: &'static str = "nostra.preferences";
}

/// Application-scoped model generation capability.
pub struct GenerationCapability;

impl CapabilityKey for GenerationCapability {
    type Handle = Arc<dyn GenerationService>;

    const NAME: &'static str = "nostra.generation";
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
    provider: ComponentId,
    preferences: PreferenceHandle,
    preference_provider: ComponentId,
    generation_service: Option<(ComponentId, Arc<dyn GenerationService>)>,
    provider_catalog: Option<ProviderCatalogSnapshot>,
    http_client: Arc<dyn HttpClient>,
}

impl CompositionRootBuilder {
    #[must_use = "composition builders must be built to install their capabilities"]
    pub const fn with_provider(mut self, provider: ComponentId) -> Self {
        self.provider = provider;
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn with_preferences(mut self, preferences: PreferenceHandle) -> Self {
        self.preferences = preferences;
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub const fn with_preferences_provider(mut self, provider: ComponentId) -> Self {
        self.preference_provider = provider;
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn with_generation_service(
        mut self,
        provider: ComponentId,
        service: Arc<dyn GenerationService>,
    ) -> Self {
        self.generation_service = Some((provider, service));
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn with_provider_catalog(mut self, catalog: ProviderCatalogSnapshot) -> Self {
        self.provider_catalog = Some(catalog);
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn with_http_client(mut self, client: Arc<dyn HttpClient>) -> Self {
        self.http_client = client;
        self
    }

    pub fn build(self) -> Result<CompositionRoot, CompositionBuildError> {
        let CompositionRootBuilder {
            session_services: session_store,
            provider,
            preferences: preference_handle,
            preference_provider,
            generation_service,
            provider_catalog,
            http_client,
        } = self;
        let (generation_provider, generation_service): (ComponentId, Arc<dyn GenerationService>) =
            generation_service.unwrap_or_else(|| {
                let catalog = provider_catalog.unwrap_or_else(|| {
                    ProviderCatalogSnapshot::new(preference_handle.snapshot().provider_profiles)
                });
                (
                    GATEWAY_GENERATION_PROVIDER,
                    default_generation_service(catalog, http_client),
                )
            });
        let application = ScopeTree::APPLICATION_SCOPE;
        let mut scopes = ScopeTree::new();
        let session_services = {
            let slot = match scopes.capability_slot::<SessionServicesCapability>(application) {
                Ok(slot) => slot,
                Err(error) => unreachable!("new application scope is open: {error}"),
            };
            let candidate =
                match slot.prepare_candidate(provider, || Ok::<_, Infallible>(session_store)) {
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
        let preferences = {
            let slot = match scopes.capability_slot::<PreferenceCapability>(application) {
                Ok(slot) => slot,
                Err(error) => unreachable!("new application scope is open: {error}"),
            };
            let candidate = match slot.prepare_candidate(preference_provider, || {
                Ok::<_, Infallible>(preference_handle)
            }) {
                Ok(candidate) => candidate,
                Err(error) => match error {},
            };
            if let Err(error) = slot.install(candidate) {
                unreachable!("new capability slot accepts its initial Provider: {error}");
            }
            match slot.current() {
                Some(lease) => lease,
                None => unreachable!("installed preference capability remains available"),
            }
        };
        let generation = {
            let slot = match scopes.capability_slot::<GenerationCapability>(application) {
                Ok(slot) => slot,
                Err(error) => unreachable!("new application scope is open: {error}"),
            };
            let candidate = match slot.prepare_candidate(generation_provider, || {
                Ok::<_, Infallible>(generation_service)
            }) {
                Ok(candidate) => candidate,
                Err(error) => match error {},
            };
            if let Err(error) = slot.install(candidate) {
                unreachable!("new capability slot accepts its initial Provider: {error}");
            }
            match slot.current() {
                Some(lease) => lease,
                None => unreachable!("installed generation capability remains available"),
            }
        };

        let snapshot = RuntimeSnapshot::new(
            [
                ComponentSnapshot::active(
                    provider,
                    application,
                    StartupPolicy::MustActivate,
                    DesiredRevision::INITIAL,
                    ComponentSnapshotDetails::default(),
                ),
                ComponentSnapshot::active(
                    preference_provider,
                    application,
                    StartupPolicy::MustActivate,
                    DesiredRevision::INITIAL,
                    ComponentSnapshotDetails::default(),
                ),
                ComponentSnapshot::active(
                    generation_provider,
                    application,
                    StartupPolicy::MustActivate,
                    DesiredRevision::INITIAL,
                    ComponentSnapshotDetails::default(),
                ),
            ],
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
            provider,
            session_services: Some(session_services),
            preference_provider,
            preferences: Some(preferences),
            generation_provider,
            generation: Some(generation),
        })
    }
}

#[must_use = "composition roots own application-scoped capabilities and must be closed"]
pub struct CompositionRoot {
    scopes: ScopeTree,
    snapshot: RuntimeSnapshot,
    provider: ComponentId,
    session_services: Option<CapabilityLease<SessionServicesCapability>>,
    preference_provider: ComponentId,
    preferences: Option<CapabilityLease<PreferenceCapability>>,
    generation_provider: ComponentId,
    generation: Option<CapabilityLease<GenerationCapability>>,
}

impl CompositionRoot {
    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn builder(session_services: SessionStores) -> CompositionRootBuilder {
        CompositionRootBuilder {
            session_services,
            provider: LOCAL_SESSION_PROVIDER,
            preferences: PreferenceHandle::json(Preferences::default()),
            preference_provider: JSON_PREFERENCE_PROVIDER,
            generation_service: None,
            provider_catalog: None,
            http_client: Arc::new(ReqwestClient::new()),
        }
    }

    /// Open the first-party local session stores and install them as the
    /// default session Provider.
    pub fn open_default() -> Result<Self, CompositionBuildError> {
        Self::builder(SessionStores::open_default())
            .with_preferences(PreferenceHandle::json(crate::preferences::load()))
            .build()
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
    pub fn preferences(&self) -> Option<&CapabilityLease<PreferenceCapability>> {
        self.preferences.as_ref()
    }

    #[must_use]
    pub fn generation(&self) -> Option<&CapabilityLease<GenerationCapability>> {
        self.generation.as_ref()
    }

    #[must_use]
    pub const fn snapshot(&self) -> &RuntimeSnapshot {
        &self.snapshot
    }

    pub async fn close(&mut self) -> Result<(), ScopeError> {
        let application = self.scopes.application();
        self.scopes.close(application).await?;
        self.session_services = None;
        self.preferences = None;
        self.generation = None;
        self.snapshot = RuntimeSnapshot::new(
            [
                ComponentSnapshot::disposed(
                    self.provider,
                    application,
                    StartupPolicy::MustActivate,
                    DesiredRevision::INITIAL,
                    ComponentSnapshotDetails::default(),
                ),
                ComponentSnapshot::disposed(
                    self.preference_provider,
                    application,
                    StartupPolicy::MustActivate,
                    DesiredRevision::INITIAL,
                    ComponentSnapshotDetails::default(),
                ),
                ComponentSnapshot::disposed(
                    self.generation_provider,
                    application,
                    StartupPolicy::MustActivate,
                    DesiredRevision::INITIAL,
                    ComponentSnapshotDetails::default(),
                ),
            ],
            [],
            LONG_TRANSITION_THRESHOLD,
        )
        .expect("disposed default composition snapshot remains valid");
        Ok(())
    }
}
