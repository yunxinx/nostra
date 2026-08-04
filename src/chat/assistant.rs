//! GPUI boundary for streaming model generations.
//!
//! Gateway events are coalesced, and transport bursts are revealed in ordered
//! grapheme-safe frames before updating entities. This keeps protocol and
//! transport work out of views while preserving canonical event order,
//! terminal gating, and task-drop cancellation.

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
const PACED_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const MAX_PENDING_QUEUE_ENTRIES: usize = 32;
const DIRECT_FOLLOW_CHUNK_GRAPHEMES: usize = 8;
const MIN_VISIBLE_GRAPHEMES_PER_COMMIT: usize = 8;
const MAX_VISIBLE_GRAPHEMES_PER_COMMIT: usize = 160;
const PACING_TARGET_FRAMES: usize = 5;

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

#[derive(Clone, Copy)]
struct GraphemeSummary {
    count: usize,
    last_start: Option<usize>,
}

#[derive(Debug)]
struct QueuedDelta {
    delta: StreamDelta,
    cursor: usize,
    graphemes: Option<usize>,
    last_grapheme_start: Option<usize>,
}

impl QueuedDelta {
    fn new(delta: StreamDelta, summary: Option<GraphemeSummary>) -> Self {
        Self {
            delta,
            cursor: 0,
            graphemes: summary.map(|summary| summary.count),
            last_grapheme_start: summary.and_then(|summary| summary.last_start),
        }
    }

    /// Merge an adjacent delta of the same streamed block, returning the
    /// original delta only when it must remain a separate ordered entry.
    fn try_merge(
        &mut self,
        delta: StreamDelta,
        summary: Option<GraphemeSummary>,
    ) -> Option<StreamDelta> {
        let next_summary = summary.unwrap_or(GraphemeSummary {
            count: 0,
            last_start: None,
        });
        match (&mut self.delta, delta) {
            (
                StreamDelta::TextDelta {
                    content_index: current_index,
                    id: current_id,
                    delta: current,
                },
                StreamDelta::TextDelta {
                    content_index,
                    id,
                    delta: next,
                },
            ) if *current_index == content_index && *current_id == id => {
                Self::append_text(
                    current,
                    &next,
                    &mut self.graphemes,
                    &mut self.last_grapheme_start,
                    next_summary,
                );
                None
            }
            (
                StreamDelta::ReasoningDelta {
                    content_index: current_index,
                    id: current_id,
                    delta: current,
                },
                StreamDelta::ReasoningDelta {
                    content_index,
                    id,
                    delta: next,
                },
            ) if *current_index == content_index && *current_id == id => {
                Self::append_text(
                    current,
                    &next,
                    &mut self.graphemes,
                    &mut self.last_grapheme_start,
                    next_summary,
                );
                None
            }
            (_, delta) => Some(delta),
        }
    }

    fn append_text(
        current: &mut String,
        next: &str,
        graphemes: &mut Option<usize>,
        last_grapheme_start: &mut Option<usize>,
        next_summary: GraphemeSummary,
    ) {
        if next.is_empty() {
            return;
        }

        let previous_count = graphemes.unwrap_or(0);
        if previous_count == 0 {
            let previous_len = current.len();
            current.push_str(next);
            *graphemes = Some(next_summary.count);
            *last_grapheme_start = next_summary
                .last_start
                .map(|last_start| previous_len + last_start);
            return;
        }

        // Only the final pending grapheme can combine with the next transport
        // chunk. Re-segment that tail and the new chunk once, instead of
        // recounting the complete visible backlog on every frame.
        let tail_start = last_grapheme_start.expect("non-empty text has a final grapheme");
        current.push_str(next);
        let combined = grapheme_summary(&current[tail_start..]);
        *graphemes = Some(previous_count - 1 + combined.count);
        *last_grapheme_start = combined
            .last_start
            .map(|last_start| tail_start + last_start);
    }

    fn take_prefix(&mut self, count: usize) -> Option<StreamDelta> {
        let total = self.graphemes?;
        if count == 0 || count >= total {
            return None;
        }

        let (content_index, id, source, reasoning) = match &self.delta {
            StreamDelta::TextDelta {
                content_index,
                id,
                delta,
            } => (*content_index, id.clone(), delta, false),
            StreamDelta::ReasoningDelta {
                content_index,
                id,
                delta,
            } => (*content_index, id.clone(), delta, true),
            _ => return None,
        };
        let remaining = &source[self.cursor..];
        let end = remaining
            .grapheme_indices(true)
            .nth(count)
            .map_or(remaining.len(), |(offset, _)| offset);
        let prefix = remaining[..end].to_string();
        self.cursor += end;
        self.graphemes = Some(total - count);

        Some(if reasoning {
            StreamDelta::ReasoningDelta {
                content_index,
                id,
                delta: prefix,
            }
        } else {
            StreamDelta::TextDelta {
                content_index,
                id,
                delta: prefix,
            }
        })
    }

