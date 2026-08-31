//! Chat history catalog sidebar state, background loading, and rendering.
//!
//! The sidebar treats the catalog snapshot as the sole source of persisted
//! rows. Drafts (unbound views) appear as temporary rows above the catalog.
//! "Opened", "generating", and "active" are visual annotations derived from
//! workspace state, never row identity. Every catalog read runs on the
//! background executor; render only reads the snapshot.

use std::{collections::HashSet, rc::Rc};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, ParentElement as _, Pixels, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    spinner::Spinner,
    v_flex,
};
use rust_i18n::t;

use crate::preferences;
use crate::session::{
    CatalogCursor, CatalogError, CatalogPage, CatalogQuery, SessionCatalogStore, SessionDomain,
    SessionId, SessionLifecycleStore, SessionSummary,
};
use crate::ui::inline_delete_confirmation::InlineDeleteConfirmation;

use super::ChatApp;
use super::chat_workspace::{
    ChatConversationSnapshot, ChatTarget, ChatWorkspace, ChatWorkspaceSnapshot,
};

/// Row height for both draft and catalog rows, matching the previous
/// conversation row so the sidebar density is unchanged.
const HISTORY_ROW_HEIGHT: Pixels = px(32.);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HistoryLoadState {
    Unloaded,
    Loading,
    Ready,
    Error(SharedString),
}

/// UI-independent snapshot of the Chat catalog sidebar.
#[derive(Clone)]
pub(super) struct ChatHistorySidebar {
    summaries: Vec<SessionSummary>,
    load_state: HistoryLoadState,
    next_cursor: Option<CatalogCursor>,
    load_more_in_flight: bool,
    /// Monotonic guard so a stale background load cannot overwrite a newer
    /// snapshot (for example, a load-more that raced a re-initialization).
    generation: u64,
}

impl ChatHistorySidebar {
    pub(super) fn new() -> Self {
        Self {
            summaries: Vec::new(),
            load_state: HistoryLoadState::Unloaded,
            next_cursor: None,
            load_more_in_flight: false,
            generation: 0,
        }
    }

    pub(super) fn summaries(&self) -> &[SessionSummary] {
        &self.summaries
    }

    #[allow(dead_code)]
    pub(super) fn load_state(&self) -> &HistoryLoadState {
        &self.load_state
    }

    pub(super) fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    pub(super) fn load_more_in_flight(&self) -> bool {
        self.load_more_in_flight
    }

    pub(super) fn is_empty(&self) -> bool {
        self.summaries.is_empty()
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Apply the initial catalog page.  Returns `true` if the snapshot changed.
    pub(super) fn apply_initial(&mut self, generation: u64, page: CatalogPage) -> bool {
        if generation != self.generation {
            return false;
        }
        self.summaries = dedup_summaries(page.sessions);
        self.next_cursor = page.next_cursor;
        self.load_state = HistoryLoadState::Ready;
        true
    }

    /// Apply a load-more page.  New rows are appended in catalog order without
    /// duplicating existing rows.
    pub(super) fn apply_load_more(&mut self, generation: u64, page: CatalogPage) -> bool {
        if generation != self.generation {
            return false;
        }
        let existing: HashSet<SessionId> = self
            .summaries
            .iter()
            .map(|summary| summary.session_id.clone())
            .collect();
        for summary in dedup_summaries(page.sessions) {
            if !existing.contains(&summary.session_id) {
                self.summaries.push(summary);
            }
        }
        self.next_cursor = page.next_cursor;
        true
    }

    fn mark_error(&mut self, generation: u64, message: SharedString) -> bool {
        if generation != self.generation {
            return false;
        }
        self.load_state = HistoryLoadState::Error(message);
        true
    }

    /// Insert or refresh a single session summary.  Used after a durable begin
    /// binds a new session so the row appears without a full reload.  The row
    /// keeps catalog creation order: a brand-new session is the newest, so it
    /// lands at the front; an existing row is updated in place.
    pub(super) fn upsert(&mut self, summary: SessionSummary) {
        let session_id = summary.session_id.clone();
        if let Some(existing) = self
            .summaries
            .iter_mut()
            .find(|row| row.session_id == session_id)
        {
            *existing = summary;
            return;
        }
        // Keep newest-first by created_at, then session_id, matching the
        // catalog's keyset ordering so an inserted row does not jump later.
        let created_at = summary.created_at;
        let uuid = summary.session_id.uuid();
        let position = self
            .summaries
            .partition_point(|row| (row.created_at, row.session_id.uuid()) > (created_at, uuid));
        self.summaries.insert(position, summary);
    }

    /// Remove a session from the snapshot.  Called after a permanent delete
    /// succeeds so the row disappears regardless of whether it was opened.
    pub(super) fn remove(&mut self, session_id: &SessionId) -> bool {
        let before = self.summaries.len();
        self.summaries.retain(|row| &row.session_id != session_id);
        self.summaries.len() != before
    }
}

fn dedup_summaries(sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
    let mut seen = HashSet::with_capacity(sessions.len());
    sessions
        .into_iter()
        .filter(|summary| seen.insert(summary.session_id.clone()))
        .collect()
}

impl ChatWorkspace {
    // ---------- Background catalog loading ----------

