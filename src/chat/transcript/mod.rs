//! Canonical conversation transcript: turns, stream state, and published updates.

mod model;
mod reconcile;
mod source;

#[cfg(test)]
mod tests;

use gpui::{Context, EventEmitter, SharedString};
use rust_i18n::t;

use crate::llm::{
    GatewayError, IndexedMessage, Message as LlmMessage, ProviderMetadata, ReasoningContent,
    ToolCall,
};

use super::conversation_runtime::ConversationStreamEvent;

pub(crate) use self::model::{
    Part, PartId, PartKind, PartSource, Role, Turn, TurnId, is_replayable,
};
pub(crate) use self::source::{
    ResolvedStateSource, TranscriptCursor, TranscriptPage, TranscriptSource,
};

use self::model::{allocate_part_id, allocate_turn_id, apply_indexed_message};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartChange {
    Append,
    Replace,
    Finished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptEvent {
    TailAppended {
        turn_ids: Vec<TurnId>,
    },
    PartInserted {
        turn_id: TurnId,
        part_id: PartId,
    },
    PartChanged {
        turn_id: TurnId,
        part_id: PartId,
        change: PartChange,
        delta: SharedString,
    },
    TurnReplaced {
        turn_id: TurnId,
    },
    Reset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptSnapshot {
    revision: u64,
    turn_count: usize,
    streaming: Option<(TurnId, PartId)>,
    // Forward contract for P2 forward loading (`load_before`); tests read it
    // via the `has_earlier` accessor, and `PartialEq` keeps the lint quiet.
    has_earlier: bool,
}

impl TranscriptSnapshot {
    #[must_use]
    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub(crate) const fn turn_count(&self) -> usize {
        self.turn_count
    }

    #[must_use]
    pub(crate) const fn streaming(&self) -> Option<(TurnId, PartId)> {
        self.streaming
    }

    #[must_use]
    pub(crate) const fn is_streaming(&self) -> bool {
        self.streaming.is_some()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn has_earlier(&self) -> bool {
        self.has_earlier
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptUpdate {
    snapshot: TranscriptSnapshot,
    event: TranscriptEvent,
}

impl TranscriptUpdate {
    #[must_use]
    pub(crate) fn snapshot(&self) -> &TranscriptSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub(crate) fn event(&self) -> &TranscriptEvent {
        &self.event
    }
}

pub(crate) struct Transcript {
    revision: u64,
    turns: Vec<Turn>,
    streaming: Option<(TurnId, PartId)>,
    source_cursor: Option<TranscriptCursor>,
    next_turn_id: u64,
    next_part_id: u64,
}

impl Transcript {
    pub(crate) fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            revision: 0,
            turns: Vec::new(),
            streaming: None,
            source_cursor: None,
            next_turn_id: 1,
            next_part_id: 1,
        }
    }

    #[must_use]
    pub(crate) fn turns(&self) -> &[Turn] {
        &self.turns
    }

    #[must_use]
    pub(crate) fn turn(&self, id: TurnId) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.turn_id == id)
    }

    #[must_use]
    pub(crate) fn snapshot(&self) -> TranscriptSnapshot {
        TranscriptSnapshot {
            revision: self.revision,
            turn_count: self.turns.len(),
            streaming: self.streaming,
            has_earlier: self.source_cursor.is_some(),
        }
    }

    #[must_use]
    pub(crate) fn replayable_history(&self) -> Vec<LlmMessage> {
        let end = match self.turns.last() {
            Some(turn)
                if turn.role == Role::Assistant
                    && turn.parts.is_empty()
                    && turn.error.is_none() =>
            {
                self.turns.len().saturating_sub(1)
            }
            _ => self.turns.len(),
        };
        self.turns[..end]
            .iter()
            .map(Turn::to_llm)
            .filter(is_replayable)
            .collect()
    }

    #[must_use]
    pub(crate) fn title(&self) -> Option<SharedString> {
        title_from_turns(&self.turns)
    }

    #[must_use]
    pub(crate) fn copyable_text(&self, turn_id: TurnId) -> Option<SharedString> {
        let turn = self.turn(turn_id)?;
        let text = copyable_text(turn);
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn load(
        &mut self,
        page: TranscriptPage,
        cursor: Option<TranscriptCursor>,
        cx: &mut Context<Self>,
    ) -> TranscriptUpdate {
        self.next_turn_id = 1;
        self.next_part_id = 1;
        self.turns = self.adopt_turns(page.turns);
        self.source_cursor = cursor.or(page.cursor_before);
        self.streaming = None;
        self.publish(TranscriptEvent::Reset, cx)
    }

    pub(crate) fn begin_turn(
        &mut self,
        user: LlmMessage,
        cx: &mut Context<Self>,
    ) -> (TurnId, TranscriptUpdate) {
        let user_turn = Turn::from_llm(
            user,
            allocate_turn_id(&mut self.next_turn_id),
            &mut self.next_part_id,
        );
        let assistant_id = allocate_turn_id(&mut self.next_turn_id);
        let assistant = Turn::empty(Role::Assistant, assistant_id);
        let turn_ids = vec![user_turn.turn_id, assistant_id];
        self.turns.push(user_turn);
        self.turns.push(assistant);
        self.streaming = None;
        let update = self.publish(
            TranscriptEvent::TailAppended {
                turn_ids: turn_ids.clone(),
            },
            cx,
        );
        (assistant_id, update)
    }

    pub(crate) fn apply_stream_batch(
        &mut self,
        events: &[ConversationStreamEvent],
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptUpdate> {
        let mut updates = Vec::new();
        for event in events {
            updates.extend(self.apply_stream_event(event, cx));
        }
        updates
    }

    pub(crate) fn finish_turn(
        &mut self,
        message: Option<IndexedMessage>,
        error: Option<GatewayError>,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        let last = self.turns.last_mut()?;
        let turn_id = last.turn_id;
        if let Some(message) = message {
            apply_indexed_message(last, message, &mut self.next_part_id);
        }
        last.error = error;
        last.finish_reasoning(None);
        self.streaming = None;
        Some(self.publish(TranscriptEvent::TurnReplaced { turn_id }, cx))
    }

    #[cfg(test)]
    pub(crate) fn push_canonical_turn(
        &mut self,
        message: LlmMessage,
        cx: &mut Context<Self>,
    ) -> TranscriptUpdate {
        let turn = Turn::from_llm(
            message,
            allocate_turn_id(&mut self.next_turn_id),
            &mut self.next_part_id,
        );
        let turn_id = turn.turn_id;
        self.turns.push(turn);
        self.publish(
            TranscriptEvent::TailAppended {
                turn_ids: vec![turn_id],
            },
            cx,
        )
    }

    #[cfg(test)]
    pub(crate) fn push_empty_turn(
        &mut self,
        role: Role,
        cx: &mut Context<Self>,
    ) -> TranscriptUpdate {
        let turn_id = allocate_turn_id(&mut self.next_turn_id);
        self.turns.push(Turn::empty(role, turn_id));
        self.publish(
            TranscriptEvent::TailAppended {
                turn_ids: vec![turn_id],
            },
            cx,
        )
    }

    fn adopt_turns(&mut self, turns: Vec<Turn>) -> Vec<Turn> {
        turns
            .into_iter()
            .map(|mut turn| {
                turn.turn_id = allocate_turn_id(&mut self.next_turn_id);
                for part in &mut turn.parts {
                    part.part_id = allocate_part_id(&mut self.next_part_id);
                }
                turn
            })
            .collect()
    }

    fn apply_stream_event(
        &mut self,
        event: &ConversationStreamEvent,
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptUpdate> {
        match event {
            ConversationStreamEvent::TextStarted { content_index, id } => self
                .start_text(*content_index, id.clone(), cx)
                .into_iter()
                .collect(),
            ConversationStreamEvent::TextDelta {
                content_index,
                id,
                delta,
            } => self.append_text(*content_index, id, delta, cx),
            ConversationStreamEvent::TextFinished {
                content_index,
                id,
                replay,
            } => self
                .finish_text(*content_index, id, replay.clone(), cx)
                .into_iter()
                .collect(),
            ConversationStreamEvent::ReasoningStarted { content_index, id } => self
                .start_reasoning(*content_index, id.clone(), cx)
                .into_iter()
                .collect(),
            ConversationStreamEvent::ReasoningDelta {
                content_index,
                id,
                delta,
            } => self.append_reasoning(*content_index, id, delta, cx),
            ConversationStreamEvent::ReasoningFinished {
                content_index,
                id,
                replay,
            } => self
                .finish_reasoning_block(*content_index, id, replay.clone(), cx)
                .into_iter()
                .collect(),
            ConversationStreamEvent::ReasoningSnapshotUpdated {
                content_index,
                id,
                reasoning,
            } => self
                .update_reasoning_snapshot(*content_index, id, reasoning.clone(), cx)
                .into_iter()
                .collect(),
            ConversationStreamEvent::ToolCallStarted {
                content_index,
                index,
                id,
                name,
            } => self
                .start_tool_call(*content_index, *index, id.clone(), name.clone(), cx)
                .into_iter()
                .collect(),
            ConversationStreamEvent::ToolCallFinished {
                content_index,
                index,
                tool_call,
            } => self
                .finish_tool_call(*content_index, *index, (**tool_call).clone(), cx)
                .into_iter()
                .collect(),
        }
    }

    fn insert_stream_part(
        &mut self,
        content_index: usize,
        stream_id: &str,
        present: fn(&Turn, usize, &str) -> bool,
        source: PartSource,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        let (turn_id, part_id) = {
            let last = self.turns.last_mut()?;
            if present(last, content_index, stream_id) {
                return None;
            }
            let part_id = allocate_part_id(&mut self.next_part_id);
            last.parts
                .push(Part::new(part_id, content_index, source, false));
            last.parts.sort_by_key(|part| part.content_index);
            (last.turn_id, part_id)
        };
        self.streaming = Some((turn_id, part_id));
        Some(self.publish(TranscriptEvent::PartInserted { turn_id, part_id }, cx))
    }

    fn append_stream_part(
        &mut self,
        kind: StreamKind,
        content_index: usize,
        stream_id: &str,
        delta: &str,
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptUpdate> {
        if delta.is_empty() {
            return Vec::new();
        }
        let mut updates = Vec::new();
        let missing = self.turns.last().is_none_or(|turn| {
            !turn
                .parts
                .iter()
                .any(|part| kind.matches(part, content_index, stream_id))
        });
        if missing {
            updates.extend(self.insert_stream_part(
                content_index,
                stream_id,
                kind.present(),
                kind.start_source(stream_id.to_string()),
                cx,
            ));
        }
        let (turn_id, part_id) = {
            let Some(last) = self.turns.last_mut() else {
                return updates;
            };
            let Some(part) = last
                .parts
                .iter_mut()
                .find(|part| kind.matches(part, content_index, stream_id))
            else {
                return updates;
            };
            if part.finished {
                return updates;
            }
            kind.append_delta(&mut part.source, delta);
            (last.turn_id, part.part_id)
        };
        self.streaming = Some((turn_id, part_id));
        updates.push(self.publish(
            TranscriptEvent::PartChanged {
                turn_id,
                part_id,
                change: PartChange::Append,
                delta: delta.to_string().into(),
            },
            cx,
        ));
        updates
    }

    fn finish_stream_part(
        &mut self,
        kind: StreamKind,
        content_index: usize,
        stream_id: &str,
        replay: Option<ProviderMetadata>,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        let (turn_id, part_id) = {
            let last = self.turns.last_mut()?;
            let part = last
                .parts
                .iter_mut()
                .find(|part| kind.matches(part, content_index, stream_id))?;
            if let Some(replay) = replay {
                kind.set_replay(&mut part.source, replay);
            }
            part.finished = true;
            (last.turn_id, part.part_id)
        };
        Some(self.publish(
            TranscriptEvent::PartChanged {
                turn_id,
                part_id,
                change: PartChange::Finished,
                delta: SharedString::default(),
            },
            cx,
        ))
    }

    fn start_text(
        &mut self,
        content_index: usize,
        id: String,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        self.insert_stream_part(
            content_index,
            &id,
            prose_stream_present,
            PartSource::Prose {
                text: String::new(),
                replay: ProviderMetadata::default(),
                stream_id: id.clone(),
            },
            cx,
        )
    }

    fn append_text(
        &mut self,
        content_index: usize,
        id: &str,
        delta: &str,
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptUpdate> {
        self.append_stream_part(StreamKind::Prose, content_index, id, delta, cx)
    }

    fn finish_text(
        &mut self,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        self.finish_stream_part(StreamKind::Prose, content_index, id, replay, cx)
    }

    fn start_reasoning(
        &mut self,
        content_index: usize,
        id: String,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        self.insert_stream_part(
            content_index,
            &id,
            reasoning_stream_present,
            PartSource::Reasoning {
                reasoning: ReasoningContent {
                    display: String::new(),
                    replay: None,
                },
                stream_id: id.clone(),
            },
            cx,
        )
    }

    fn append_reasoning(
        &mut self,
        content_index: usize,
        id: &str,
        delta: &str,
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptUpdate> {
        self.append_stream_part(StreamKind::Reasoning, content_index, id, delta, cx)
    }

    fn finish_reasoning_block(
        &mut self,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        self.finish_stream_part(StreamKind::Reasoning, content_index, id, replay, cx)
    }

    fn update_reasoning_snapshot(
        &mut self,
        content_index: usize,
        id: &str,
        snapshot: ReasoningContent,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        let (turn_id, part_id) = {
            let last = self.turns.last_mut()?;
            let part = last
                .parts
                .iter_mut()
                .find(|part| part.matches_reasoning(content_index, id))?;
            if !part.finished {
                return None;
            }
            if let PartSource::Reasoning { reasoning, .. } = &mut part.source {
                *reasoning = snapshot;
            }
            (last.turn_id, part.part_id)
        };
        Some(self.publish(
            TranscriptEvent::PartChanged {
                turn_id,
                part_id,
                change: PartChange::Replace,
                delta: SharedString::default(),
            },
            cx,
        ))
    }

    fn start_tool_call(
        &mut self,
        content_index: usize,
        index: usize,
        id: String,
        name: String,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        self.insert_stream_part(
            content_index,
            &id,
            tool_call_present,
            PartSource::ToolCall {
                index,
                id: id.clone(),
                name,
                tool_call: None,
            },
            cx,
        )
    }

    fn finish_tool_call(
        &mut self,
        content_index: usize,
        index: usize,
        tool_call: ToolCall,
        cx: &mut Context<Self>,
    ) -> Option<TranscriptUpdate> {
        let last = self.turns.last_mut()?;
        let turn_id = last.turn_id;
        if let Some(part) = last
            .parts
            .iter_mut()
            .find(|part| part.matches_tool_call_index(content_index, index))
        {
            if let PartSource::ToolCall {
                id,
                name,
                tool_call: current,
                ..
            } = &mut part.source
            {
                *id = tool_call.id.clone();
                *name = tool_call.name.clone();
                *current = Some(tool_call);
            }
            part.finished = true;
            let part_id = part.part_id;
            return Some(self.publish(
                TranscriptEvent::PartChanged {
                    turn_id,
                    part_id,
                    change: PartChange::Finished,
                    delta: SharedString::default(),
                },
                cx,
            ));
        }
        let part_id = allocate_part_id(&mut self.next_part_id);
        last.parts.push(Part::new(
            part_id,
            content_index,
            PartSource::ToolCall {
                index,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                tool_call: Some(tool_call),
            },
            true,
        ));
        last.parts.sort_by_key(|part| part.content_index);
        Some(self.publish(TranscriptEvent::PartInserted { turn_id, part_id }, cx))
    }

    fn publish(&mut self, event: TranscriptEvent, cx: &mut Context<Self>) -> TranscriptUpdate {
        self.revision = self.revision.saturating_add(1);
        let update = TranscriptUpdate {
            snapshot: self.snapshot(),
            event,
        };
        // Emit after this entity unlocks so in-lock writers can apply `update`
        // immediately. Subscribers that already observed `revision` skip the
        // deferred copy; others apply it once.
        let transcript = cx.weak_entity();
        let emitted = update.clone();
        cx.defer(move |cx| {
            let _ = transcript.update(cx, |_, cx| cx.emit(emitted));
        });
        update
    }
}

impl EventEmitter<TranscriptUpdate> for Transcript {}

/// The two incrementally-streamed part families share one insert / append /
/// finish lifecycle and differ only in source shape and matching predicate.
#[derive(Clone, Copy)]
enum StreamKind {
    Prose,
    Reasoning,
}

impl StreamKind {
    fn present(self) -> fn(&Turn, usize, &str) -> bool {
        match self {
            Self::Prose => prose_stream_present,
            Self::Reasoning => reasoning_stream_present,
        }
    }

    fn matches(self, part: &Part, content_index: usize, stream_id: &str) -> bool {
        match self {
            Self::Prose => part.matches_text(content_index, stream_id),
            Self::Reasoning => part.matches_reasoning(content_index, stream_id),
        }
    }

    fn start_source(self, stream_id: String) -> PartSource {
        match self {
            Self::Prose => PartSource::Prose {
                text: String::new(),
                replay: ProviderMetadata::default(),
                stream_id,
            },
            Self::Reasoning => PartSource::Reasoning {
                reasoning: ReasoningContent {
                    display: String::new(),
                    replay: None,
                },
                stream_id,
            },
        }
    }

    fn append_delta(self, source: &mut PartSource, delta: &str) {
        match (self, source) {
            (Self::Prose, PartSource::Prose { text, .. }) => text.push_str(delta),
            (Self::Reasoning, PartSource::Reasoning { reasoning, .. }) => {
                reasoning.display.push_str(delta);
            }
            _ => {}
        }
    }

    fn set_replay(self, source: &mut PartSource, replay: ProviderMetadata) {
        match (self, source) {
            (
                Self::Prose,
                PartSource::Prose {
                    replay: current, ..
                },
            ) => *current = replay,
            (Self::Reasoning, PartSource::Reasoning { reasoning, .. }) => {
                reasoning.replay = Some(replay);
            }
            _ => {}
        }
    }
}

/// Prose parts match on `content_index` plus the provider stream id; tool
/// calls carry no stable stream id and match on `content_index` alone.
fn prose_stream_present(turn: &Turn, content_index: usize, stream_id: &str) -> bool {
    turn.parts
        .iter()
        .any(|part| part.matches_text(content_index, stream_id))
}

fn reasoning_stream_present(turn: &Turn, content_index: usize, stream_id: &str) -> bool {
    turn.parts
        .iter()
        .any(|part| part.matches_reasoning(content_index, stream_id))
}

fn tool_call_present(turn: &Turn, content_index: usize, _: &str) -> bool {
    turn.parts
        .iter()
        .any(|part| part.matches_tool_call(content_index))
}

#[must_use]
pub(crate) fn derive_title(text: &str) -> SharedString {
    let mut cleaned = text.replace('\n', " ");
    if cleaned.chars().count() > 40 {
        cleaned = cleaned.chars().take(37).collect::<String>() + "...";
    }
    if cleaned.trim().is_empty() {
        t!("chat.default_title").to_string().into()
    } else {
        cleaned.into()
    }
}

#[must_use]
pub(crate) fn title_from_turns(turns: &[Turn]) -> Option<SharedString> {
    turns
        .iter()
        .find(|turn| turn.role == Role::User)
        .and_then(|turn| {
            turn.parts.iter().find_map(|part| match &part.source {
                PartSource::Prose { text, .. } if !text.trim().is_empty() => {
                    Some(derive_title(text))
                }
                _ => None,
            })
        })
}

#[must_use]
pub(crate) fn title_from_llm_messages<'a>(
    messages: impl Iterator<Item = &'a LlmMessage>,
) -> Option<SharedString> {
    use crate::llm::ContentBlock;
    messages
        .filter(|message| message.role == crate::llm::Role::User)
        .find_map(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                    Some(derive_title(text))
                }
                _ => None,
            })
        })
}

#[must_use]
pub(crate) fn copyable_text(turn: &Turn) -> SharedString {
    turn.parts
        .iter()
        .filter_map(|part| match &part.source {
            PartSource::Prose { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .fold(String::new(), |mut text, part| {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
            text
        })
        .into()
}

#[must_use]
pub(crate) fn stream_ended(turn: &Turn) -> bool {
    turn.parts.iter().all(|part| match part.kind() {
        PartKind::Prose | PartKind::Reasoning => part.finished,
        PartKind::ToolCall | PartKind::ToolResult => true,
    })
}

#[must_use]
pub(crate) fn has_copyable_text(turn: &Turn) -> bool {
    turn.parts.iter().any(|part| {
        matches!(
            &part.source,
            PartSource::Prose { text, .. } if !text.trim().is_empty()
        )
    })
}

#[must_use]
pub(crate) fn title_from_resolved_state(
    state: &crate::session::ResolvedSessionState,
) -> Option<SharedString> {
    title_from_llm_messages(state.messages.iter().map(|message| &message.message))
}