    fn into_remaining_delta(mut self) -> StreamDelta {
        if self.cursor > 0 {
            match &mut self.delta {
                StreamDelta::TextDelta { delta, .. }
                | StreamDelta::ReasoningDelta { delta, .. } => {
                    *delta = delta.split_off(self.cursor);
                }
                _ => {}
            }
        }
        self.delta
    }
}

fn grapheme_summary(text: &str) -> GraphemeSummary {
    let mut count = 0;
    let mut last_start = None;
    for (offset, _) in text.grapheme_indices(true) {
        count += 1;
        last_start = Some(offset);
    }
    GraphemeSummary { count, last_start }
}

#[derive(Default)]
struct PendingDeltas {
    deltas: VecDeque<QueuedDelta>,
    pending_graphemes: usize,
    flush_scheduled: bool,
    paced: bool,
    held_tail_once: bool,
}

impl PendingDeltas {
    fn push(&mut self, delta: StreamDelta) -> FlushAction {
        let summary = delta.grapheme_summary();
        self.paced |= summary.is_some_and(|summary| summary.count > DIRECT_FOLLOW_CHUNK_GRAPHEMES);
        // Re-segment the new authoritative tail together with the carried
        // cluster before it becomes visible. This protects combining marks and
        // ZWJ sequences that a provider splits across transport chunks.
        self.held_tail_once = false;
        let mut delta = Some(delta);
        if let Some(back) = self.deltas.back_mut() {
            let previous_count = back.graphemes.unwrap_or(0);
            match back.try_merge(delta.take().expect("delta is available"), summary) {
                None => {
                    self.pending_graphemes = self
                        .pending_graphemes
                        .saturating_sub(previous_count)
                        .saturating_add(back.graphemes.unwrap_or(0));
                }
                Some(unmerged) => delta = Some(unmerged),
            }
        }
        if let Some(delta) = delta {
            self.pending_graphemes = self
                .pending_graphemes
                .saturating_add(summary.map_or(0, |summary| summary.count));
            self.deltas.push_back(QueuedDelta::new(delta, summary));
        }
        if self.deltas.len() >= MAX_PENDING_QUEUE_ENTRIES {
            self.flush_scheduled = false;
            FlushAction::Immediate
        } else if self.flush_scheduled {
            FlushAction::Pending
        } else {
            self.flush_scheduled = true;
            FlushAction::Schedule
        }
    }

    #[cfg(test)]
    fn take(&mut self) -> Vec<StreamDelta> {
        self.flush_scheduled = false;
        self.pending_graphemes = 0;
        self.paced = false;
        self.held_tail_once = false;
        std::mem::take(&mut self.deltas)
            .into_iter()
            .map(QueuedDelta::into_remaining_delta)
            .collect()
    }

    fn take_frame(&mut self, terminal: bool) -> Vec<StreamDelta> {
        let backlog = self.pending_graphemes;
        // Several individually smooth transport chunks can arrive before the
        // UI timer gets a turn. Once their aggregate exceeds one commit, treat
        // the queue as a burst so terminal catch-up cannot reveal it at once.
        self.paced |= backlog > MAX_VISIBLE_GRAPHEMES_PER_COMMIT;
        let mut budget = if self.paced && backlog > 0 {
            backlog
                .div_ceil(PACING_TARGET_FRAMES)
                .clamp(
                    MIN_VISIBLE_GRAPHEMES_PER_COMMIT,
                    MAX_VISIBLE_GRAPHEMES_PER_COMMIT,
                )
                .min(backlog)
        } else {
            usize::MAX
        };
        let mut visible = Vec::new();

        while let Some(front) = self.deltas.front() {
            let Some(total) = front.graphemes else {
                visible.push(
                    self.deltas
                        .pop_front()
                        .expect("front delta exists")
                        .into_remaining_delta(),
                );
                continue;
            };
            if total == 0 {
                self.deltas.pop_front();
                continue;
            }
            if budget == 0 {
                break;
            }

            let hold_transport_tail = !terminal && self.deltas.len() == 1 && !self.held_tail_once;
            let available = total.saturating_sub(usize::from(hold_transport_tail));
            if available == 0 {
                self.held_tail_once = true;
                break;
            }

            let count = available.min(budget);
            if count == total {
                visible.push(
                    self.deltas
                        .pop_front()
                        .expect("front delta exists")
                        .into_remaining_delta(),
                );
            } else {
                let prefix = self
                    .deltas
                    .front_mut()
                    .and_then(|delta| delta.take_prefix(count))
                    .expect("front text delta contains a visible prefix");
                visible.push(prefix);
            }
            self.pending_graphemes = self.pending_graphemes.saturating_sub(count);
            budget = budget.saturating_sub(count);

            if hold_transport_tail && count == available {
                self.held_tail_once = true;
                break;
            }
        }

        if self.deltas.is_empty() {
            self.flush_scheduled = false;
            self.paced = false;
            self.held_tail_once = false;
        }
        visible
    }

