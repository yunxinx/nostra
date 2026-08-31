//! Default application composition and typed root capabilities.

use std::{cell::RefCell, convert::Infallible, fmt, rc::Rc, sync::Arc, time::Duration};

use http_client::HttpClient;
use reqwest_client::ReqwestClient;

use crate::{
    llm::{
        GatewayGenerationService, GenerationService, HttpTransport, InMemoryMetrics,
        ProviderCatalogSnapshot,
    },
    preferences::{JSON_PROVIDER_NAME, PreferenceHandle, Preferences},
    session::{ConversationContext, ProjectIdentity, SessionStores},
};

use super::{
    CapabilityKey, CapabilityLease, ComponentId, ComponentSnapshot, ComponentSnapshotDetails,
    DesiredRevision, ExitCoordinator, NORMAL_EXIT_TIMEOUT, RuntimeSnapshot, RuntimeSnapshotError,
    ScopeError, ScopeId, ScopeTree, StartupAuditError, StartupPolicy,
};

const LOCAL_SESSION_PROVIDER: ComponentId = ComponentId::new("nostra.session.local");
const JSON_PREFERENCE_PROVIDER: ComponentId = ComponentId::new(JSON_PROVIDER_NAME);
const GATEWAY_GENERATION_PROVIDER: ComponentId = ComponentId::new("nostra.generation.gateway");
const LONG_TRANSITION_THRESHOLD: Duration = Duration::from_secs(30);

fn default_preference_handle() -> PreferenceHandle {
    #[cfg(test)]
    {
        PreferenceHandle::in_memory(Preferences::default())
    }
    #[cfg(not(test))]
    {
        PreferenceHandle::json(Preferences::default())
    }
}

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

/// The application-scoped handles projected from a composition root.
///
/// Consumers receive this cloneable bundle at construction time instead of
/// resolving capability Providers through the foreground application context.
#[derive(Clone)]
pub struct RuntimeServices {
    session_services: SessionStores,
    preference_handle: PreferenceHandle,
    generation_service: Arc<dyn GenerationService>,
    exit_coordinator: Arc<ExitCoordinator>,
    scopes: Rc<RuntimeScopeOwner>,
}

impl RuntimeServices {
    fn new(
        session_services: SessionStores,
        preference_handle: PreferenceHandle,
        generation_service: Arc<dyn GenerationService>,
        exit_coordinator: Arc<ExitCoordinator>,
        scopes: Rc<RuntimeScopeOwner>,
    ) -> Self {
        Self {
            session_services,
            preference_handle,
            generation_service,
            exit_coordinator,
            scopes,
        }
    }

    #[must_use]
    pub fn session_services(&self) -> &SessionStores {
        &self.session_services
    }

    #[must_use]
    pub fn chat_conversation(&self) -> ConversationContext {
        self.session_services.chat_conversation()
    }

    #[must_use]
    pub fn project_conversation(&self, project: ProjectIdentity) -> ConversationContext {
        self.session_services.project_conversation(project)
    }

    pub fn create_conversation_scope(&self) -> Result<ConversationScopeHandle, ScopeError> {
        self.scopes.create_conversation()
    }

    #[must_use]
    pub fn application_scope(&self) -> ScopeId {
        self.scopes.application
    }

    #[must_use]
    pub fn window_scope(&self) -> ScopeId {
        self.scopes.window
    }

    #[cfg(test)]
    pub(crate) fn scope_count(&self) -> usize {
        self.scopes.scope_count()
    }

    #[must_use]
    pub fn preference_handle(&self) -> &PreferenceHandle {
        &self.preference_handle
    }

    #[must_use]
    pub fn generation_service(&self) -> Arc<dyn GenerationService> {
        Arc::clone(&self.generation_service)
    }

    #[must_use]
    pub fn exit_coordinator(&self) -> Arc<ExitCoordinator> {
        Arc::clone(&self.exit_coordinator)
    }
}

struct RuntimeScopeOwner {
    tree: RefCell<Option<ScopeTree>>,
    application: ScopeId,
    window: ScopeId,
}

impl RuntimeScopeOwner {
    fn new(mut tree: ScopeTree) -> Result<Self, ScopeError> {
        let application = tree.application();
        let window = tree.create_window()?;
        Ok(Self {
            tree: RefCell::new(Some(tree)),
            application,
            window,
        })
    }

    fn create_conversation(self: &Rc<Self>) -> Result<ConversationScopeHandle, ScopeError> {
        let scope = self
            .tree
            .borrow_mut()
            .as_mut()
            .ok_or(ScopeError::NotOpen {
                scope: self.window,
                state: super::ScopeState::Closing,
            })?
            .create_conversation(self.window)?;
        Ok(ConversationScopeHandle {
            owner: Rc::clone(self),
            scope,
        })
    }

    async fn close_application(&self) -> Result<(), ScopeError> {
        let mut tree = self.take_tree(self.application)?;
        let result = tree.close(self.application).await;
        *self.tree.borrow_mut() = Some(tree);
        result
    }

    async fn close_scope(&self, scope: ScopeId) -> Result<(), ScopeError> {
        let mut tree = self.take_tree(scope)?;
        let result = match tree.state(scope) {
            None => Ok(()),
            Some(_) => match tree.close(scope).await {
                Ok(()) => tree.remove_closed(scope),
                Err(error) => Err(error),
            },
        };
        *self.tree.borrow_mut() = Some(tree);
        result
    }

