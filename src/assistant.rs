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
use gpui::{Context, Entity, Task};
use gpui_component::text::TextViewState;
use rust_i18n::t;

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
    Text(String),
    Reasoning(String),
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
            (Some(StreamDelta::Text(current)), StreamDelta::Text(next))
            | (Some(StreamDelta::Reasoning(current)), StreamDelta::Reasoning(next)) => {
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
    target: Entity<TextViewState>,
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

        let run = generation
            .run(|event| !process_event(event, &pending, &mut flush_task, &target, &view, cx));
        if Abortable::new(run, registration).await.is_err()
            && let Some(event) = generation.cancel()
        {
            process_event(event, &pending, &mut flush_task, &target, &view, cx);
        }
    });
    ReplyTask { _task: task, abort }
}

fn process_event(
    event: GenerationEvent,
    pending: &Rc<RefCell<PendingDeltas>>,
    flush_task: &mut Option<Task<()>>,
    target: &Entity<TextViewState>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    match event {
        GenerationEvent::TextDelta { delta, .. } => queue_delta(
            StreamDelta::Text(delta),
            pending,
            flush_task,
            target,
            view,
            cx,
        ),
        GenerationEvent::ReasoningDelta { delta, .. } => queue_delta(
            StreamDelta::Reasoning(delta),
            pending,
            flush_task,
            target,
            view,
            cx,
        ),
        GenerationEvent::ToolCallFinished { tool_call, .. } => {
            let deltas = drain_pending(pending);
            let mut display = visible_text(&deltas);
            display.push_str(&format!(
                "\n\n{}\n",
                t!("chat.tool_requested", name = tool_call.name.clone())
            ));
            target.update(cx, |state, cx| state.push_str(&display, cx));
            view.update(cx, |chat, cx| {
                apply_deltas(chat, deltas);
                chat.append_stream_tool_call(*tool_call);
                chat.finish_stream_batch(cx);
            })
            .is_err()
        }
        GenerationEvent::Finished(outcome) => {
            let outcome = *outcome;
            let deltas = drain_pending(pending);
            let display = visible_text(&deltas);
            if !display.is_empty() {
                target.update(cx, |state, cx| state.push_str(&display, cx));
            }
            // A failure becomes a card rather than trailing markdown, so the
            // upstream body keeps its own frame, highlighting, and copy button
            // instead of being flattened into the assistant's prose.
            let failure = terminal_failure(outcome.status, outcome.error, outcome.request_id);
            let message = outcome.message;
            view.update(cx, |chat, cx| {
                apply_deltas(chat, deltas);
                chat.follow_stream();
                chat.finish_reply(message, failure, cx);
            })
            .ok();
            true
        }
        _ => false,
    }
}

fn queue_delta(
    delta: StreamDelta,
    pending: &Rc<RefCell<PendingDeltas>>,
    flush_task: &mut Option<Task<()>>,
    target: &Entity<TextViewState>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let action = pending.borrow_mut().push(delta);
    match action {
        FlushAction::Pending => false,
        FlushAction::Immediate => flush_pending(pending, target, view, cx),
        FlushAction::Schedule => {
            let pending = Rc::clone(pending);
            let target = target.clone();
            let view = view.clone();
            *flush_task = Some(cx.spawn(async move |cx| {
                cx.background_executor().timer(STREAM_FLUSH_INTERVAL).await;
                flush_pending(&pending, &target, &view, cx);
            }));
            false
        }
    }
}

fn flush_pending(
    pending: &Rc<RefCell<PendingDeltas>>,
    target: &Entity<TextViewState>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let deltas = drain_pending(pending);
    if deltas.is_empty() {
        return false;
    }
    let display = visible_text(&deltas);
    if !display.is_empty() {
        target.update(cx, |state, cx| state.push_str(&display, cx));
    }
    view.update(cx, |chat, cx| {
        apply_deltas(chat, deltas);
        chat.finish_stream_batch(cx);
    })
    .is_err()
}

fn drain_pending(pending: &Rc<RefCell<PendingDeltas>>) -> Vec<StreamDelta> {
    pending.borrow_mut().take()
}

fn visible_text(deltas: &[StreamDelta]) -> String {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::Text(text) => Some(text.as_str()),
            StreamDelta::Reasoning(_) => None,
        })
        .collect()
}

fn apply_deltas(chat: &mut ChatView, deltas: Vec<StreamDelta>) {
    for delta in deltas {
        match delta {
            StreamDelta::Text(text) => chat.append_stream_text(&text),
            StreamDelta::Reasoning(reasoning) => {
                chat.append_stream_reasoning(&reasoning);
            }
        }
    }
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
            pending.push(StreamDelta::Text("a".into())),
            FlushAction::Schedule
        );
        assert_eq!(
            pending.push(StreamDelta::Text("b".into())),
            FlushAction::Pending
        );
        pending.push(StreamDelta::Reasoning("c".into()));
        pending.push(StreamDelta::Text("d".into()));

        assert_eq!(
            pending.take(),
            vec![
                StreamDelta::Text("ab".into()),
                StreamDelta::Reasoning("c".into()),
                StreamDelta::Text("d".into()),
            ]
        );
    }

    #[test]
    fn pending_deltas_schedule_each_non_empty_batch_once() {
        let mut pending = PendingDeltas::default();
        assert_eq!(
            pending.push(StreamDelta::Text("first".into())),
            FlushAction::Schedule
        );
        assert_eq!(
            pending.push(StreamDelta::Text("second".into())),
            FlushAction::Pending
        );
        pending.take();
        assert_eq!(
            pending.push(StreamDelta::Text("third".into())),
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
