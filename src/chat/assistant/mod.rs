//! Runtime bridge for streaming model generations.
//!
//! Canonical gateway events are coalesced into ordered, grapheme-safe semantic
//! batches. The owning conversation runtime receives those batches and decides
//! how they are projected into its presentation consumers.

mod buffer;
#[cfg(test)]
mod tests;

use self::buffer::*;

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use futures::future::{AbortHandle, Abortable};
use gpui::{Context, Task};

use crate::{
    chat::conversation_runtime::{
        ConversationRequestGeneration, ConversationRuntime, ConversationRuntimeEvent,
        ConversationStreamEvent,
    },
    llm::{
        GatewayError, GenerateRequest, GenerationEvent, GenerationHandle, GenerationOutcome,
        GenerationRequest, GenerationService, Message as LlmMessage, ModelSelection,
    },
};

#[cfg(test)]
use crate::chat::ChatView;
#[cfg(test)]
use crate::llm::OutcomeStatus;

pub struct ReplyTask {
    _task: Task<()>,
    abort: AbortHandle,
}

pub(super) struct ReplyRequest {
    pub history: Vec<LlmMessage>,
    pub selection: ModelSelection,
    pub generation_service: Arc<dyn GenerationService>,
    pub conversation_id: String,
    pub turn_id: String,
    pub request_generation: ConversationRequestGeneration,
}

