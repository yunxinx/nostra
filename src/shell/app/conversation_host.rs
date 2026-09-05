//! Shared ownership and immutable projection for workspace conversations.
//!
//! Each `Conversation` owns its durable parts (runtime, transcript,
//! composer) plus an *optional* view. Warm conversations keep a built
//! `ChatView`; cold ones have released it and keep only the saved row
//! projection (heights + disclosure) and scroll anchor, restored when the
//! conversation is selected again (R8).

use std::collections::HashMap;
use std::time::Instant;

use gpui::{App, Entity, ListOffset, SharedString, Subscription};

use crate::chat::ChatView;
use crate::chat::conversation_runtime::ConversationRuntime;
use crate::chat::projection::RowProjection;
use crate::chat::transcript::Transcript;
use crate::llm::ModelSelection;
use crate::providers;
use crate::session::SessionId;
use crate::ui::reference_picker::{ChatReferenceComposer, ComposerStatus};

/// Stable identity for one hosted conversation. Independent of the view
/// entity, so workspace routing survives a later optional view.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ConversationId(u64);

impl ConversationId {
    #[must_use]
    pub(crate) const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Catalog last-model, or a restored session model. `Conversation` owns this
/// value and syncs it to the view; do not seed from `ChatView::selection`.
pub(super) fn seed_conversation_selection(
    restored: Option<ModelSelection>,
    cx: &mut App,
) -> Option<ModelSelection> {
    restored.or_else(|| providers::last_selection_from(&providers::ensure_global(cx).snapshot()))
}

pub(super) fn conversation_generating(runtime: &Entity<ConversationRuntime>, cx: &App) -> bool {
    runtime.read(cx).snapshot().is_generating()
}

/// Conversations kept warm: the active one plus this many recently used.
pub(super) const WARM_CONVERSATIONS: usize = 3;

/// Idle duration after which a non-warm conversation drops its view.
pub(super) const IDLE_COLD_AFTER: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// How often the workspace checks for idle cold candidates.
pub(super) const IDLE_COLD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) struct Conversation<M> {
    pub(super) id: ConversationId,
    pub(super) runtime: Entity<ConversationRuntime>,
    pub(super) transcript: Entity<Transcript>,
    pub(super) composer: Entity<ChatReferenceComposer>,
    /// Rebuild inputs for warming a cold conversation back up.
    pub(super) composer_status: std::rc::Rc<std::cell::Cell<ComposerStatus>>,
    pub(super) references_enabled: bool,
    /// `None` while the conversation is cold (R8).
    pub(super) view: Option<Entity<ChatView>>,
    /// Saved by `cool`: the row projection with height cache and disclosure.
    pub(super) projection: Option<RowProjection>,
    /// Saved by `cool`: where the reader was.
    pub(super) scroll_anchor: Option<ListOffset>,
    pub(super) metadata: M,
    pub(super) title: SharedString,
    pub(super) selection: Option<ModelSelection>,
    pub(super) session_id: Option<SessionId>,
    pub(super) is_generating: bool,
    pub(super) last_active_at: Instant,
    pub(super) _subscriptions: [Subscription; 2],
}

impl<M> Conversation<M> {
    /// Update the sidebar title from the transcript. Returns whether it changed.
    pub(super) fn refresh_title(&mut self, cx: &App) -> bool {
        let Some(title) = self.transcript.read(cx).title() else {
            return false;
        };
        if self.title == title {
            return false;
        }
        self.title = title;
        true
    }

    /// Parts needed to rebuild the view on warm.
    pub(super) fn parts(&self) -> crate::chat::ChatViewParts {
        crate::chat::ChatViewParts {
            runtime: self.runtime.clone(),
            transcript: self.transcript.clone(),
            composer: self.composer.clone(),
            composer_status: self.composer_status.clone(),
            references_enabled: self.references_enabled,
        }
    }

