//! Default application composition and typed root capabilities.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    convert::Infallible,
    fmt,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
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
    session::{ConversationContext, ProjectIdentity, SessionStores},
    ui::markdown::{
        MarkdownExtensionKey, MarkdownExtensionSnapshot, builtin_extension_contributions,
    },
};

use super::diagnostics::ComponentTransitionState;
use super::{
    AsyncStop, CapabilityId, CapabilityKey, CapabilityLease, ComponentId, ComponentSnapshot,
    ComponentSnapshotDetails, ContributionRegistration, ContributionRegistry,
    ContributionRegistryError, ContributionRevision, DesiredRevision, DisposeError, EffectScope,
    ExclusiveSlotError, ExitCoordinator, NORMAL_EXIT_TIMEOUT, ProviderRegistration,
    ReconcileFailure, ReconcileStage, RuntimeComponentDiagnostic, RuntimeComponentState,
    RuntimeResourceCounts, RuntimeSnapshot, RuntimeSnapshotError, RuntimeSnapshotReader,
    RuntimeSnapshotSource, ScopeError, ScopeId, ScopeTree, StartupAuditError, StartupPolicy,
};
use super::{
    generation_mount::{GenerationConsumerBinding, GenerationConsumerMount},
    provider::{
        PreparedProvider, ProviderDefinitions, ProviderFactory, ProviderPrepareError,
        ProviderSelectionError, prepared_provider_factory, provider_factory, ready_provider,
    },
};

#[cfg(test)]
use super::ContributionId;

const LOCAL_SESSION_PROVIDER: ComponentId = ComponentId::new("nostra.session.local");
const JSON_PREFERENCE_PROVIDER: ComponentId = ComponentId::new(JSON_PROVIDER_NAME);
const GATEWAY_GENERATION_PROVIDER: ComponentId = ComponentId::new("nostra.generation.gateway");
const LONG_TRANSITION_THRESHOLD: Duration = Duration::from_secs(30);

struct ProviderLifecycleDiagnostic {
    event: &'static str,
    capability: CapabilityId,
    provider: ComponentId,
    scope: ScopeId,
    generation: super::ComponentGeneration,
    duration: Duration,
}

impl fmt::Display for ProviderLifecycleDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "event={} capability={} component={} scope={} provider_generation={} transition_duration_ms={}",
            self.event,
            self.capability.name(),
            self.provider,
            self.scope.raw(),
            self.generation.get(),
            self.duration.as_millis(),
        )
    }
}

fn log_runtime_snapshot(snapshot: &RuntimeSnapshot) {
    for component in snapshot.components() {
        let diagnostic = RuntimeComponentDiagnostic::new(component);
        match component.state() {
            RuntimeComponentState::Failed => {
                crate::logging::error("runtime.lifecycle", diagnostic);
            }
            RuntimeComponentState::Pending => {
                crate::logging::warn("runtime.lifecycle", diagnostic);
            }
            RuntimeComponentState::Preparing | RuntimeComponentState::Quiescing => {
                crate::logging::info("runtime.lifecycle", diagnostic);
            }
            RuntimeComponentState::Active | RuntimeComponentState::Disposed => {}
        }
    }
}

fn log_provider_lifecycle<K: CapabilityKey>(
    event: &'static str,
    capability: CapabilityId,
    lease: &CapabilityLease<K>,
    duration: Duration,
) {
    crate::logging::info(
        "runtime.lifecycle",
        ProviderLifecycleDiagnostic {
            event,
            capability,
            provider: lease.provider(),
            scope: lease.scope(),
            generation: lease.generation(),
            duration,
        },
    );
}

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
fn default_generation_service(
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
    markdown_extensions: MarkdownExtensionSnapshot,
    runtime_snapshots: RuntimeSnapshotReader,
    exit_coordinator: Arc<ExitCoordinator>,
    scopes: Rc<RuntimeScopeOwner>,
}

impl RuntimeServices {
    fn new(
        session_services: SessionStores,
        preference_handle: PreferenceHandle,
        generation_service: Arc<dyn GenerationService>,
        markdown_extensions: MarkdownExtensionSnapshot,
        runtime_snapshots: RuntimeSnapshotReader,
        exit_coordinator: Arc<ExitCoordinator>,
        scopes: Rc<RuntimeScopeOwner>,
    ) -> Self {
        Self {
            session_services,
            preference_handle,
            generation_service,
            markdown_extensions,
            runtime_snapshots,
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
    pub(crate) const fn markdown_extensions(&self) -> &MarkdownExtensionSnapshot {
        &self.markdown_extensions
    }

    #[must_use]
    pub fn runtime_snapshots(&self) -> RuntimeSnapshotReader {
        self.runtime_snapshots.clone()
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

    fn revoke_capability<K: CapabilityKey>(
        &self,
        registration: &ProviderRegistration<K>,
    ) -> Result<bool, ScopeError> {
        let mut tree = self.tree.borrow_mut();
        let tree = tree.as_mut().ok_or(ScopeError::NotOpen {
            scope: registration.scope(),
            state: super::ScopeState::Closing,
        })?;
        Ok(tree
            .capability_slot::<K>(registration.scope())?
            .revoke(registration))
    }

    fn install_capability<K: CapabilityKey>(
        &self,
        scope: ScopeId,
        provider: ComponentId,
        handle: K::Handle,
    ) -> Result<(ProviderRegistration<K>, CapabilityLease<K>), CapabilityInstallError> {
        let mut tree = self.tree.borrow_mut();
        let tree = tree.as_mut().ok_or(ScopeError::NotOpen {
            scope,
            state: super::ScopeState::Closing,
        })?;
        let slot = tree.capability_slot::<K>(scope)?;
        let candidate = match slot.prepare_candidate(provider, || Ok::<_, Infallible>(handle)) {
            Ok(candidate) => candidate,
            Err(error) => match error {},
        };
        let registration = slot.install(candidate)?;
        let lease = slot.current().ok_or(CapabilityInstallError::Unavailable {
            capability: CapabilityId::of::<K>(),
        })?;
        Ok((registration, lease))
    }

    #[cfg(test)]
    fn scope_count(&self) -> usize {
        self.tree.borrow().as_ref().map_or(0, ScopeTree::len)
    }
}

#[derive(Debug)]
enum CapabilityInstallError {
    Scope(ScopeError),
    Slot(ExclusiveSlotError),
    Unavailable { capability: CapabilityId },
}

impl fmt::Display for CapabilityInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scope(error) => write!(f, "Provider scope is unavailable: {error}"),
            Self::Slot(error) => write!(f, "Provider capability could not be published: {error}"),
            Self::Unavailable { capability } => write!(
                f,
                "Provider did not publish capability `{}`",
                capability.name()
            ),
        }
    }
}

impl std::error::Error for CapabilityInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scope(error) => Some(error),
            Self::Slot(error) => Some(error),
            Self::Unavailable { .. } => None,
        }
    }
}

impl From<ScopeError> for CapabilityInstallError {
    fn from(error: ScopeError) -> Self {
        Self::Scope(error)
    }
}

impl From<ExclusiveSlotError> for CapabilityInstallError {
    fn from(error: ExclusiveSlotError) -> Self {
        Self::Slot(error)
    }
}

struct CapabilityRegistrationEffect<K: CapabilityKey> {
    scopes: Rc<RuntimeScopeOwner>,
    registration: Option<ProviderRegistration<K>>,
    published: Rc<Cell<bool>>,
}

impl<K: CapabilityKey> CapabilityRegistrationEffect<K> {
    fn revoke(&mut self) -> anyhow::Result<()> {
        let Some(registration) = self.registration.as_ref() else {
            return Ok(());
        };
        let revoked = self.scopes.revoke_capability(registration)?;
        self.published.set(false);
        if !revoked {
            anyhow::bail!(
                "Provider registration no longer owns capability `{}`",
                K::NAME
            );
        }
        self.registration = None;
        Ok(())
    }
}

impl<K: CapabilityKey> AsyncStop for CapabilityRegistrationEffect<K> {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        Box::pin(async move { self.revoke() })
    }
}

impl<K: CapabilityKey> Drop for CapabilityRegistrationEffect<K> {
    fn drop(&mut self) {
        let Some(registration) = self.registration.as_ref() else {
            return;
        };
        let provider = registration.provider();
        let scope = registration.scope();
        if self.revoke().is_err() {
            crate::logging::error(
                "runtime.lifecycle",
                format_args!(
                    "event=provider_registration_drop_failed capability={} component={} scope={} error_kind=revoke_failed",
                    K::NAME,
                    provider,
                    scope.raw(),
                ),
            );
        }
    }
}