impl ReplyTask {
    pub fn cancel(&self) {
        self.abort.abort();
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(
        dropped: Rc<std::cell::Cell<bool>>,
        cx: &mut Context<ConversationRuntime>,
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

pub(super) fn stream_reply(
    request: ReplyRequest,
    cx: &mut Context<ConversationRuntime>,
) -> Result<ReplyTask, GatewayError> {
    let prepared = request.generation_service.start(GenerationRequest::new(
        request.selection.clone(),
        GenerateRequest {
            messages: request.history,
            conversation_id: request.conversation_id,
            turn_id: request.turn_id,
            ..GenerateRequest::default()
        },
    ));
    let generation = prepared?;
    let request_generation = request.request_generation;
    let (abort, registration) = AbortHandle::new_pair();
    let task = cx.spawn(async move |runtime, cx| {
        let pending = Rc::new(RefCell::new(PendingDeltas::default()));
        let mut flush_task: Option<Task<()>> = None;
        let mut terminal: Option<GenerationOutcome> = None;
        let mut generation: GenerationHandle = generation;

        let run = generation.run(|event| {
            !process_event(
                event,
                &pending,
                &mut flush_task,
                &mut terminal,
                &runtime,
                request_generation,
                cx,
            )
        });
        if Abortable::new(run, registration).await.is_err()
            && let Some(event) = generation.cancel()
        {
            process_event(
                event,
                &pending,
                &mut flush_task,
                &mut terminal,
                &runtime,
                request_generation,
                cx,
            );
        }

        let Some(outcome) = terminal else {
            return;
        };
        // The scheduled coalescing task and terminal catch-up must never race
        // to consume the same ordered queue. Dropping the task cancels its
        // timer; this ReplyTask future then owns pacing through completion.
        flush_task.take();
        while !pending.borrow().deltas.is_empty() {
            if flush_pending_frame(&pending, &runtime, request_generation, cx, true) {
                return;
            }
            if pending.borrow().deltas.is_empty() {
                break;
            }
            let interval = pending.borrow().next_interval();
            cx.background_executor().timer(interval).await;
        }

        runtime
            .update(cx, |runtime, cx| {
                runtime.finish_generation(request_generation, outcome, cx)
            })
            .ok();
    });
    Ok(ReplyTask { _task: task, abort })
}

fn process_event(
    event: GenerationEvent,
    pending: &Rc<RefCell<PendingDeltas>>,
    flush_task: &mut Option<Task<()>>,
    terminal: &mut Option<GenerationOutcome>,
    runtime: &gpui::WeakEntity<ConversationRuntime>,
    request_generation: ConversationRequestGeneration,
    cx: &mut gpui::AsyncApp,
) -> bool {
    match event {
        GenerationEvent::Finished(outcome) => {
            *terminal = Some(*outcome);
            true
        }
        event => project_stream_delta(event).is_some_and(|delta| {
            queue_delta(delta, pending, flush_task, runtime, request_generation, cx)
        }),
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
    runtime: &gpui::WeakEntity<ConversationRuntime>,
    request_generation: ConversationRequestGeneration,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let action = pending.borrow_mut().push(delta);
    match action {
        FlushAction::Pending => false,
        FlushAction::Immediate => {
            flush_task.take();
            if flush_pending_frame(pending, runtime, request_generation, cx, false) {
                return true;
            }
            if !pending.borrow().deltas.is_empty() {
                pending.borrow_mut().schedule();
                *flush_task = Some(spawn_flush_task(
                    Rc::clone(pending),
                    runtime.clone(),
                    request_generation,
                    cx,
                    pending.borrow().next_interval(),
                ));
            }
            false
        }
        FlushAction::Schedule => {
            *flush_task = Some(spawn_flush_task(
                Rc::clone(pending),
                runtime.clone(),
                request_generation,
                cx,
                STREAM_FLUSH_INTERVAL,
            ));
            false
        }
    }
}

fn spawn_flush_task(
    pending: Rc<RefCell<PendingDeltas>>,
    runtime: gpui::WeakEntity<ConversationRuntime>,
    request_generation: ConversationRequestGeneration,
    cx: &mut gpui::AsyncApp,
    first_interval: Duration,
) -> Task<()> {
    cx.spawn(async move |cx| {
        let mut interval = first_interval;
        loop {
            cx.background_executor().timer(interval).await;
            if flush_pending_frame(&pending, &runtime, request_generation, cx, false) {
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
    runtime: &gpui::WeakEntity<ConversationRuntime>,
    request_generation: ConversationRequestGeneration,
    cx: &mut gpui::AsyncApp,
    terminal: bool,
) -> bool {
    let deltas = pending.borrow_mut().take_frame(terminal);
    if deltas.is_empty() {
        return false;
    }
    let events = deltas.into_iter().map(semantic_event).collect();
    runtime
        .update(cx, |runtime, cx| {
            runtime.publish_event(
                ConversationRuntimeEvent::StreamBatch {
                    generation: request_generation,
                    events,
                },
                cx,
            )
        })
        .is_err()
}

fn semantic_event(delta: StreamDelta) -> ConversationStreamEvent {
    match delta {
        StreamDelta::TextStarted { content_index, id } => {
            ConversationStreamEvent::TextStarted { content_index, id }
        }
        StreamDelta::TextDelta {
            content_index,
            id,
            delta,
        } => ConversationStreamEvent::TextDelta {
            content_index,
            id,
            delta,
        },
        StreamDelta::TextFinished {
            content_index,
            id,
            replay,
        } => ConversationStreamEvent::TextFinished {
            content_index,
            id,
            replay,
        },
        StreamDelta::ReasoningStarted { content_index, id } => {
            ConversationStreamEvent::ReasoningStarted { content_index, id }
        }
        StreamDelta::ReasoningDelta {
            content_index,
            id,
            delta,
        } => ConversationStreamEvent::ReasoningDelta {
            content_index,
            id,
            delta,
        },
        StreamDelta::ReasoningFinished {
            content_index,
            id,
            replay,
        } => ConversationStreamEvent::ReasoningFinished {
            content_index,
            id,
            replay,
        },
        StreamDelta::ReasoningSnapshotUpdated {
            content_index,
            id,
            reasoning,
        } => ConversationStreamEvent::ReasoningSnapshotUpdated {
            content_index,
            id,
            reasoning,
        },
        StreamDelta::ToolCallStarted {
            content_index,
            index,
            id,
            name,
        } => ConversationStreamEvent::ToolCallStarted {
            content_index,
            index,
            id,
            name,
        },
        StreamDelta::ToolCallFinished {
            content_index,
            index,
            tool_call,
        } => ConversationStreamEvent::ToolCallFinished {
            content_index,
            index,
            tool_call,
        },
    }
}

#[cfg(test)]
pub(crate) fn apply_generation_events_for_test(
    chat: &mut ChatView,
    events: Vec<GenerationEvent>,
    cx: &mut Context<ChatView>,
) {
    for event in events {
        match project_stream_delta(event).map(semantic_event) {
            Some(ConversationStreamEvent::TextStarted { content_index, id }) => {
                chat.start_stream_text(content_index, id, cx);
            }
            Some(ConversationStreamEvent::TextDelta {
                content_index,
                id,
                delta,
            }) => {
                chat.append_stream_text(content_index, id, &delta, cx);
            }
            Some(ConversationStreamEvent::TextFinished {
                content_index,
                id,
                replay,
            }) => {
                chat.finish_stream_text(content_index, &id, replay, cx);
            }
            Some(ConversationStreamEvent::ReasoningStarted { content_index, id }) => {
                chat.start_stream_reasoning(content_index, id);
            }
            Some(ConversationStreamEvent::ReasoningDelta {
                content_index,
                id,
                delta,
            }) => {
                chat.append_stream_reasoning(content_index, id, &delta, cx);
            }
            Some(ConversationStreamEvent::ReasoningFinished {
                content_index,
                id,
                replay,
            }) => {
                chat.finish_stream_reasoning(content_index, &id, replay, cx);
            }
            Some(ConversationStreamEvent::ReasoningSnapshotUpdated {
                content_index,
                id,
                reasoning,
            }) => {
                chat.update_stream_reasoning_snapshot(content_index, &id, reasoning, cx);
            }
            Some(ConversationStreamEvent::ToolCallStarted {
                content_index,
                index,
                id,
                name,
            }) => {
                chat.start_stream_tool_call(content_index, index, id, name);
            }
            Some(ConversationStreamEvent::ToolCallFinished {
                content_index,
                index,
                tool_call,
            }) => {
                chat.finish_stream_tool_call(content_index, index, *tool_call);
            }
            None => {}
        }
    }
}

#[cfg(test)]
fn terminal_failure(
    status: OutcomeStatus,
    error: Option<GatewayError>,
    request_id: String,
) -> Option<GatewayError> {
    (status == OutcomeStatus::Failed).then(|| {
        let mut error =
            error.unwrap_or_else(|| GatewayError::provider("provider request failed", None));
        error.request_id.get_or_insert(request_id);
        error
    })
}