    /// Kick off the initial catalog page on the background executor.  The first
    /// frame is never blocked: render shows a loading state until the snapshot
    /// lands.  Re-running this while a load is in flight is a no-op.
    pub(super) fn start_catalog_initial_load(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(
            self.history.load_state,
            HistoryLoadState::Unloaded | HistoryLoadState::Error(_)
        ) {
            return;
        }
        let stores = self.session_services.clone();
        let catalog_store = match stores.chat_catalog() {
            Ok(store) => store,
            Err(error) => {
                self.history.load_state = HistoryLoadState::Error(error.to_string().into());
                return;
            }
        };

        let generation = self.history.next_generation();
        self.history.load_state = HistoryLoadState::Loading;
        let app = cx.entity();
        let window_handle = window.window_handle();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    catalog_store.list_sessions(SessionDomain::Chat, CatalogQuery::first_page())
                })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    this.apply_catalog_initial(generation, result, window, cx);
                });
            });
        });
        self._catalog_initial_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_catalog_initial(
        &mut self,
        generation: u64,
        result: Result<CatalogPage, CatalogError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(page) => {
                self.history.apply_initial(generation, page);
            }
            Err(error) => {
                let message = error.to_string().into();
                self.history.mark_error(generation, message);
            }
        }
        self._catalog_initial_task = None;
        self.notify_changed(cx);

        // Startup restore is a separate, cancellable selection that must not
        // block the first catalog frame.  Only attempt it once, immediately
        // after the initial page lands successfully.
        if matches!(self.history.load_state, HistoryLoadState::Ready)
            && !self.startup_restore_attempted
        {
            self.startup_restore_attempted = true;
            self.maybe_restore_last_chat_on_start(window, cx);
        }
    }

    /// Append the next catalog page.  No-op when the directory is exhausted or
    /// a load is already in flight.
    pub(super) fn start_catalog_load_more(&mut self, cx: &mut Context<Self>) {
        if self.history.load_more_in_flight() || !self.history.has_more() {
            return;
        }
        let Some(cursor) = self.history.next_cursor.clone() else {
            return;
        };
        let stores = self.session_services.clone();
        let catalog_store = match stores.chat_catalog() {
            Ok(store) => store,
            Err(_) => return,
        };

        self.history.load_more_in_flight = true;
        let generation = self.history.generation;
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    catalog_store.list_sessions(
                        SessionDomain::Chat,
                        CatalogQuery {
                            cursor: Some(cursor),
                            ..CatalogQuery::first_page()
                        },
                    )
                })
                .await;
            app.update(cx, |this, cx| {
                this.apply_catalog_load_more(generation, result, cx);
            });
        });
        self._catalog_load_more_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_catalog_load_more(
        &mut self,
        generation: u64,
        result: Result<CatalogPage, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        self.history.load_more_in_flight = false;
        self._catalog_load_more_task = None;
        if let Ok(page) = result {
            self.history.apply_load_more(generation, page);
        } else {
            // A failed load-more leaves the existing snapshot intact; the user
            // can retry via the button, which remains available because the
            // cursor did not advance.
        }
        self.notify_changed(cx);
    }

    /// Refresh a single session's summary in the background.  Used after a
    /// durable begin binds a new session so the sidebar reflects it without a
    /// full reload.
    pub(super) fn refresh_history_summary(
        &mut self,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let stores = self.session_services.clone();
        let catalog_store = match stores.chat_catalog() {
            Ok(store) => store,
            Err(_) => return,
        };
        let app = cx.entity();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { catalog_store.get_session_summary(&session_id) })
                .await;
            app.update(cx, |this, cx| {
                this.apply_history_summary_refresh(result, cx);
            });
        });
        self._summary_refresh_task = Some(task);
    }

    fn apply_history_summary_refresh(
        &mut self,
        result: Result<Option<SessionSummary>, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        self._summary_refresh_task = None;
        if let Ok(Some(summary)) = result {
            self.history.upsert(summary);
            self.notify_changed(cx);
        }
    }

    // ---------- Startup restore ----------

    /// Lazily restore the last explicitly active Chat session if the user has
    /// enabled the preference and the session is still present in the catalog.
    /// This is a cancellable background selection that never creates a draft.
    fn maybe_restore_last_chat_on_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.preference_handle.snapshot();
        if !prefs.restore_last_chat_on_start {
            return;
        }
        let Some(session_id) = prefs.last_active_chat_session.clone() else {
            return;
        };
        if !self
            .history
            .summaries()
            .iter()
            .any(|summary| summary.session_id == session_id)
        {
            // The session is gone or the catalog is empty; fall back to the
            // empty workspace rather than fabricating a draft.
            return;
        }
        self.select_session(session_id, window, cx);
    }

    /// Record the session id a successful selection or durable begin bound, so
    /// startup restore can re-target it next launch.  Only persisted sessions
    /// are recorded; drafts never have a session id.
    pub(super) fn record_active_session(&self, session_id: &SessionId, cx: &mut Context<Self>) {
        let current = self.preference_handle.snapshot().last_active_chat_session;
        if current.as_ref() == Some(session_id) {
            return;
        }
        preferences::update_with(cx, &self.preference_handle, |prefs| {
            prefs.last_active_chat_session = Some(session_id.clone());
        });
    }

    // ---------- Unopened session deletion ----------

    /// Permanently delete a catalog session that has no opened view.  The
    /// deletion runs on the background executor through the same durability
    /// path as opened-view deletion; the catalog row is removed only after the
    /// mutation succeeds.
    pub(super) fn delete_unopened_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming == Some(ChatTarget::Session(session_id.clone())) {
            self.delete_confirmation.dismiss_for_unmount(window, cx);
            self.confirming = None;
        }

        let stores = self.session_services.clone();
        let store = match stores.chat() {
            Ok(store) => store,
            Err(error) => {
                crate::logging::error(
                    "chat.workspace",
                    format_args!("cannot delete unopened session: {error}"),
                );
                return;
            }
        };

        let app = cx.entity();
        let window_handle = window.window_handle();
        let result_session_id = session_id.clone();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let guard = store.reserve_operation()?;
                    let mut authorized = guard.authorized_store();
                    authorized.delete_session(&session_id)
                })
                .await;
            let _ = window_handle.update(cx, |_, window, cx| {
                app.update(cx, |this, cx| {
                    this.apply_unopened_session_delete(result_session_id, result, window, cx);
                });
            });
        });
        self._history_delete_task = Some(task);
        self.notify_changed(cx);
    }

    fn apply_unopened_session_delete(
        &mut self,
        session_id: SessionId,
        result: Result<(), crate::session::SessionError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._history_delete_task = None;
        match result {
            Ok(()) => {
                self.history.remove(&session_id);
            }
            Err(error) => {
                crate::logging::error(
                    "chat.workspace",
                    format_args!("failed to delete unopened session {session_id}: {error}"),
                );
                let message = t!("chat.error.persistence_delete_failed").to_string();
                window.push_notification(
                    (
                        gpui_component::notification::NotificationType::Error,
                        message,
                    ),
                    cx,
                );
            }
        }
        self.notify_changed(cx);
    }
}