    /// Request deletion through the warm view, or straight through the
    /// runtime when the conversation is cold: a cold conversation must stay
    /// deletable exactly like a warm one (R8).
    pub(super) fn request_delete(&self, cx: &mut App) -> crate::chat::ChatDeleteRequest {
        match &self.view {
            Some(view) => view.update(cx, |chat, cx| chat.request_delete(cx)),
            None => self
                .runtime
                .update(cx, |runtime, cx| runtime.request_delete(cx)),
        }
    }

    /// Finalize persistence before shutdown for warm and cold conversations
    /// alike: only the warm path has a view to apply the snapshot to, but the
    /// runtime work is identical.
    pub(super) fn prepare_for_shutdown(&self, cx: &mut App) {
        match &self.view {
            Some(view) => view.update(cx, |chat, cx| chat.prepare_for_shutdown(cx)),
            None => self
                .runtime
                .update(cx, |runtime, cx| runtime.prepare_for_shutdown(cx)),
        }
    }

    /// Close the conversation scope through the warm view, or through the
    /// runtime when the view is cold.
    pub(super) fn close_scope(&self, cx: &mut App) {
        match &self.view {
            Some(view) => view.update(cx, |chat, cx| chat.close_scope(cx)),
            None => {
                self.runtime
                    .update(cx, |runtime, cx| runtime.close_scope(cx));
            }
        }
    }

    /// Whether the runtime still has in-flight work. Read from the runtime
    /// (not the view) so a cold conversation that is still generating keeps
    /// blocking project deletion.
    pub(super) fn has_in_flight_work(&self, cx: &App) -> bool {
        self.runtime.read(cx).snapshot().has_in_flight_work()
    }
}

#[derive(Clone)]
pub(super) struct ConversationSnapshot<M> {
    id: ConversationId,
    view: Option<Entity<ChatView>>,
    metadata: M,
    title: SharedString,
    selection: Option<ModelSelection>,
    session_id: Option<SessionId>,
    is_generating: bool,
}

impl<M> ConversationSnapshot<M> {
    pub(super) fn view(&self) -> Option<Entity<ChatView>> {
        self.view.clone()
    }

    pub(super) fn metadata(&self) -> &M {
        &self.metadata
    }

    pub(super) fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub(super) fn selection(&self) -> Option<ModelSelection> {
        self.selection.clone()
    }

    pub(super) fn session_id(&self) -> Option<SessionId> {
        self.session_id.clone()
    }

    pub(super) fn is_generating(&self) -> bool {
        self.is_generating
    }

    pub(super) fn id(&self) -> ConversationId {
        self.id
    }
}

#[derive(Clone)]
pub(super) struct ConversationHostSnapshot<M> {
    conversations: Vec<ConversationSnapshot<M>>,
    opened_session_index: HashMap<SessionId, ConversationId>,
    active: Option<ConversationId>,
}

impl<M> ConversationHostSnapshot<M> {
    pub(super) fn empty() -> Self {
        Self {
            conversations: Vec::new(),
            opened_session_index: HashMap::new(),
            active: None,
        }
    }

    pub(super) fn conversations(&self) -> &[ConversationSnapshot<M>] {
        &self.conversations
    }

    pub(super) fn active(&self) -> Option<ConversationId> {
        self.active
    }

    pub(super) fn active_view(&self) -> Option<Entity<ChatView>> {
        self.active
            .and_then(|target| self.conversation(target))
            .and_then(ConversationSnapshot::view)
    }

    pub(super) fn active_session_id(&self) -> Option<SessionId> {
        self.active
            .and_then(|target| self.conversation(target))
            .and_then(ConversationSnapshot::session_id)
    }

    pub(super) fn conversation(&self, target: ConversationId) -> Option<&ConversationSnapshot<M>> {
        self.conversations
            .iter()
            .find(|conversation| conversation.id == target)
    }

    pub(super) fn opened_target(&self, session_id: &SessionId) -> Option<ConversationId> {
        self.opened_session_index.get(session_id).copied()
    }

    #[cfg(test)]
    pub(super) fn opened_session_index(&self) -> &HashMap<SessionId, ConversationId> {
        &self.opened_session_index
    }
}

