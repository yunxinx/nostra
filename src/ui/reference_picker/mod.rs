//! Reusable `$` Chat message reference composer.
//!
//! The composer owns an Agent draft's Chat references: typing (or clicking)
//! `$` opens a searchable popover over the Chat history catalog. Selection
//! state is a typed [`ChatMessageRef`] plus discardable presentation metadata
//! only — message bodies live in the Chat store and are resolved by a future
//! Agent runtime through the reference capability, never copied here.
//!
//! All catalog search and exact reads run on the background executor through
//! the read-only [`SharedChatReferenceStore`] capability; render only reads
//! snapshots, and every asynchronous apply is guarded by a query generation so
//! stale results cannot overwrite newer state.

use std::{cell::RefCell, collections::HashSet, rc::Rc};

use chrono::{Datelike as _, TimeZone as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, App, AppContext as _, Context, ElementId, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, RenderOnce,
    SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Selectable, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::{List, ListDelegate, ListState},
    popover::Popover,
    tag::Tag,
    v_flex,
};
use rust_i18n::t;

use crate::llm::Role;
use crate::session::{
    ChatMessagePreview, ChatMessageRead, ChatMessageRef, ChatMessageReferenceStore,
    ChatMessageSearchCursor, ChatMessageSearchPage, ChatMessageSearchQuery,
    ChatMessageUnavailableReason, ChatReferenceError, SharedChatReferenceStore,
};
use crate::ui::popover::PopoverDismissHandle;

/// Width of the reference picker popover.
const PICKER_WIDTH: Pixels = px(400.);
/// Height of the reference picker popover; results scroll inside it.
const PICKER_HEIGHT: Pixels = px(340.);
/// Character shown on the trigger button and used to open the picker.
const TRIGGER_CHARACTER: char = '$';

type SelectHandler = Rc<dyn Fn(&ChatMessagePreview, &mut App)>;

// ---------------------------------------------------------------------------
// Draft state
// ---------------------------------------------------------------------------

/// A Chat reference held by an Agent draft.
///
/// This is presentation metadata only: the durable part is the typed
/// [`ChatMessageRef`]; the label fields are disposable display copies bounded
/// by the catalog projection. The canonical message body is never retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatReferenceDraft {
    pub reference: ChatMessageRef,
    pub session_title: Option<String>,
    pub snippet: Option<String>,
    pub timestamp: i64,
}