impl ChatApp {
    // ---------- Sidebar content rendering ----------

    /// Render the scrollable catalog + draft list.  Replaces the previous
    /// opened-views-only list with a catalog snapshot plus temporary draft rows
    /// above it.
    pub(super) fn render_history_content(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let snapshot = self.chat_snapshot();
        let mut children: Vec<AnyElement> = Vec::new();

        let drafts: Vec<&ChatConversationSnapshot> = snapshot
            .conversations()
            .iter()
            .filter(|conversation| conversation.session_id().is_none())
            .collect();

        let loading_and_empty =
            matches!(snapshot.history().load_state(), HistoryLoadState::Loading)
                && snapshot.history().is_empty();

        if loading_and_empty {
            children.push(self.render_history_loading_state(cx).into_any_element());
        }

        // Draft rows sit above the catalog so a freshly created new chat is
        // immediately reachable even before its first durable turn.
        for conversation in &drafts {
            let target = conversation.target();
            children.push(
                self.render_draft_row(snapshot, conversation, target, window, cx)
                    .into_any_element(),
            );
        }

        // Catalog rows.
        let active_session_id = snapshot.active_session_id();
        for summary in snapshot.history().summaries() {
            children.push(
                self.render_catalog_row(snapshot, summary, active_session_id.as_ref(), window, cx)
                    .into_any_element(),
            );
        }

        let ready = matches!(snapshot.history().load_state(), HistoryLoadState::Ready);
        let error_and_empty = matches!(snapshot.history().load_state(), HistoryLoadState::Error(_))
            && snapshot.history().is_empty();
        let no_rows = drafts.is_empty() && snapshot.history().is_empty();

        if error_and_empty {
            children.push(self.render_history_error_state(cx).into_any_element());
        } else if ready && no_rows {
            children.push(self.render_history_empty_state(cx).into_any_element());
        } else if snapshot.history().has_more() {
            children.push(self.render_load_more_row(cx).into_any_element());
        }

        v_flex()
            .id("chats")
            .debug_selector(|| "sidebar-list-surface".to_string())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_1()
            .children(children)
            .into_any_element()
    }

