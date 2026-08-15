//! GPUI boundary for streaming model generations.
//!
//! Gateway events are coalesced, and transport bursts are revealed in ordered
//! grapheme-safe frames before updating entities. This keeps protocol and
//! transport work out of views while preserving canonical event order,
//! terminal gating, and task-drop cancellation.

mod buffer;
#[cfg(test)]
mod tests;

use self::buffer::*;

use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::Duration,
};

use futures::future::{AbortHandle, Abortable};
use gpui::{Context, Task};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::{
    chat::ChatView,
    llm::{
        Gateway, GatewayError, GenerateRequest, GenerationEvent, GenerationOutcome, HttpTransport,
        InMemoryMetrics, Message as LlmMessage, ModelSelection, OutcomeStatus,
    },
    providers,
    session::ChatTurnTerminal,
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

impl ReplyTask {
    pub fn cancel(&self) {
        self.abort.abort();
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(
        dropped: Rc<std::cell::Cell<bool>>,
        cx: &mut Context<ChatView>,
    ) -> Self {
        struct DropFlag(Rc<std::cell::Cell<bool>>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let (abort, _registration) = AbortHandle::new_pair();
        let task = cx.spawn(async move |_, _| {
            let _drop_flag = DropFlag(dropped);
            std::future::pending::<()>().await;
        });
        Self { _task: task, abort }
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
        let mut terminal: Option<GenerationOutcome> = None;
        let mut generation = match prepared {
            Ok(generation) => generation,
            Err(error) => {
                // Same card as an upstream failure. There is no response body to
                // show — the request never left — so it renders headline-only.
                view.update(cx, |chat, cx| chat.finish_reply_request_failed(error, cx))
                    .ok();
                return;
            }
        };

        let run = generation.run(|event| {
            !process_event(event, &pending, &mut flush_task, &mut terminal, &view, cx)
        });
        if Abortable::new(run, registration).await.is_err()
            && let Some(event) = generation.cancel()
        {
            process_event(event, &pending, &mut flush_task, &mut terminal, &view, cx);
        }

        let Some(outcome) = terminal else {
            return;
        };
        // The scheduled coalescing task and terminal catch-up must never race
        // to consume the same ordered queue. Dropping the task cancels its
        // timer; this ReplyTask future then owns pacing through completion.
        flush_task.take();
        while !pending.borrow().deltas.is_empty() {
            if flush_pending_frame(&pending, &view, cx, true) {
                return;
            }
            if pending.borrow().deltas.is_empty() {
                break;
            }
            let interval = pending.borrow().next_interval();
            cx.background_executor().timer(interval).await;
        }

        let terminal = ChatTurnTerminal::from_generation(&outcome);
        let failure = terminal_failure(
            outcome.status,
            outcome.error.clone(),
            outcome.request_id.clone(),
        );
        view.update(cx, |chat, cx| {
            chat.follow_stream();
            chat.finish_reply_with_terminal(outcome.message, terminal, failure, cx);
        })
        .ok();
    });
    ReplyTask { _task: task, abort }
}

fn process_event(
    event: GenerationEvent,
    pending: &Rc<RefCell<PendingDeltas>>,
    flush_task: &mut Option<Task<()>>,
    terminal: &mut Option<GenerationOutcome>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    match event {
        GenerationEvent::Finished(outcome) => {
            *terminal = Some(*outcome);
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
        FlushAction::Immediate => {
            flush_task.take();
            if flush_pending_frame(pending, view, cx, false) {
                return true;
            }
            if !pending.borrow().deltas.is_empty() {
                pending.borrow_mut().schedule();
                *flush_task = Some(spawn_flush_task(
                    Rc::clone(pending),
                    view.clone(),
                    cx,
                    pending.borrow().next_interval(),
                ));
            }
            false
        }
        FlushAction::Schedule => {
            *flush_task = Some(spawn_flush_task(
                Rc::clone(pending),
                view.clone(),
                cx,
                STREAM_FLUSH_INTERVAL,
            ));
            false
        }
    }
}

fn spawn_flush_task(
    pending: Rc<RefCell<PendingDeltas>>,
    view: gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
    first_interval: Duration,
) -> Task<()> {
    cx.spawn(async move |cx| {
        let mut interval = first_interval;
        loop {
            cx.background_executor().timer(interval).await;
            if flush_pending_frame(&pending, &view, cx, false) {
                return;
            }
            let Some(next_interval) = ({
                let mut pending = pending.borrow_mut();
                if pending.deltas.is_empty() {
                    pending.flush_scheduled = false;
                    None
                } else {
                    Some(pending.next_interval())
                }
            }) else {
                return;
            };
            interval = next_interval;
        }
    })
}

fn flush_pending_frame(
    pending: &Rc<RefCell<PendingDeltas>>,
    view: &gpui::WeakEntity<ChatView>,
    cx: &mut gpui::AsyncApp,
    terminal: bool,
) -> bool {
    let deltas = pending.borrow_mut().take_frame(terminal);
    if deltas.is_empty() {
        return false;
    }
    view.update(cx, |chat, cx| {
        apply_deltas(chat, deltas, cx);
        chat.finish_stream_batch(cx);
    })
    .is_err()
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