impl ChatReferenceDraft {
    /// Project a bounded exact read into a draft label. The read itself is
    /// dropped; only the reference and short display text survive.
    fn from_read(read: ChatMessageRead) -> Self {
        Self {
            reference: read.reference,
            session_title: read.session_title,
            snippet: read.message.preview(),
            timestamp: read.timestamp,
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
    /// branches never leak into the picker.
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
    /// No query yet (or blank query): nothing has been requested.
    Idle,
    /// A request for the current generation is in flight.
    Searching,
    /// The latest request succeeded; `next_cursor` may offer a next page.
    Ready,
    /// The latest search request failed.
    Failed,
}

/// UI-independent snapshot of the picker's search: query, keyset pagination,
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

    /// Begin loading the next keyset page. Returns `None` unless the previous
    /// page succeeded and offered a cursor.
    fn begin_load_more(&mut self) -> Option<(u64, ChatMessageSearchQuery)> {
        if self.status != ReferenceSearchStatus::Ready {
            return None;
        }
        let cursor = self.next_cursor.clone()?;
        let generation = self.next_generation();
        self.status = ReferenceSearchStatus::Searching;
        Some((
            generation,
            ChatMessageSearchQuery {
                cursor: Some(cursor),
                ..ChatMessageSearchQuery::new(self.query.clone())
            },
        ))
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

    /// Append a load-more page. Stale generations are ignored; rows already
    /// present are not re-inserted.
    fn apply_load_more(&mut self, generation: u64, page: ChatMessageSearchPage) -> bool {
        if generation != self.generation {
            return false;
        }
        let fresh: Vec<ChatMessagePreview> = page
            .messages
            .into_iter()
            .filter(|row| {
                !self
                    .results
                    .iter()
                    .any(|existing| existing.reference == row.reference)
            })
            .collect();
        self.results.extend(fresh);
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

    /// Mark an in-flight load-more failed. Rows stay so scrolling can retry;
    /// stale generations are ignored.
    fn fail_load_more(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.status = ReferenceSearchStatus::Ready;
        true
    }

    fn item(&self, ix: IndexPath) -> Option<&ChatMessagePreview> {
        self.results.get(ix.row)
    }

    fn is_loading(&self) -> bool {
        self.status == ReferenceSearchStatus::Searching && self.results.is_empty()
    }

    fn has_more(&self) -> bool {
        self.status == ReferenceSearchStatus::Ready && self.next_cursor.is_some()
    }
}

fn dedup_previews(previews: Vec<ChatMessagePreview>) -> Vec<ChatMessagePreview> {
    let mut seen = HashSet::with_capacity(previews.len());
    previews
        .into_iter()
        .filter(|row| seen.insert(row.reference.clone()))
        .collect()
}

/// A `$` opens the picker once per insertion: reopening requires the draft
/// value to first drop every `$`.
fn dollar_newly_present(previous: bool, value: &str) -> bool {
    !previous && value.contains(TRIGGER_CHARACTER)
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

// ---------------------------------------------------------------------------
// List row element
// ---------------------------------------------------------------------------

/// One search result row: role, source session title, local time, and the
/// bounded catalog snippet. `checked` marks an already-referenced message.
#[derive(IntoElement)]
struct ReferenceRow {
    id: ElementId,
    selected: bool,
    checked: bool,
    role: Role,
    title: SharedString,
    snippet: Option<SharedString>,
    time: String,
}

impl Selectable for ReferenceRow {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for ReferenceRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let role_tag = match self.role {
            Role::User => Tag::primary(),
            _ => Tag::secondary(),
        }
        .small()
        .rounded_full()
        .child(role_label(self.role));

        div()
            .id(self.id)
            .relative()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .when(!self.selected, |this| {
                this.hover(|this| this.bg(theme.accent.opacity(0.7)))
            })
            .when(self.selected, |this| this.bg(theme.accent))
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
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
                                    .text_color(theme.foreground)
                                    .child(self.title),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(self.time),
                            )
                            .when(self.checked, |this| {
                                this.child(
                                    Icon::new(IconName::Check)
                                        .size_4()
                                        .text_color(theme.primary),
                                )
                            }),
                    )
                    .child(h_flex().min_w_0().items_center().when_some(
                        self.snippet,
                        |this, snippet| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(snippet),
                            )
                        },
                    )),
            )
    }
}

// ---------------------------------------------------------------------------
// List delegate
// ---------------------------------------------------------------------------

/// Searchable-list delegate backing the reference picker. Search and
/// pagination run on the background executor through the shared read-only
/// capability; results land only when their query generation is current.
struct ChatReferenceListDelegate {
    search: ReferenceSearch,
    cursor: Option<IndexPath>,
    /// References already held by the composer's draft, shared so confirm
    /// checks render without moving state across entities.
    selected: Rc<RefCell<HashSet<ChatMessageRef>>>,
    on_select: SelectHandler,
}

impl ChatReferenceListDelegate {
    fn new(selected: Rc<RefCell<HashSet<ChatMessageRef>>>, on_select: SelectHandler) -> Self {
        Self {
            search: ReferenceSearch::new(),
            cursor: None,
            selected,
            on_select,
        }
    }

    fn initial_index(&self) -> Option<IndexPath> {
        (!self.search.results.is_empty()).then(IndexPath::default)
    }
}

fn chat_reference_store(cx: &App) -> Option<SharedChatReferenceStore> {
    cx.try_global::<crate::session::SessionStores>()
        .cloned()?
        .chat_references()
        .ok()
}

impl ListDelegate for ChatReferenceListDelegate {
    type Item = ReferenceRow;

