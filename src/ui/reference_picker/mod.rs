//! Agent draft composer with in-input `$` Chat reference completion.
//!
//! The composer mirrors the chat input card: a floating card holding the
//! reference chips, a multi-line input, and a toolbar row. Typing a `$` token
//! (`$` at a word boundary followed by non-whitespace query characters) opens
//! a completion popup the width of the card; results come from the read-only
//! Chat reference capability on the background executor, guarded by a query
//! generation so stale pages cannot land. Hits are grouped by Chat session.
//! Confirming a session row inserts a session-level chip (no exact read);
//! Tab/Right expands matching turns, and confirming a turn validates with a
//! background exact read. The draft never copies message bodies.
//!
//! The input binds Tab, arrows, and indent, so an open popup captures those
//! actions on this wrapper before Input handles them. Popup navigation also
//! uses `ctrl-n`/`ctrl-p`. Enter arrives as `InputEvent::PressEnter`.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use chrono::{Datelike as _, TimeZone as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Pixels, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, ThemeStyled as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{
        IndentInline, InputEvent, MoveLeft, MoveRight, OutdentInline, Textarea, TextareaState,
    },
    spinner::Spinner,
    tag::Tag,
    v_flex,
};
use rust_i18n::t;

use crate::llm::Role;
use crate::session::{
    ChatMessagePreview, ChatMessageRead, ChatMessageRef, ChatMessageReferenceStore,
    ChatMessageSearchCursor, ChatMessageSearchPage, ChatMessageSearchQuery,
    ChatMessageUnavailableReason, ChatReferenceError, ChatSessionRef, SessionId,
    SharedChatReferenceStore,
};

/// Max height of the popup result list; rows scroll inside it.
const POPUP_MAX_HEIGHT: Pixels = px(288.);
/// Trigger character typed in the input to open completion.
const TRIGGER_CHARACTER: char = '$';
/// A query longer than this is treated as ordinary text (no completion).
const MAX_QUERY_CHARS: usize = 64;
const COMPOSER_MAX_WIDTH: Pixels = px(760.);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ComposerStatus {
    pub pending: bool,
    pub send_disabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ComposerEvent {
    Submit(String),
    Stop,
}

// ---------------------------------------------------------------------------
// Draft state
// ---------------------------------------------------------------------------

/// Identity of a Chat reference held by an Agent draft. A session-level
/// chip and a message-level chip in the same chat are distinct.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ChatReferenceKind {
    Session(ChatSessionRef),
    Message(ChatMessageRef),
}

/// A Chat reference held by an Agent draft.
///
/// Presentation metadata only: the durable part is [`ChatReferenceKind`];
/// the label fields are disposable display copies. The canonical message
/// body is never retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatReferenceDraft {
    pub kind: ChatReferenceKind,
    pub session_title: Option<String>,
    pub snippet: Option<String>,
    pub timestamp: i64,
}

impl ChatReferenceDraft {
    /// Project a bounded exact read into a message-level draft label.
    fn from_read(read: ChatMessageRead) -> Self {
        Self {
            kind: ChatReferenceKind::Message(read.reference),
            session_title: read.session_title,
            snippet: read.message.preview(),
            timestamp: read.timestamp,
        }
    }

    /// Session-level chip: title only, no exact message read.
    fn from_session(reference: ChatSessionRef, title: Option<String>, timestamp: i64) -> Self {
        Self {
            kind: ChatReferenceKind::Session(reference),
            session_title: title,
            snippet: None,
            timestamp,
        }
    }

    /// Short chip label: the message snippet when available, otherwise the
    /// source session title.
    fn chip_label(&self) -> SharedString {
        let snippet = self
            .snippet
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        match snippet {
            Some(snippet) => first_line_bounded(snippet),
            None => self
                .session_title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map_or_else(
                    || t!("reference_picker.untitled_chat").to_string().into(),
                    first_line_bounded,
                ),
        }
    }
}

/// First line of `text`, bounded to a chip-friendly length.
fn first_line_bounded(text: &str) -> SharedString {
    let first_line = text.lines().next().unwrap_or_default();
    let mut label: String = first_line.chars().take(48).collect();
    if first_line.chars().count() > 48 || text.lines().count() > 1 {
        label.push('…');
    }
    label.into()
}

