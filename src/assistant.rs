//! GPUI boundary for streaming model generations.
//!
//! Gateway events are coalesced for one frame before updating entities. This
//! keeps protocol and transport work out of views while preserving canonical
//! event order and task-drop cancellation.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::Duration,
};

use futures::future::{AbortHandle, Abortable};
use gpui::{Context, Task};

use crate::{
    chat::ChatView,
    llm::{
        Gateway, GatewayError, GenerateRequest, GenerationEvent, HttpTransport, InMemoryMetrics,
        Message as LlmMessage, ModelSelection, OutcomeStatus,
    },
    providers,
};

fn metrics() -> Arc<InMemoryMetrics> {
    static METRICS: OnceLock<Arc<InMemoryMetrics>> = OnceLock::new();
    METRICS
        .get_or_init(|| Arc::new(InMemoryMetrics::new(256)))
        .clone()
}

pub struct ReplyTask {
    _task: Task<()>,
    abort: AbortHandle,
}

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
const MAX_PENDING_DELTAS: usize = 32;

#[derive(Debug, PartialEq)]
enum StreamDelta {
    TextStarted {
        content_index: usize,
        id: String,
    },
    TextDelta {
        content_index: usize,
        id: String,
        delta: String,
    },
    TextFinished {
        content_index: usize,
        id: String,
        replay: Option<crate::llm::ReplayMetadata>,
    },
    ReasoningStarted {
        content_index: usize,
        id: String,
    },
    ReasoningDelta {
        content_index: usize,
        id: String,
        delta: String,
    },
    ReasoningFinished {
        content_index: usize,
        id: String,
        replay: Option<crate::llm::ReplayMetadata>,
    },
    ReasoningSnapshotUpdated {
        content_index: usize,
        id: String,
        reasoning: crate::llm::ReasoningContent,
    },
    ToolCallStarted {
        content_index: usize,
        index: usize,
        id: String,
        name: String,
    },
    ToolCallFinished {
        content_index: usize,
        index: usize,
        tool_call: Box<crate::llm::ToolCall>,
    },
}

#[derive(Default)]
struct PendingDeltas {
    deltas: Vec<StreamDelta>,
    event_count: usize,
    flush_scheduled: bool,
}

impl PendingDeltas {
    fn push(&mut self, delta: StreamDelta) -> FlushAction {
        self.event_count = self.event_count.saturating_add(1);
        match (self.deltas.last_mut(), delta) {
            (
                Some(StreamDelta::TextDelta {
                    content_index: current_index,
                    id: current_id,
                    delta: current,
                }),
                StreamDelta::TextDelta {
                    content_index,
                    id,
                    delta: next,
                },
            ) if *current_index == content_index && *current_id == id => {
                current.push_str(&next);
            }
            (
                Some(StreamDelta::ReasoningDelta {
                    content_index: current_index,
                    id: current_id,
                    delta: current,
                }),
                StreamDelta::ReasoningDelta {
                    content_index,
                    id,
                    delta: next,
                },
            ) if *current_index == content_index && *current_id == id => {
                current.push_str(&next);
            }
            (_, delta) => self.deltas.push(delta),
        }
        if self.event_count >= MAX_PENDING_DELTAS {
            self.flush_scheduled = false;
            FlushAction::Immediate
        } else if self.flush_scheduled {
            FlushAction::Pending
        } else {
            self.flush_scheduled = true;
            FlushAction::Schedule
        }
    }

