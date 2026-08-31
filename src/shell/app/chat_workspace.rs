//! Chat workspace instance state and lifecycle.

use std::collections::HashMap;

use gpui::{App, Context, Entity, Subscription, Task, Window};
use gpui_component::{WindowExt as _, notification::NotificationType};
use rust_i18n::t;

use crate::chat::{
    ChatDeleteRequest, ChatEvent, ChatView, create_conversation_runtime, derive_chat_title,
};
use crate::llm::ModelSelection;
use crate::preferences::PreferenceHandle;
use crate::runtime::RuntimeServices;
use crate::session::{
    ChatSessionCatalogController, ResolvedSessionState, SessionId, SessionStores,
};
use crate::ui::inline_delete_confirmation::InlineDeleteConfirmationHandle;

use super::{SidebarTarget, history_sidebar::ChatHistorySidebar};

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

pub(super) struct Conversation {
    pub(super) view: Entity<ChatView>,
    pub(super) title: gpui::SharedString,
    pub(super) selection: Option<ModelSelection>,
    pub(super) session_id: Option<SessionId>,
    pub(super) _subscription: Subscription,
}

/// Immutable projection consumed by the root render path.
#[derive(Clone)]
pub(super) struct ChatConversationSnapshot {
    view: Entity<ChatView>,
    title: gpui::SharedString,
    selection: Option<ModelSelection>,
    session_id: Option<SessionId>,
    is_generating: bool,
}

impl ChatConversationSnapshot {
    pub(super) fn view(&self) -> Entity<ChatView> {
        self.view.clone()
    }

    pub(super) fn title(&self) -> gpui::SharedString {
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

    pub(super) fn target(&self) -> gpui::EntityId {
        self.view.entity_id()
    }
}

#[derive(Clone)]
pub(super) struct ChatWorkspaceSnapshot {
    conversations: Vec<ChatConversationSnapshot>,
    opened_session_index: HashMap<SessionId, gpui::EntityId>,
    active: Option<gpui::EntityId>,
    history: ChatHistorySidebar,
    hovered: Option<SidebarTarget>,
    confirming: Option<SidebarTarget>,
    delete_confirmation: InlineDeleteConfirmationHandle,
}

impl ChatWorkspaceSnapshot {
    pub(super) fn empty() -> Self {
        Self {
            conversations: Vec::new(),
            opened_session_index: HashMap::new(),
            active: None,
            history: ChatHistorySidebar::new(),
            hovered: None,
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
        }
    }

    pub(super) fn conversations(&self) -> &[ChatConversationSnapshot] {
        &self.conversations
    }

    pub(super) fn active(&self) -> Option<gpui::EntityId> {
        self.active
    }

    pub(super) fn active_view(&self) -> Option<Entity<ChatView>> {
        self.active.and_then(|target| {
            self.conversations
                .iter()
                .find(|conversation| conversation.target() == target)
                .map(ChatConversationSnapshot::view)
        })
    }

    pub(super) fn active_session_id(&self) -> Option<SessionId> {
        self.active.and_then(|target| {
            self.conversations
                .iter()
                .find(|conversation| conversation.target() == target)
                .and_then(ChatConversationSnapshot::session_id)
        })
    }

    pub(super) fn conversation(&self, target: gpui::EntityId) -> Option<&ChatConversationSnapshot> {
        self.conversations
            .iter()
            .find(|conversation| conversation.target() == target)
    }

    pub(super) fn opened_target(&self, session_id: &SessionId) -> Option<gpui::EntityId> {
        self.opened_session_index.get(session_id).copied()
    }

    #[allow(dead_code)]
    pub(super) fn opened_session_index(&self) -> &HashMap<SessionId, gpui::EntityId> {
        &self.opened_session_index
    }

    pub(super) fn history(&self) -> &ChatHistorySidebar {
        &self.history
    }

    pub(super) fn hovered(&self) -> Option<&SidebarTarget> {
        self.hovered.as_ref()
    }

    pub(super) fn confirming(&self) -> Option<&SidebarTarget> {
        self.confirming.as_ref()
    }