/// Why confirming a reference failed, in displayable terms.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReferenceConfirmError {
    TooLarge { limit: usize },
    Unavailable(ChatMessageUnavailableReason),
    Read,
}

impl ReferenceConfirmError {
    /// Map a typed store error to its displayable form. Transport and
    /// projection details collapse into the generic read failure so hidden
    /// branches never leak into the composer.
    fn from_store(error: &ChatReferenceError) -> Self {
        match error {
            ChatReferenceError::TooLarge { limit } => Self::TooLarge { limit: *limit },
            ChatReferenceError::Unavailable(unavailable) => Self::Unavailable(unavailable.reason),
            _ => Self::Read,
        }
    }

    fn localized(&self) -> String {
        match self {
            Self::TooLarge { limit } => {
                t!("reference_picker.error_too_large", limit = limit).to_string()
            }
            Self::Unavailable(ChatMessageUnavailableReason::SessionDeleted) => {
                t!("reference_picker.error_unavailable_session").to_string()
            }
            Self::Unavailable(ChatMessageUnavailableReason::MessageDeleted) => {
                t!("reference_picker.error_unavailable_message").to_string()
            }
            Self::Unavailable(ChatMessageUnavailableReason::SourceCorrupt) => {
                t!("reference_picker.error_unavailable_corrupt").to_string()
            }
            Self::Read => t!("reference_picker.error_read").to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Search snapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceSearchStatus {
    /// No query yet (blank token): nothing has been requested.
    Idle,
    /// A request for the current generation is in flight.
    Searching,
    /// The latest request succeeded; `next_cursor` may offer a next page.
    Ready,
    /// The latest search request failed.
    Failed,
}

/// UI-independent snapshot of the completion search: query, keyset pagination,
/// and a generation counter that invalidates results from older requests.
#[derive(Debug)]
struct ReferenceSearch {
    query: String,
    results: Vec<ChatMessagePreview>,
    next_cursor: Option<ChatMessageSearchCursor>,
    status: ReferenceSearchStatus,
    generation: u64,
}

impl ReferenceSearch {
    fn new() -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            next_cursor: None,
            status: ReferenceSearchStatus::Idle,
            generation: 0,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Begin a fresh query search. Returns `None` for a blank query — no
    /// store call is made and the snapshot resets to
    /// [`ReferenceSearchStatus::Idle`].
    fn begin(&mut self, query: &str) -> Option<(u64, ChatMessageSearchQuery)> {
        let query = query.trim();
        let generation = self.next_generation();
        self.query.clear();
        self.query.push_str(query);
        self.results.clear();
        self.next_cursor = None;
        if query.is_empty() {
            self.status = ReferenceSearchStatus::Idle;
            return None;
        }
        self.status = ReferenceSearchStatus::Searching;
        Some((generation, ChatMessageSearchQuery::new(query.to_string())))
    }

    /// Apply a fresh search page. Stale generations are ignored.
    fn apply_search(&mut self, generation: u64, page: ChatMessageSearchPage) -> bool {
        if generation != self.generation {
            return false;
        }
        self.results = dedup_previews(page.messages);
        self.next_cursor = page.next_cursor;
        self.status = ReferenceSearchStatus::Ready;
        true
    }

    /// Mark the in-flight search failed. Stale generations are ignored.
    fn fail(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.status = ReferenceSearchStatus::Failed;
        true
    }

    fn is_loading(&self) -> bool {
        self.status == ReferenceSearchStatus::Searching && self.results.is_empty()
    }
}

fn dedup_previews(previews: Vec<ChatMessagePreview>) -> Vec<ChatMessagePreview> {
    let mut seen = HashSet::with_capacity(previews.len());
    previews
        .into_iter()
        .filter(|row| seen.insert(row.reference.clone()))
        .collect()
}

/// Search hits grouped by Chat session, preserving first-seen order.
#[derive(Clone, Debug)]
struct SessionGroup {
    session_id: SessionId,
    session_title: Option<String>,
    timestamp: i64,
    messages: Vec<usize>,
}

fn group_search_results(results: &[ChatMessagePreview]) -> Vec<SessionGroup> {
    let mut groups: Vec<SessionGroup> = Vec::new();
    let mut index_by_session: HashMap<SessionId, usize> = HashMap::new();
    for (index, row) in results.iter().enumerate() {
        let session_id = row.reference.session_id.clone();
        if let Some(&group_index) = index_by_session.get(&session_id) {
            groups[group_index].messages.push(index);
            if row.timestamp > groups[group_index].timestamp {
                groups[group_index].timestamp = row.timestamp;
            }
            if groups[group_index].session_title.is_none() {
                groups[group_index].session_title = row.session_title.clone();
            }
        } else {
            index_by_session.insert(session_id.clone(), groups.len());
            groups.push(SessionGroup {
                session_id,
                session_title: row.session_title.clone(),
                timestamp: row.timestamp,
                messages: vec![index],
            });
        }
    }
    groups
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisibleCompletionRow {
    Session { group_index: usize },
    Message { result_index: usize },
}

// ---------------------------------------------------------------------------
// $ token parsing
// ---------------------------------------------------------------------------

/// The `$query` token the caret currently sits in. Offsets are byte indexes
/// into the input value; `start` points at the `$`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveToken {
    start: usize,
    end: usize,
    query: String,
}

/// Parse the completion token at `cursor` (a byte offset). A token is active
/// when the text back to the nearest `$` contains no whitespace, and the `$`
/// itself sits at a word boundary (text start or after whitespace).
fn active_dollar_token(text: &str, cursor: usize) -> Option<ActiveToken> {
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];
    let mut tail_start = cursor;
    for (index, ch) in before.char_indices().rev() {
        if ch.is_whitespace() || ch == TRIGGER_CHARACTER {
            break;
        }
        tail_start = index;
    }
    if tail_start == 0 {
        return None;
    }
    let dollar_at = tail_start - 1;
    if before.as_bytes().get(dollar_at) != Some(&b'$') {
        return None;
    }
    let head = &before[..dollar_at];
    let at_boundary = head.is_empty() || head.chars().next_back().is_some_and(char::is_whitespace);
    if !at_boundary {
        return None;
    }
    let query = &before[tail_start..cursor];
    if query.chars().count() > MAX_QUERY_CHARS {
        return None;
    }
    Some(ActiveToken {
        start: dollar_at,
        end: cursor,
        query: query.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Time formatting
// ---------------------------------------------------------------------------

/// Compact local rendering of a message timestamp: time for today, date+time
/// within the current year, date only for older years.
fn format_reference_time(now_millis: i64, timestamp_millis: i64) -> String {
    let (Some(now), Some(then)) = (
        chrono::Local.timestamp_millis_opt(now_millis).single(),
        chrono::Local
            .timestamp_millis_opt(timestamp_millis)
            .single(),
    ) else {
        return String::new();
    };
    let pattern = if then.year() == now.year() && then.ordinal() == now.ordinal() {
        "%H:%M"
    } else if then.year() == now.year() {
        "%m-%d %H:%M"
    } else {
        "%Y-%m-%d"
    };
    then.format(pattern).to_string()
}

fn role_label(role: Role) -> SharedString {
    match role {
        Role::User => t!("reference_picker.role_user"),
        Role::Assistant => t!("reference_picker.role_assistant"),
        Role::System => t!("reference_picker.role_system"),
        Role::Developer => t!("reference_picker.role_developer"),
        Role::Tool => t!("reference_picker.role_tool"),
    }
    .to_string()
    .into()
}

fn chat_reference_store(cx: &App) -> Option<SharedChatReferenceStore> {
    cx.try_global::<crate::session::SessionStores>()
        .cloned()?
        .chat_references()
        .ok()
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

/// Live state of the `$` completion popup.
#[derive(Debug)]
struct CompletionState {
    token: Option<ActiveToken>,
    search: ReferenceSearch,
    cursor: usize,
    expanded_session: Option<SessionId>,
}

/// Chat-style draft composer for the Agent workspace: reference chips, a
/// multi-line input, a toolbar row, and the `$` completion popup that floats
/// above the card while the caret is inside a `$token`.
///
/// Entities are created only in the constructor — never in render. All store
/// I/O runs on the background executor; each search task is stored so a newer
/// token drops (cancels) the previous request, and a query generation guards
/// late results on top of that.
pub(crate) struct ChatReferenceComposer {
    input: Entity<TextareaState>,
    references_enabled: bool,
    status: Rc<Cell<ComposerStatus>>,
    completion: CompletionState,
    drafts: Vec<ChatReferenceDraft>,
    selected: HashSet<ChatReferenceKind>,
    /// Message references with an in-flight exact read, so quick re-confirms
    /// cannot enqueue duplicate work.
    pending: HashSet<ChatMessageRef>,
    confirm_error: Option<ReferenceConfirmError>,
    _search_task: Option<Task<()>>,
    /// In-flight exact reads. Stored so dropping the composer cancels them;
    /// completed handles are cheap to retain until the next confirm.
    _read_tasks: Vec<Task<()>>,
    _input_subscription: Subscription,
}

impl ChatReferenceComposer {
    #[allow(dead_code)]
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_references(Rc::new(Cell::new(ComposerStatus::default())), window, cx)
    }

    pub(crate) fn chat(
        status: Rc<Cell<ComposerStatus>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(false, status, window, cx)
    }

    pub(crate) fn with_references(
        status: Rc<Cell<ComposerStatus>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(true, status, window, cx)
    }

    fn build(
        references_enabled: bool,
        status: Rc<Cell<ComposerStatus>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder: SharedString = if references_enabled {
            t!("reference_picker.composer_placeholder").to_string()
        } else {
            t!("chat.placeholder").to_string()
        }
        .into();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(placeholder)
        });
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change if this.references_enabled => {
                        this.sync_completion_with_input(input, window, cx)
                    }
                    InputEvent::PressEnter { shift: false, .. } if this.completion.is_open() => {
                        this.confirm_completion(window, cx);
                    }
                    InputEvent::PressEnter { shift: false, .. } => {
                        let text = input.read(cx).value().trim().to_string();
                        if !text.is_empty() && !this.status.get().send_disabled {
                            cx.emit(ComposerEvent::Submit(text));
                        }
                    }
                    _ => {}
                },
            );

        Self {
            input,
            references_enabled,
            status,
            completion: CompletionState {
                token: None,
                search: ReferenceSearch::new(),
                cursor: 0,
                expanded_session: None,
            },
            drafts: Vec::new(),
            selected: HashSet::new(),
            pending: HashSet::new(),
            confirm_error: None,
            _search_task: None,
            _read_tasks: Vec::new(),
            _input_subscription: subscription,
        }
    }

    pub(crate) fn input(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    #[cfg(test)]
    pub(crate) fn references_enabled(&self) -> bool {
        self.references_enabled
    }

    pub(crate) fn set_placeholder(
        &self,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx)
        });
    }

    pub(crate) fn clear_after_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.drafts.clear();
        self.selected.clear();
        self.pending.clear();
        self.confirm_error = None;
        self.close_completion_popup();
        cx.notify();
    }

    /// True while the completion popup should render.
    pub(crate) fn is_completion_open(&self) -> bool {
        self.completion.is_open()
    }

    /// Focus the draft input (e.g. after starting a new conversation draft).
    pub(crate) fn focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Programmatically close the popup (e.g. when leaving the workspace).
    pub(crate) fn dismiss_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.is_open() {
            self.close_completion_popup();
            cx.notify();
        }
    }

    fn close_completion_popup(&mut self) {
        self.completion.token = None;
        self.completion.expanded_session = None;
        self._search_task = None;
    }

    fn visible_completion_rows(&self) -> Vec<VisibleCompletionRow> {
        let groups = group_search_results(&self.completion.search.results);
        let mut rows = Vec::new();
        for (group_index, group) in groups.iter().enumerate() {
            rows.push(VisibleCompletionRow::Session { group_index });
            if self.completion.expanded_session.as_ref() == Some(&group.session_id) {
                for &result_index in &group.messages {
                    rows.push(VisibleCompletionRow::Message { result_index });
                }
            }
        }
        rows
    }

    // ----- completion flow -----

    fn sync_completion_with_input(
        &mut self,
        input: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (value, cursor) = {
            let state = input.read(cx);
            (state.value(), state.cursor())
        };
        let token = active_dollar_token(&value, cursor);
        let changed = self.completion.token != token;
        self.completion.token = token.clone();
        if !changed {
            return;
        }
        self.completion.cursor = 0;
        self.completion.expanded_session = None;
        match token {
            None => {
                self._search_task = None;
            }
            Some(_) => {
                // `begin` resets the snapshot for the new token and decides
                // whether a store request is warranted (blank query → hint).
                self.start_search(window, cx);
            }
        }
        cx.notify();
    }

    fn start_search(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(token) = self.completion.token.clone() else {
            return;
        };
        let Some((generation, request)) = self.completion.search.begin(&token.query) else {
            // Blank query: no store call; the popup shows the typing hint.
            return;
        };
        let Some(store) = chat_reference_store(cx) else {
            self.completion.search.fail(generation);
            cx.notify();
            return;
        };
        self._search_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { store.search_chat_messages(request) })
                .await;
            _ = this.update(cx, |composer, cx| {
                match result {
                    Ok(page) => {
                        composer.completion.search.apply_search(generation, page);
                    }
                    Err(error) => {
                        crate::logging::error(
                            "reference.picker",
                            format_args!("chat reference search failed: {error}"),
                        );
                        composer.completion.search.fail(generation);
                    }
                }
                if composer.completion.token.is_some() {
                    composer.completion.cursor = 0;
                }
                cx.notify();
            });
        }));
    }

    /// Move the popup cursor by `delta`, clamped to visible session/turn rows.
    fn move_completion_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.completion.is_open() {
            return;
        }
        let len = self.visible_completion_rows().len();
        if len == 0 {
            return;
        }
        let current = self.completion.cursor.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        if next != self.completion.cursor {
            self.completion.cursor = next;
            cx.notify();
        }
    }

    fn expand_highlighted_session(&mut self, cx: &mut Context<Self>) {
        let rows = self.visible_completion_rows();
        let Some(row) = rows.get(self.completion.cursor).copied() else {
            return;
        };
        match row {
            VisibleCompletionRow::Session { group_index } => {
                let groups = group_search_results(&self.completion.search.results);
                let Some(group) = groups.get(group_index) else {
                    return;
                };
                if self.completion.expanded_session.as_ref() == Some(&group.session_id) {
                    if !group.messages.is_empty() {
                        self.completion.cursor += 1;
                        cx.notify();
                    }
                    return;
                }
                self.completion.expanded_session = Some(group.session_id.clone());
                cx.notify();
            }
            VisibleCompletionRow::Message { .. } => {}
        }
    }

    fn collapse_highlighted_session(&mut self, cx: &mut Context<Self>) {
        let rows = self.visible_completion_rows();
        let Some(row) = rows.get(self.completion.cursor).copied() else {
            return;
        };
        match row {
            VisibleCompletionRow::Message { .. } => {
                let Some(session_id) = self.completion.expanded_session.clone() else {
                    return;
                };
                self.completion.expanded_session = None;
                let groups = group_search_results(&self.completion.search.results);
                let rows = self.visible_completion_rows();
                if let Some(index) = rows.iter().position(|row| match row {
                    VisibleCompletionRow::Session { group_index } => groups
                        .get(*group_index)
                        .is_some_and(|group| group.session_id == session_id),
                    VisibleCompletionRow::Message { .. } => false,
                }) {
                    self.completion.cursor = index;
                }
                cx.notify();
            }
            VisibleCompletionRow::Session { group_index } => {
                let groups = group_search_results(&self.completion.search.results);
                let Some(group) = groups.get(group_index) else {
                    return;
                };
                if self.completion.expanded_session.as_ref() == Some(&group.session_id) {
                    self.completion.expanded_session = None;
                    cx.notify();
                }
            }
        }
    }

    /// Confirm the row under the popup cursor (Enter).
    fn confirm_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.visible_completion_rows();
        let Some(row) = rows.get(self.completion.cursor).copied() else {
            return;
        };
        match row {
            VisibleCompletionRow::Session { group_index } => {
                let groups = group_search_results(&self.completion.search.results);
                let Some(group) = groups.get(group_index).cloned() else {
                    return;
                };
                self.complete_session(&group, window, cx);
            }
            VisibleCompletionRow::Message { result_index } => {
                let Some(preview) = self.completion.search.results.get(result_index).cloned()
                else {
                    return;
                };
                self.complete_message(&preview, window, cx);
            }
        }
    }

    fn remove_active_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (value, cursor) = {
            let state = self.input.read(cx);
            (state.value(), state.cursor())
        };
        if let Some(token) = active_dollar_token(&value, cursor) {
            let cursor_after_removal = token.start;
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..token.start]);
            new_value.push_str(&value[token.end..]);
            self.input.update(cx, |state, cx| {
                state.replace_all(new_value, window, cx);
                state.set_selected_range(cursor_after_removal..cursor_after_removal, cx);
            });
        }
    }

    fn complete_session(
        &mut self,
        group: &SessionGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(reference) = ChatSessionRef::new(group.session_id.clone()) else {
            return;
        };
        self.remove_active_token(window, cx);
        self.close_completion_popup();
        self.confirm_error = None;
        let kind = ChatReferenceKind::Session(reference.clone());
        if self.selected.insert(kind.clone()) {
            self.drafts.push(ChatReferenceDraft::from_session(
                reference,
                group.session_title.clone(),
                group.timestamp,
            ));
        }
        cx.notify();
    }

    fn complete_message(
        &mut self,
        row: &ChatMessagePreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_active_token(window, cx);
        // `replace_all` does not emit InputEvent::Change, so close the popup
        // explicitly. Leaving `token` set would keep a stale overlay open.
        self.close_completion_popup();
        self.handle_select(row, cx);
    }

    // ----- reference selection -----

    /// A row was confirmed. The typed reference is validated by a background
    /// exact read before it joins the draft, so unavailable and oversized
    /// sources surface as typed errors instead of ghost chips.
    fn handle_select(&mut self, row: &ChatMessagePreview, cx: &mut Context<Self>) {
        self.confirm_error = None;
        let kind = ChatReferenceKind::Message(row.reference.clone());
        if self.selected.contains(&kind) || self.pending.contains(&row.reference) {
            return;
        }
        let Some(store) = chat_reference_store(cx) else {
            self.confirm_error = Some(ReferenceConfirmError::Read);
            cx.notify();
            return;
        };
        self.pending.insert(row.reference.clone());
        let read_reference = row.reference.clone();
        let applied_reference = row.reference.clone();
        let task = cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { store.read_chat_message(&read_reference) });
            let result = read.await;
            _ = this.update(cx, |composer, cx| {
                composer.apply_read(applied_reference, result, cx)
            });
        });
        self._read_tasks.push(task);
        cx.notify();
    }

    fn apply_read(
        &mut self,
        reference: ChatMessageRef,
        result: Result<ChatMessageRead, ChatReferenceError>,
        cx: &mut Context<Self>,
    ) {
        self.pending.remove(&reference);
        match result {
            Ok(read) => {
                let draft = ChatReferenceDraft::from_read(read);
                if self.selected.insert(draft.kind.clone()) {
                    self.drafts.push(draft);
                }
            }
            Err(error) => {
                crate::logging::error(
                    "reference.picker",
                    format_args!("chat reference read failed: {error}"),
                );
                self.confirm_error = Some(ReferenceConfirmError::from_store(&error));
            }
        }
        cx.notify();
    }

    /// Remove a reference from the draft. This only touches local draft
    /// state; the Chat source is untouched.
    fn remove_draft(&mut self, kind: ChatReferenceKind, cx: &mut Context<Self>) {
        if let ChatReferenceKind::Message(reference) = &kind {
            self.pending.remove(reference);
        }
        self.drafts.retain(|draft| draft.kind != kind);
        self.selected.remove(&kind);
        cx.notify();
    }

    // ----- rendering -----

    fn render_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_wrap()
            .gap_1()
            .px_1p5()
            .pt_1()
            .children(self.drafts.iter().enumerate().map(|(index, draft)| {
                let kind = draft.kind.clone();
                Tag::secondary()
                    .small()
                    .rounded_full()
                    .max_w(px(280.))
                    .child(
                        h_flex()
                            .min_w_0()
                            .items_center()
                            .gap_1()
                            .child(div().min_w_0().truncate().child(draft.chip_label()))
                            .child(
                                Button::new(SharedString::from(format!(
                                    "reference-chip-remove-{index}"
                                )))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(t!("reference_picker.remove").to_string())
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.remove_draft(kind.clone(), cx);
                                    },
                                )),
                            ),
                    )
            }))
    }

    fn render_session_completion_row(
        &self,
        visible_index: usize,
        group: &SessionGroup,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let title: SharedString = group
            .session_title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(
                || t!("reference_picker.untitled_chat").to_string(),
                ToOwned::to_owned,
            )
            .into();
        let kind = ChatSessionRef::new(group.session_id.clone())
            .ok()
            .map(ChatReferenceKind::Session);
        let checked = kind
            .as_ref()
            .is_some_and(|kind| self.selected.contains(kind));
        let selected = visible_index == self.completion.cursor;
        let expanded = self.completion.expanded_session.as_ref() == Some(&group.session_id);
        let chevron = if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        let session_id = group.session_id.clone();
        let confirm_group = group.clone();
        let time = format_reference_time(chrono::Local::now().timestamp_millis(), group.timestamp);

        div()
            .id(SharedString::from(format!(
                "reference-session-{}",
                group.session_id
            )))
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .cursor_default()
            .when(!selected, |this| {
                this.hover(|this| this.bg(theme.accent.opacity(0.7)))
            })
            .when(selected, |this| this.bg(theme.accent))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.complete_session(&confirm_group, window, cx);
            }))
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "reference-session-expand-{}",
                                group.session_id
                            )))
                            .cursor_default()
                            .p_0p5()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if this.completion.expanded_session.as_ref() == Some(&session_id) {
                                    this.completion.expanded_session = None;
                                } else {
                                    this.completion.expanded_session = Some(session_id.clone());
                                }
                                let groups = group_search_results(&this.completion.search.results);
                                let rows = this.visible_completion_rows();
                                if let Some(index) = rows.iter().position(|row| match row {
                                    VisibleCompletionRow::Session { group_index } => groups
                                        .get(*group_index)
                                        .is_some_and(|group| group.session_id == session_id),
                                    VisibleCompletionRow::Message { .. } => false,
                                }) {
                                    this.completion.cursor = index;
                                }
                                cx.notify();
                            }))
                            .child(
                                Icon::new(chevron)
                                    .size_3p5()
                                    .text_color(theme.muted_foreground),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(time),
                    )
                    .when(checked, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size_4()
                                .text_color(theme.primary),
                        )
                    }),
            )
    }

    fn render_message_completion_row(
        &self,
        visible_index: usize,
        result_index: usize,
        row: &ChatMessagePreview,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let role_tag = match row.role {
            Role::User => Tag::primary(),
            _ => Tag::secondary(),
        }
        .small()
        .rounded_full()
        .child(role_label(row.role));
        let checked = self
            .selected
            .contains(&ChatReferenceKind::Message(row.reference.clone()));
        let selected = visible_index == self.completion.cursor;
        let time = format_reference_time(chrono::Local::now().timestamp_millis(), row.timestamp);

        div()
            .id(SharedString::from(format!(
                "reference-message-{}",
                row.reference.entry_id
            )))
            .ml_4()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .when(!selected, |this| {
                this.hover(|this| this.bg(theme.accent.opacity(0.7)))
            })
            .when(selected, |this| this.bg(theme.accent))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.completion.cursor = visible_index;
                if let Some(row) = this.completion.search.results.get(result_index).cloned() {
                    this.complete_message(&row, window, cx);
                }
            }))
            .child(
                v_flex().min_w_0().gap_0p5().child(
                    h_flex()
                        .min_w_0()
                        .items_center()
                        .gap_1p5()
                        .child(role_tag)
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(row.preview.clone().unwrap_or_default()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(time),
                        )
                        .when(checked, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size_4()
                                    .text_color(theme.primary),
                            )
                        }),
                ),
            )
    }

    fn render_completion_popup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let search = &self.completion.search;

        let body: Vec<gpui::AnyElement> = if search.is_loading() {
            vec![
                h_flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .py_4()
                    .text_color(theme.muted_foreground)
                    .child(Spinner::new().small())
                    .into_any_element(),
            ]
        } else if search.results.is_empty() {
            let (icon, message) = match search.status {
                ReferenceSearchStatus::Failed => (
                    IconName::Info,
                    t!("reference_picker.error_search").to_string(),
                ),
                ReferenceSearchStatus::Idle => {
                    (IconName::Search, t!("reference_picker.hint").to_string())
                }
                _ => (IconName::Search, t!("reference_picker.empty").to_string()),
            };
            vec![
                v_flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .py_4()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(icon).size_6())
                    .child(div().text_xs().child(message))
                    .into_any_element(),
            ]
        } else {
            let groups = group_search_results(&search.results);
            let rows = self.visible_completion_rows();
            rows.iter()
                .enumerate()
                .filter_map(|(visible_index, row)| match *row {
                    VisibleCompletionRow::Session { group_index } => {
                        groups.get(group_index).map(|group| {
                            self.render_session_completion_row(visible_index, group, cx)
                                .into_any_element()
                        })
                    }
                    VisibleCompletionRow::Message { result_index } => {
                        search.results.get(result_index).map(|preview| {
                            self.render_message_completion_row(
                                visible_index,
                                result_index,
                                preview,
                                cx,
                            )
                            .into_any_element()
                        })
                    }
                })
                .collect()
        };

        v_flex()
            .id("reference-completion")
            .w_full()
            .max_h(POPUP_MAX_HEIGHT)
            .overflow_y_scroll()
            .p_1()
            .child(v_flex().children(body))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let status = self.status.get();
        h_flex()
            .px_1p5()
            .items_center()
            .gap_1()
            .when_some(self.confirm_error.as_ref(), |this, error| {
                this.child(
                    h_flex()
                        .min_w_0()
                        .flex_1()
                        .items_center()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Info)
                                .size_3p5()
                                .text_color(theme.warning_foreground),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(error.localized()),
                        ),
                )
            })
            .child(
                Button::new("attach")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip(t!("chat.attach").to_string()),
            )
            .child(div().flex_1())
            .when(status.pending, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("chat.generating").to_string()),
                )
            })
            .child(if status.pending {
                Button::new("stop")
                    .primary()
                    .icon(IconName::Close)
                    .small()
                    .tooltip(t!("chat.stop_tooltip").to_string())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
            } else {
                Button::new("send")
                    .primary()
                    .small()
                    .icon(IconName::ArrowUp)
                    .disabled(status.send_disabled)
                    .tooltip(t!("chat.send_tooltip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let text = this.input.read(cx).value().trim().to_string();
                        if !text.is_empty() && !this.status.get().send_disabled {
                            cx.emit(ComposerEvent::Submit(text));
                        }
                    }))
            })
    }
}