    fn render_history_loading_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .h(HISTORY_ROW_HEIGHT)
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(Spinner::new().small())
            .child(t!("sidebar.loading_chats").to_string())
    }

    fn render_history_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_2()
            .py_3()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(div().child(t!("sidebar.empty").to_string()))
            .child(div().text_xs().child(t!("sidebar.empty_hint").to_string()))
    }

    fn render_history_error_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.chat_workspace().downgrade();
        v_flex()
            .px_2()
            .py_2()
            .gap_2()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.6))
            .child(div().child(t!("sidebar.load_failed").to_string()))
            .child(
                Button::new("history-retry")
                    .ghost()
                    .small()
                    .label(t!("sidebar.retry").to_string())
                    .on_click(move |_, window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.start_catalog_initial_load(window, cx)
                            })
                            .ok();
                    }),
            )
    }

    fn render_load_more_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let in_flight = self.chat_snapshot().history().load_more_in_flight();
        let workspace = self.chat_workspace().downgrade();
        let row = div()
            .id("history-load-more")
            .w_full()
            .h(HISTORY_ROW_HEIGHT)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground.opacity(0.7))
            .cursor_default()
            .on_click(move |_: &ClickEvent, _, cx| {
                workspace
                    .update(cx, |workspace, cx| workspace.start_catalog_load_more(cx))
                    .ok();
            });
        if in_flight {
            row.child(Spinner::new().small())
                .child(t!("sidebar.loading_chats").to_string())
        } else {
            row.child(t!("sidebar.load_more").to_string())
        }
    }

    fn render_draft_row(
        &self,
        snapshot: &ChatWorkspaceSnapshot,
        conversation: &ChatConversationSnapshot,
        target: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = conversation.title();
        let is_active = snapshot.active() == Some(target);
        let is_generating = conversation.is_generating();
        let sidebar_target = ChatTarget::View(target);
        let is_confirming = snapshot.confirming() == Some(&sidebar_target);
        let actions_visible =
            is_active || snapshot.hovered() == Some(&sidebar_target) || is_confirming;
        let index = snapshot
            .conversations()
            .iter()
            .position(|conv| conv.target() == target)
            .unwrap_or_default();
        let workspace = self.chat_workspace().downgrade();

        self.render_history_row(
            ("conv-row", target),
            ("conv", target),
            title,
            is_active,
            is_generating,
            actions_visible,
            is_confirming,
            sidebar_target,
            {
                let workspace = workspace.clone();
                move |_, _, cx| {
                    workspace
                        .update(cx, |workspace, cx| workspace.select(index, cx))
                        .ok();
                }
            },
            {
                let workspace = workspace.clone();
                move |event: &KeyDownEvent, window, cx| {
                    if crate::ui::consume_button_key(event, window, cx) {
                        workspace
                            .update(cx, |workspace, cx| workspace.select(index, cx))
                            .ok();
                    }
                    let _ = window;
                }
            },
            window,
            cx,
        )
    }

    fn render_catalog_row(
        &self,
        snapshot: &ChatWorkspaceSnapshot,
        summary: &SessionSummary,
        active_session_id: Option<&SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session_id = summary.session_id.clone();
        let sidebar_target = ChatTarget::Session(session_id.clone());
        let opened_target = snapshot.opened_target(&session_id);
        let is_active = active_session_id == Some(&session_id);
        let is_generating = opened_target
            .and_then(|target| snapshot.conversation(target))
            .map(ChatConversationSnapshot::is_generating)
            .unwrap_or(false);

        // Prefer the live conversation title (which tracks in-flight edits)
        // when the session is opened; otherwise fall back to the catalog title
        // or a localized placeholder.
        let title: SharedString = opened_target
            .and_then(|target| snapshot.conversation(target))
            .map(ChatConversationSnapshot::title)
            .or_else(|| {
                summary
                    .title
                    .as_ref()
                    .filter(|title| !title.trim().is_empty())
                    .map(|title| title.clone().into())
            })
            .or_else(|| {
                summary
                    .preview
                    .as_ref()
                    .filter(|preview| !preview.trim().is_empty())
                    .map(|preview| crate::chat::derive_chat_title(preview))
            })
            .unwrap_or_else(|| t!("chat.default_title").to_string().into());

        let is_confirming = snapshot.confirming() == Some(&sidebar_target);
        let actions_visible =
            is_active || snapshot.hovered() == Some(&sidebar_target) || is_confirming;
        let workspace = self.chat_workspace().downgrade();

        self.render_history_row(
            format!("history-row-{session_id}"),
            format!("history-button-{session_id}"),
            title,
            is_active,
            is_generating,
            actions_visible,
            is_confirming,
            sidebar_target.clone(),
            {
                let session_id = session_id.clone();
                {
                    let workspace = workspace.clone();
                    move |_, window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.select_session(session_id.clone(), window, cx)
                            })
                            .ok();
                    }
                }
            },
            {
                let session_id = session_id.clone();
                {
                    let workspace = workspace.clone();
                    move |event: &KeyDownEvent, window, cx| {
                        if crate::ui::consume_button_key(event, window, cx) {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.select_session(session_id.clone(), window, cx)
                                })
                                .ok();
                        }
                    }
                }
            },
            window,
            cx,
        )
    }

    /// Shared row renderer for draft and catalog rows.  The two `on_click` /
    /// `on_key_down` callbacks are already in listener form (`Fn(&Event, &mut
    /// Window, &mut App)`); everything else (annotations, actions button, focus
    /// ring) is identical.
    #[allow(clippy::too_many_arguments)]
    fn render_history_row(
        &self,
        row_id: impl Into<ElementId>,
        button_id: impl Into<ElementId>,
        title: SharedString,
        is_active: bool,
        is_generating: bool,
        actions_visible: bool,
        is_confirming: bool,
        target: ChatTarget,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_key_down: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_id: ElementId = row_id.into();
        let button_id: ElementId = button_id.into();
        let focus_handle = window
            .use_keyed_state(button_id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let target_for_hover = target.clone();
        let target_for_actions = target;
        let workspace = self.chat_workspace().downgrade();

        let title_element = if is_generating {
            h_flex()
                .min_w_0()
                .flex_1()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .flex_1()
                        .min_w_0()
                        .child(title.clone()),
                )
                .child(
                    Spinner::new()
                        .xsmall()
                        .color(cx.theme().sidebar_foreground.opacity(0.6)),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .text_ellipsis()
                .child(title.clone())
                .into_any_element()
        };

        div()
            .id(row_id)
            .relative()
            .w_full()
            .h(HISTORY_ROW_HEIGHT)
            .on_hover(move |hovered: &bool, _, cx| {
                let entered = *hovered;
                if entered {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.set_hovered(Some(target_for_hover.clone()), cx)
                        })
                        .ok();
                } else {
                    let target = target_for_hover.clone();
                    workspace
                        .update(cx, |workspace, cx| {
                            if workspace.snapshot().hovered() == Some(&target) {
                                workspace.set_hovered(None, cx);
                            }
                        })
                        .ok();
                }
            })
            .child(
                div()
                    .id(button_id)
                    .role(Role::Button)
                    .aria_label(title.clone())
                    .aria_selected(is_active)
                    .track_focus(&focus_handle.tab_stop(true))
                    .focus_visible(|this| this.border_1().border_color(focus_ring))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded(cx.theme().radius)
                    .text_sm()
                    .text_color(cx.theme().sidebar_foreground)
                    .cursor_default()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .when(is_active || actions_visible, |this| {
                        this.bg(cx.theme().sidebar_accent)
                            .text_color(cx.theme().sidebar_accent_foreground)
                    })
                    .on_key_down(on_key_down)
                    .on_click(on_click)
                    .child(title_element)
                    .when(!actions_visible, |this| {
                        this.child(div().absolute().right_2().top(px(6.)).size_5().occlude())
                    }),
            )
            .child(
                div()
                    .absolute()
                    .right_2()
                    .top(px(6.))
                    .size_5()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(self.render_chat_sidebar_actions(
                        target_for_actions,
                        actions_visible,
                        is_confirming,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_chat_sidebar_actions(
        &self,
        target: ChatTarget,
        visible: bool,
        confirming: bool,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = self.chat_workspace().downgrade();
        let ids = match &target {
            ChatTarget::View(entity) => SidebarActionIds {
                trigger_id: ("conversation-actions", *entity).into(),
                confirm_id: ("conversation-delete-confirm", *entity).into(),
                trigger_debug_selector: format!("conversation-actions-{}", entity.as_u64()),
                delete_label: t!("sidebar.delete_chat").to_string(),
                confirm_title: t!("sidebar.delete_chat_title").to_string(),
            },
            ChatTarget::Session(session) => SidebarActionIds {
                trigger_id: format!("history-actions-{session}").into(),
                confirm_id: format!("history-delete-confirm-{session}").into(),
                trigger_debug_selector: format!("history-actions-{session}"),
                delete_label: t!("sidebar.delete_chat").to_string(),
                confirm_title: t!("sidebar.delete_chat_title").to_string(),
            },
        };
        render_sidebar_actions(
            SidebarActionSpec {
                target,
                visible,
                confirming,
                ids,
                handle: self.chat_snapshot().delete_confirmation(),
            },
            move |target, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.clear_delete_confirmation(&target, window, cx)
                    })
                    .ok();
            },
            {
                let workspace = self.chat_workspace().downgrade();
                move |target, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.confirm_delete_target(target, window, cx)
                        })
                        .ok();
                }
            },
            {
                let workspace = self.chat_workspace().downgrade();
                move |target, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.begin_delete_confirmation(target, window, cx)
                        })
                        .ok();
                }
            },
        )
    }
}