    fn sections_count(&self, _: &App) -> usize {
        1
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.search.results.len()
    }

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        let Some((generation, request)) = self.search.begin(query) else {
            cx.notify();
            return Task::ready(());
        };
        let Some(store) = chat_reference_store(cx) else {
            self.search.fail(generation);
            cx.notify();
            return Task::ready(());
        };
        let search = cx
            .background_executor()
            .spawn(async move { store.search_chat_messages(request) });
        cx.spawn_in(window, async move |list, window| {
            let result = search.await;
            _ = list.update_in(window, |list, window, cx| {
                let delegate = list.delegate_mut();
                match result {
                    Ok(page) => {
                        delegate.search.apply_search(generation, page);
                    }
                    Err(error) => {
                        crate::logging::error(
                            "reference.picker",
                            format_args!("chat reference search failed: {error}"),
                        );
                        delegate.search.fail(generation);
                    }
                }
                let cursor = delegate.initial_index();
                list.set_selected_index(cursor, window, cx);
                cx.notify();
            });
        })
    }

    fn has_more(&self, _: &App) -> bool {
        self.search.has_more()
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some((generation, request)) = self.search.begin_load_more() else {
            return;
        };
        let Some(store) = chat_reference_store(cx) else {
            self.search.fail_load_more(generation);
            cx.notify();
            return;
        };
        let search = cx
            .background_executor()
            .spawn(async move { store.search_chat_messages(request) });
        // The list invokes `load_more` fire-and-forget, so this continuation
        // outlives the call. It is generation-guarded and idempotent, so a
        // detached apply cannot corrupt a newer query's snapshot.
        cx.spawn_in(window, async move |list, window| {
            let result = search.await;
            _ = list.update_in(window, |list, _, cx| {
                let delegate = list.delegate_mut();
                match result {
                    Ok(page) => {
                        delegate.search.apply_load_more(generation, page);
                    }
                    Err(error) => {
                        crate::logging::error(
                            "reference.picker",
                            format_args!("chat reference load more failed: {error}"),
                        );
                        delegate.search.fail_load_more(generation);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = self.search.item(ix)?;
        let checked = self.selected.borrow().contains(&row.reference);
        let title: SharedString = row
            .session_title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(
                || t!("reference_picker.untitled_chat").to_string(),
                ToOwned::to_owned,
            )
            .into();
        Some(ReferenceRow {
            id: ElementId::Name(SharedString::from(format!("reference-row-{}", ix.row))),
            selected: false,
            checked,
            role: row.role,
            title,
            snippet: row.preview.clone().map(SharedString::from),
            time: format_reference_time(chrono::Local::now().timestamp_millis(), row.timestamp),
        })
    }

    fn render_initial(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<gpui::AnyElement> {
        Some(
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .px_2()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::Inbox).size_8())
                .child(
                    div()
                        .text_xs()
                        .child(t!("reference_picker.initial_hint").to_string()),
                )
                .into_any_element(),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let failed = self.search.status == ReferenceSearchStatus::Failed;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .px_2()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Search).size_8())
            .child(div().text_xs().child(if failed {
                t!("reference_picker.error_search").to_string()
            } else {
                t!("reference_picker.empty").to_string()
            }))
    }

    fn loading(&self, _: &App) -> bool {
        self.search.is_loading()
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.cursor = ix;
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(row) = self.cursor.and_then(|ix| self.search.item(ix)) else {
            return;
        };
        // The composer owns the draft: route the typed reference there and let
        // its background read decide between adding the draft or surfacing a
        // typed error. The popover stays open for further selections.
        (self.on_select)(row, cx);
    }
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

/// Composer hosting the `$` reference picker: a draft input, the popover, and
/// the removable chip row of confirmed references.
///
/// Entities are created only in event handlers (`new`, `open_picker`) — never
/// in render. Store I/O always runs on the background executor.
pub(crate) struct ChatReferenceComposer {
    input: Entity<InputState>,
    list: Entity<ListState<ChatReferenceListDelegate>>,
    open: bool,
    /// Whether the draft value currently contains a `$`; suppresses reopening
    /// until every `$` has been removed.
    dollar_present: bool,
    drafts: Vec<ChatReferenceDraft>,
    selected: Rc<RefCell<HashSet<ChatMessageRef>>>,
    /// References with an in-flight exact read, so quick re-confirms cannot
    /// enqueue duplicate work.
    pending: HashSet<ChatMessageRef>,
    confirm_error: Option<ReferenceConfirmError>,
    popover: PopoverDismissHandle,
    /// In-flight exact reads. Stored so dropping the composer cancels them;
    /// completed handles are cheap to retain until the next confirm.
    _read_tasks: Vec<Task<()>>,
    _input_subscription: Subscription,
}

impl ChatReferenceComposer {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder: SharedString = t!("reference_picker.composer_placeholder")
            .to_string()
            .into();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let value = input.read(cx).value();
            if dollar_newly_present(this.dollar_present, value.as_ref()) {
                this.open_picker(window, cx);
            }
            this.dollar_present = value.contains(TRIGGER_CHARACTER);
        });

        let selected = Rc::new(RefCell::new(HashSet::new()));
        let list = new_reference_list(selected.clone(), select_handler(cx), window, cx);

        Self {
            input,
            list,
            open: false,
            dollar_present: false,
            drafts: Vec::new(),
            selected,
            pending: HashSet::new(),
            confirm_error: None,
            popover: PopoverDismissHandle::default(),
            _read_tasks: Vec::new(),
            _input_subscription: subscription,
        }
    }

    /// Programmatically close the picker (e.g. when leaving the workspace).
    pub(crate) fn dismiss_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.popover.dismiss(window, cx);
            cx.notify();
        }
    }

    /// Open the picker with a fresh search session.
    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = new_reference_list(Rc::clone(&self.selected), select_handler(cx), window, cx);
        defer_reference_list_focus(list.clone(), window, cx);
        self.list = list;
        self.confirm_error = None;
        self.open = true;
        cx.notify();
    }

    fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open {
            if !self.open {
                self.open_picker(window, cx);
            }
        } else if self.open {
            self.open = false;
            cx.notify();
        }
    }

    /// A row was confirmed in the picker. The typed reference is validated by
    /// a background exact read before it joins the draft, so unavailable and
    /// oversized sources surface as typed errors instead of ghost chips.
    fn handle_select(&mut self, row: &ChatMessagePreview, cx: &mut Context<Self>) {
        self.confirm_error = None;
        if self.selected.borrow().contains(&row.reference) || self.pending.contains(&row.reference)
        {
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
                if self.selected.borrow_mut().insert(draft.reference.clone()) {
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
    fn remove_draft(&mut self, reference: ChatMessageRef, cx: &mut Context<Self>) {
        self.pending.remove(&reference);
        self.drafts.retain(|draft| draft.reference != reference);
        self.selected.borrow_mut().remove(&reference);
        cx.notify();
    }

    fn render_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_wrap()
            .gap_1()
            .children(self.drafts.iter().enumerate().map(|(index, draft)| {
                let reference = draft.reference.clone();
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
                                        this.remove_draft(reference.clone(), cx);
                                    },
                                )),
                            ),
                    )
            }))
    }
}

