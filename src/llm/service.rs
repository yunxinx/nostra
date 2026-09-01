//! Stable generation service boundary used by runtime Consumers.
//!
//! Provider catalog resolution stays in the default adapter, which reads a live
//! [`ProviderCatalogSource`] once per request so configuration edits apply to
//! the next generation without replacing the Provider. Consumers receive a
//! prepared handle and only observe canonical events and terminal outcomes.

use std::{future::Future, pin::Pin, sync::Arc};

use super::{
    Gateway, GatewayError, GenerateRequest, Generation, GenerationEvent, HttpTransport,
    ModelSelection, OutcomeObserver, ProviderCatalogSnapshot, ProviderCatalogSource,
};

/// Request context accepted by a generation Provider.
///
/// Routing selection is kept beside the protocol-neutral request so Consumers
/// do not need to resolve provider profiles or construct transport requests.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerationRequest {
    pub selection: ModelSelection,
    pub request: GenerateRequest,
}

impl GenerationRequest {
    #[must_use]
    pub fn new(selection: ModelSelection, request: GenerateRequest) -> Self {
        Self { selection, request }
    }
}

/// Application-scoped model generation capability.
pub trait GenerationService: Send + Sync {
    fn start(&self, request: GenerationRequest) -> Result<GenerationHandle, GatewayError>;
}

/// A running generation that exposes only canonical lifecycle operations.
pub struct GenerationHandle {
    runner: Box<dyn GenerationRunner>,
}

impl GenerationHandle {
    pub async fn run(&mut self, mut on_event: impl FnMut(GenerationEvent) -> bool) {
        self.runner.run(&mut on_event).await;
    }

    pub fn cancel(&mut self) -> Option<GenerationEvent> {
        self.runner.cancel()
    }

    /// Wrap a Provider-owned runner behind the stable generation handle.
    #[must_use]
    pub fn from_runner(runner: impl GenerationRunner + 'static) -> Self {
        Self {
            runner: Box::new(runner),
        }
    }
}

/// Provider-owned lifecycle implementation used to construct a handle.
pub trait GenerationRunner {
    fn run<'a>(
        &'a mut self,
        on_event: &'a mut dyn FnMut(GenerationEvent) -> bool,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn cancel(&mut self) -> Option<GenerationEvent>;
}

/// First-party generation Provider backed by the existing Gateway.
pub struct GatewayGenerationService {
    catalog: Arc<dyn ProviderCatalogSource>,
    gateway: Gateway,
}

impl GatewayGenerationService {
    #[must_use]
    pub fn new(
        catalog: Arc<dyn ProviderCatalogSource>,
        transport: HttpTransport,
        observer: Option<Arc<dyn OutcomeObserver>>,
    ) -> Self {
        Self::from_gateway(catalog, Gateway::new(transport, observer))
    }

    #[must_use]
    pub fn from_gateway(catalog: Arc<dyn ProviderCatalogSource>, gateway: Gateway) -> Self {
        Self { catalog, gateway }
    }

    /// Resolve the routing catalog currently published by the source.
    #[must_use]
    pub fn catalog(&self) -> ProviderCatalogSnapshot {
        self.catalog.catalog()
    }
}

impl GenerationService for GatewayGenerationService {
    fn start(&self, request: GenerationRequest) -> Result<GenerationHandle, GatewayError> {
        let catalog = self.catalog.catalog();
        let generation = self
            .gateway
            .prepare(&catalog, &request.selection, request.request)?;
        Ok(GenerationHandle::from_runner(GatewayGenerationRunner {
            generation,
        }))
    }
}

struct GatewayGenerationRunner {
    generation: Generation,
}

impl GenerationRunner for GatewayGenerationRunner {
    fn run<'a>(
        &'a mut self,
        on_event: &'a mut dyn FnMut(GenerationEvent) -> bool,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(self.generation.run(on_event))
    }

    fn cancel(&mut self) -> Option<GenerationEvent> {
        self.generation.cancel()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use reqwest_client::ReqwestClient;

    use super::*;
    use crate::llm::{
        CompatibilityProfile, InMemoryMetrics, ModelConfig, OutcomeStatus, Protocol,
        ProviderProfile, SecretString,
    };

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "provider".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "model".into(),
                model_id: "vendor/model".into(),
                display_name: None,
            }],
        }
    }

    #[test]
    fn gateway_service_resolves_the_catalog_before_creating_a_handle() {
        let metrics = Arc::new(InMemoryMetrics::new(8));
        let service = GatewayGenerationService::new(
            Arc::new(ProviderCatalogSnapshot::new(vec![profile()])),
            HttpTransport::new(Arc::new(ReqwestClient::new())),
            Some(metrics.clone()),
        );
        assert_eq!(service.catalog().profiles().len(), 1);

        let request = GenerationRequest::new(
            ModelSelection {
                profile_id: "provider".into(),
                model_id: "model".into(),
            },
            GenerateRequest {
                conversation_id: "conversation".into(),
                turn_id: "turn".into(),
                ..Default::default()
            },
        );
        let mut handle = service.start(request).expect("valid selection prepares");
        let terminal = handle.cancel().expect("cancelled handle emits terminal");
        assert!(
            matches!(terminal, GenerationEvent::Finished(outcome) if outcome.status == OutcomeStatus::Cancelled)
        );
        assert_eq!(metrics.recent().len(), 1);
        assert_eq!(metrics.recent()[0].status, OutcomeStatus::Cancelled);
    }

    #[test]
    fn gateway_service_rejects_selection_before_transport_is_used() {
        let service = GatewayGenerationService::new(
            Arc::new(ProviderCatalogSnapshot::new(vec![profile()])),
            HttpTransport::new(Arc::new(ReqwestClient::new())),
            None,
        );
        let request = GenerationRequest::new(
            ModelSelection {
                profile_id: "provider".into(),
                model_id: "missing".into(),
            },
            GenerateRequest::default(),
        );
        assert!(service.start(request).is_err());
    }

    #[derive(Default)]
    struct MutableCatalog {
        profiles: Mutex<Vec<ProviderProfile>>,
    }

    impl MutableCatalog {
        fn publish(&self, profiles: Vec<ProviderProfile>) {
            *self
                .profiles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = profiles;
        }
    }

    impl ProviderCatalogSource for MutableCatalog {
        fn catalog(&self) -> ProviderCatalogSnapshot {
            ProviderCatalogSnapshot::new(
                self.profiles
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            )
        }
    }

    /// A Provider configured after the service was built must become routable
    /// without replacing the generation Provider.
    #[test]
    fn a_selection_configured_after_construction_resolves_on_the_next_request() {
        let catalog = Arc::new(MutableCatalog::default());
        let service = GatewayGenerationService::new(
            Arc::clone(&catalog) as Arc<dyn ProviderCatalogSource>,
            HttpTransport::new(Arc::new(ReqwestClient::new())),
            None,
        );
        let request = || {
            GenerationRequest::new(
                ModelSelection {
                    profile_id: "provider".into(),
                    model_id: "model".into(),
                },
                GenerateRequest::default(),
            )
        };

        assert!(
            service.start(request()).is_err(),
            "an empty catalog cannot route a selection"
        );

        catalog.publish(vec![profile()]);

        let mut handle = service
            .start(request())
            .expect("a newly configured Provider routes without a Provider replacement");
        handle.cancel();
    }
}