struct ActiveProvider<K: CapabilityKey> {
    lease: CapabilityLease<K>,
    preparation_effects: EffectScope,
    activation_effects: EffectScope,
    published: Rc<Cell<bool>>,
}

impl<K: CapabilityKey> ActiveProvider<K> {
    fn install(
        scopes: Rc<RuntimeScopeOwner>,
        scope: ScopeId,
        provider: ComponentId,
        prepared: &mut PreparedProvider<K>,
    ) -> Result<Self, ProviderActivationError> {
        Self::install_handle(scopes, scope, provider, prepared.handle().clone(), prepared)
    }

    fn install_handle(
        scopes: Rc<RuntimeScopeOwner>,
        scope: ScopeId,
        provider: ComponentId,
        handle: K::Handle,
        prepared: &mut PreparedProvider<K>,
    ) -> Result<Self, ProviderActivationError> {
        prepared
            .activate()
            .map_err(|source| ProviderActivationError {
                capability: CapabilityId::of::<K>(),
                provider,
                source,
            })?;
        let (registration, lease) = scopes
            .install_capability::<K>(scope, provider, handle)
            .map_err(|source| ProviderActivationError {
                capability: CapabilityId::of::<K>(),
                provider,
                source: anyhow::Error::new(source),
            })?;
        let published = Rc::new(Cell::new(true));
        let (preparation_effects, mut activation_effects) = prepared.take_effects();
        activation_effects.own_async(CapabilityRegistrationEffect {
            scopes,
            registration: Some(registration),
            published: Rc::clone(&published),
        });
        Ok(Self {
            lease,
            preparation_effects,
            activation_effects,
            published,
        })
    }

    fn lease(&self) -> Option<&CapabilityLease<K>> {
        self.published.get().then_some(&self.lease)
    }

    const fn retained_lease(&self) -> &CapabilityLease<K> {
        &self.lease
    }

    fn resource_counts(&self, quiescence_barrier: bool) -> RuntimeResourceCounts {
        RuntimeResourceCounts::new(
            self.preparation_effects.effect_count() + self.activation_effects.effect_count(),
            0,
            0,
            usize::from(quiescence_barrier),
        )
    }

    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.dispose_activation().await?;
        self.dispose_preparation().await
    }

    async fn dispose_activation(&mut self) -> anyhow::Result<()> {
        self.activation_effects.quiesce_and_dispose().await
    }

    async fn dispose_preparation(&mut self) -> anyhow::Result<()> {
        self.preparation_effects.quiesce_and_dispose().await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingProviderState {
    Prepared,
    RollingBack,
}

struct PendingGenerationProvider {
    provider: ComponentId,
    prepared: PreparedProvider<GenerationCapability>,
    prepared_at: std::time::Instant,
    state: PendingProviderState,
    transition: Option<CapabilityComponentTransition>,
    failure: Option<ReconcileFailure>,
}

impl PendingGenerationProvider {
    fn diagnostic_transition(&self) -> Option<CapabilityComponentTransition> {
        if self.failure.is_some() {
            None
        } else {
            self.transition.or(Some(CapabilityComponentTransition {
                stage: ReconcileStage::Preparing,
                started_at: self.prepared_at,
            }))
        }
    }
}

async fn rollback_prepared_provider<K: CapabilityKey>(
    provider: ComponentId,
    prepared: &mut PreparedProvider<K>,
) -> Result<(), ProviderRollbackError> {
    prepared
        .rollback()
        .await
        .map_err(|source| ProviderRollbackError::new::<K>(provider, source))
}

async fn dispose_active_provider<K: CapabilityKey>(
    provider: ComponentId,
    active: &mut ActiveProvider<K>,
) -> Result<(), ProviderRollbackError> {
    active
        .dispose()
        .await
        .map_err(|source| ProviderRollbackError::new::<K>(provider, source))
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

struct MarkdownContributionComponent {
    id: ComponentId,
    effects: EffectScope,
    active: bool,
    failure: Option<ReconcileFailure>,
}

struct MarkdownContributionRegistrationEffect {
    registry: Rc<RefCell<ContributionRegistry<MarkdownExtensionKey>>>,
    registration: Option<ContributionRegistration<MarkdownExtensionKey>>,
}

impl MarkdownContributionRegistrationEffect {
    fn revoke(&mut self) -> Result<(), ContributionRegistryError> {
        let Some(registration) = self.registration.as_ref() else {
            return Ok(());
        };
        self.registry.borrow_mut().revoke(registration)?;
        self.registration = None;
        Ok(())
    }
}

impl AsyncStop for MarkdownContributionRegistrationEffect {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        Box::pin(async move {
            self.revoke().map_err(anyhow::Error::new)?;
            Ok(())
        })
    }
}

impl Drop for MarkdownContributionRegistrationEffect {
    fn drop(&mut self) {
        if let Err(error) = self.revoke() {
            crate::logging::error(
                "runtime.composition",
                format_args!("failed to revoke Markdown contribution during drop: {error}"),
            );
        }
    }
}

impl MarkdownContributionComponent {
    fn new(
        registry: Rc<RefCell<ContributionRegistry<MarkdownExtensionKey>>>,
        registration: ContributionRegistration<MarkdownExtensionKey>,
    ) -> Self {
        let id = ComponentId::new(registration.id().as_str());
        let mut effects = EffectScope::new();
        effects.own_async(MarkdownContributionRegistrationEffect {
            registry,
            registration: Some(registration),
        });
        Self {
            id,
            effects,
            active: true,
            failure: None,
        }
    }

    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.effects.quiesce_and_dispose().await?;
        self.active = false;
        self.failure = None;
        Ok(())
    }

    fn snapshot(&self, scope: ScopeId) -> ComponentSnapshot {
        if let Some(failure) = self.failure.clone() {
            ComponentSnapshot::failed(
                StartupPolicy::MustActivate,
                failure,
                ComponentSnapshotDetails::new(
                    [],
                    [],
                    RuntimeResourceCounts::new(self.effects.effect_count(), 0, 0, 1),
                ),
            )
        } else if self.active {
            ComponentSnapshot::active(
                self.id,
                scope,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::new([], [], RuntimeResourceCounts::new(1, 0, 0, 0)),
            )
        } else {
            ComponentSnapshot::disposed(
                self.id,
                scope,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::default(),
            )
        }
    }
}

fn capability_component_snapshot(
    component: CapabilityComponentState,
    scope: ScopeId,
    observed_at: std::time::Instant,
) -> ComponentSnapshot {
    if let Some(transition) = component.transition {
        ComponentSnapshot::transitioning_explicit(
            ComponentTransitionState::new(
                component.id,
                scope,
                StartupPolicy::MustActivate,
                component.revision,
                transition.stage,
                transition.started_at,
                component.failure,
            ),
            observed_at,
            ComponentSnapshotDetails::new([], [], component.resources),
        )
    } else if let Some(failure) = component.failure {
        ComponentSnapshot::failed(
            StartupPolicy::MustActivate,
            failure,
            ComponentSnapshotDetails::new([], [], component.resources),
        )
    } else if component.active {
        ComponentSnapshot::active(
            component.id,
            scope,
            StartupPolicy::MustActivate,
            component.revision,
            ComponentSnapshotDetails::new([], [], component.resources),
        )
    } else {
        ComponentSnapshot::disposed(
            component.id,
            scope,
            StartupPolicy::MustActivate,
            component.revision,
            ComponentSnapshotDetails::default(),
        )
    }
}

#[derive(Clone, Copy)]
struct CapabilityComponentTransition {
    stage: ReconcileStage,
    started_at: std::time::Instant,
}

struct CapabilityComponentState {
    id: ComponentId,
    active: bool,
    revision: DesiredRevision,
    transition: Option<CapabilityComponentTransition>,
    failure: Option<ReconcileFailure>,
    resources: RuntimeResourceCounts,
}

fn composition_snapshot(
    application: ScopeId,
    capability_components: impl IntoIterator<Item = CapabilityComponentState>,
    markdown_components: &[MarkdownContributionComponent],
    markdown_revision: u64,
) -> Result<RuntimeSnapshot, RuntimeSnapshotError> {
    let observed_at = std::time::Instant::now();
    RuntimeSnapshot::new(
        capability_components
            .into_iter()
            .map(|component| capability_component_snapshot(component, application, observed_at))
            .chain(
                markdown_components
                    .iter()
                    .map(|component| component.snapshot(application)),
            ),
        [ContributionRevision::new(
            CapabilityId::of::<MarkdownExtensionKey>(),
            markdown_revision,
        )],
        LONG_TRANSITION_THRESHOLD,
    )
}