fn select_handler(cx: &mut Context<ChatReferenceComposer>) -> SelectHandler {
    let this = cx.entity().downgrade();
    Rc::new(move |row: &ChatMessagePreview, cx: &mut App| {
        _ = this.update(cx, |composer, cx| composer.handle_select(row, cx));
    })
}

fn new_reference_list(
    selected: Rc<RefCell<HashSet<ChatMessageRef>>>,
    on_select: SelectHandler,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ListState<ChatReferenceListDelegate>> {
    cx.new(|cx| {
        ListState::new(
            ChatReferenceListDelegate::new(selected, on_select),
            window,
            cx,
        )
        .searchable(true)
    })
}

fn defer_reference_list_focus(
    list: Entity<ListState<ChatReferenceListDelegate>>,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        list.update(cx, |list, cx| list.focus(window, cx));
    });
}

impl Render for ChatReferenceComposer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.list.clone();
        let popover = self.popover.clone();
        let confirm_error = self.confirm_error.clone();

        let picker =
            Popover::new("chat-reference-picker")
                .p_0()
                .text_sm()
                .anchor(Anchor::BottomLeft)
                .open(self.open)
                .on_open_change(cx.listener(|this, open, window, cx| {
                    this.set_open(*open, window, cx);
                }))
                .trigger(
                    Button::new("reference-trigger")
                        .ghost()
                        .small()
                        .label(TRIGGER_CHARACTER.to_string())
                        .tooltip(t!("reference_picker.trigger_tooltip").to_string()),
                )
                .track_focus(&self.list.focus_handle(cx))
                .content(move |_, _, cx| {
                    popover.bind(cx.weak_entity());
                    v_flex()
                        .size_full()
                        .min_h_0()
                        .child(List::new(&list).small().p_1().search_placeholder(
                            t!("reference_picker.search_placeholder").to_string(),
                        ))
                        .when_some(confirm_error.clone(), |this, error| {
                            this.child(
                                h_flex()
                                    .items_start()
                                    .gap_1p5()
                                    .border_t_1()
                                    .border_color(cx.theme().border)
                                    .px_2p5()
                                    .py_1p5()
                                    .child(
                                        Icon::new(IconName::Info)
                                            .size_4()
                                            .text_color(cx.theme().warning_foreground),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_xs()
                                            .text_color(cx.theme().foreground)
                                            .child(error.localized()),
                                    ),
                            )
                        })
                })
                .w(PICKER_WIDTH)
                .h(PICKER_HEIGHT);

        v_flex()
            .gap_1()
            .when(!self.drafts.is_empty(), |this| {
                this.child(self.render_chips(cx))
            })
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .child(picker)
                    .child(Input::new(&self.input).appearance(false).flex_1().min_w_0()),
            )
    }
}

#[cfg(test)]
mod tests;