/// Stable UI identifiers and labels for one workspace-local sidebar target.
pub(super) struct SidebarActionIds {
    pub(super) trigger_id: ElementId,
    pub(super) confirm_id: ElementId,
    pub(super) trigger_debug_selector: String,
    pub(super) delete_label: String,
    pub(super) confirm_title: String,
}

pub(super) struct SidebarActionSpec<T> {
    pub(super) target: T,
    pub(super) visible: bool,
    pub(super) confirming: bool,
    pub(super) ids: SidebarActionIds,
    pub(super) handle: crate::ui::inline_delete_confirmation::InlineDeleteConfirmationHandle,
}

/// Render the common trailing action control without understanding the target
/// type or which workspace owns it.
pub(super) fn render_sidebar_actions<T>(
    spec: SidebarActionSpec<T>,
    on_clear: impl Fn(T, &mut Window, &mut App) + 'static,
    on_confirm: impl Fn(T, &mut Window, &mut App) + 'static,
    on_begin: impl Fn(T, &mut Window, &mut App) + 'static,
) -> AnyElement
where
    T: Clone + 'static,
{
    let SidebarActionSpec {
        target,
        visible,
        confirming,
        ids,
        handle,
    } = spec;
    let on_begin = Rc::new(on_begin);
    let trigger = Button::new(ids.trigger_id)
        .ghost()
        .xsmall()
        .icon(IconName::Ellipsis)
        .tooltip(t!("sidebar.more_actions").to_string())
        .debug_selector(move || ids.trigger_debug_selector.clone());

    if confirming {
        let target_for_open = target.clone();
        let target_for_confirm = target;
        InlineDeleteConfirmation::new(
            ids.confirm_id,
            trigger,
            ids.confirm_title,
            t!("sidebar.delete_chat_cancel").to_string(),
            t!("sidebar.delete_chat_confirm").to_string(),
            handle,
        )
        .on_open_change(move |open: &bool, window, cx| {
            if !*open {
                on_clear(target_for_open.clone(), window, cx);
            }
        })
        .on_confirm(move |window, cx| {
            on_confirm(target_for_confirm.clone(), window, cx);
        })
        .into_any_element()
    } else {
        let target_for_begin = target;
        trigger
            .when(!visible, |this| this.invisible())
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                let target = target_for_begin.clone();
                let on_begin = Rc::clone(&on_begin);
                menu.item(
                    gpui_component::menu::PopupMenuItem::new(ids.delete_label.clone()).on_click(
                        move |_, window, cx| {
                            on_begin(target.clone(), window, cx);
                        },
                    ),
                )
            })
            .into_any_element()
    }
}