fn validate_component_ownership(
    definitions: impl IntoIterator<Item = (ComponentId, CapabilityId)>,
) -> Result<(), ProviderSelectionError> {
    let mut owners = BTreeMap::new();
    for (provider, capability) in definitions {
        if let Some(first) = owners.insert(provider, capability)
            && first != capability
        {
            return Err(ProviderSelectionError::ComponentCollision {
                provider,
                first,
                second: capability,
            });
        }
    }
    Ok(())
}

pub struct ProviderActivationError {
    capability: CapabilityId,
    provider: ComponentId,
    source: anyhow::Error,
}

impl ProviderActivationError {
    #[must_use]
    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    #[must_use]
    pub const fn provider(&self) -> ComponentId {
        self.provider
    }
}

impl fmt::Debug for ProviderActivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderActivationError")
            .field("capability", &self.capability.name())
            .field("provider", &self.provider)
            .field("kind", &"activation_failed")
            .finish()
    }
}

impl fmt::Display for ProviderActivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Provider `{}` for capability `{}` failed to activate",
            self.provider,
            self.capability.name()
        )
    }
}

impl std::error::Error for ProviderActivationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub struct ProviderRollbackError {
    capability: CapabilityId,
    provider: ComponentId,
    source: anyhow::Error,
}

impl ProviderRollbackError {
    fn new<K: CapabilityKey>(provider: ComponentId, source: anyhow::Error) -> Self {
        Self {
            capability: CapabilityId::of::<K>(),
            provider,
            source,
        }
    }

    #[must_use]
    pub const fn capability(&self) -> CapabilityId {
        self.capability
    }

    #[must_use]
    pub const fn provider(&self) -> ComponentId {
        self.provider
    }
}

impl fmt::Debug for ProviderRollbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRollbackError")
            .field("capability", &self.capability.name())
            .field("provider", &self.provider)
            .field("kind", &"rollback_failed")
            .finish()
    }
}

impl fmt::Display for ProviderRollbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Provider `{}` for capability `{}` failed to roll back",
            self.provider,
            self.capability.name()
        )
    }
}

impl std::error::Error for ProviderRollbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupCleanupStage {
    GenerationActivation,
    PreferenceActivation,
    SessionActivation,
    MarkdownContributions,
    PreferencePreparation,
    SessionPreparation,
    GenerationPreparation,
    ScopeTree,
}

impl fmt::Display for StartupCleanupStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::GenerationActivation => "generation_activation",
            Self::PreferenceActivation => "preference_activation",
            Self::SessionActivation => "session_activation",
            Self::MarkdownContributions => "markdown_contributions",
            Self::PreferencePreparation => "preference_preparation",
            Self::SessionPreparation => "session_preparation",
            Self::GenerationPreparation => "generation_preparation",
            Self::ScopeTree => "scope_tree",
        };
        f.write_str(name)
    }
}

pub struct StartupCleanupError {
    stage: StartupCleanupStage,
    component: Option<ComponentId>,
    source: anyhow::Error,
    owner: Box<StartupCleanup>,
}

impl StartupCleanupError {
    fn new(
        stage: StartupCleanupStage,
        component: Option<ComponentId>,
        source: anyhow::Error,
        owner: StartupCleanup,
    ) -> Self {
        Self {
            stage,
            component,
            source,
            owner: Box::new(owner),
        }
    }

    #[must_use]
    pub const fn stage(&self) -> StartupCleanupStage {
        self.stage
    }

    #[must_use]
    pub const fn component(&self) -> Option<ComponentId> {
        self.component
    }

    #[must_use]
    pub fn retained_effect_count(&self) -> usize {
        self.owner.effect_count()
    }

    async fn retry(self) -> Result<(), Self> {
        let Self {
            stage: _,
            component: _,
            source: _,
            owner,
        } = self;
        let mut owner = owner;
        match owner.dispose().await {
            Ok(()) => Ok(()),
            Err(failure) => Err(Self::new(
                failure.stage,
                failure.component,
                failure.source,
                *owner,
            )),
        }
    }
}

impl fmt::Debug for StartupCleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupCleanupError")
            .field("stage", &self.stage)
            .field("component", &self.component)
            .field("kind", &"startup_cleanup_failed")
            .field("pending_ownership", &true)
            .field("retained_effect_count", &self.retained_effect_count())
            .finish()
    }
}

impl fmt::Display for StartupCleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "startup cleanup failed at {}", self.stage)?;
        if let Some(component) = self.component {
            write!(f, " for component `{component}`")?;
        }
        Ok(())
    }
}

impl std::error::Error for StartupCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

struct StartupCleanupFailure {
    stage: StartupCleanupStage,
    component: Option<ComponentId>,
    source: anyhow::Error,
}

struct StartupProvider<K: CapabilityKey> {
    provider: ComponentId,
    prepared: Option<PreparedProvider<K>>,
    active: Option<ActiveProvider<K>>,
}

impl<K: CapabilityKey> StartupProvider<K> {
    fn new(provider: ComponentId, prepared: PreparedProvider<K>) -> Self {
        Self {
            provider,
            prepared: Some(prepared),
            active: None,
        }
    }

    fn prepared_mut(&mut self) -> &mut PreparedProvider<K> {
        let Some(prepared) = self.prepared.as_mut() else {
            unreachable!("startup Provider remains prepared until activation succeeds")
        };
        prepared
    }

    fn activate(
        &mut self,
        scopes: Rc<RuntimeScopeOwner>,
        scope: ScopeId,
    ) -> Result<(), ProviderActivationError> {
        let provider = self.provider;
        let active = ActiveProvider::install(scopes, scope, provider, self.prepared_mut())?;
        self.prepared = None;
        self.active = Some(active);
        Ok(())
    }

    fn activate_handle(
        &mut self,
        scopes: Rc<RuntimeScopeOwner>,
        scope: ScopeId,
        handle: K::Handle,
    ) -> Result<(), ProviderActivationError> {
        let provider = self.provider;
        let active =
            ActiveProvider::install_handle(scopes, scope, provider, handle, self.prepared_mut())?;
        self.prepared = None;
        self.active = Some(active);
        Ok(())
    }

    async fn dispose_activation(&mut self) -> Result<(), ProviderRollbackError> {
        let result = if let Some(active) = self.active.as_mut() {
            active.dispose_activation().await
        } else if let Some(prepared) = self.prepared.as_mut() {
            prepared.rollback_activation().await
        } else {
            Ok(())
        };
        result.map_err(|source| ProviderRollbackError::new::<K>(self.provider, source))
    }

    async fn dispose_preparation(&mut self) -> Result<(), ProviderRollbackError> {
        let result = if let Some(active) = self.active.as_mut() {
            active.dispose_preparation().await
        } else if let Some(prepared) = self.prepared.as_mut() {
            prepared.rollback_preparation().await
        } else {
            Ok(())
        };
        result.map_err(|source| ProviderRollbackError::new::<K>(self.provider, source))
    }

    fn take_active(&mut self) -> ActiveProvider<K> {
        let Some(active) = self.active.take() else {
            unreachable!("startup Provider must be active before composition publication")
        };
        active
    }

    fn active(&self) -> &ActiveProvider<K> {
        let Some(active) = self.active.as_ref() else {
            unreachable!("startup Provider must be active before composition publication")
        };
        active
    }

    fn effect_count(&self) -> usize {
        if let Some(active) = &self.active {
            active.resource_counts(false).effects()
        } else {
            self.prepared
                .as_ref()
                .map_or(0, PreparedProvider::effect_count)
        }
    }
}

struct StartupCleanup {
    scopes: Rc<RuntimeScopeOwner>,
    generation: Option<StartupProvider<GenerationCapability>>,
    session: Option<StartupProvider<SessionServicesCapability>>,
    preferences: Option<StartupProvider<PreferenceCapability>>,
    markdown_components: Vec<MarkdownContributionComponent>,
}

async fn finish_startup_failure(
    cleanup: StartupCleanup,
    cause: CompositionBuildError,
) -> CompositionBuildError {
    let mut cleanup = cleanup;
    match cleanup.dispose().await {
        Ok(()) => cause,
        Err(failure) => CompositionBuildError::StartupCleanup {
            cause: Box::new(cause),
            cleanup: StartupCleanupError::new(
                failure.stage,
                failure.component,
                failure.source,
                cleanup,
            ),
        },
    }
}

