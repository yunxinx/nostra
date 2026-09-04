//! Shared ownership and immutable projection for workspace conversations.

use std::collections::HashMap;

use gpui::{App, Entity, SharedString, Subscription};

use crate::chat::ChatView;
use crate::chat::conversation_runtime::ConversationRuntime;
use crate::chat::transcript::Transcript;
use crate::llm::ModelSelection;
use crate::providers;
use crate::session::SessionId;
use crate::ui::reference_picker::ChatReferenceComposer;

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

pub(super) struct Conversation<M> {
    pub(super) id: ConversationId,
    pub(super) runtime: Entity<ConversationRuntime>,
    pub(super) transcript: Entity<Transcript>,
    pub(super) composer: Entity<ChatReferenceComposer>,
    pub(super) view: Entity<ChatView>,
    pub(super) metadata: M,
    pub(super) title: SharedString,
    pub(super) selection: Option<ModelSelection>,
    pub(super) session_id: Option<SessionId>,
    pub(super) is_generating: bool,
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
}

#[derive(Clone)]
pub(super) struct ConversationSnapshot<M> {
    id: ConversationId,
    view: Entity<ChatView>,
    metadata: M,
    title: SharedString,
    selection: Option<ModelSelection>,
    session_id: Option<SessionId>,
    is_generating: bool,
}

impl<M> ConversationSnapshot<M> {
    pub(super) fn view(&self) -> Entity<ChatView> {
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
            .and_then(|target| self.conversation(target).map(ConversationSnapshot::view))
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
