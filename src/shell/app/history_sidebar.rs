//! Chat history catalog sidebar state, background loading, and rendering.
//!
//! The sidebar treats the catalog snapshot as the sole source of persisted
//! rows. Host-owned rows (unbound drafts, and bound views whose catalog
//! summary has not landed yet) appear above that snapshot. "Opened",
//! "generating", and "active" are visual annotations derived from workspace
//! state, never row identity. Every catalog read runs on the background
//! executor; render only reads the snapshot. Do not insert a placeholder
//! `SessionSummary` to bridge the bind → catalog gap.

use std::{collections::HashSet, rc::Rc};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Pixels, Role, SharedString, StatefulInteractiveElement as _,
    Styled, Window, div, linear_color_stop, linear_gradient, px,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    collapsible::Collapsible,
    h_flex,
    menu::DropdownMenu as _,
    spinner::Spinner,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::contrast;

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
use super::conversation_host::ConversationId;
use super::history_groups::{HistoryRow, history_sections};
use super::workspace_host::WorkspaceCommand;
use crate::runtime::CHAT_WORKSPACE_ID;

/// Row height for both draft and catalog rows, matching the previous
/// conversation row so the sidebar density is unchanged.
const HISTORY_ROW_HEIGHT: Pixels = px(32.);
/// Height of a section header band.  Smaller than a row so the header reads as
/// a label rather than another entry.
const HISTORY_SECTION_HEIGHT: Pixels = px(22.);
/// Trailing action button box, its gap, and the cluster's inset from the row's
/// trailing edge.  The title fade is derived from these, so the geometry is
/// stated once.
const HISTORY_ACTION_BUTTON: Pixels = px(24.);
const HISTORY_ACTION_GAP: Pixels = px(2.);
const HISTORY_ACTION_INSET: Pixels = px(4.);
/// Distance in front of the cluster over which a long title fades out.
const HISTORY_ACTION_FADE_RAMP: Pixels = px(40.);
/// Hover group declared by a section header's toggle.  The chevron binds to it
/// so it only appears while the pointer is over the label itself; because the
/// group is pushed and popped around the toggle's children, every section
/// resolves to its own toggle even though they share one name.
const HISTORY_SECTION_HOVER_GROUP: &str = "history-section-toggle";

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
    favorites: Vec<SessionSummary>,
    timeline: Vec<SessionSummary>,
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
            favorites: Vec::new(),
            timeline: Vec::new(),
            load_state: HistoryLoadState::Unloaded,
            next_cursor: None,
            load_more_in_flight: false,
            generation: 0,
        }
    }

    pub(super) fn favorites(&self) -> &[SessionSummary] {
        &self.favorites
    }

    pub(super) fn timeline(&self) -> &[SessionSummary] {
        &self.timeline
    }

    pub(super) fn contains_session(&self, session_id: &SessionId) -> bool {
        self.favorites
            .iter()
            .chain(self.timeline.iter())
            .any(|summary| &summary.session_id == session_id)
    }

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
        self.favorites.is_empty() && self.timeline.is_empty()
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Apply the initial catalog pages.  Returns `true` if the snapshot changed.
    pub(super) fn apply_initial(
        &mut self,
        generation: u64,
        favorites: CatalogPage,
        timeline: CatalogPage,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.favorites = dedup_summaries(favorites.sessions);
        let favorite_ids: HashSet<SessionId> = self
            .favorites
            .iter()
            .map(|summary| summary.session_id.clone())
            .collect();
        self.timeline = dedup_summaries(timeline.sessions)
            .into_iter()
            .filter(|summary| !favorite_ids.contains(&summary.session_id))
            .collect();
        self.next_cursor = timeline.next_cursor;
        self.load_state = HistoryLoadState::Ready;
        true
    }

    /// Apply a load-more page.  New rows are appended in catalog order without
    /// duplicating existing rows or leaking favorites into the timeline.
    pub(super) fn apply_load_more(&mut self, generation: u64, page: CatalogPage) -> bool {
        if generation != self.generation {
            return false;
        }
        let existing: HashSet<SessionId> = self
            .favorites
            .iter()
            .chain(self.timeline.iter())
            .map(|summary| summary.session_id.clone())
            .collect();
        for summary in dedup_summaries(page.sessions) {
            if !existing.contains(&summary.session_id) && !summary.favorited {
                self.timeline.push(summary);
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
    /// binds a new session so the row appears without a full reload, and after
    /// a favorite toggle.  The row keeps catalog creation order within its
    /// destination vec.
    pub(super) fn upsert(&mut self, summary: SessionSummary) {
        let session_id = summary.session_id.clone();
        self.favorites.retain(|row| row.session_id != session_id);
        self.timeline.retain(|row| row.session_id != session_id);
        insert_sorted_summary(
            if summary.favorited {
                &mut self.favorites
            } else {
                &mut self.timeline
            },
            summary,
        );
    }

    /// Remove a session from the snapshot.  Called after a permanent delete
    /// succeeds so the row disappears regardless of whether it was opened.
    pub(super) fn remove(&mut self, session_id: &SessionId) -> bool {
        let before = self.favorites.len() + self.timeline.len();
        self.favorites.retain(|row| &row.session_id != session_id);
        self.timeline.retain(|row| &row.session_id != session_id);
        self.favorites.len() + self.timeline.len() != before
    }

    pub(super) fn summary(&self, session_id: &SessionId) -> Option<&SessionSummary> {
        self.favorites
            .iter()
            .chain(self.timeline.iter())
            .find(|row| &row.session_id == session_id)
    }
}

fn insert_sorted_summary(rows: &mut Vec<SessionSummary>, summary: SessionSummary) {
    let created_at = summary.created_at;
    let uuid = summary.session_id.uuid();
    let position =
        rows.partition_point(|row| (row.created_at, row.session_id.uuid()) > (created_at, uuid));
    rows.insert(position, summary);
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
        let stores = self.runtime_services.session_services().clone();
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
                    let favorites = catalog_store
                        .list_sessions(SessionDomain::Chat, CatalogQuery::favorites())?;
                    let timeline = catalog_store
                        .list_sessions(SessionDomain::Chat, CatalogQuery::timeline_first_page())?;
                    Ok((favorites, timeline))
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
        result: Result<(CatalogPage, CatalogPage), CatalogError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((favorites, timeline)) => {
                if favorites.next_cursor.is_some() {
                    // Only reachable if the cap was raised or the files were
                    // edited outside the app: the extra rows are in neither
                    // list, so say where they went.
                    crate::logging::warn(
                        "chat.workspace",
                        format_args!(
                            "more than {} favorites on disk; the rest are not listed",
                            crate::session::MAX_FAVORITES
                        ),
                    );
                }
                self.history.apply_initial(generation, favorites, timeline);
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
        let stores = self.runtime_services.session_services().clone();
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
                            favorited: Some(false),
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
        let stores = self.runtime_services.session_services().clone();
        let catalog_store = match stores.chat_catalog() {
            Ok(store) => store,
            Err(_) => return,
        };
        let app = cx.entity();
        let task_session_id = session_id.clone();
        let task = cx.spawn(async move |_this, cx| {
            let refreshed = session_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { catalog_store.get_session_summary(&session_id) })
                .await;
            app.update(cx, |this, cx| {
                this.apply_history_summary_refresh(refreshed, result, cx);
            });
        });
        self._summary_refresh_tasks.insert(task_session_id, task);
    }

    fn apply_history_summary_refresh(
        &mut self,
        session_id: SessionId,
        result: Result<Option<SessionSummary>, CatalogError>,
        cx: &mut Context<Self>,
    ) {
        self._summary_refresh_tasks.remove(&session_id);
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
        if !self.history.contains_session(&session_id) {
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

        let stores = self.runtime_services.session_services().clone();
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
                // The rows below close the gap, so one of them lands under a
                // pointer that never moved.
                self.park_pointer(None, window);
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

    /// Render the scrollable catalog plus host-owned rows above it.
    pub(super) fn render_history_content(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let snapshot = self.chat_snapshot();
        let mut children: Vec<AnyElement> = Vec::new();

        let pending: Vec<&ChatConversationSnapshot> = snapshot
            .conversations()
            .iter()
            .filter(|conversation| {
                is_pending_history_conversation(
                    conversation.session_id().as_ref(),
                    snapshot.history(),
                )
            })
            .collect();

        let loading_and_empty =
            matches!(snapshot.history().load_state(), HistoryLoadState::Loading)
                && snapshot.history().is_empty();

        if loading_and_empty {
            children.push(self.render_history_loading_state(cx).into_any_element());
        }

        let tints = contrast::sidebar_row_tints(cx);
        let now_millis = chrono::Local::now().timestamp_millis();
        let sections = history_sections(
            now_millis,
            pending,
            snapshot.history().favorites(),
            snapshot.history().timeline(),
            |summary: &&SessionSummary| summary.created_at,
        );
        let active_session_id = snapshot.active_session_id();
        for section in sections {
            let kind = section.kind;
            let open = snapshot.history_section_open(kind);
            let header_label = t!(kind.i18n_key()).to_string();
            let workspace = self.chat_workspace().downgrade();
            let workspace_for_key = workspace.clone();
            let toggle_id = format!("history-section-toggle-{}", kind.i18n_key());
            let focus_handle = window
                .use_keyed_state(SharedString::from(toggle_id.clone()), cx, |_, cx| {
                    cx.focus_handle()
                })
                .read(cx)
                .clone();
            // Only the label and its chevron toggle the section, so the header
            // band itself stays inert: no full-width highlight, no stray click
            // target next to the rows.
            let header = h_flex().w_full().h(HISTORY_SECTION_HEIGHT).px_1().child(
                h_flex()
                    .id(toggle_id)
                    .debug_selector(move || format!("history-section-header-{}", kind.i18n_key()))
                    .group(HISTORY_SECTION_HOVER_GROUP)
                    // Every other interactive row in this sidebar is a tab stop
                    // that Enter/Space activates; a collapse control that only
                    // answers the mouse would be the one exception.
                    .role(Role::Button)
                    .aria_label(header_label.clone())
                    .aria_expanded(open)
                    .track_focus(&focus_handle.tab_stop(true))
                    .focus_visible(|this| {
                        this.border_1().border_color(cx.theme().ring.opacity(0.2))
                    })
                    .h_full()
                    .items_center()
                    .gap_0p5()
                    .px_1()
                    .rounded(cx.theme().radius)
                    .cursor_default()
                    .text_xs()
                    .text_color(contrast::sidebar_muted_text(cx, 0.6))
                    .hover(|this| this.bg(tints.hover))
                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                        if crate::ui::consume_button_key(event, window, cx) {
                            workspace_for_key
                                .update(cx, |workspace, cx| {
                                    workspace.toggle_history_section(kind, cx)
                                })
                                .ok();
                        }
                    })
                    .on_click(move |_, _, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.toggle_history_section(kind, cx)
                            })
                            .ok();
                    })
                    .child(header_label)
                    .child(
                        div()
                            .invisible()
                            .group_hover(HISTORY_SECTION_HOVER_GROUP, |this| this.visible())
                            .child(Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })),
                    ),
            );
            let mut rows: Vec<AnyElement> = Vec::new();
            for row in section.rows {
                match row {
                    HistoryRow::Pending(conversation) => {
                        let target = conversation.id();
                        rows.push(
                            self.render_host_row(snapshot, conversation, target, window, cx)
                                .into_any_element(),
                        );
                    }
                    HistoryRow::Catalog(summary) => {
                        rows.push(
                            self.render_catalog_row(
                                snapshot,
                                summary,
                                active_session_id.as_ref(),
                                window,
                                cx,
                            )
                            .into_any_element(),
                        );
                    }
                }
            }
            children.push(
                Collapsible::new()
                    .open(open)
                    .w_full()
                    .child(header)
                    .content(v_flex().w_full().gap_1().children(rows))
                    .into_any_element(),
            );
        }

        let ready = matches!(snapshot.history().load_state(), HistoryLoadState::Ready);
        let error_and_empty = matches!(snapshot.history().load_state(), HistoryLoadState::Error(_))
            && snapshot.history().is_empty();
        let no_rows = snapshot.conversations().iter().all(|conversation| {
            !is_pending_history_conversation(conversation.session_id().as_ref(), snapshot.history())
        }) && snapshot.history().is_empty();

        if error_and_empty {
            children.push(self.render_history_error_state(cx).into_any_element());
        } else if ready && no_rows {
            children.push(self.render_history_empty_state(cx).into_any_element());
        } else if snapshot.history().has_more() {
            children.push(self.render_load_more_row(cx).into_any_element());
        }

        // While hover is parked no row binds a hover style, so nothing in the
        // list asks for a repaint when the pointer starts moving again.  This
        // listener exists only for those frames, and the frame it triggers
        // releases it.
        let pointer_parked = snapshot.parked_pointer() == Some(window.mouse_position());
        let workspace = self.chat_workspace().downgrade();

        v_flex()
            .id("chats")
            .debug_selector(|| "sidebar-list-surface".to_string())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_1()
            .when(pointer_parked, |this| {
                this.on_mouse_move(move |_, _, cx| {
                    workspace
                        .update(cx, |workspace, cx| workspace.release_parked_pointer(cx))
                        .ok();
                })
            })
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
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
            .child(Spinner::new().small())
            .child(t!("sidebar.loading_chats").to_string())
    }

    fn render_history_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_2()
            .py_3()
            .gap_1()
            .text_sm()
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
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
            .text_color(contrast::sidebar_muted_text(cx, 0.6))
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
            .text_color(contrast::sidebar_muted_text(cx, 0.7))
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

    fn render_host_row(
        &self,
        snapshot: &ChatWorkspaceSnapshot,
        conversation: &ChatConversationSnapshot,
        target: ConversationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = conversation.title();
        let is_active = snapshot.active() == Some(target);
        let is_generating = conversation.is_generating();
        let sidebar_target = ChatTarget::Conversation(target);
        let is_confirming = snapshot.confirming() == Some(&sidebar_target);
        let app = cx.entity().downgrade();

        self.render_history_row(
            ("conv-row", target.as_u64()),
            ("conv", target.as_u64()),
            title,
            is_active,
            is_generating,
            is_confirming,
            false,
            false,
            sidebar_target,
            {
                let app = app.clone();
                move |_, _, cx| {
                    app.update(cx, |app, cx| {
                        app.dispatch_workspace_command(
                            CHAT_WORKSPACE_ID,
                            WorkspaceCommand::SelectConversation(target),
                            None,
                            cx,
                        );
                    })
                    .ok();
                }
            },
            {
                let app = app.clone();
                move |event: &KeyDownEvent, window, cx| {
                    if crate::ui::consume_button_key(event, window, cx) {
                        app.update(cx, |app, cx| {
                            app.dispatch_workspace_command(
                                CHAT_WORKSPACE_ID,
                                WorkspaceCommand::SelectConversation(target),
                                None,
                                cx,
                            );
                        })
                        .ok();
                    }
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
                    .map(|preview| crate::chat::derive_title(preview))
            })
            .unwrap_or_else(|| t!("chat.default_title").to_string().into());

        let is_confirming = snapshot.confirming() == Some(&sidebar_target);
        let app = cx.entity().downgrade();

        self.render_history_row(
            format!("history-row-{session_id}"),
            format!("history-button-{session_id}"),
            title,
            is_active,
            is_generating,
            is_confirming,
            true,
            summary.favorited,
            sidebar_target.clone(),
            {
                let session_id = session_id.clone();
                {
                    let app = app.clone();
                    move |_, window, cx| {
                        app.update(cx, |app, cx| {
                            app.dispatch_workspace_command(
                                CHAT_WORKSPACE_ID,
                                WorkspaceCommand::RestoreChatSession(session_id.clone()),
                                Some(window),
                                cx,
                            );
                        })
                        .ok();
                    }
                }
            },
            {
                let session_id = session_id.clone();
                {
                    let app = app.clone();
                    move |event: &KeyDownEvent, window, cx| {
                        if crate::ui::consume_button_key(event, window, cx) {
                            app.update(cx, |app, cx| {
                                app.dispatch_workspace_command(
                                    CHAT_WORKSPACE_ID,
                                    WorkspaceCommand::RestoreChatSession(session_id.clone()),
                                    Some(window),
                                    cx,
                                );
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

    /// Shared row renderer for draft and catalog rows.  The row itself is the
    /// activation control and declares the hover group its own trailing chrome
    /// binds to, so revealing the actions costs no state and cannot go stale.
    /// The title always spans the row; the actions float above its trailing
    /// edge behind a fade, so the text dissolves under them instead of
    /// reflowing when they appear.  The two `on_click` / `on_key_down`
    /// callbacks are already in listener form (`Fn(&Event, &mut Window, &mut
    /// App)`); everything else (annotations, actions, focus ring) is identical.
    #[allow(clippy::too_many_arguments)]
    fn render_history_row(
        &self,
        row_id: impl Into<ElementId>,
        focus_key: impl Into<ElementId>,
        title: SharedString,
        is_active: bool,
        is_generating: bool,
        is_confirming: bool,
        can_favorite: bool,
        favorited: bool,
        target: ChatTarget,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_key_down: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_id: ElementId = row_id.into();
        let focus_handle = window
            .use_keyed_state(focus_key.into(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focus_ring = cx.theme().ring.opacity(0.2);
        let row_selector = chat_history_row_selector(&target);
        let reveal = RowReveal {
            hover_group: format!("{row_selector}-hover").into(),
            pinned: is_confirming,
            parked: !self
                .chat_snapshot()
                .row_takes_hover(&target, window.mouse_position()),
        };
        let hover_group = reveal.hover_group.clone();
        let tints = contrast::sidebar_row_tints(cx);
        let selected = is_active || is_confirming;
        // The fade has to match whatever tint the row is wearing when the
        // actions show, or it reads as a patch instead of the row fading out.
        let fade_tint = if selected {
            tints.selected
        } else {
            tints.hover
        };
        let (fade_width, fade_ramp_end) = history_actions_fade(can_favorite);

        let mut title_element = h_flex().min_w_0().flex_1().items_center().gap_2().child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .min_w_0()
                .flex_1()
                .child(title.clone()),
        );
        if is_generating {
            let generating_selector = chat_history_generating_selector(&target);
            // The spinner shares the trailing edge with the actions, so it
            // steps aside for them rather than sitting underneath.
            title_element = title_element.child(
                div()
                    .flex_none()
                    .debug_selector(move || generating_selector)
                    .hide_on_row_hover(&reveal)
                    .child(
                        Spinner::new()
                            .xsmall()
                            .color(contrast::sidebar_muted_text(cx, 0.6)),
                    ),
            );
        }

        div()
            .id(row_id)
            .debug_selector(move || row_selector)
            .group(hover_group.clone())
            .role(Role::Button)
            .aria_label(title.clone())
            .aria_selected(is_active)
            .track_focus(&focus_handle.tab_stop(true))
            .focus_visible(|this| this.border_1().border_color(focus_ring))
            .relative()
            .w_full()
            .h(HISTORY_ROW_HEIGHT)
            .flex()
            .items_center()
            .px_2()
            .rounded(cx.theme().radius)
            .text_sm()
            .text_color(contrast::sidebar_text(cx))
            .cursor_default()
            .whitespace_nowrap()
            .when(selected, |this| {
                this.bg(tints.selected).text_color(tints.selected_text)
            })
            .when(!selected && !reveal.parked, |this| {
                this.hover(|this| this.bg(tints.hover).text_color(tints.hover_text))
            })
            .on_key_down(on_key_down)
            .on_click(on_click)
            .child(title_element)
            // Painted between the title and the actions, and tinted with the
            // row's own hover background, so a long title fades out under the
            // buttons instead of being cut by a hard edge.  It carries the
            // row's radius so it cannot square off the corners, and no listener
            // or `occlude()`, so it can never block the row's hitbox.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(fade_width)
                    .rounded(cx.theme().radius)
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(fade_tint.opacity(0.), 0.),
                        linear_color_stop(fade_tint, fade_ramp_end),
                    ))
                    .reveal_on_row_hover(&reveal),
            )
            .child(
                h_flex()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right(HISTORY_ACTION_INSET)
                    .items_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(self.render_chat_sidebar_actions(
                        target,
                        &reveal,
                        can_favorite,
                        favorited,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_chat_sidebar_actions(
        &self,
        target: ChatTarget,
        reveal: &RowReveal,
        can_favorite: bool,
        favorited: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let confirming = reveal.pinned;
        let workspace = self.chat_workspace().downgrade();
        let ids = match &target {
            ChatTarget::Conversation(entity) => SidebarActionIds {
                trigger_id: ("conversation-actions", entity.as_u64()).into(),
                confirm_id: ("conversation-delete-confirm", entity.as_u64()).into(),
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
        let warning = cx.theme().warning;
        let trigger_debug_selector = ids.trigger_debug_selector.clone();
        let favorite_selector = chat_history_favorite_selector(&target);
        // The bundled `IconName::Delete` is the keyboard delete-key glyph,
        // which reads as "backspace" next to a title; a bin says "destroy this
        // conversation" and the danger colour says it once more.
        let delete = history_action_button(ids.trigger_id)
            .icon(
                Icon::default()
                    .path("icons/trash-2.svg")
                    .text_color(cx.theme().danger),
            )
            .tooltip(ids.delete_label.clone())
            .debug_selector(move || trigger_debug_selector.clone())
            .reveal_on_row_hover(reveal);
        let delete = if confirming {
            let target_for_open = target.clone();
            let target_for_confirm = target.clone();
            let on_clear_workspace = workspace.clone();
            let on_confirm_workspace = workspace.clone();
            InlineDeleteConfirmation::new(
                ids.confirm_id,
                delete,
                ids.confirm_title,
                t!("sidebar.delete_chat_cancel").to_string(),
                t!("sidebar.delete_chat_confirm").to_string(),
                self.chat_snapshot().delete_confirmation(),
            )
            .on_open_change(move |open: &bool, window, cx| {
                if !*open {
                    on_clear_workspace
                        .update(cx, |workspace, cx| {
                            workspace.clear_delete_confirmation(&target_for_open, window, cx)
                        })
                        .ok();
                }
            })
            .on_confirm(move |window, cx| {
                on_confirm_workspace
                    .update(cx, |workspace, cx| {
                        workspace.confirm_delete_target(target_for_confirm.clone(), window, cx)
                    })
                    .ok();
            })
            .into_any_element()
        } else {
            let target_for_begin = target.clone();
            let on_begin = workspace.clone();
            delete
                .on_click(move |_, window, cx| {
                    on_begin
                        .update(cx, |workspace, cx| {
                            workspace.begin_delete_confirmation(
                                target_for_begin.clone(),
                                window,
                                cx,
                            )
                        })
                        .ok();
                })
                .into_any_element()
        };

        let star = can_favorite.then(|| {
            let target_for_star = target;
            let on_star = workspace;
            // Amber in both states so the control is legible at a glance;
            // fill, not colour, is what carries the favorited state.
            let icon = Icon::new(if favorited {
                IconName::StarFill
            } else {
                IconName::Star
            })
            .text_color(warning);
            history_action_button(favorite_selector.clone())
                .debug_selector(move || favorite_selector)
                .icon(icon)
                .tooltip(
                    if favorited {
                        t!("sidebar.unfavorite_chat")
                    } else {
                        t!("sidebar.favorite_chat")
                    }
                    .to_string(),
                )
                .reveal_on_row_hover(reveal)
                .on_click(move |_, window, cx| {
                    on_star
                        .update(cx, |workspace, cx| {
                            workspace.toggle_favorite(target_for_star.clone(), window, cx)
                        })
                        .ok();
                })
        });

        h_flex()
            .gap(HISTORY_ACTION_GAP)
            .items_center()
            .child(delete)
            .children(star)
            .into_any_element()
    }
}

/// Trailing action button.  The box is pinned instead of left to `Button`'s
/// icon-only sizing: a delete trigger that opens a confirmation gains a
/// prepaint hook, which appends a canvas child, and `Button` then treats it as
/// a labelled button and swaps its square box for label padding — the row's
/// controls would resize the moment one of them is armed.
fn history_action_button(id: impl Into<ElementId>) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .size(HISTORY_ACTION_BUTTON)
        .p_0()
}

/// How a row's trailing chrome decides whether to show itself.
struct RowReveal {
    hover_group: SharedString,
    /// A confirmation is open: stay visible even with the pointer away, or the
    /// popover loses the trigger it is anchored to.
    pinned: bool,
    /// The list reordered under a stationary pointer and this row is not the
    /// one the user acted on: it slid under the cursor and was never pointed
    /// at, so it stays quiet until the pointer moves.
    parked: bool,
}

trait RevealOnRowHover: Styled + InteractiveElement + Sized {
    fn reveal_on_row_hover(self, reveal: &RowReveal) -> Self {
        if reveal.pinned {
            self
        } else if reveal.parked {
            self.invisible()
        } else {
            self.invisible()
                .group_hover(reveal.hover_group.clone(), |this| this.visible())
        }
    }

    /// The inverse: chrome that gives its place up to the trailing actions
    /// whenever those are showing.
    fn hide_on_row_hover(self, reveal: &RowReveal) -> Self {
        if reveal.pinned {
            self.invisible()
        } else if reveal.parked {
            self
        } else {
            self.group_hover(reveal.hover_group.clone(), |this| this.invisible())
        }
    }
}

impl<T: Styled + InteractiveElement + Sized> RevealOnRowHover for T {}

/// Width the trailing cluster covers, including its inset from the row edge.
fn history_actions_width(can_favorite: bool) -> Pixels {
    let cluster = HISTORY_ACTION_INSET + HISTORY_ACTION_BUTTON;
    if can_favorite {
        cluster + HISTORY_ACTION_GAP + HISTORY_ACTION_BUTTON
    } else {
        cluster
    }
}

/// Fade box in front of the trailing cluster: fully transparent where the
/// title still has to be readable, opaque everywhere the buttons sit.
fn history_actions_fade(can_favorite: bool) -> (Pixels, f32) {
    let width = HISTORY_ACTION_FADE_RAMP + history_actions_width(can_favorite);
    (width, HISTORY_ACTION_FADE_RAMP.as_f32() / width.as_f32())
}

/// Unbound drafts, and bound views that are not yet in the catalog snapshot.
/// Catalog rows are only real `SessionSummary` records.
pub(super) fn is_pending_history_conversation(
    session_id: Option<&SessionId>,
    history: &ChatHistorySidebar,
) -> bool {
    session_id.is_none_or(|id| !history.contains_session(id))
}

fn chat_history_generating_selector(target: &ChatTarget) -> String {
    match target {
        ChatTarget::Conversation(entity) => format!("conversation-generating-{}", entity.as_u64()),
        ChatTarget::Session(session) => format!("history-generating-{session}"),
    }
}

fn chat_history_row_selector(target: &ChatTarget) -> String {
    match target {
        ChatTarget::Conversation(entity) => format!("conversation-row-{}", entity.as_u64()),
        ChatTarget::Session(session) => format!("history-row-{session}"),
    }
}

fn chat_history_favorite_selector(target: &ChatTarget) -> String {
    match target {
        ChatTarget::Conversation(entity) => format!("conversation-favorite-{}", entity.as_u64()),
        ChatTarget::Session(session) => format!("history-favorite-{session}"),
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

#[cfg(test)]
mod trailing_geometry {
    use super::{HISTORY_ACTION_BUTTON, history_actions_fade, history_actions_width};
    use gpui::px;

    #[test]
    fn a_favoritable_row_reserves_both_buttons() {
        let with_star = history_actions_width(true);
        let without = history_actions_width(false);
        assert_eq!(with_star - without, HISTORY_ACTION_BUTTON + px(2.));
    }

    #[test]
    fn the_fade_is_opaque_before_the_buttons_begin() {
        for can_favorite in [false, true] {
            let (width, ramp_end) = history_actions_fade(can_favorite);
            let opaque_from = width * ramp_end;
            assert!(
                width - opaque_from >= history_actions_width(can_favorite),
                "the gradient must reach full opacity before the cluster starts"
            );
            assert!(ramp_end > 0. && ramp_end < 1.);
        }
    }
}

#[cfg(test)]
mod favorite_routing {
    use super::ChatHistorySidebar;
    use crate::session::{SessionDomain, SessionId, SessionSummary};
    use std::path::PathBuf;

    fn summary(created_at: i64, favorited: bool) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::new(SessionDomain::Chat),
            domain: SessionDomain::Chat,
            project: None,
            title: Some(format!("session {created_at}")),
            preview: None,
            model: None,
            total_tokens: 0,
            created_at,
            updated_at: created_at,
            favorited,
            jsonl_path: PathBuf::from("sessions/row.jsonl"),
        }
    }

    fn ids(rows: &[SessionSummary]) -> Vec<SessionId> {
        rows.iter().map(|row| row.session_id.clone()).collect()
    }

    /// A session lives in exactly one of the two lists, and unstarring returns
    /// it to its place in the timeline rather than to the end of it.
    #[test]
    fn starring_moves_a_row_between_the_lists_and_keeps_its_time_order() {
        let mut history = ChatHistorySidebar::new();
        let (old, middle, recent) = (summary(1, false), summary(2, false), summary(3, false));
        let middle_id = middle.session_id.clone();
        for row in [old.clone(), middle.clone(), recent.clone()] {
            history.upsert(row);
        }
        assert_eq!(
            ids(history.timeline()),
            ids(&[recent.clone(), middle.clone(), old.clone()]),
            "the timeline is newest first"
        );

        let mut starred = middle.clone();
        starred.favorited = true;
        history.upsert(starred.clone());
        assert_eq!(ids(history.favorites()), vec![middle_id.clone()]);
        assert_eq!(
            ids(history.timeline()),
            ids(&[recent.clone(), old.clone()]),
            "a starred session leaves its time bucket"
        );
        assert!(history.contains_session(&middle_id));

        history.upsert(middle);
        assert!(history.favorites().is_empty());
        assert_eq!(
            ids(history.timeline()),
            ids(&[recent, starred, old]),
            "unstarring returns the row to its created_at position"
        );
    }
}

#[cfg(test)]
mod pending_history_rows {
    use std::path::PathBuf;

    use super::is_pending_history_conversation;
    use crate::session::{SessionDomain, SessionId, SessionSummary};

    fn summary(session_id: SessionId) -> SessionSummary {
        SessionSummary {
            session_id,
            domain: SessionDomain::Chat,
            project: None,
            title: Some("cataloged".into()),
            preview: None,
            model: None,
            total_tokens: 0,
            created_at: 1,
            updated_at: 1,
            favorited: false,
            jsonl_path: PathBuf::from("sessions/cataloged.jsonl"),
        }
    }

    #[test]
    fn unbound_and_uncataloged_rows_stay_on_the_host_list() {
        let cataloged = SessionId::new(SessionDomain::Chat);
        let bound = SessionId::new(SessionDomain::Chat);
        let mut history = super::ChatHistorySidebar::new();
        history.upsert(summary(cataloged.clone()));
        assert!(is_pending_history_conversation(None, &history));
        assert!(is_pending_history_conversation(Some(&bound), &history));
        assert!(!is_pending_history_conversation(Some(&cataloged), &history));
    }
}