impl StartupCleanup {
    fn new(scopes: Rc<RuntimeScopeOwner>) -> Self {
        Self {
            scopes,
            generation: None,
            session: None,
            preferences: None,
            markdown_components: Vec::new(),
        }
    }

    async fn dispose(&mut self) -> Result<(), StartupCleanupFailure> {
        macro_rules! dispose_provider_phase {
            ($provider:expr, $method:ident, $stage:expr) => {
                if let Some(provider) = $provider.as_mut()
                    && let Err(error) = provider.$method().await
                {
                    return Err(StartupCleanupFailure {
                        stage: $stage,
                        component: Some(provider.provider),
                        source: anyhow::Error::new(error),
                    });
                }
            };
        }

        dispose_provider_phase!(
            self.generation,
            dispose_activation,
            StartupCleanupStage::GenerationActivation
        );
        dispose_provider_phase!(
            self.preferences,
            dispose_activation,
            StartupCleanupStage::PreferenceActivation
        );
        dispose_provider_phase!(
            self.session,
            dispose_activation,
            StartupCleanupStage::SessionActivation
        );
        for component in self.markdown_components.iter_mut().rev() {
            if let Err(source) = component.dispose().await {
                return Err(StartupCleanupFailure {
                    stage: StartupCleanupStage::MarkdownContributions,
                    component: Some(component.id),
                    source,
                });
            }
        }
        dispose_provider_phase!(
            self.preferences,
            dispose_preparation,
            StartupCleanupStage::PreferencePreparation
        );
        dispose_provider_phase!(
            self.session,
            dispose_preparation,
            StartupCleanupStage::SessionPreparation
        );
        dispose_provider_phase!(
            self.generation,
            dispose_preparation,
            StartupCleanupStage::GenerationPreparation
        );
        if let Err(error) = self.scopes.close_application().await {
            return Err(StartupCleanupFailure {
                stage: StartupCleanupStage::ScopeTree,
                component: None,
                source: anyhow::Error::new(error),
            });
        }
        Ok(())
    }

    fn effect_count(&self) -> usize {
        self.generation
            .as_ref()
            .map_or(0, StartupProvider::effect_count)
            + self
                .session
                .as_ref()
                .map_or(0, StartupProvider::effect_count)
            + self
                .preferences
                .as_ref()
                .map_or(0, StartupProvider::effect_count)
            + self
                .markdown_components
                .iter()
                .map(|component| component.effects.effect_count())
                .sum::<usize>()
    }
}

#[derive(Debug)]
pub enum CompositionReplaceError {
    Providers(ProviderSelectionError),
    Prepare(ProviderPrepareError),
    Activate(ProviderActivationError),
    Rollback(ProviderRollbackError),
    RevisionExhausted { capability: CapabilityId },
    Scope(ScopeError),
    Slot(ExclusiveSlotError),
    Snapshot(RuntimeSnapshotError),
    Unavailable { capability: CapabilityId },
}

impl fmt::Display for CompositionReplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Providers(error) => write!(f, "replacement Provider is invalid: {error}"),
            Self::Prepare(error) => write!(f, "replacement Provider preparation failed: {error}"),
            Self::Activate(error) => write!(f, "replacement Provider activation failed: {error}"),
            Self::Rollback(error) => write!(f, "replacement Provider rollback failed: {error}"),
            Self::RevisionExhausted { capability } => write!(
                f,
                "replacement revisions are exhausted for capability `{}`",
                capability.name()
            ),
            Self::Scope(error) => write!(f, "replacement scope is unavailable: {error}"),
            Self::Slot(error) => {
                write!(f, "replacement capability could not be published: {error}")
            }
            Self::Snapshot(error) => write!(f, "replacement snapshot is invalid: {error}"),
            Self::Unavailable { capability } => write!(
                f,
                "replacement did not publish capability `{}`",
                capability.name()
            ),
        }
    }
}

impl std::error::Error for CompositionReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Providers(error) => Some(error),
            Self::Prepare(error) => Some(error),
            Self::Activate(error) => Some(error),
            Self::Rollback(error) => Some(error),
            Self::RevisionExhausted { .. } => None,
            Self::Scope(error) => Some(error),
            Self::Slot(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::Unavailable { .. } => None,
        }
    }
}

impl From<ScopeError> for CompositionReplaceError {
    fn from(error: ScopeError) -> Self {
        Self::Scope(error)
    }
}

impl From<ExclusiveSlotError> for CompositionReplaceError {
    fn from(error: ExclusiveSlotError) -> Self {
        Self::Slot(error)
    }
}

impl From<RuntimeSnapshotError> for CompositionReplaceError {
    fn from(error: RuntimeSnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

#[derive(Debug)]
pub enum CompositionBuildError {
    Providers(ProviderSelectionError),
    Prepare(ProviderPrepareError),
    Activate(ProviderActivationError),
    Contributions(ContributionRegistryError),
    Snapshot(RuntimeSnapshotError),
    Startup(StartupAuditError),
    StartupCleanup {
        cause: Box<Self>,
        cleanup: StartupCleanupError,
    },
    Scope(ScopeError),
}

impl fmt::Display for CompositionBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Providers(error) => {
                write!(f, "default composition Providers are invalid: {error}")
            }
            Self::Prepare(error) => {
                write!(
                    f,
                    "default composition Provider preparation failed: {error}"
                )
            }
            Self::Activate(error) => {
                write!(f, "default composition Provider activation failed: {error}")
            }
            Self::Contributions(error) => {
                write!(f, "default composition contributions are invalid: {error}")
            }
            Self::Snapshot(error) => write!(f, "default composition snapshot is invalid: {error}"),
            Self::Startup(error) => write!(f, "default composition failed startup audit: {error}"),
            Self::StartupCleanup { cause, cleanup } => write!(
                f,
                "default composition cleanup failed after `{cause}`: {cleanup}"
            ),
            Self::Scope(error) => write!(f, "default composition could not create scopes: {error}"),
        }
    }
}

impl std::error::Error for CompositionBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Providers(error) => Some(error),
            Self::Prepare(error) => Some(error),
            Self::Activate(error) => Some(error),
            Self::Contributions(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::StartupCleanup { cause, .. } => Some(cause.as_ref()),
            Self::Scope(error) => Some(error),
        }
    }
}

impl CompositionBuildError {
    #[must_use]
    pub fn startup_cleanup(&self) -> Option<&StartupCleanupError> {
        match self {
            Self::StartupCleanup { cleanup, .. } => Some(cleanup),
            _ => None,
        }
    }

    /// Retry an incomplete startup rollback. On success, returns the original
    /// build failure after every retained effect and scope has been released.
    pub async fn retry_startup_cleanup(self) -> Result<Self, Self> {
        let Self::StartupCleanup { cause, cleanup } = self else {
            return Ok(self);
        };
        match cleanup.retry().await {
            Ok(()) => Ok(*cause),
            Err(cleanup) => Err(Self::StartupCleanup { cause, cleanup }),
        }
    }
}

#[must_use = "composition builders must be built to install their capabilities"]
pub struct CompositionRootBuilder {
    session_providers: Vec<(ComponentId, ProviderFactory<SessionServicesCapability>)>,
    selected_session_provider: ComponentId,
    preferences: PreferenceHandle,
    preference_provider: ComponentId,
    generation_providers: Vec<(ComponentId, ProviderFactory<GenerationCapability>)>,
    selected_generation_provider: ComponentId,
    provider_catalog: Option<ProviderCatalogSnapshot>,
    http_client: Arc<dyn HttpClient>,
}