    fn next_interval(&self) -> Duration {
        if self.paced {
            PACED_FLUSH_INTERVAL
        } else {
            STREAM_FLUSH_INTERVAL
        }
    }

    fn schedule(&mut self) {
        self.flush_scheduled = true;
    }
}

impl StreamDelta {
    fn grapheme_summary(&self) -> Option<GraphemeSummary> {
        match self {
            Self::TextDelta { delta, .. } | Self::ReasoningDelta { delta, .. } => {
                Some(grapheme_summary(delta))
            }
            _ => None,
        }
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
                view.update(cx, |chat, cx| chat.finish_reply(None, Some(error), cx))
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

        let failure = terminal_failure(outcome.status, outcome.error, outcome.request_id);
        view.update(cx, |chat, cx| {
            chat.follow_stream();
            chat.finish_reply(outcome.message, failure, cx);
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
    fn paced_frames_are_grapheme_safe_and_gate_lifecycle_events() {
        let mut small_burst = PendingDeltas::default();
        for _ in 0..1_000 {
            let action = small_burst.push(StreamDelta::TextDelta {
                content_index: 0,
                id: "burst".into(),
                delta: "流".into(),
            });
            assert_ne!(
                action,
                FlushAction::Immediate,
                "adjacent text deltas must be paced as one transport burst"
            );
        }
        let first_burst_frame = small_burst.take_frame(false);
        let [StreamDelta::TextDelta { delta, .. }] = first_burst_frame.as_slice() else {
            panic!("first small-delta burst frame must contain only visible text");
        };
        assert!(delta.graphemes(true).count() <= MAX_VISIBLE_GRAPHEMES_PER_COMMIT);
        assert!(delta.graphemes(true).count() < 1_000);

        let source = format!("{}e\u{301}👩‍👩‍👧‍👦", "流".repeat(400));
        let source_graphemes = source.graphemes(true).count();
        let mut pending = PendingDeltas::default();
        pending.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: source.clone(),
        });
        pending.push(StreamDelta::TextFinished {
            content_index: 0,
            id: "text-0".into(),
            replay: None,
        });

        let first = pending.take_frame(false);
        let [StreamDelta::TextDelta { delta: first, .. }] = first.as_slice() else {
            panic!("first paced frame must contain only visible text");
        };
        assert!(first.graphemes(true).count() <= MAX_VISIBLE_GRAPHEMES_PER_COMMIT);
        assert!(first.graphemes(true).count() < source_graphemes);

        let mut rendered = first.clone();
        let mut finished = false;
        while !pending.deltas.is_empty() {
            for delta in pending.take_frame(false) {
                match delta {
                    StreamDelta::TextDelta { delta, .. } => {
                        assert!(!finished, "text must not cross its finish boundary");
                        rendered.push_str(&delta);
                    }
                    StreamDelta::TextFinished { .. } => {
                        assert_eq!(rendered, source);
                        finished = true;
                    }
                    other => panic!("unexpected paced delta: {other:?}"),
                }
            }
        }
        assert!(finished);
        assert_eq!(rendered, source);

        let mut split_grapheme = PendingDeltas::default();
        split_grapheme.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "e".into(),
        });
        assert!(split_grapheme.take_frame(false).is_empty());
        split_grapheme.push(StreamDelta::TextDelta {
            content_index: 0,
            id: "text-0".into(),
            delta: "\u{301}".into(),
        });
        split_grapheme.push(StreamDelta::TextFinished {
            content_index: 0,
            id: "text-0".into(),
            replay: None,
        });
        let combined = split_grapheme.take_frame(false);
        let [
            StreamDelta::TextDelta { delta, .. },
            StreamDelta::TextFinished { .. },
        ] = combined.as_slice()
        else {
            panic!("completed split grapheme must precede its finish boundary");
        };
        assert_eq!(delta, "e\u{301}");
        assert_eq!(delta.graphemes(true).count(), 1);
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