    pub(super) fn delete_confirmation(&self) -> InlineDeleteConfirmationHandle {
        self.delete_confirmation.clone()
    }
}

struct SessionRestore {
    request: SelectionRequest,
    session_id: SessionId,
    state: ResolvedSessionState,
}

pub(super) struct ChatWorkspace {
    pub(super) conversations: Vec<Conversation>,
    pub(super) opened_session_index: HashMap<SessionId, gpui::EntityId>,
    pub(super) active: Option<gpui::EntityId>,
    pub(super) selection_epoch: SelectionEpoch,
    pub(super) _selection_task: Option<Task<()>>,
    pub(super) history: ChatHistorySidebar,
    pub(super) _catalog_initial_task: Option<Task<()>>,
    pub(super) _catalog_load_more_task: Option<Task<()>>,
    pub(super) _summary_refresh_task: Option<Task<()>>,
    pub(super) _history_delete_task: Option<Task<()>>,
    pub(super) startup_restore_attempted: bool,
    pub(super) hovered: Option<SidebarTarget>,
    pub(super) confirming: Option<SidebarTarget>,
    pub(super) delete_confirmation: InlineDeleteConfirmationHandle,
    pub(super) session_services: SessionStores,
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
            conversations: Vec::new(),
            opened_session_index: HashMap::new(),
            active: None,
            selection_epoch: SelectionEpoch::default(),
            _selection_task: None,
            history: ChatHistorySidebar::new(),
            _catalog_initial_task: None,
            _catalog_load_more_task: None,
            _summary_refresh_task: None,
            _history_delete_task: None,
            startup_restore_attempted: false,
            hovered: None,
            confirming: None,
            delete_confirmation: InlineDeleteConfirmationHandle::default(),
            session_services: services.session_services().clone(),
            runtime_services: services,
            preference_handle,
            snapshot: ChatWorkspaceSnapshot::empty(),
        };
        workspace.start_catalog_initial_load(window, cx);
        workspace.publish_snapshot_with(cx);
        workspace
    }

    pub(super) fn snapshot(&self) -> &ChatWorkspaceSnapshot {
        &self.snapshot
    }

    fn publish_snapshot_with(&mut self, cx: &App) {
        self.snapshot = ChatWorkspaceSnapshot {
            conversations: self
                .conversations
                .iter()
                .map(|conversation| ChatConversationSnapshot {
                    view: conversation.view.clone(),
                    title: conversation.title.clone(),
                    selection: conversation.selection.clone(),
                    session_id: conversation.session_id.clone(),
                    is_generating: conversation.view.read(cx).is_generating(),
                })
                .collect(),
            opened_session_index: self.opened_session_index.clone(),
            active: self.active,
            history: self.history.clone(),
            hovered: self.hovered.clone(),
            confirming: self.confirming.clone(),
            delete_confirmation: self.delete_confirmation.clone(),
        };
    }

    pub(super) fn notify_changed(&mut self, cx: &mut Context<Self>) {
        self.publish_snapshot_with(cx);
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
        for conversation in &self.conversations {
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
            self.session_services.chat_conversation(),
            self.runtime_services.generation_service(),
            cx,
        );
        let view = ChatView::view_with_generation_service_and_preferences(
            runtime,
            self.preference_handle.clone(),
            window,
            cx,
        );
        let subscription = self.subscribe_conversation(&view, window, cx);
        let selection = view.read(cx).selection();
        self.conversations.push(Conversation {
            view,
            title,
            selection,
            session_id: None,
            _subscription: subscription,
        });
        self.active = self
            .conversations
            .last()
            .map(|conversation| conversation.view.entity_id());
        self.notify_changed(cx);
    }

    pub(super) fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(target) = self
            .conversations
            .get(index)
            .map(|conversation| conversation.view.entity_id())
        else {
            return;
        };
        self.invalidate_selection_request();
        if self.active != Some(target) {
            self.active = Some(target);
            self.notify_changed(cx);
        }
    }

    pub(super) fn select_target(&mut self, target: gpui::EntityId, cx: &mut Context<Self>) {
        if !self
            .conversations
            .iter()
            .any(|conversation| conversation.view.entity_id() == target)
        {
            return;
        }
        self.invalidate_selection_request();
        if self.active != Some(target) {
            self.active = Some(target);
            self.notify_changed(cx);
        }
    }

    pub(super) fn select_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.opened_session_index.get(&session_id).copied() {
            self.select_target(target, cx);
            return;
        }
        let stores = self.session_services.clone();
        let Ok(catalog_store) = stores.chat_catalog() else {
            return;
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
            let Ok(state) = result else {
                return;
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
        if let Some(target) = self.opened_session_index.get(&restore.session_id).copied() {
            self.active = Some(target);
            self.record_active_session(&restore.session_id, cx);
            self.notify_changed(cx);
            return;
        }
        let title = restore
            .state
            .messages
            .iter()
            .find(|message| message.message.role == crate::llm::Role::User)
            .and_then(|message| {
                message
                    .message
                    .content
                    .iter()
                    .find_map(|block| match block {
                        crate::llm::ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            Some(derive_chat_title(text))
                        }
                        _ => None,
                    })
            })
            .unwrap_or_else(|| t!("chat.default_title").to_string().into());
        let Some(scope) = self.create_scope(window, cx) else {
            return;
        };
        let runtime = create_conversation_runtime(
            scope,
            self.session_services.chat_conversation(),
            self.runtime_services.generation_service(),
            cx,
        );
        let view = ChatView::view_with_generation_service_and_preferences(
            runtime,
            self.preference_handle.clone(),
            window,
            cx,
        );
        if view
            .update(cx, |chat, cx| {
                chat.restore_from_session(&restore.session_id, &restore.state, cx)
            })
            .is_err()
        {
            view.update(cx, |chat, cx| chat.close_scope(cx));
            return;
        }
        let subscription = self.subscribe_conversation(&view, window, cx);
        let target = view.entity_id();
        self.conversations.push(Conversation {
            selection: view.read(cx).selection(),
            view,
            title,
            session_id: Some(restore.session_id.clone()),
            _subscription: subscription,
        });
        self.opened_session_index
            .insert(restore.session_id.clone(), target);
        self.active = Some(target);
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
            let Some(index) = workspace
                .conversations
                .iter()
                .position(|conversation| conversation.view.entity_id() == target)
            else {
                return;
            };
            match event {
                ChatEvent::TitleChanged(title) => {
                    workspace.conversations[index].title = title.clone()
                }
                ChatEvent::SelectionChanged(selection) => {
                    workspace.conversations[index].selection = Some(selection.clone());
                }
                ChatEvent::SessionBound(session_id) => {
                    if workspace.conversations[index].session_id.as_ref() != Some(session_id) {
                        workspace.conversations[index].session_id = Some(session_id.clone());
                        workspace
                            .opened_session_index
                            .insert(session_id.clone(), target);
                        workspace.refresh_history_summary(session_id.clone(), cx);
                        workspace.record_active_session(session_id, cx);
                    }
                }
                ChatEvent::StateChanged => {}
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
        let Some(target) = self.active else {
            return false;
        };
        let Some(conversation) = self
            .conversations
            .iter()
            .find(|conversation| conversation.view.entity_id() == target)
        else {
            return false;
        };
        conversation
            .view
            .update(cx, |chat, cx| chat.select_model(selection, cx));
        true
    }

    pub(super) fn delete_conversation(
        &mut self,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_confirming = self.confirming == Some(SidebarTarget::View(target));
        if was_confirming {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
        }
        let Some(index) = self
            .conversations
            .iter()
            .position(|conversation| conversation.view.entity_id() == target)
        else {
            if was_confirming {
                self.notify_changed(cx);
            }
            return;
        };
        let request = self.conversations[index]
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
        let Some(index) = self
            .conversations
            .iter()
            .position(|conversation| conversation.view.entity_id() == target)
        else {
            return;
        };
        if self.active == Some(target) {
            self.invalidate_selection_request();
        }
        let removed = self.conversations.remove(index);
        if removed.session_id.is_none() {
            removed.view.update(cx, |chat, cx| chat.close_scope(cx));
        }
        if let Some(session_id) = &removed.session_id {
            self.opened_session_index.remove(session_id);
            self.history.remove(session_id);
        }
        if self.active == Some(target) {
            self.active = self
                .conversations
                .get(index)
                .map(|conversation| conversation.view.entity_id())
                .or_else(|| {
                    index.checked_sub(1).and_then(|index| {
                        self.conversations
                            .get(index)
                            .map(|conversation| conversation.view.entity_id())
                    })
                });
        }
        self.notify_changed(cx);
    }

    pub(super) fn begin_delete_confirmation(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_confirmation.dismiss_for_unmount(window, cx);
        self.confirming = Some(target);
        self.notify_changed(cx);
    }

    pub(super) fn clear_delete_confirmation(
        &mut self,
        target: &SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming.as_ref() == Some(target) {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
            self.notify_changed(cx);
        }
    }

    pub(super) fn set_hovered(&mut self, target: Option<SidebarTarget>, cx: &mut Context<Self>) {
        if self.hovered != target {
            self.hovered = target;
            self.notify_changed(cx);
        }
    }

    pub(super) fn confirm_delete_target(
        &mut self,
        target: SidebarTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            SidebarTarget::View(entity) => self.delete_conversation(entity, window, cx),
            SidebarTarget::Session(session_id) => {
                if let Some(entity) = self.opened_session_index.get(&session_id).copied() {
                    self.delete_conversation(entity, window, cx);
                } else {
                    self.delete_unopened_session(session_id, window, cx);
                }
            }
            _ => {}
        }
    }
}