impl CompositionRootBuilder {
    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn register_session_provider(
        mut self,
        provider: ComponentId,
        prepare: impl Fn() -> anyhow::Result<SessionStores> + 'static,
    ) -> Self {
        self.session_providers.push((
            provider,
            provider_factory::<SessionServicesCapability>(prepare),
        ));
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn register_prepared_session_provider(
        mut self,
        provider: ComponentId,
        prepare: impl Fn() -> anyhow::Result<PreparedProvider<SessionServicesCapability>> + 'static,
    ) -> Self {
        self.session_providers.push((
            provider,
            prepared_provider_factory::<SessionServicesCapability>(prepare),
        ));
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub const fn select_session_provider(mut self, provider: ComponentId) -> Self {
        self.selected_session_provider = provider;
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
    pub fn register_generation_provider(
        mut self,
        provider: ComponentId,
        service: Arc<dyn GenerationService>,
    ) -> Self {
        self.generation_providers
            .push((provider, ready_provider::<GenerationCapability>(service)));
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn register_generation_provider_factory(
        mut self,
        provider: ComponentId,
        prepare: impl Fn() -> anyhow::Result<Arc<dyn GenerationService>> + 'static,
    ) -> Self {
        self.generation_providers
            .push((provider, provider_factory::<GenerationCapability>(prepare)));
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn register_prepared_generation_provider(
        mut self,
        provider: ComponentId,
        prepare: impl Fn() -> anyhow::Result<PreparedProvider<GenerationCapability>> + 'static,
    ) -> Self {
        self.generation_providers.push((
            provider,
            prepared_provider_factory::<GenerationCapability>(prepare),
        ));
        self
    }

    #[must_use = "composition builders must be built to install their capabilities"]
    pub const fn select_generation_provider(mut self, provider: ComponentId) -> Self {
        self.selected_generation_provider = provider;
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

    pub async fn build(self) -> Result<CompositionRoot, CompositionBuildError> {
        let CompositionRootBuilder {
            session_providers,
            selected_session_provider,
            preferences: preference_handle,
            preference_provider,
            generation_providers,
            selected_generation_provider,
            provider_catalog,
            http_client,
        } = self;
        let session_definitions = ProviderDefinitions::<SessionServicesCapability>::with_factories(
            session_providers,
            selected_session_provider,
        )
        .map_err(CompositionBuildError::Providers)?;
        let catalog = provider_catalog.unwrap_or_else(|| {
            ProviderCatalogSnapshot::new(preference_handle.snapshot().provider_profiles)
        });
        let default_generation_factory = provider_factory::<GenerationCapability>(move || {
            Ok(default_generation_service(
                catalog.clone(),
                Arc::clone(&http_client),
            ))
        });
        let mut all_generation_providers =
            vec![(GATEWAY_GENERATION_PROVIDER, default_generation_factory)];
        all_generation_providers.extend(generation_providers);
        let generation_definitions = ProviderDefinitions::<GenerationCapability>::with_factories(
            all_generation_providers,
            selected_generation_provider,
        )
        .map_err(CompositionBuildError::Providers)?;
        let markdown_definitions = builtin_extension_contributions();
        validate_component_ownership(
            session_definitions
                .ids()
                .map(|provider| (provider, CapabilityId::of::<SessionServicesCapability>()))
                .chain(std::iter::once((
                    preference_provider,
                    CapabilityId::of::<PreferenceCapability>(),
                )))
                .chain(
                    generation_definitions
                        .ids()
                        .map(|provider| (provider, CapabilityId::of::<GenerationCapability>())),
                )
                .chain(markdown_definitions.iter().map(|definition| {
                    (
                        ComponentId::new(definition.id().as_str()),
                        CapabilityId::of::<MarkdownExtensionKey>(),
                    )
                })),
        )
        .map_err(CompositionBuildError::Providers)?;
        let scopes = Rc::new(
            RuntimeScopeOwner::new(ScopeTree::new()).map_err(CompositionBuildError::Scope)?,
        );
        let application = scopes.application;
        let mut startup = StartupCleanup::new(Rc::clone(&scopes));
        let (generation_provider, generation_prepared) = match generation_definitions
            .prepare_selected()
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(
                    finish_startup_failure(startup, CompositionBuildError::Prepare(error)).await,
                );
            }
        };
        startup.generation = Some(StartupProvider::new(
            generation_provider,
            generation_prepared,
        ));
        let (session_provider, session_prepared) = match session_definitions.prepare_selected() {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(
                    finish_startup_failure(startup, CompositionBuildError::Prepare(error)).await,
                );
            }
        };
        startup.session = Some(StartupProvider::new(session_provider, session_prepared));
        startup.preferences = Some(StartupProvider::new(
            preference_provider,
            PreparedProvider::<PreferenceCapability>::new(preference_handle),
        ));
        let markdown_registry = Rc::new(RefCell::new(
            ContributionRegistry::<MarkdownExtensionKey>::new(application),
        ));
        let markdown_registration = {
            markdown_registry
                .borrow_mut()
                .register_batch(application, markdown_definitions)
        };
        let markdown_registrations = match markdown_registration {
            Ok(registrations) => registrations,
            Err(error) => {
                return Err(finish_startup_failure(
                    startup,
                    CompositionBuildError::Contributions(error),
                )
                .await);
            }
        };
        startup.markdown_components = markdown_registrations
            .into_iter()
            .map(|registration| {
                MarkdownContributionComponent::new(Rc::clone(&markdown_registry), registration)
            })
            .collect::<Vec<_>>();
        let markdown_snapshot = { markdown_registry.borrow().snapshot(application) };
        let markdown_extensions = match markdown_snapshot {
            Ok(snapshot) => MarkdownExtensionSnapshot::from(&snapshot),
            Err(error) => {
                return Err(finish_startup_failure(
                    startup,
                    CompositionBuildError::Contributions(error),
                )
                .await);
            }
        };
        let session_activation = {
            let Some(session) = startup.session.as_mut() else {
                unreachable!("prepared session Provider is owned by startup")
            };
            session.activate(Rc::clone(&scopes), application)
        };
        if let Err(error) = session_activation {
            return Err(
                finish_startup_failure(startup, CompositionBuildError::Activate(error)).await,
            );
        }
        let preference_activation = {
            let Some(preferences) = startup.preferences.as_mut() else {
                unreachable!("prepared preference Provider is owned by startup")
            };
            preferences.activate(Rc::clone(&scopes), application)
        };
        if let Err(error) = preference_activation {
            return Err(
                finish_startup_failure(startup, CompositionBuildError::Activate(error)).await,
            );
        }
        let generation_service = {
            let Some(generation) = startup.generation.as_mut() else {
                unreachable!("prepared generation Provider is owned by startup")
            };
            generation.prepared_mut().handle().clone()
        };
        let generation_binding = GenerationConsumerBinding::new(generation_service);
        let generation_mount = GenerationConsumerMount::new(generation_binding.service());
        let generation_activation = {
            let Some(generation) = startup.generation.as_mut() else {
                unreachable!("prepared generation Provider is owned by startup")
            };
            generation.activate_handle(
                Rc::clone(&scopes),
                application,
                generation_binding.service(),
            )
        };
        if let Err(error) = generation_activation {
            return Err(
                finish_startup_failure(startup, CompositionBuildError::Activate(error)).await,
            );
        }

        let (session_services, preferences, generation) = {
            let Some(session) = startup.session.as_ref() else {
                unreachable!("active session Provider is owned by startup")
            };
            let Some(preferences) = startup.preferences.as_ref() else {
                unreachable!("active preference Provider is owned by startup")
            };
            let Some(generation) = startup.generation.as_ref() else {
                unreachable!("active generation Provider is owned by startup")
            };
            (session.active(), preferences.active(), generation.active())
        };
        let exit_coordinator = Arc::new(ExitCoordinator::new(
            session_services.retained_lease().handle().clone(),
            preferences.retained_lease().handle().clone(),
        ));

        let snapshot = match composition_snapshot(
            application,
            [
                CapabilityComponentState {
                    id: session_provider,
                    active: true,
                    revision: DesiredRevision::INITIAL,
                    transition: None,
                    failure: None,
                    resources: session_services.resource_counts(false),
                },
                CapabilityComponentState {
                    id: preference_provider,
                    active: true,
                    revision: DesiredRevision::INITIAL,
                    transition: None,
                    failure: None,
                    resources: preferences.resource_counts(false),
                },
                CapabilityComponentState {
                    id: generation_provider,
                    active: true,
                    revision: DesiredRevision::INITIAL,
                    transition: None,
                    failure: None,
                    resources: generation.resource_counts(false),
                },
            ],
            &startup.markdown_components,
            markdown_extensions.revision(),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(finish_startup_failure(
                    startup,
                    CompositionBuildError::Snapshot(error),
                )
                .await);
            }
        };
        log_provider_lifecycle(
            "provider_published",
            CapabilityId::of::<SessionServicesCapability>(),
            session_services.retained_lease(),
            Duration::ZERO,
        );
        log_provider_lifecycle(
            "provider_published",
            CapabilityId::of::<PreferenceCapability>(),
            preferences.retained_lease(),
            Duration::ZERO,
        );
        log_provider_lifecycle(
            "provider_published",
            CapabilityId::of::<GenerationCapability>(),
            generation.retained_lease(),
            Duration::ZERO,
        );
        log_runtime_snapshot(&snapshot);
        if let Err(error) = snapshot.audit_startup() {
            crate::logging::error(
                "runtime.lifecycle",
                format_args!(
                    "event=startup_audit status=failed blocker_count={}",
                    error.blockers().len(),
                ),
            );
            return Err(
                finish_startup_failure(startup, CompositionBuildError::Startup(error)).await,
            );
        }
        crate::logging::info(
            "runtime.lifecycle",
            "event=startup_audit status=active blocker_count=0",
        );

        let runtime_snapshots = RuntimeSnapshotSource::new(snapshot.clone());
        let session_services = {
            let Some(session) = startup.session.as_mut() else {
                unreachable!("active session Provider is owned by startup")
            };
            session.take_active()
        };
        let preferences = {
            let Some(preferences) = startup.preferences.as_mut() else {
                unreachable!("active preference Provider is owned by startup")
            };
            preferences.take_active()
        };
        let generation = {
            let Some(generation) = startup.generation.as_mut() else {
                unreachable!("active generation Provider is owned by startup")
            };
            generation.take_active()
        };
        let markdown_components = std::mem::take(&mut startup.markdown_components);
        Ok(CompositionRoot {
            scopes,
            snapshot,
            runtime_snapshots,
            session_definitions,
            provider: session_provider,
            session_services: Some(session_services),
            session_failure: None,
            preference_provider,
            preferences: Some(preferences),
            preference_failure: None,
            generation_definitions,
            generation_provider,
            generation_revision: DesiredRevision::INITIAL,
            generation_transition: None,
            generation_failure: None,
            generation: Some(generation),
            generation_binding: Some(generation_binding),
            pending_generation: None,
            generation_mount,
            markdown_components,
            markdown_registry,
            markdown_extensions: Some(markdown_extensions),
            exit_coordinator,
            closed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn build_blocking(self) -> Result<CompositionRoot, CompositionBuildError> {
        futures::executor::block_on(self.build())
    }
}

#[must_use = "composition roots own application-scoped capabilities and must be closed"]
pub struct CompositionRoot {
    scopes: Rc<RuntimeScopeOwner>,
    snapshot: RuntimeSnapshot,
    runtime_snapshots: RuntimeSnapshotSource,
    session_definitions: ProviderDefinitions<SessionServicesCapability>,
    provider: ComponentId,
    session_services: Option<ActiveProvider<SessionServicesCapability>>,
    session_failure: Option<ReconcileFailure>,
    preference_provider: ComponentId,
    preferences: Option<ActiveProvider<PreferenceCapability>>,
    preference_failure: Option<ReconcileFailure>,
    generation_definitions: ProviderDefinitions<GenerationCapability>,
    generation_provider: ComponentId,
    generation_revision: DesiredRevision,
    generation_transition: Option<CapabilityComponentTransition>,
    generation_failure: Option<ReconcileFailure>,
    generation: Option<ActiveProvider<GenerationCapability>>,
    generation_binding: Option<GenerationConsumerBinding>,
    pending_generation: Option<PendingGenerationProvider>,
    generation_mount: GenerationConsumerMount,
    markdown_components: Vec<MarkdownContributionComponent>,
    markdown_registry: Rc<RefCell<ContributionRegistry<MarkdownExtensionKey>>>,
    markdown_extensions: Option<MarkdownExtensionSnapshot>,
    exit_coordinator: Arc<ExitCoordinator>,
    closed: bool,
}

impl CompositionRoot {
    #[must_use = "composition builders must be built to install their capabilities"]
    pub fn builder(session_services: SessionStores) -> CompositionRootBuilder {
        CompositionRootBuilder {
            session_providers: vec![(
                LOCAL_SESSION_PROVIDER,
                ready_provider::<SessionServicesCapability>(session_services),
            )],
            selected_session_provider: LOCAL_SESSION_PROVIDER,
            preferences: default_preference_handle(),
            preference_provider: JSON_PREFERENCE_PROVIDER,
            generation_providers: Vec::new(),
            selected_generation_provider: GATEWAY_GENERATION_PROVIDER,
            provider_catalog: None,
            http_client: Arc::new(ReqwestClient::new()),
        }
    }

    /// Open the first-party local session stores and install them as the
    /// default session Provider.
    pub async fn open_default() -> Result<Self, CompositionBuildError> {
        Self::builder(SessionStores::open_default())
            .with_preferences(PreferenceHandle::json(crate::preferences::load()))
            .build()
            .await
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
        self.session_services.as_ref()?.lease()
    }

    #[must_use]
    pub fn preferences(&self) -> Option<&CapabilityLease<PreferenceCapability>> {
        self.preferences.as_ref()?.lease()
    }

    #[must_use]
    pub fn generation(&self) -> Option<&CapabilityLease<GenerationCapability>> {
        self.generation.as_ref()?.lease()
    }

    #[must_use]
    pub fn registered_session_providers(&self) -> Vec<ComponentId> {
        self.session_definitions.ids().collect()
    }

    #[must_use]
    pub fn registered_generation_providers(&self) -> Vec<ComponentId> {
        self.generation_definitions.ids().collect()
    }

    #[cfg(test)]
    pub(crate) fn exhaust_generation_revision_for_test(&mut self) {
        self.generation_revision = DesiredRevision::for_test(u64::MAX);
    }

    pub async fn replace_generation_provider(
        &mut self,
        provider: ComponentId,
    ) -> Result<bool, CompositionReplaceError> {
        if self.generation_provider == provider
            && self.generation.is_some()
            && self.generation_transition.is_none()
            && self.generation_failure.is_none()
        {
            return Ok(false);
        }
        if !self.generation_definitions.contains(provider) {
            let error = ProviderSelectionError::Unknown {
                capability: CapabilityId::of::<GenerationCapability>(),
                provider,
            };
            crate::logging::warn(
                "runtime.lifecycle",
                format_args!(
                    "event=provider_candidate_rejected capability={} component={} scope={} error_kind=unknown_provider",
                    CapabilityId::of::<GenerationCapability>().name(),
                    provider,
                    self.scopes.application.raw(),
                ),
            );
            return Err(CompositionReplaceError::Providers(error));
        }
        let next_revision = self.generation_revision.next().ok_or_else(|| {
            crate::logging::error(
                "runtime.lifecycle",
                format_args!(
                    "event=provider_replacement_rejected capability={} component={} scope={} error_kind=revision_exhausted",
                    CapabilityId::of::<GenerationCapability>().name(),
                    provider,
                    self.scopes.application.raw(),
                ),
            );
            CompositionReplaceError::RevisionExhausted {
                capability: CapabilityId::of::<GenerationCapability>(),
            }
        })?;

        let rollback_existing = self.pending_generation.as_ref().is_some_and(|pending| {
            pending.provider != provider || pending.state == PendingProviderState::RollingBack
        });
        if rollback_existing {
            self.rollback_pending_generation().await?;
        }
        if self.pending_generation.is_none() {
            let (_, prepared) = self.generation_definitions.prepare(provider).map_err(|error| {
                crate::logging::warn(
                    "runtime.lifecycle",
                    format_args!(
                        "event=provider_candidate_rejected capability={} component={} scope={} error_kind={}",
                        CapabilityId::of::<GenerationCapability>().name(),
                        provider,
                        self.scopes.application.raw(),
                        error.kind().as_str(),
                    ),
                );
                CompositionReplaceError::Prepare(error)
            })?;
            self.pending_generation = Some(PendingGenerationProvider {
                provider,
                prepared,
                prepared_at: std::time::Instant::now(),
                state: PendingProviderState::Prepared,
                transition: None,
                failure: None,
            });
        }

        let transition_started = std::time::Instant::now();
        self.generation_revision = next_revision;
        self.generation_failure = None;

        if self.generation_binding.is_some() && self.generation.is_some() {
            self.generation_transition = Some(CapabilityComponentTransition {
                stage: ReconcileStage::Stopping,
                started_at: transition_started,
            });
            if let Err(error) = self.rebuild_snapshot() {
                self.generation_transition = None;
                return Err(CompositionReplaceError::Snapshot(error));
            }
            let Some(binding) = self.generation_binding.as_ref() else {
                unreachable!("generation binding was checked before quiescence")
            };
            let Some(lease) = self.generation.as_ref() else {
                unreachable!("generation lease was checked before quiescence")
            };
            log_provider_lifecycle(
                "provider_quiescing",
                CapabilityId::of::<GenerationCapability>(),
                lease.retained_lease(),
                Duration::ZERO,
            );
            binding.quiesce().await;
            self.generation_mount.unmount();
            let dispose_result = {
                let Some(active) = self.generation.as_mut() else {
                    unreachable!("generation Provider remains owned during quiescence")
                };
                dispose_active_provider(self.generation_provider, active).await
            };
            if let Err(error) = dispose_result {
                self.generation_transition = None;
                self.generation_failure = Some(ReconcileFailure::error(
                    self.scopes.application,
                    self.generation_provider,
                    self.generation_revision,
                    ReconcileStage::Stopping,
                    error.to_string(),
                ));
                self.rebuild_snapshot()?;
                return Err(CompositionReplaceError::Rollback(error));
            }
            self.generation = None;
            self.generation_binding = None;
            self.generation_transition = None;
            self.generation_failure = None;
        }

        self.generation_provider = provider;
        self.generation_transition = Some(CapabilityComponentTransition {
            stage: ReconcileStage::Activating,
            started_at: transition_started,
        });
        self.rebuild_snapshot()?;

        let candidate_binding = {
            let Some(pending) = self.pending_generation.as_ref() else {
                unreachable!("prepared generation candidate is retained until activation")
            };
            GenerationConsumerBinding::new(pending.prepared.handle().clone())
        };
        let activation = {
            let Some(pending) = self.pending_generation.as_mut() else {
                unreachable!("prepared generation candidate is retained until activation")
            };
            ActiveProvider::install_handle(
                Rc::clone(&self.scopes),
                self.scopes.application,
                provider,
                candidate_binding.service(),
                &mut pending.prepared,
            )
        };
        let generation = match activation {
            Ok(generation) => generation,
            Err(error) => {
                self.rollback_failed_generation_activation(&error).await?;
                return Err(CompositionReplaceError::Activate(error));
            }
        };
        self.generation = Some(generation);
        self.generation_mount.remount(candidate_binding.service());
        self.generation_binding = Some(candidate_binding);
        self.pending_generation = None;
        self.generation_transition = None;
        self.generation_failure = None;
        if let Some(lease) = &self.generation {
            log_provider_lifecycle(
                "provider_published",
                CapabilityId::of::<GenerationCapability>(),
                lease.retained_lease(),
                transition_started.elapsed(),
            );
        }
        self.rebuild_snapshot()?;
        Ok(true)
    }

    async fn rollback_pending_generation(&mut self) -> Result<(), CompositionReplaceError> {
        let Some(pending) = self.pending_generation.as_mut() else {
            return Ok(());
        };
        pending.state = PendingProviderState::RollingBack;
        let provider = pending.provider;
        pending.transition = Some(CapabilityComponentTransition {
            stage: ReconcileStage::RollingBack,
            started_at: std::time::Instant::now(),
        });
        pending.failure = None;
        self.rebuild_snapshot()?;
        let rollback = {
            let Some(pending) = self.pending_generation.as_mut() else {
                unreachable!("pending generation remains owned during rollback")
            };
            rollback_prepared_provider::<GenerationCapability>(provider, &mut pending.prepared)
                .await
        };
        match rollback {
            Ok(()) => {
                self.pending_generation = None;
                self.rebuild_snapshot()?;
                Ok(())
            }
            Err(error) => {
                let Some(pending) = self.pending_generation.as_mut() else {
                    unreachable!("failed rollback retains its pending generation Provider")
                };
                pending.transition = None;
                pending.failure = Some(ReconcileFailure::error(
                    self.scopes.application,
                    provider,
                    self.generation_revision,
                    ReconcileStage::RollingBack,
                    error.to_string(),
                ));
                self.rebuild_snapshot()?;
                Err(CompositionReplaceError::Rollback(error))
            }
        }
    }

    async fn rollback_failed_generation_activation(
        &mut self,
        activation: &ProviderActivationError,
    ) -> Result<(), CompositionReplaceError> {
        let provider = activation.provider();
        let Some(pending) = self.pending_generation.as_mut() else {
            unreachable!("failed activation retains its prepared Provider")
        };
        pending.state = PendingProviderState::RollingBack;
        pending.transition = Some(CapabilityComponentTransition {
            stage: ReconcileStage::RollingBack,
            started_at: std::time::Instant::now(),
        });
        pending.failure = None;
        self.generation_transition = None;
        self.generation_failure = None;
        self.rebuild_snapshot()?;
        let rollback = {
            let Some(pending) = self.pending_generation.as_mut() else {
                unreachable!("failed activation retains its prepared Provider")
            };
            rollback_prepared_provider::<GenerationCapability>(provider, &mut pending.prepared)
                .await
        };
        match rollback {
            Ok(()) => {
                self.pending_generation = None;
                self.generation_transition = None;
                self.generation_failure = Some(ReconcileFailure::error(
                    self.scopes.application,
                    provider,
                    self.generation_revision,
                    ReconcileStage::Activating,
                    activation.to_string(),
                ));
                self.rebuild_snapshot()?;
                Ok(())
            }
            Err(error) => {
                let Some(pending) = self.pending_generation.as_mut() else {
                    unreachable!("failed rollback retains its pending generation Provider")
                };
                pending.transition = None;
                pending.failure = Some(ReconcileFailure::error(
                    self.scopes.application,
                    provider,
                    self.generation_revision,
                    ReconcileStage::RollingBack,
                    error.to_string(),
                ));
                self.rebuild_snapshot()?;
                Err(CompositionReplaceError::Rollback(error))
            }
        }
    }

    #[must_use]
    pub(crate) fn markdown_extensions(&self) -> Option<&MarkdownExtensionSnapshot> {
        self.markdown_extensions.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn markdown_registry_snapshot(
        &self,
    ) -> Result<MarkdownExtensionSnapshot, ContributionRegistryError> {
        self.markdown_registry
            .borrow()
            .snapshot(self.application_scope())
            .map(|snapshot| MarkdownExtensionSnapshot::from(&snapshot))
    }

    #[cfg(test)]
    pub(crate) async fn deactivate_markdown_contribution_for_test(
        &mut self,
        contribution: ContributionId,
    ) -> Result<bool, CompositionBuildError> {
        let component_id = ComponentId::new(contribution.as_str());
        let Some(component) = self
            .markdown_components
            .iter_mut()
            .find(|component| component.id == component_id && component.active)
        else {
            return Ok(false);
        };
        component.dispose().await.map_err(|source| {
            CompositionBuildError::Scope(ScopeError::Dispose {
                scope: self.scopes.application,
                source,
            })
        })?;
        self.refresh_markdown_extensions()
            .map_err(CompositionBuildError::Contributions)?;
        self.rebuild_snapshot()
            .map_err(CompositionBuildError::Snapshot)?;
        Ok(true)
    }

    /// Project the active application capabilities into the bundle consumed by
    /// the foreground shell. A partially closed root cannot produce services.
    #[must_use]
    pub fn services(&self) -> Option<RuntimeServices> {
        Some(RuntimeServices::new(
            self.session_services()?.handle().clone(),
            self.preferences()?.handle().clone(),
            self.generation_mount.service(),
            self.markdown_extensions()?.clone(),
            self.runtime_snapshots.reader(),
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
        if self.closed {
            return Ok(());
        }
        let application = self.scopes.application;
        let snapshot = self
            .preferences
            .as_ref()
            .map(|provider| provider.retained_lease().handle().snapshot())
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
            self.session_failure = Some(ReconcileFailure::error(
                application,
                self.provider,
                DesiredRevision::INITIAL,
                ReconcileStage::Stopping,
                source.to_string(),
            ));
            self.rebuild_snapshot()
                .map_err(|snapshot_error| ScopeError::Dispose {
                    scope: application,
                    source: anyhow::Error::new(snapshot_error),
                })?;
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
        if self.closed {
            return Ok(());
        }
        self.close_scopes().await
    }

    async fn close_scopes(&mut self) -> Result<(), ScopeError> {
        let application = self.scopes.application;
        if self.closed {
            return Ok(());
        }
        if self.pending_generation.is_some() {
            self.rollback_pending_generation()
                .await
                .map_err(|source| ScopeError::Dispose {
                    scope: application,
                    source: anyhow::Error::new(source),
                })?;
        }
        if self.generation_binding.is_some() && self.generation.is_some() {
            if let Some(revision) = self.generation_revision.next() {
                self.generation_revision = revision;
            } else {
                crate::logging::error(
                    "runtime.lifecycle",
                    format_args!(
                        "event=provider_close_revision_exhausted capability={} component={} scope={} error_kind=revision_exhausted",
                        CapabilityId::of::<GenerationCapability>().name(),
                        self.generation_provider,
                        application.raw(),
                    ),
                );
            }
            self.generation_failure = None;
            self.generation_transition = Some(CapabilityComponentTransition {
                stage: ReconcileStage::Stopping,
                started_at: std::time::Instant::now(),
            });
            self.rebuild_snapshot()
                .map_err(|source| ScopeError::Dispose {
                    scope: application,
                    source: anyhow::Error::new(source),
                })?;
            let Some(binding) = self.generation_binding.as_ref() else {
                unreachable!("generation binding was checked before close")
            };
            let Some(lease) = self.generation.as_ref() else {
                unreachable!("generation lease was checked before close")
            };
            log_provider_lifecycle(
                "provider_quiescing",
                CapabilityId::of::<GenerationCapability>(),
                lease.retained_lease(),
                Duration::ZERO,
            );
            binding.quiesce().await;
            self.generation_mount.unmount();
            let dispose_result = {
                let Some(active) = self.generation.as_mut() else {
                    unreachable!("generation Provider remains owned during close")
                };
                dispose_active_provider(self.generation_provider, active).await
            };
            if let Err(error) = dispose_result {
                self.generation_transition = None;
                self.generation_failure = Some(ReconcileFailure::error(
                    application,
                    self.generation_provider,
                    self.generation_revision,
                    ReconcileStage::Stopping,
                    error.to_string(),
                ));
                self.rebuild_snapshot()
                    .map_err(|source| ScopeError::Dispose {
                        scope: application,
                        source: anyhow::Error::new(source),
                    })?;
                return Err(ScopeError::Dispose {
                    scope: application,
                    source: anyhow::Error::new(error),
                });
            }
            self.generation = None;
            self.generation_binding = None;
            self.generation_transition = None;
            self.generation_failure = None;
        }
        for component in self.markdown_components.iter_mut().rev() {
            if let Err(source) = component.dispose().await {
                component.failure = Some(ReconcileFailure::error(
                    application,
                    component.id,
                    DesiredRevision::INITIAL,
                    ReconcileStage::Stopping,
                    source.to_string(),
                ));
                self.refresh_markdown_extensions()
                    .map_err(|source| ScopeError::Dispose {
                        scope: application,
                        source: anyhow::Error::new(source),
                    })?;
                self.rebuild_snapshot()
                    .map_err(|source| ScopeError::Dispose {
                        scope: application,
                        source: anyhow::Error::new(source),
                    })?;
                return Err(ScopeError::Dispose {
                    scope: application,
                    source,
                });
            }
        }
        self.refresh_markdown_extensions()
            .map_err(|source| ScopeError::Dispose {
                scope: application,
                source: anyhow::Error::new(source),
            })?;
        self.rebuild_snapshot()
            .map_err(|source| ScopeError::Dispose {
                scope: application,
                source: anyhow::Error::new(source),
            })?;
        if let Some(preferences) = self.preferences.as_mut() {
            if let Err(error) = dispose_active_provider(self.preference_provider, preferences).await
            {
                self.preference_failure = Some(ReconcileFailure::error(
                    application,
                    self.preference_provider,
                    DesiredRevision::INITIAL,
                    ReconcileStage::Stopping,
                    error.to_string(),
                ));
                self.rebuild_snapshot()
                    .map_err(|source| ScopeError::Dispose {
                        scope: application,
                        source: anyhow::Error::new(source),
                    })?;
                return Err(ScopeError::Dispose {
                    scope: application,
                    source: anyhow::Error::new(error),
                });
            }
            self.preference_failure = None;
        }
        if let Some(session_services) = self.session_services.as_mut() {
            if let Err(error) = dispose_active_provider(self.provider, session_services).await {
                self.session_failure = Some(ReconcileFailure::error(
                    application,
                    self.provider,
                    DesiredRevision::INITIAL,
                    ReconcileStage::Stopping,
                    error.to_string(),
                ));
                self.rebuild_snapshot()
                    .map_err(|source| ScopeError::Dispose {
                        scope: application,
                        source: anyhow::Error::new(source),
                    })?;
                return Err(ScopeError::Dispose {
                    scope: application,
                    source: anyhow::Error::new(error),
                });
            }
            self.session_failure = None;
        }
        self.scopes.close_application().await?;
        self.session_services = None;
        self.session_failure = None;
        self.preferences = None;
        self.preference_failure = None;
        self.generation = None;
        self.generation_binding = None;
        self.pending_generation = None;
        self.generation_mount.unmount();
        self.generation_transition = None;
        self.generation_failure = None;
        self.markdown_extensions = None;
        self.closed = true;
        self.rebuild_snapshot()
            .map_err(|source| ScopeError::Dispose {
                scope: application,
                source: anyhow::Error::new(source),
            })?;
        Ok(())
    }

    fn refresh_markdown_extensions(&mut self) -> Result<(), ContributionRegistryError> {
        let snapshot = self
            .markdown_registry
            .borrow()
            .snapshot(self.scopes.application)?;
        self.markdown_extensions = Some(MarkdownExtensionSnapshot::from(&snapshot));
        Ok(())
    }

    fn rebuild_snapshot(&mut self) -> Result<(), RuntimeSnapshotError> {
        let pending_is_primary = self.pending_generation.as_ref().is_some_and(|pending| {
            pending.provider == self.generation_provider && self.generation.is_none()
        });
        let pending_resources = |pending: &PendingGenerationProvider| {
            RuntimeResourceCounts::new(
                pending.prepared.effect_count(),
                0,
                0,
                usize::from(pending.state == PendingProviderState::RollingBack),
            )
        };
        let generation_resources = if let Some(generation) = &self.generation {
            generation.resource_counts(
                self.generation_transition.is_some() || self.generation_failure.is_some(),
            )
        } else if pending_is_primary {
            self.pending_generation
                .as_ref()
                .map_or_else(RuntimeResourceCounts::default, pending_resources)
        } else {
            RuntimeResourceCounts::default()
        };
        let primary_pending = self
            .pending_generation
            .as_ref()
            .filter(|_| pending_is_primary);
        let generation_transition = self
            .generation_transition
            .or_else(|| primary_pending.and_then(PendingGenerationProvider::diagnostic_transition));
        let generation_failure = self
            .generation_failure
            .clone()
            .or_else(|| primary_pending.and_then(|pending| pending.failure.clone()));
        let mut capability_components = vec![
            CapabilityComponentState {
                id: self.provider,
                active: self
                    .session_services
                    .as_ref()
                    .and_then(ActiveProvider::lease)
                    .is_some(),
                revision: DesiredRevision::INITIAL,
                transition: None,
                failure: self.session_failure.clone(),
                resources: self
                    .session_services
                    .as_ref()
                    .map_or_else(RuntimeResourceCounts::default, |provider| {
                        provider.resource_counts(self.session_failure.is_some())
                    }),
            },
            CapabilityComponentState {
                id: self.preference_provider,
                active: self
                    .preferences
                    .as_ref()
                    .and_then(ActiveProvider::lease)
                    .is_some(),
                revision: DesiredRevision::INITIAL,
                transition: None,
                failure: self.preference_failure.clone(),
                resources: self
                    .preferences
                    .as_ref()
                    .map_or_else(RuntimeResourceCounts::default, |provider| {
                        provider.resource_counts(self.preference_failure.is_some())
                    }),
            },
            CapabilityComponentState {
                id: self.generation_provider,
                active: self
                    .generation
                    .as_ref()
                    .and_then(ActiveProvider::lease)
                    .is_some(),
                revision: self.generation_revision,
                transition: generation_transition,
                failure: generation_failure,
                resources: generation_resources,
            },
        ];
        if let Some(pending) = self.pending_generation.as_ref()
            && pending.provider != self.generation_provider
        {
            capability_components.push(CapabilityComponentState {
                id: pending.provider,
                active: false,
                revision: self.generation_revision,
                transition: pending.diagnostic_transition(),
                failure: pending.failure.clone(),
                resources: pending_resources(pending),
            });
        }
        self.snapshot = composition_snapshot(
            self.scopes.application,
            capability_components,
            &self.markdown_components,
            self.markdown_registry.borrow().revision(),
        )?;
        log_runtime_snapshot(&self.snapshot);
        if !self.runtime_snapshots.publish(self.snapshot.clone()) {
            crate::logging::error(
                "runtime.lifecycle",
                "event=snapshot_publication_failed reason=revision_exhausted",
            );
        }
        Ok(())
    }
}