pub(super) struct RemovedConversation<M> {
    pub(super) conversation: Conversation<M>,
    pub(super) index: usize,
    pub(super) was_active: bool,
}

pub(super) struct ConversationHost<M> {
    conversations: Vec<Conversation<M>>,
    opened_session_index: HashMap<SessionId, ConversationId>,
    active: Option<ConversationId>,
    next_id: u64,
}

impl<M> ConversationHost<M> {
    pub(super) fn new() -> Self {
        Self {
            conversations: Vec::new(),
            opened_session_index: HashMap::new(),
            active: None,
            next_id: 1,
        }
    }

    pub(super) fn allocate_id(&mut self) -> ConversationId {
        let id = ConversationId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    pub(super) fn conversations(&self) -> &[Conversation<M>] {
        &self.conversations
    }

    pub(super) fn conversations_mut(&mut self) -> &mut [Conversation<M>] {
        &mut self.conversations
    }

    pub(super) fn conversation(&self, target: ConversationId) -> Option<&Conversation<M>> {
        self.conversations
            .iter()
            .find(|conversation| conversation.id == target)
    }

    pub(super) fn conversation_mut(
        &mut self,
        target: ConversationId,
    ) -> Option<&mut Conversation<M>> {
        self.conversations
            .iter_mut()
            .find(|conversation| conversation.id == target)
    }

    pub(super) fn conversation_index(&self, target: ConversationId) -> Option<usize> {
        self.conversations
            .iter()
            .position(|conversation| conversation.id == target)
    }

    pub(super) fn active(&self) -> Option<ConversationId> {
        self.active
    }

    pub(super) fn opened_target(&self, session_id: &SessionId) -> Option<ConversationId> {
        self.opened_session_index.get(session_id).copied()
    }

    pub(super) fn push_and_activate(&mut self, conversation: Conversation<M>) -> ConversationId {
        let target = conversation.id;
        if let Some(session_id) = &conversation.session_id {
            self.opened_session_index.insert(session_id.clone(), target);
        }
        self.conversations.push(conversation);
        self.active = Some(target);
        target
    }

    pub(super) fn bind_session(&mut self, target: ConversationId, session_id: SessionId) -> bool {
        let Some(conversation) = self.conversation_mut(target) else {
            return false;
        };
        if conversation.session_id.as_ref() == Some(&session_id) {
            return false;
        }
        conversation.session_id = Some(session_id.clone());
        self.opened_session_index.insert(session_id, target);
        true
    }

    pub(super) fn select_target(&mut self, target: ConversationId) -> bool {
        if self.active == Some(target) || self.conversation(target).is_none() {
            return false;
        }
        self.active = Some(target);
        true
    }

    pub(super) fn set_active(&mut self, target: ConversationId) -> bool {
        if self.conversation(target).is_none() {
            return false;
        }
        let changed = self.active != Some(target);
        self.active = Some(target);
        changed
    }

    pub(super) fn remove(&mut self, target: ConversationId) -> Option<RemovedConversation<M>> {
        let index = self.conversation_index(target)?;
        let was_active = self.active == Some(target);
        let conversation = self.conversations.remove(index);
        if let Some(session_id) = &conversation.session_id {
            self.opened_session_index.remove(session_id);
        }
        if was_active {
            self.active = None;
        }
        Some(RemovedConversation {
            conversation,
            index,
            was_active,
        })
    }

    pub(super) fn select_neighbor(&mut self, removed_index: usize) {
        self.active = self
            .conversations
            .get(removed_index)
            .map(|conversation| conversation.id)
            .or_else(|| {
                removed_index.checked_sub(1).and_then(|index| {
                    self.conversations
                        .get(index)
                        .map(|conversation| conversation.id)
                })
            });
    }

    pub(super) fn retain(
        &mut self,
        mut keep: impl FnMut(&Conversation<M>) -> bool,
    ) -> Vec<Conversation<M>> {
        let mut retained = Vec::with_capacity(self.conversations.len());
        let mut removed = Vec::new();
        for conversation in self.conversations.drain(..) {
            if keep(&conversation) {
                retained.push(conversation);
            } else {
                if let Some(session_id) = &conversation.session_id {
                    self.opened_session_index.remove(session_id);
                }
                removed.push(conversation);
            }
        }
        self.conversations = retained;
        if self
            .active
            .is_some_and(|target| removed.iter().any(|conversation| conversation.id == target))
        {
            self.active = None;
        }
        removed
    }
}

impl<M: Clone> ConversationHost<M> {
    pub(super) fn snapshot(&self) -> ConversationHostSnapshot<M> {
        ConversationHostSnapshot {
            conversations: self
                .conversations
                .iter()
                .map(|conversation| ConversationSnapshot {
                    id: conversation.id,
                    view: conversation.view.clone(),
                    metadata: conversation.metadata.clone(),
                    title: conversation.title.clone(),
                    selection: conversation.selection.clone(),
                    session_id: conversation.session_id.clone(),
                    is_generating: conversation.is_generating,
                })
                .collect(),
            opened_session_index: self.opened_session_index.clone(),
            active: self.active,
        }
    }
}

impl<M> ConversationHost<M> {
    /// Mark a conversation as used right now (touch on selection).
    pub(super) fn touch(&mut self, id: ConversationId) {
        if let Some(conversation) = self.conversation_mut(id) {
            conversation.last_active_at = Instant::now();
        }
    }