    fn take(&mut self) -> Vec<StreamDelta> {
        self.flush_scheduled = false;
        self.event_count = 0;
        std::mem::take(&mut self.deltas)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FlushAction {
    Schedule,
    Pending,
    Immediate,
}

impl ReplyTask {
    pub fn cancel(&self) {
        self.abort.abort();
    }
}

pub fn stream_reply(
    history: Vec<LlmMessage>,
    selection: Option<ModelSelection>,
    conversation_id: String,
    turn_id: String,
    cx: &mut Context<ChatView>,
) -> ReplyTask {
    let profiles = providers::snapshot(cx);
    let gateway = Gateway::new(HttpTransport::new(cx.http_client()), Some(metrics()));
    let prepared = selection
        .as_ref()
        .ok_or_else(|| GatewayError::configuration("model selection is unavailable"))
        .and_then(|selection| {
            gateway.prepare(
                &profiles,
                selection,
                GenerateRequest {
                    messages: history,
                    conversation_id,
                    turn_id,
                    ..GenerateRequest::default()
                },
            )
        });
    let (abort, registration) = AbortHandle::new_pair();
    let task = cx.spawn(async move |view, cx| {
        let pending = Rc::new(RefCell::new(PendingDeltas::default()));
        let mut flush_task: Option<Task<()>> = None;
        let mut generation = match prepared {
            Ok(generation) => generation,
            Err(error) => {
                // Same card as an upstream failure. There is no response body to
                // show — the request never left — so it renders headline-only.
                view.update(cx, |chat, cx| chat.finish_reply(None, Some(error), cx))
                    .ok();
                return;
            }
        };

        let run =
            generation.run(|event| !process_event(event, &pending, &mut flush_task, &view, cx));
        if Abortable::new(run, registration).await.is_err()
            && let Some(event) = generation.cancel()
        {
            process_event(event, &pending, &mut flush_task, &view, cx);
        }
    });
    ReplyTask { _task: task, abort }
}

fn process_event(
    event: GenerationEvent,
    pending: &Rc<RefCell<PendingDeltas>>,
    flush_task: &mut Option<Task<()>>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    match event {
        GenerationEvent::Finished(outcome) => {
            let outcome = *outcome;
            let deltas = drain_pending(pending);
            // A failure becomes a card rather than trailing markdown, so the
            // upstream body keeps its own frame, highlighting, and copy button
            // instead of being flattened into the assistant's prose.
            let failure = terminal_failure(outcome.status, outcome.error, outcome.request_id);
            let message = outcome.message;
            view.update(cx, |chat, cx| {
                apply_deltas(chat, deltas, cx);
                chat.follow_stream();
                chat.finish_reply(message, failure, cx);
            })
            .ok();
            true
        }
        event => project_stream_delta(event)
            .is_some_and(|delta| queue_delta(delta, pending, flush_task, view, cx)),
    }
}

/// Project canonical content events into the ordered UI delta stream.
///
/// Both protocol adapters emit explicit block starts and finishes. Keeping
/// those boundaries in the projection lets the view render and close each card
/// without inferring lifecycle state from an adjacent block's content.
fn project_stream_delta(event: GenerationEvent) -> Option<StreamDelta> {
    match event {
        GenerationEvent::TextStarted { content_index, id } => {
            Some(StreamDelta::TextStarted { content_index, id })
        }
        GenerationEvent::TextDelta {
            content_index,
            id,
            delta,
        } => Some(StreamDelta::TextDelta {
            content_index,
            id,
            delta,
        }),
        GenerationEvent::TextFinished {
            content_index,
            id,
            replay,
        } => Some(StreamDelta::TextFinished {
            content_index,
            id,
            replay,
        }),
        GenerationEvent::ReasoningStarted { content_index, id } => {
            Some(StreamDelta::ReasoningStarted { content_index, id })
        }
        GenerationEvent::ReasoningDelta {
            content_index,
            id,
            delta,
        } => Some(StreamDelta::ReasoningDelta {
            content_index,
            id,
            delta,
        }),
        GenerationEvent::ReasoningFinished {
            content_index,
            id,
            replay,
        } => Some(StreamDelta::ReasoningFinished {
            content_index,
            id,
            replay,
        }),
        GenerationEvent::ReasoningSnapshotUpdated {
            content_index,
            id,
            reasoning,
        } => Some(StreamDelta::ReasoningSnapshotUpdated {
            content_index,
            id,
            reasoning,
        }),
        GenerationEvent::ToolCallStarted {
            content_index,
            index,
            id,
            name,
        } => Some(StreamDelta::ToolCallStarted {
            content_index,
            index,
            id,
            name,
        }),
        GenerationEvent::ToolCallFinished {
            content_index,
            index,
            tool_call,
        } => Some(StreamDelta::ToolCallFinished {
            content_index,
            index,
            tool_call,
        }),
        _ => None,
    }
}

fn queue_delta(
    delta: StreamDelta,
    pending: &Rc<RefCell<PendingDeltas>>,
    flush_task: &mut Option<Task<()>>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let action = pending.borrow_mut().push(delta);
    match action {
        FlushAction::Pending => false,
        FlushAction::Immediate => flush_pending(pending, view, cx),
        FlushAction::Schedule => {
            let pending = Rc::clone(pending);
            let view = view.clone();
            *flush_task = Some(cx.spawn(async move |cx| {
                cx.background_executor().timer(STREAM_FLUSH_INTERVAL).await;
                flush_pending(&pending, &view, cx);
            }));
            false
        }
    }
}

fn flush_pending(
    pending: &Rc<RefCell<PendingDeltas>>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let deltas = drain_pending(pending);
    if deltas.is_empty() {
        return false;
    }
    view.update(cx, |chat, cx| {
        apply_deltas(chat, deltas, cx);
        chat.finish_stream_batch(cx);
    })
    .is_err()
}

fn drain_pending(pending: &Rc<RefCell<PendingDeltas>>) -> Vec<StreamDelta> {
    pending.borrow_mut().take()
}

/// Replay one coalesced batch into the view, in canonical order.
///
/// Each delta is routed by block id to its own `MessagePart`; lifecycle
/// boundaries remain explicit markers, mirroring pi's
/// `thinking_start`/`thinking_delta`/`thinking_end` stream model.
fn apply_deltas(chat: &mut ChatView, deltas: Vec<StreamDelta>, cx: &mut Context<ChatView>) {
    for delta in deltas {
        match delta {
            StreamDelta::TextStarted { content_index, id } => {
                chat.start_stream_text(content_index, id, cx);
            }
            StreamDelta::TextDelta {
                content_index,
                id,
                delta,
            } => {
                chat.append_stream_text(content_index, id, &delta, cx);
            }
            StreamDelta::TextFinished {
                content_index,
                id,
                replay,
            } => {
                chat.finish_stream_text(content_index, &id, replay);
            }
            StreamDelta::ReasoningStarted { content_index, id } => {
                chat.start_stream_reasoning(content_index, id);
            }
            StreamDelta::ReasoningDelta {
                content_index,
                id,
                delta,
            } => {
                chat.append_stream_reasoning(content_index, id, &delta, cx);
            }
            StreamDelta::ReasoningFinished {
                content_index,
                id,
                replay,
            } => {
                chat.finish_stream_reasoning(content_index, &id, replay);
            }
            StreamDelta::ReasoningSnapshotUpdated {
                content_index,
                id,
                reasoning,
            } => {
                chat.update_stream_reasoning_snapshot(content_index, &id, reasoning, cx);
            }
            StreamDelta::ToolCallStarted {
                content_index,
                index,
                id,
                name,
            } => {
                chat.start_stream_tool_call(content_index, index, id, name);
            }
            StreamDelta::ToolCallFinished {
                content_index,
                index,
                tool_call,
            } => {
                chat.finish_stream_tool_call(content_index, index, *tool_call);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn apply_generation_events_for_test(
    chat: &mut ChatView,
    events: Vec<GenerationEvent>,
    cx: &mut Context<ChatView>,
) {
    let deltas = events
        .into_iter()
        .filter_map(project_stream_delta)
        .collect();
    apply_deltas(chat, deltas, cx);
}

/// Stand-in for a `Failed` outcome that arrived without an error attached. The
/// gateway always populates one, so this only guards against a future adapter
/// reporting failure without a reason.
fn terminal_failure(
    status: OutcomeStatus,
    error: Option<GatewayError>,
    request_id: String,
) -> Option<GatewayError> {
    (status == OutcomeStatus::Failed).then(|| {
        let mut error = error.unwrap_or_else(|| {
            // Defensive fallback for a future adapter that violates the gateway
            // invariant by reporting Failed without an attached error.
            GatewayError::provider("provider request failed", None)
        });
        // The outcome is the authoritative correlation boundary. Preserve an
        // adapter-provided id, but never let a failed UI card lose the outcome id.
        error.request_id.get_or_insert(request_id);
        error
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_deltas_coalesce_adjacent_kinds_and_preserve_order() {
        let mut pending = PendingDeltas::default();
        assert_eq!(
            pending.push(StreamDelta::TextDelta {
                content_index: 0,
                id: "text-0".into(),
                delta: "a".into(),
            }),
            FlushAction::Schedule
        );
        assert_eq!(
            pending.push(StreamDelta::TextDelta {
                content_index: 0,
                id: "text-0".into(),
                delta: "b".into(),
            }),
            FlushAction::Pending
        );
        pending.push(StreamDelta::ReasoningDelta {
            content_index: 1,
            id: "reasoning-0".into(),
            delta: "c".into(),
        });
        pending.push(StreamDelta::ReasoningFinished {
            content_index: 1,
            id: "reasoning-0".into(),
            replay: None,
        });
        pending.push(StreamDelta::TextDelta {
            content_index: 2,
            id: "text-1".into(),
            delta: "d".into(),
        });

        assert_eq!(
            pending.take(),
            vec![
                StreamDelta::TextDelta {
                    content_index: 0,
                    id: "text-0".into(),
                    delta: "ab".into(),
                },
                StreamDelta::ReasoningDelta {
                    content_index: 1,
                    id: "reasoning-0".into(),
                    delta: "c".into(),
                },
                StreamDelta::ReasoningFinished {
                    content_index: 1,
                    id: "reasoning-0".into(),
                    replay: None,
                },
                StreamDelta::TextDelta {
                    content_index: 2,
                    id: "text-1".into(),
                    delta: "d".into(),
                },
            ]
        );
    }

    #[test]
    fn pending_deltas_schedule_each_non_empty_batch_once() {
        let mut pending = PendingDeltas::default();
        assert_eq!(
            pending.push(StreamDelta::TextDelta {
                content_index: 0,
                id: "text-0".into(),
                delta: "first".into(),
            }),
            FlushAction::Schedule
        );
        assert_eq!(
            pending.push(StreamDelta::TextDelta {
                content_index: 0,
                id: "text-0".into(),
                delta: "second".into(),
            }),
            FlushAction::Pending
        );
        pending.take();
        assert_eq!(
            pending.push(StreamDelta::TextDelta {
                content_index: 0,
                id: "text-0".into(),
                delta: "third".into(),
            }),
            FlushAction::Schedule
        );
    }

    #[test]
    fn failed_terminal_always_carries_the_outcome_request_id() {
        let fallback = terminal_failure(OutcomeStatus::Failed, None, "request-1".into())
            .expect("failed outcome");
        assert_eq!(fallback.request_id.as_deref(), Some("request-1"));

        let mut adapter_error = GatewayError::provider("failed", None);
        adapter_error.request_id = Some("adapter-id".into());
        let preserved = terminal_failure(
            OutcomeStatus::Failed,
            Some(adapter_error),
            "outcome-id".into(),
        )
        .expect("failed outcome");
        assert_eq!(preserved.request_id.as_deref(), Some("adapter-id"));

        assert!(terminal_failure(OutcomeStatus::Completed, None, "unused".into()).is_none());
    }
}
