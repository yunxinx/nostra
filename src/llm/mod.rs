//! Stable, UI-independent surface of Nostra's model generation gateway.
//!
//! Views depend on the canonical types re-exported here. Provider-specific wire
//! formats remain private to `protocol`, and HTTP/SSE mechanics remain in
//! `transport`.

mod config;
mod error;
mod event;
mod gateway;
mod metrics;
mod model;
mod protocol;
mod service;
mod transport;

pub use config::{
    ModelConfig, ModelId, ModelSelection, ProfileId, ProviderCatalogSnapshot, ProviderProfile,
    SecretString, resolve_selection,
};
pub use error::{ErrorKind, GatewayError};
pub use event::{
    FinishReason, GenerationEvent, GenerationOutcome, IndexedContentBlock, IndexedMessage,
    OutcomeStatus, StreamMetadata,
};
pub use gateway::{Gateway, Generation, RequestContext};
pub use metrics::{AggregateKey, AggregateUsage, InMemoryMetrics, OutcomeObserver};
pub use model::*;
pub use protocol::{
    CompatibilityProfile, MaxTokensField, Protocol, ReasoningField, ResponsesInstructionsPolicy,
    SystemRolePolicy,
};
pub use service::{
    GatewayGenerationService, GenerationHandle, GenerationRequest, GenerationRunner,
    GenerationService,
};
pub use transport::HttpTransport;

pub(crate) use protocol::ProtocolSession;
