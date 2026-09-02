//! Chat workspace instance state and lifecycle.

use std::collections::{HashMap, HashSet};

use gpui::{Context, Entity, Subscription, Task, Window};
use gpui_component::{WindowExt as _, notification::NotificationType};
use rust_i18n::t;

use crate::chat::{
    ChatDeleteRequest, ChatEvent, ChatView, create_conversation_runtime, derive_title_from_state,
};
use crate::llm::ModelSelection;
use crate::preferences::PreferenceHandle;
use crate::runtime::RuntimeServices;
use crate::session::{
    ChatSessionCatalogController, FavoriteChange, MAX_FAVORITES, ResolvedSessionState,
    SessionEntryKind, SessionId, SessionLifecycleStore,
};
use crate::ui::inline_delete_confirmation::InlineDeleteConfirmationHandle;

use super::{
    conversation_host::{Conversation, ConversationHost, ConversationHostSnapshot},
    history_groups::HistorySectionKind,
    history_sidebar::ChatHistorySidebar,
};

/// Identity of a Chat sidebar row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ChatTarget {
    View(gpui::EntityId),
    Session(SessionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SelectionRequest(u64);

#[derive(Default)]
pub(super) struct SelectionEpoch {
    current: u64,
}

impl SelectionEpoch {
    pub(super) fn begin(&mut self) -> SelectionRequest {
        self.current = self.current.wrapping_add(1);
        SelectionRequest(self.current)
    }

    pub(super) fn invalidate(&mut self) {
        self.current = self.current.wrapping_add(1);
    }

    pub(super) fn is_current(&self, request: SelectionRequest) -> bool {
        request.0 == self.current
    }
}

pub(super) type ChatConversationSnapshot = super::conversation_host::ConversationSnapshot<()>;

/// Where the pointer was when the history list last reordered beneath it.
///
/// Rows take hover from the pointer's position, recomputed every frame, so a
/// list that reorders under a stationary pointer would hand the highlight to
/// whichever row slid underneath — a row the user never pointed at.  While the
/// pointer has not moved from `at`, only `owner` (the row the user acted on)
/// may take hover; it is the one row the pointer really is on.
#[derive(Clone)]
struct ParkedPointer {
    at: gpui::Point<gpui::Pixels>,
    owner: Option<ChatTarget>,
}

#[derive(Clone)]
pub(super) struct ChatWorkspaceSnapshot {
    conversations: ConversationHostSnapshot<()>,
    history: ChatHistorySidebar,
    confirming: Option<ChatTarget>,
    collapsed_history_sections: HashSet<HistorySectionKind>,
    delete_confirmation: InlineDeleteConfirmationHandle,
    parked_pointer: Option<ParkedPointer>,
}

impl ChatWorkspaceSnapshot {
    pub(super) fn empty() -> Self {
        Self {
            conversations: ConversationHostSnapshot::empty(),
            history: ChatHistorySidebar::new(),
            confirming: None,
            collapsed_history_sections: HashSet::new(),
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            parked_pointer: None,
        }
    }

    pub(super) fn conversations(&self) -> &[ChatConversationSnapshot] {
        self.conversations.conversations()
    }

    pub(super) fn active(&self) -> Option<gpui::EntityId> {
        self.conversations.active()
    }

    pub(super) fn active_view(&self) -> Option<Entity<ChatView>> {
        self.conversations.active_view()
    }

    pub(super) fn active_session_id(&self) -> Option<SessionId> {
        self.conversations.active_session_id()
    }

    pub(super) fn conversation(&self, target: gpui::EntityId) -> Option<&ChatConversationSnapshot> {
        self.conversations.conversation(target)
    }

    pub(super) fn opened_target(&self, session_id: &SessionId) -> Option<gpui::EntityId> {
        self.conversations.opened_target(session_id)
    }

    #[cfg(test)]
    pub(super) fn opened_session_index(
        &self,
    ) -> &std::collections::HashMap<SessionId, gpui::EntityId> {
        self.conversations.opened_session_index()
    }

    pub(super) fn history(&self) -> &ChatHistorySidebar {
        &self.history
    }

    pub(super) fn confirming(&self) -> Option<&ChatTarget> {
        self.confirming.as_ref()
    }

    pub(super) fn delete_confirmation(&self) -> InlineDeleteConfirmationHandle {
        self.delete_confirmation.clone()
    }

    pub(super) fn history_section_open(&self, kind: HistorySectionKind) -> bool {
        !self.collapsed_history_sections.contains(&kind)
    }

    /// Whether `target`'s row may take hover with the pointer at `pointer`.
    /// See [`ParkedPointer`].
    pub(super) fn row_takes_hover(
        &self,
        target: &ChatTarget,
        pointer: gpui::Point<gpui::Pixels>,
    ) -> bool {
        match &self.parked_pointer {
            Some(parked) if parked.at == pointer => parked.owner.as_ref() == Some(target),
            _ => true,
        }
    }

    /// The position hover is frozen at, if the pointer has not left it yet.
    pub(super) fn parked_pointer(&self) -> Option<gpui::Point<gpui::Pixels>> {
        self.parked_pointer.as_ref().map(|parked| parked.at)
    }
}

struct SessionRestore {
    request: SelectionRequest,
    session_id: SessionId,
    state: ResolvedSessionState,
}

pub(super) struct ChatWorkspace {
    pub(super) conversations: ConversationHost<()>,
    pub(super) selection_epoch: SelectionEpoch,
    pub(super) _selection_task: Option<Task<()>>,
    pub(super) history: ChatHistorySidebar,
    pub(super) _catalog_initial_task: Option<Task<()>>,
    pub(super) _catalog_load_more_task: Option<Task<()>>,
    /// Keyed by session: a per-session slot cancels a superseded write for
    /// *that* row without dropping another row's in-flight one.  A single slot
    /// would let a second star silently cancel the first one's append.
    pub(super) _summary_refresh_tasks: HashMap<SessionId, Task<()>>,
    pub(super) _history_delete_task: Option<Task<()>>,
    pub(super) _favorite_tasks: HashMap<SessionId, Task<()>>,
    pub(super) startup_restore_attempted: bool,
    pub(super) confirming: Option<ChatTarget>,
    pub(super) collapsed_history_sections: HashSet<HistorySectionKind>,
    pub(super) delete_confirmation: InlineDeleteConfirmationHandle,
    parked_pointer: Option<ParkedPointer>,
    pub(super) runtime_services: RuntimeServices,
    pub(super) preference_handle: PreferenceHandle,
    pub(super) snapshot: ChatWorkspaceSnapshot,
}

impl ChatWorkspace {
    pub(super) fn new(
        services: RuntimeServices,
        preference_handle: PreferenceHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut workspace = Self {
            conversations: ConversationHost::new(),
            selection_epoch: SelectionEpoch::default(),
            _selection_task: None,
            history: ChatHistorySidebar::new(),
            _catalog_initial_task: None,
            _catalog_load_more_task: None,
            _summary_refresh_tasks: HashMap::new(),
            _history_delete_task: None,
            _favorite_tasks: HashMap::new(),
            startup_restore_attempted: false,
            confirming: None,
            collapsed_history_sections: HashSet::new(),
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            parked_pointer: None,
            runtime_services: services,
            preference_handle,
            snapshot: ChatWorkspaceSnapshot::empty(),
        };
        workspace.start_catalog_initial_load(window, cx);
        workspace.publish_snapshot();
        workspace
    }

    pub(super) fn snapshot(&self) -> &ChatWorkspaceSnapshot {
        &self.snapshot
    }

    fn publish_snapshot(&mut self) {
        self.snapshot = ChatWorkspaceSnapshot {
            conversations: self.conversations.snapshot(),
            history: self.history.clone(),
            confirming: self.confirming.clone(),
            collapsed_history_sections: self.collapsed_history_sections.clone(),
            delete_confirmation: self.delete_confirmation.clone(),
            parked_pointer: self.parked_pointer.clone(),
        };
    }

    pub(super) fn notify_changed(&mut self, cx: &mut Context<Self>) {
        self.publish_snapshot();
        cx.notify();
    }

    fn create_scope(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<crate::runtime::ConversationScopeHandle> {
        match self.runtime_services.create_conversation_scope() {
            Ok(scope) => Some(scope),
            Err(error) => {
                crate::logging::error(
                    "runtime.scope",
                    format_args!("failed to create conversation scope: {error}"),
                );
                window.push_notification(
                    (
                        NotificationType::Error,
                        t!("chat.error.runtime_unavailable").to_string(),
                    ),
                    cx,
                );
                None
            }
        }
    }

    pub(super) fn prepare_for_shutdown(&mut self, cx: &mut Context<Self>) {
        for conversation in self.conversations.conversations() {
            conversation
                .view
                .update(cx, |chat, cx| chat.prepare_for_shutdown(cx));
        }
    }

    pub(super) fn spawn_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.invalidate_selection_request();
        let title = t!("chat.default_title").to_string().into();
        let Some(scope) = self.create_scope(window, cx) else {
            return;
        };
        let runtime = create_conversation_runtime(
            scope,
            self.runtime_services.session_services().chat_conversation(),
            self.runtime_services.generation_service(),
            cx,
        );
        let view =
            ChatView::view_with_runtime_services(runtime, &self.runtime_services, window, cx);
        let subscription = self.subscribe_conversation(&view, window, cx);
        let selection = view.read(cx).selection();
        let is_generating = view.read(cx).is_generating();
        self.conversations.push_and_activate(Conversation {
            view,
            metadata: (),
            title,
            selection,
            session_id: None,
            is_generating,
            _subscription: subscription,
        });
        self.notify_changed(cx);
    }

    pub(super) fn select_target(&mut self, target: gpui::EntityId, cx: &mut Context<Self>) {
        self.invalidate_selection_request();
        if self.conversations.select_target(target) {
            self.notify_changed(cx);
        }
    }

    pub(super) fn select_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.conversations.opened_target(&session_id) {
            self.select_target(target, cx);
            return;
        }
        let catalog_store = match self.runtime_services.session_services().chat_catalog() {
            Ok(store) => store,
            Err(error) => {
                crate::logging::error(
                    "chat.restore",
                    format_args!("failed to open the chat session catalog: {error}"),
                );
                window.push_notification(
                    (
                        NotificationType::Error,
                        t!("chat.error.runtime_unavailable").to_string(),
                    ),
                    cx,
                );
                return;
            }
        };
        let request = self.begin_selection_request();
        let workspace = cx.entity();
        let window_handle = window.window_handle();
        let task = cx.spawn(async move |_this, cx| {
            let selected_id = session_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut controller = ChatSessionCatalogController::new(catalog_store);
                    controller.load_initial().and_then(|_| {
                        controller
                            .select(&selected_id)
                            .map(|selected| selected.state)
                    })
                })
                .await;
            let state = match result {
                Ok(state) => state,
                Err(error) => {
                    crate::logging::error(
                        "chat.restore",
                        format_args!("failed to load chat session {session_id}: {error}"),
                    );
                    let _ = window_handle.update(cx, |_, window, cx| {
                        window.push_notification(
                            (
                                NotificationType::Error,
                                t!("chat.error.runtime_unavailable").to_string(),
                            ),
                            cx,
                        );
                    });
                    return;
                }
            };
            let _ = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.apply_session_restore(
                        SessionRestore {
                            request,
                            session_id,
                            state,
                        },
                        window,
                        cx,
                    );
                });
            });
        });
        self._selection_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_session_restore(
        &mut self,
        restore: SessionRestore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selection_epoch.is_current(restore.request) {
            return;
        }
        if let Some(target) = self.conversations.opened_target(&restore.session_id) {
            self.conversations.set_active(target);
            self.record_active_session(&restore.session_id, cx);
            self.notify_changed(cx);
            return;
        }
        let title = derive_title_from_state(&restore.state)
            .unwrap_or_else(|| t!("chat.default_title").to_string().into());
        let Some(scope) = self.create_scope(window, cx) else {
            return;
        };
        let runtime = create_conversation_runtime(
            scope,
            self.runtime_services.session_services().chat_conversation(),
            self.runtime_services.generation_service(),
            cx,
        );
        let view =
            ChatView::view_with_runtime_services(runtime, &self.runtime_services, window, cx);
        if let Err(error) = view.update(cx, |chat, cx| {
            chat.restore_from_session(&restore.session_id, &restore.state, cx)
        }) {
            crate::logging::error(
                "chat.restore",
                format_args!(
                    "failed to restore chat session {}: {error}",
                    restore.session_id
                ),
            );
            view.update(cx, |chat, cx| chat.close_scope(cx));
            window.push_notification(
                (
                    NotificationType::Error,
                    t!("chat.error.runtime_unavailable").to_string(),
                ),
                cx,
            );
            return;
        }
        let subscription = self.subscribe_conversation(&view, window, cx);
        self.conversations.push_and_activate(Conversation {
            selection: view.read(cx).selection(),
            is_generating: view.read(cx).is_generating(),
            view,
            metadata: (),
            title,
            session_id: Some(restore.session_id.clone()),
            _subscription: subscription,
        });
        self.record_active_session(&restore.session_id, cx);
        self.notify_changed(cx);
    }

    fn begin_selection_request(&mut self) -> SelectionRequest {
        self._selection_task = None;
        self.selection_epoch.begin()
    }

    fn invalidate_selection_request(&mut self) {
        self._selection_task = None;
        self.selection_epoch.invalidate();
    }

    fn subscribe_conversation(
        &mut self,
        view: &Entity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |workspace, view, event, window, cx| {
            let target = view.entity_id();
            let Some(index) = workspace.conversations.conversation_index(target) else {
                return;
            };
            match event {
                ChatEvent::TitleChanged(title) => {
                    workspace.conversations.conversations_mut()[index].title = title.clone()
                }
                ChatEvent::SelectionChanged(selection) => {
                    workspace.conversations.conversations_mut()[index].selection =
                        Some(selection.clone());
                }
                ChatEvent::SessionBound(session_id) => {
                    if workspace
                        .conversations
                        .bind_session(target, session_id.clone())
                    {
                        workspace.refresh_history_summary(session_id.clone(), cx);
                        workspace.record_active_session(session_id, cx);
                    }
                }
                ChatEvent::StateChanged => {
                    workspace.conversations.conversations_mut()[index].is_generating =
                        view.read(cx).is_generating();
                }
                ChatEvent::DeleteCompleted => {
                    cx.defer_in(window, move |workspace, window, cx| {
                        workspace.remove_conversation(target, window, cx);
                    });
                    return;
                }
            }
            workspace.notify_changed(cx);
        })
    }

    pub(super) fn select_model(
        &mut self,
        selection: ModelSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target) = self.conversations.active() else {
            return false;
        };
        let Some(conversation) = self.conversations.conversation(target) else {
            return false;
        };
        conversation
            .view
            .update(cx, |chat, cx| chat.select_model(selection, cx));
        true
    }

    pub(super) fn request_delete_active(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target) = self.conversations.active() else {
            return false;
        };
        if self.conversations.conversation(target).is_none() {
            return false;
        }
        self.begin_delete_confirmation(ChatTarget::View(target), window, cx);
        true
    }

    pub(super) fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_confirming = self.confirming == Some(ChatTarget::View(target));
        if was_confirming {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
        }
        let Some(conversation) = self.conversations.conversation(target) else {
            if was_confirming {
                self.notify_changed(cx);
            }
            return;
        };
        let request = conversation
            .view
            .update(cx, |chat, cx| chat.request_delete(cx));
        if request == ChatDeleteRequest::RemoveNow {
            self.remove_conversation(target, window, cx);
            return;
        }
        if was_confirming {
            self.notify_changed(cx);
        }
    }

    pub(super) fn remove_conversation(
        &mut self,
        target: gpui::EntityId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active = self.conversations.active() == Some(target);
        if was_active {
            self.invalidate_selection_request();
        }
        let Some(removed) = self.conversations.remove(target) else {
            return;
        };
        if removed.conversation.session_id.is_none() {
            removed
                .conversation
                .view
                .update(cx, |chat, cx| chat.close_scope(cx));
        }
        if let Some(session_id) = &removed.conversation.session_id {
            self.history.remove(session_id);
        }
        if removed.was_active {
            self.conversations.select_neighbor(removed.index);
        }
        self.notify_changed(cx);
    }

    pub(super) fn begin_delete_confirmation(
        &mut self,
        target: ChatTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_confirmation.dismiss_for_unmount(window, cx);
        self.confirming = Some(target);
        self.notify_changed(cx);
    }

    pub(super) fn clear_delete_confirmation(
        &mut self,
        target: &ChatTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming.as_ref() == Some(target) {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
            self.notify_changed(cx);
        }
    }

    /// Freeze row hover at the pointer's current position because the history
    /// list is about to reorder beneath it.  `owner` is the row the user acted
    /// on, the only one that may keep hover until the pointer moves again.
    pub(super) fn park_pointer(&mut self, owner: Option<ChatTarget>, window: &Window) {
        self.parked_pointer = Some(ParkedPointer {
            at: window.mouse_position(),
            owner,
        });
    }

    /// Let hover follow the pointer again after it has moved.
    pub(super) fn release_parked_pointer(&mut self, cx: &mut Context<Self>) {
        if self.parked_pointer.take().is_some() {
            self.notify_changed(cx);
        }
    }

    pub(super) fn toggle_history_section(
        &mut self,
        kind: HistorySectionKind,
        cx: &mut Context<Self>,
    ) {
        if !self.collapsed_history_sections.remove(&kind) {
            self.collapsed_history_sections.insert(kind);
        }
        self.notify_changed(cx);
    }

    pub(super) fn toggle_favorite(
        &mut self,
        target: ChatTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = match &target {
            ChatTarget::Session(session_id) => session_id.clone(),
            ChatTarget::View(entity) => {
                let Some(session_id) = self
                    .conversations
                    .conversation(*entity)
                    .and_then(|conversation| conversation.session_id.clone())
                else {
                    return;
                };
                session_id
            }
        };
        let Some(current) = self.history.summary(&session_id).cloned() else {
            return;
        };
        let next = !current.favorited;
        // The favorite group is a pinned shortlist loaded in one unpaginated
        // query, so it has a cap; going over it would drop rows out of both
        // lists silently.  Refuse at the boundary and say so.
        if next && self.history.favorites().len() >= MAX_FAVORITES {
            window.push_notification(
                (
                    NotificationType::Warning,
                    t!("sidebar.favorite_limit", limit = MAX_FAVORITES).to_string(),
                ),
                cx,
            );
            return;
        }
        let mut optimistic = current;
        optimistic.favorited = next;
        self.history.upsert(optimistic);
        // The row leaves for the Favorites section on the next frame, sliding
        // another row under a pointer that never moved.
        self.park_pointer(Some(target.clone()), window);

        let stores = self.runtime_services.session_services().clone();
        let store = match stores.chat() {
            Ok(store) => store,
            Err(error) => {
                crate::logging::error(
                    "chat.workspace",
                    format_args!("cannot toggle favorite: {error}"),
                );
                self.refresh_history_summary(session_id, cx);
                self.notify_changed(cx);
                return;
            }
        };

        let app = cx.entity();
        let window_handle = window.window_handle();
        let result_session_id = session_id.clone();
        let task_session_id = session_id.clone();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let guard = store.reserve_operation()?;
                    let mut authorized = guard.authorized_store();
                    authorized.append(
                        &session_id,
                        vec![SessionEntryKind::FavoriteChange(FavoriteChange {
                            favorited: next,
                        })],
                    )?;
                    Ok(())
                })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    this.apply_favorite_toggle(result_session_id, result, window, cx);
                });
            });
        });
        self._favorite_tasks.insert(task_session_id, task);
        self.notify_changed(cx);
    }

    fn apply_favorite_toggle(
        &mut self,
        session_id: SessionId,
        result: Result<(), crate::session::SessionError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._favorite_tasks.remove(&session_id);
        if let Err(error) = result {
            crate::logging::error(
                "chat.workspace",
                format_args!("failed to toggle favorite: {error}"),
            );
            window.push_notification(
                (
                    NotificationType::Error,
                    t!("sidebar.favorite_failed").to_string(),
                ),
                cx,
            );
        }
        self.refresh_history_summary(session_id, cx);
        self.notify_changed(cx);
    }

    pub(super) fn confirm_delete_target(
        &mut self,
        target: ChatTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            ChatTarget::View(entity) => self.delete_conversation(entity, window, cx),
            ChatTarget::Session(session_id) => {
                if let Some(entity) = self.conversations.opened_target(&session_id) {
                    self.delete_conversation(entity, window, cx);
                } else {
                    self.delete_unopened_session(session_id, window, cx);
                }
            }
        }
    }
}
