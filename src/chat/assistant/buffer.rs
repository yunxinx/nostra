//! Grapheme-safe buffering and paced stream flush state.

use super::*;

pub(super) const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
pub(super) const PACED_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
pub(super) const MAX_PENDING_QUEUE_ENTRIES: usize = 32;
pub(super) const DIRECT_FOLLOW_CHUNK_GRAPHEMES: usize = 8;
pub(super) const MIN_VISIBLE_GRAPHEMES_PER_COMMIT: usize = 8;
pub(super) const MAX_VISIBLE_GRAPHEMES_PER_COMMIT: usize = 160;
pub(super) const PACING_TARGET_FRAMES: usize = 5;

#[derive(Debug, PartialEq)]
pub(super) enum StreamDelta {
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
pub(super) struct GraphemeSummary {
    count: usize,
    last_start: Option<usize>,
}

#[derive(Debug)]
pub(super) struct QueuedDelta {
    delta: StreamDelta,
    cursor: usize,
    graphemes: Option<usize>,
    last_grapheme_start: Option<usize>,
}

impl QueuedDelta {
    pub(super) fn new(delta: StreamDelta, summary: Option<GraphemeSummary>) -> Self {
        Self {
            delta,
            cursor: 0,
            graphemes: summary.map(|summary| summary.count),
            last_grapheme_start: summary.and_then(|summary| summary.last_start),
        }
    }

    /// Merge an adjacent delta of the same streamed block, returning the
    /// original delta only when it must remain a separate ordered entry.
    pub(super) fn try_merge(
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

    pub(super) fn append_text(
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

    pub(super) fn take_prefix(&mut self, count: usize) -> Option<StreamDelta> {
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

    pub(super) fn into_remaining_delta(mut self) -> StreamDelta {
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

pub(super) fn grapheme_summary(text: &str) -> GraphemeSummary {
    let mut count = 0;
    let mut last_start = None;
    for (offset, _) in text.grapheme_indices(true) {
        count += 1;
        last_start = Some(offset);
    }
    GraphemeSummary { count, last_start }
}

#[derive(Default)]
pub(super) struct PendingDeltas {
    pub(super) deltas: VecDeque<QueuedDelta>,
    pending_graphemes: usize,
    pub(super) flush_scheduled: bool,
    paced: bool,
    held_tail_once: bool,
}

impl PendingDeltas {
    pub(super) fn push(&mut self, delta: StreamDelta) -> FlushAction {
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
    pub(super) fn take(&mut self) -> Vec<StreamDelta> {
        self.flush_scheduled = false;
        self.pending_graphemes = 0;
        self.paced = false;
        self.held_tail_once = false;
        std::mem::take(&mut self.deltas)
            .into_iter()
            .map(QueuedDelta::into_remaining_delta)
            .collect()
    }

    pub(super) fn take_frame(&mut self, terminal: bool) -> Vec<StreamDelta> {
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

    pub(super) fn next_interval(&self) -> Duration {
        if self.paced {
            PACED_FLUSH_INTERVAL
        } else {
            STREAM_FLUSH_INTERVAL
        }
    }

    pub(super) fn schedule(&mut self) {
        self.flush_scheduled = true;
    }
}

impl StreamDelta {
    pub(super) fn grapheme_summary(&self) -> Option<GraphemeSummary> {
        match self {
            Self::TextDelta { delta, .. } | Self::ReasoningDelta { delta, .. } => {
                Some(grapheme_summary(delta))
            }
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum FlushAction {
    Schedule,
    Pending,
    Immediate,
}
