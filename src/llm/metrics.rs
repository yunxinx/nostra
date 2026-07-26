//! Bounded in-memory generation outcomes and per-route usage aggregation.
//!
//! Stored records intentionally omit canonical messages; metrics retain only
//! routing, timing, status, safe errors, and normalized token counts.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use crate::llm::{
    GenerationEvent, GenerationOutcome, OutcomeStatus, Protocol, RequestContext, Usage,
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
            // Outcomes may carry the assembled assistant message for the caller,
            // but observability storage must never retain conversation content.
            let mut record = outcome.clone();
            record.message = None;
            state.recent.push_back(record);
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
        outcome.message = Some(crate::llm::Message {
            role: crate::llm::Role::Assistant,
            content: vec![crate::llm::ContentBlock::Text {
                text: "private text".into(),
                provider_metadata: Default::default(),
            }],
            provider_metadata: Default::default(),
        });
        metrics.on_finish(&outcome);
        assert!(metrics.recent()[0].message.is_none());
    }
}