    /// Conversations that may drop their view: not active, not among the
    /// warm set (active + the most recently used), and idle past `idle`.
    pub(super) fn cold_candidates(
        &self,
        now: Instant,
        warm_limit: usize,
        idle: std::time::Duration,
    ) -> Vec<ConversationId> {
        let mut by_recency: Vec<(ConversationId, Instant)> = self
            .conversations
            .iter()
            .map(|conversation| (conversation.id, conversation.last_active_at))
            .collect();
        by_recency.sort_by_key(|&(_, at)| std::cmp::Reverse(at));
        let warm: std::collections::HashSet<ConversationId> = by_recency
            .iter()
            .take(warm_limit.max(1))
            .map(|(id, _)| *id)
            .chain(self.active)
            .collect();
        self.conversations
            .iter()
            .filter(|conversation| {
                !warm.contains(&conversation.id)
                    && now.duration_since(conversation.last_active_at) >= idle
                    && conversation.view.is_some()
            })
            .map(|conversation| conversation.id)
            .collect()
    }

    /// Drop a conversation's view, keeping runtime, transcript, composer,
    /// selection, and the saved projection + scroll anchor. Streaming
    /// continues through the workspace-owned subscriptions (R8).
    pub(super) fn cool(&mut self, id: ConversationId, cx: &mut App) -> bool {
        let Some(conversation) = self.conversation_mut(id) else {
            return false;
        };
        let Some(view) = conversation.view.take() else {
            return false;
        };
        let (projection, anchor) = view.update(cx, |chat, cx| chat.cool_down(cx));
        conversation.projection = Some(projection);
        conversation.scroll_anchor = anchor;
        true
    }

    /// Whether the conversation currently has no view.
    pub(super) fn is_cold(&self, id: ConversationId) -> bool {
        self.conversation(id)
            .is_some_and(|conversation| conversation.view.is_none())
    }

    /// Rebuild a cold conversation's view with the saved projection and
    /// scroll anchor.
    pub(super) fn warm(
        &mut self,
        id: ConversationId,
        build: impl FnOnce(
            crate::chat::ChatViewParts,
            Option<(RowProjection, Option<ListOffset>)>,
        ) -> Entity<ChatView>,
    ) -> bool {
        let Some(conversation) = self.conversation_mut(id) else {
            return false;
        };
        if conversation.view.is_some() {
            return false;
        }
        let restore = conversation
            .projection
            .take()
            .map(|projection| (projection, conversation.scroll_anchor.take()));
        let view = build(conversation.parts(), restore);
        if let Some(conversation) = self.conversation_mut(id) {
            conversation.view = Some(view);
            conversation.last_active_at = Instant::now();
        }
        true
    }
}
