//! Bounded in-memory generation outcomes and per-route usage aggregation.
//!
//! Stored records intentionally omit canonical messages; metrics retain only
//! routing, timing, status, safe errors, and normalized token counts.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use crate::llm::{
    GatewayError, GenerationEvent, GenerationOutcome, OutcomeStatus, Protocol, RequestContext,
    Usage,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AggregateKey {
    pub profile_id: String,
    pub model_id: String,
    pub protocol: Protocol,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggregateUsage {
    pub requests: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub usage: Usage,
}

/// Observes the live generation lifecycle.
///
/// A failed terminal outcome intentionally includes the captured upstream body:
/// the GPUI boundary needs that verbatim diagnostic for the error card. This is
/// not a redaction boundary. Observability implementations may inspect it during
/// the callback, but must not persist it; [`InMemoryMetrics`] enforces that rule
/// before writing to its ring buffer.
pub trait OutcomeObserver: Send + Sync {
    fn on_start(&self, _: &RequestContext) {}
    fn on_upstream_response(&self, _: &RequestContext, _: u16) {}
    fn on_event(&self, _: &RequestContext, _: &GenerationEvent) {}
    fn on_finish(&self, outcome: &GenerationOutcome);
}

struct MetricsState {
    recent: VecDeque<GenerationOutcome>,
    aggregates: HashMap<AggregateKey, AggregateUsage>,
}

pub struct InMemoryMetrics {
    capacity: usize,
    state: Mutex<MetricsState>,
}

impl InMemoryMetrics {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(MetricsState {
                recent: VecDeque::new(),
                aggregates: HashMap::new(),
            }),
        }
    }

    pub fn recent(&self) -> Vec<GenerationOutcome> {
        self.state
            .lock()
            .map(|state| state.recent.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn aggregate(&self, key: &AggregateKey) -> Option<AggregateUsage> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.aggregates.get(key).cloned())
    }
}

/// Build the storage projection field by field. In particular, do not clone the
/// canonical message or captured upstream response and then clear them: either
/// can be large, and the response may contain provider-controlled sensitive
/// text. The live outcome remains unchanged for verbatim UI display and copy.
fn metrics_record(outcome: &GenerationOutcome) -> GenerationOutcome {
    GenerationOutcome {
        request_id: outcome.request_id.clone(),
        profile_id: outcome.profile_id.clone(),
        model_id: outcome.model_id.clone(),
        protocol: outcome.protocol,
        status: outcome.status,
        finish_reason: outcome.finish_reason.clone(),
        usage: outcome.usage.clone(),
        response_id: outcome.response_id.clone(),
        upstream_model: outcome.upstream_model.clone(),
        time_to_first_event: outcome.time_to_first_event,
        latency: outcome.latency,
        message: None,
        error: outcome.error.as_ref().map(metrics_error),
    }
}

/// Copy only the allowlisted error tier. `upstream_body` is intentionally shown
/// exactly as received in the failed turn; metrics is a storage boundary, not a
/// reason to redact or rewrite what the user sees.
fn metrics_error(error: &GatewayError) -> GatewayError {
    error.storage_safe_clone()
}

impl OutcomeObserver for InMemoryMetrics {
    fn on_finish(&self, outcome: &GenerationOutcome) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let key = AggregateKey {
            profile_id: outcome.profile_id.clone(),
            model_id: outcome.model_id.clone(),
            protocol: outcome.protocol,
        };
        let aggregate = state.aggregates.entry(key).or_default();
        aggregate.requests = aggregate.requests.saturating_add(1);
        match outcome.status {
            OutcomeStatus::Completed => aggregate.completed = aggregate.completed.saturating_add(1),
            OutcomeStatus::Failed => aggregate.failed = aggregate.failed.saturating_add(1),
            OutcomeStatus::Cancelled => aggregate.cancelled = aggregate.cancelled.saturating_add(1),
        }
        aggregate.usage.add_assign(&outcome.usage);
        if self.capacity > 0 {
            state.recent.push_back(metrics_record(outcome));
            while state.recent.len() > self.capacity {
                state.recent.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bounds_recent_and_aggregates_outcomes() {
        let metrics = InMemoryMetrics::new(1);
        let outcome = GenerationOutcome {
            request_id: "r".into(),
            profile_id: "p".into(),
            model_id: "m".into(),
            protocol: Protocol::Responses,
            status: OutcomeStatus::Completed,
            finish_reason: None,
            usage: Usage {
                total_tokens: 4,
                ..Default::default()
            },
            response_id: None,
            upstream_model: None,
            time_to_first_event: None,
            latency: Duration::ZERO,
            message: None,
            error: None,
        };
        metrics.on_finish(&outcome);
        metrics.on_finish(&outcome);
        assert_eq!(metrics.recent().len(), 1);
        let aggregate = metrics
            .aggregate(&AggregateKey {
                profile_id: "p".into(),
                model_id: "m".into(),
                protocol: Protocol::Responses,
            })
            .expect("aggregate");
        assert_eq!(aggregate.requests, 2);
        assert_eq!(aggregate.usage.total_tokens, 8);
        assert_eq!(aggregate.usage.provenance, outcome.usage.provenance);
    }

    #[test]
    fn recent_outcomes_never_retain_message_content() {
        let metrics = InMemoryMetrics::new(1);
        let mut outcome = GenerationOutcome {
            request_id: "r".into(),
            profile_id: "p".into(),
            model_id: "m".into(),
            protocol: Protocol::Responses,
            status: OutcomeStatus::Completed,
            finish_reason: None,
            usage: Usage::default(),
            response_id: None,
            upstream_model: None,
            time_to_first_event: None,
            latency: Duration::ZERO,
            message: None,
            error: None,
        };
        outcome.message = Some(crate::llm::IndexedMessage::from_message(
            crate::llm::Message {
                role: crate::llm::Role::Assistant,
                content: vec![crate::llm::ContentBlock::Text {
                    text: "private text".into(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
        ));
        metrics.on_finish(&outcome);
        assert!(metrics.recent()[0].message.is_none());
    }

    /// Captured upstream error text is meant for the view that renders it, not
    /// for a ring buffer that outlives the turn. The safe tier must survive so
    /// failures stay diagnosable in metrics.
    #[test]
    fn recent_outcomes_never_retain_captured_upstream_bodies() {
        let metrics = InMemoryMetrics::new(1);
        let outcome = GenerationOutcome {
            request_id: "r".into(),
            profile_id: "p".into(),
            model_id: "m".into(),
            protocol: Protocol::Responses,
            status: OutcomeStatus::Failed,
            finish_reason: None,
            usage: Usage::default(),
            response_id: None,
            upstream_model: None,
            time_to_first_event: None,
            latency: Duration::ZERO,
            message: None,
            error: Some(
                crate::llm::GatewayError::provider(
                    "provider rejected the request",
                    Some("bad_request".into()),
                )
                .with_upstream_body(r#"{"error":{"message":"echoed prompt"}}"#),
            ),
        };
        // The caller's own copy keeps the body.
        assert!(
            outcome
                .error
                .as_ref()
                .expect("error")
                .upstream_body()
                .is_some()
        );

        metrics.on_finish(&outcome);
        let recorded = metrics.recent()[0].error.clone().expect("recorded error");
        assert_eq!(recorded.upstream_body(), None);
        assert_eq!(recorded.safe_message(), "provider rejected the request");
        assert_eq!(recorded.provider_code.as_deref(), Some("bad_request"));
    }
}