    fn take_tree(&self, scope: ScopeId) -> Result<ScopeTree, ScopeError> {
        self.tree.borrow_mut().take().ok_or(ScopeError::NotOpen {
            scope,
            state: super::ScopeState::Closing,
        })
    }

    #[cfg(test)]
    fn scope_count(&self) -> usize {
        self.tree.borrow().as_ref().map_or(0, ScopeTree::len)
    }
}

/// Identity and ownership link for one runtime conversation scope.
#[derive(Clone)]
pub struct ConversationScopeHandle {
    owner: Rc<RuntimeScopeOwner>,
    scope: ScopeId,
}

impl ConversationScopeHandle {
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub fn parent_scope(&self) -> ScopeId {
        self.owner.window
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.owner
            .tree
            .borrow()
            .as_ref()
            .is_some_and(|tree| tree.state(self.scope) == Some(super::ScopeState::Open))
    }

    /// Close this conversation scope and wait for all owned effects to quiesce.
    /// Repeated calls after a successful close are no-ops.
    pub async fn close(&self) -> Result<(), ScopeError> {
        self.owner.close_scope(self.scope).await
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Rc::new(RuntimeScopeOwner::new(ScopeTree::new()).expect("test scope tree"))
            .create_conversation()
            .expect("test runtime accepts a conversation scope")
    }
}

impl fmt::Debug for ConversationScopeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConversationScopeHandle")
            .field("scope", &self.scope)
            .field("open", &self.is_open())
            .finish()
    }
}

#[derive(Debug)]
pub enum CompositionBuildError {
    Snapshot(RuntimeSnapshotError),
    Startup(StartupAuditError),
    Scope(ScopeError),
}

impl fmt::Display for CompositionBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "default composition snapshot is invalid: {error}"),
            Self::Startup(error) => write!(f, "default composition failed startup audit: {error}"),
            Self::Scope(error) => write!(f, "default composition could not create scopes: {error}"),
        }
    }
}

impl std::error::Error for CompositionBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Scope(error) => Some(error),
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

        let exit_coordinator = Arc::new(ExitCoordinator::new(
            session_services.handle().clone(),
            preferences.handle().clone(),
        ));

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

        Ok(CompositionRoot {
            scopes: Rc::new(RuntimeScopeOwner::new(scopes).map_err(CompositionBuildError::Scope)?),
            snapshot,
            provider,
            session_services: Some(session_services),
            preference_provider,
            preferences: Some(preferences),
            generation_provider,
            generation: Some(generation),
            exit_coordinator,
        })
    }
}

#[must_use = "composition roots own application-scoped capabilities and must be closed"]
pub struct CompositionRoot {
    scopes: Rc<RuntimeScopeOwner>,
    snapshot: RuntimeSnapshot,
    provider: ComponentId,
    session_services: Option<CapabilityLease<SessionServicesCapability>>,
    preference_provider: ComponentId,
    preferences: Option<CapabilityLease<PreferenceCapability>>,
    generation_provider: ComponentId,
    generation: Option<CapabilityLease<GenerationCapability>>,
    exit_coordinator: Arc<ExitCoordinator>,
}

impl CompositionRoot {
    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn builder(session_services: SessionStores) -> CompositionRootBuilder {
        CompositionRootBuilder {
            session_services,
            provider: LOCAL_SESSION_PROVIDER,
            preferences: default_preference_handle(),
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
    pub fn application_scope(&self) -> ScopeId {
        self.scopes.application
    }

    #[must_use]
    pub fn window_scope(&self) -> ScopeId {
        self.scopes.window
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

    /// Project the active application capabilities into the bundle consumed by
    /// the foreground shell. A partially closed root cannot produce services.
    #[must_use]
    pub fn services(&self) -> Option<RuntimeServices> {
        Some(RuntimeServices::new(
            self.session_services()?.handle().clone(),
            self.preferences()?.handle().clone(),
            self.generation()?.handle().clone(),
            self.exit_coordinator(),
            Rc::clone(&self.scopes),
        ))
    }

    #[must_use]
    pub fn exit_coordinator(&self) -> Arc<ExitCoordinator> {
        Arc::clone(&self.exit_coordinator)
    }

    #[must_use]
    pub const fn snapshot(&self) -> &RuntimeSnapshot {
        &self.snapshot
    }

    pub async fn close(&mut self) -> Result<(), ScopeError> {
        let application = self.scopes.application;
        let snapshot = self
            .preferences
            .as_ref()
            .map(|lease| lease.handle().snapshot())
            .unwrap_or_default();
        let report = self
            .exit_coordinator
            .run_blocking(snapshot, NORMAL_EXIT_TIMEOUT);
        if let Err(error) = &report.preferences {
            crate::logging::error(
                "preferences",
                format_args!("failed to save preferences during composition close: {error}"),
            );
        }
        if let Some(source) = report.session_dispose_error() {
            return Err(ScopeError::Dispose {
                scope: application,
                source,
            });
        }
        self.close_scopes().await
    }

    /// Close runtime scopes after the exit coordinator has completed. The
    /// application quit observer uses this split form so its blocking durable
    /// work remains on a background executor.
    pub(crate) async fn close_after_exit(&mut self) -> Result<(), ScopeError> {
        self.close_scopes().await
    }

    async fn close_scopes(&mut self) -> Result<(), ScopeError> {
        let application = self.scopes.application;
        self.scopes.close_application().await?;
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