impl CompletionState {
    fn is_open(&self) -> bool {
        self.token.is_some()
    }
}

impl Render for ChatReferenceComposer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let completion_open = self.references_enabled && self.is_completion_open();
        let (background, border, radius_lg) = {
            let theme = cx.theme();
            (theme.background, theme.border, theme.radius_lg)
        };

        h_flex()
            .w_full()
            .justify_center()
            .px_6()
            .pt_2()
            .pb_3()
            .child(
                div()
                    .id("conversation-composer")
                    .relative()
                    .w_full()
                    .max_w(COMPOSER_MAX_WIDTH)
                    .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                        if this.is_completion_open() {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.expand_highlighted_session(cx);
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                        if this.is_completion_open() {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.collapse_highlighted_session(cx);
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &MoveRight, window, cx| {
                        if this.is_completion_open() {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.expand_highlighted_session(cx);
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &MoveLeft, window, cx| {
                        if this.is_completion_open() {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.collapse_highlighted_session(cx);
                        }
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if !this.is_completion_open() {
                            return;
                        }
                        let key = event.keystroke.key.as_str();
                        let control = event.keystroke.modifiers.control;
                        if key == "escape" {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.dismiss_completion(cx);
                        } else if control && key == "n" {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.move_completion_cursor(1, cx);
                        } else if control && key == "p" {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.move_completion_cursor(-1, cx);
                        }
                    }))
                    .when(completion_open, |this| {
                        this.child(
                            div()
                                .absolute()
                                .bottom_full()
                                .left_0()
                                .w_full()
                                .mb_2()
                                .popover_style(cx)
                                .shadow_md()
                                .child(self.render_completion_popup(cx)),
                        )
                    })
                    .child(
                        v_flex()
                            .w_full()
                            .gap_0p5()
                            .bg(background)
                            .border_1()
                            .border_color(border)
                            .rounded(radius_lg)
                            .shadow_md()
                            .py_1()
                            .when(self.references_enabled && !self.drafts.is_empty(), |this| {
                                this.child(self.render_chips(cx))
                            })
                            .child(
                                Textarea::new(&self.input)
                                    .appearance(false)
                                    .font_family(crate::appearance::fonts::active(cx).family())
                                    .pr(px(8.)),
                            )
                            .child(self.render_toolbar(cx)),
                    ),
            )
    }
}

impl EventEmitter<ComposerEvent> for ChatReferenceComposer {}

#[cfg(test)]
mod tests;
