//! Single conversation view: message list, streaming assistant turn, and
//! composer input.
//!
//! [`ChatView`] owns the transcript and the composer. The pieces a turn is made
//! of live beside it: [`assistant`] bridges gateway events into this view's
//! entities, and [`error_card`] / [`reasoning_card`] render the two turn parts
//! that need more than markdown prose.

mod assistant;
mod error_card;
mod reasoning_card;

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, ScrollHandle,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, point, px,
};
use gpui_component::{
    ActiveTheme, Disableable as _, ElementExt as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, RopeExt as _},
    scroll::ScrollableElement as _,
    text::TextViewStyle,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::fonts;
use crate::llm::{
    ContentBlock, GatewayError, IndexedMessage, Message as LlmMessage, ModelSelection,
    ProviderMetadata, ReasoningContent, ToolCall, ToolResult,
};
use crate::providers;
use crate::ui::markdown::MarkdownBody;

use self::error_card::TurnError;
use self::reasoning_card::ReasoningTrace;

const CONTENT_MAX_WIDTH: Pixels = px(760.);

/// While streaming, the view re-pins to the bottom only when it is already
/// within this distance of the end — used both as the pin guard and as the
/// "scrolled back down" re-arm threshold for [`ChatView::follow`].
const STICK_THRESHOLD: Pixels = px(48.);

/// First-frame fallback until the floating composer reports its actual height.
const DEFAULT_COMPOSER_HEIGHT: Pixels = px(120.);

/// Deliberately over-scrolled deferred target for the composer viewport.
/// `InputState::set_scroll_offset` applies it after the next layout pass and
/// clamps it to the fresh content size, so this lands exactly at the bottom.
const COMPOSER_SCROLL_TO_END: Pixels = px(-1_000_000.);

/// Whether a scrollable view is close enough to its end to keep following new
/// content. GPUI stores vertical scroll offsets as negative values: zero is
/// the top and `-max_offset.y` is the bottom.
fn scroll_is_near_bottom(scroll_handle: &ScrollHandle) -> bool {
    let offset = scroll_handle.offset().y;
    let max = scroll_handle.max_offset().y;
    max + offset <= STICK_THRESHOLD
}

/// Keep a stream pinned only while the user has left its viewport at the end.
fn follow_scroll(scroll_handle: &ScrollHandle, follow: bool) {
    if follow && scroll_is_near_bottom(scroll_handle) {
        scroll_handle.scroll_to_bottom();
    }
}

/// Apply the same wheel-intent rules to the transcript and to an expanded
/// reasoning card. A small upward gesture disarms following immediately;
/// scrolling back to the end re-arms it.
fn update_scroll_follow(
    follow: &mut bool,
    scroll_handle: &ScrollHandle,
    event: &ScrollWheelEvent,
    window: &Window,
) {
    let dy = event.delta.pixel_delta(window.line_height()).y;
    if dy > px(0.) {
        *follow = false;
    } else if dy < px(0.) && scroll_is_near_bottom(scroll_handle) {
        *follow = true;
    }
}

static NEXT_CONVERSATION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MESSAGE_PART_UI_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

pub enum MessagePart {
    Text {
        content_index: usize,
        ui_id: u64,
        id: String,
        text: String,
        replay: ProviderMetadata,
        finished: bool,
        body: MarkdownBody,
    },
    Reasoning {
        content_index: usize,
        ui_id: u64,
        id: String,
        reasoning: ReasoningContent,
        finished: bool,
        trace: Option<ReasoningTrace>,
    },
    ToolCall {
        content_index: usize,
        ui_id: u64,
        index: usize,
        id: String,
        name: String,
        tool_call: Option<ToolCall>,
    },
    ToolResult {
        content_index: usize,
        tool_result: ToolResult,
        body: MarkdownBody,
    },
}

pub struct Message {
    pub role: Role,
    pub parts: Vec<MessagePart>,
    pub provider_metadata: ProviderMetadata,
    /// Set when the generation for this turn failed. Rendered as a card below
    /// whatever text streamed before the failure, and deliberately kept out of
    /// the parts so a provider's error text is never replayed as conversation
    /// history on the next turn.
    pub error: Option<TurnError>,
}

impl Message {
    fn empty(role: Role) -> Self {
        Self {
            role,
            parts: Vec::new(),
            provider_metadata: ProviderMetadata::default(),
            error: None,
        }
    }

    fn from_canonical(message: LlmMessage, cx: &mut App) -> Self {
        let role = match message.role {
            crate::llm::Role::Assistant => Role::Assistant,
            _ => Role::User,
        };
        let parts = message
            .content
            .into_iter()
            .enumerate()
            .map(|(index, block)| MessagePart::from_canonical(index, block, cx))
            .collect();
        Self {
            role,
            parts,
            provider_metadata: message.provider_metadata,
            error: None,
        }
    }

    fn canonical(&self) -> LlmMessage {
        LlmMessage {
            role: match self.role {
                Role::User => crate::llm::Role::User,
                Role::Assistant => crate::llm::Role::Assistant,
            },
            content: self
                .parts
                .iter()
                .filter_map(MessagePart::canonical)
                .collect(),
            provider_metadata: self.provider_metadata.clone(),
        }
    }

    fn replace_with_canonical(&mut self, message: IndexedMessage, cx: &mut App) {
        let mut previous = std::mem::take(&mut self.parts)
            .into_iter()
            .map(|part| (part.content_index(), part))
            .collect::<std::collections::BTreeMap<_, _>>();
        self.parts = message
            .content
            .into_iter()
            .map(|part| {
                let old = previous.remove(&part.content_index);
                MessagePart::reconcile(part.content_index, old, part.block, cx)
            })
            .collect();
        self.provider_metadata = message.provider_metadata;
    }

    fn finish_reasoning(&mut self, id: Option<&str>) {
        for part in &mut self.parts {
            let MessagePart::Reasoning {
                id: part_id,
                finished,
                trace,
                ..
            } = part
            else {
                continue;
            };
            if id.is_none_or(|id| id == part_id) {
                *finished = true;
                if let Some(trace) = trace {
                    trace.finish();
                }
            }
        }
    }
}

impl MessagePart {
    fn from_canonical(index: usize, block: ContentBlock, cx: &mut App) -> Self {
        let ui_id = NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed);
        match block {
            ContentBlock::Text {
                text,
                provider_metadata,
            } => Self::Text {
                content_index: index,
                ui_id,
                id: format!("terminal-text-{index}"),
                body: MarkdownBody::new(&text, ui_id, cx),
                text,
                replay: provider_metadata,
                finished: true,
            },
            ContentBlock::Reasoning { reasoning } => Self::Reasoning {
                content_index: index,
                ui_id,
                id: format!("terminal-reasoning-{index}"),
                finished: true,
                trace: (!reasoning.display.is_empty())
                    .then(|| ReasoningTrace::completed(reasoning.display.clone(), ui_id, cx)),
                reasoning,
            },
            ContentBlock::ToolCall { tool_call } => Self::ToolCall {
                content_index: index,
                ui_id,
                index,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                tool_call: Some(tool_call),
            },
            ContentBlock::ToolResult { tool_result } => Self::ToolResult {
                content_index: index,
                body: MarkdownBody::new(&tool_result.content, ui_id, cx),
                tool_result,
            },
        }
    }

    fn canonical(&self) -> Option<ContentBlock> {
        match self {
            Self::Text { text, replay, .. } if !text.is_empty() => Some(ContentBlock::Text {
                text: text.clone(),
                provider_metadata: replay.clone(),
            }),
            Self::Reasoning { reasoning, .. }
                if !reasoning.display.is_empty() || reasoning.replay.is_some() =>
            {
                Some(ContentBlock::Reasoning {
                    reasoning: reasoning.clone(),
                })
            }
            Self::ToolCall {
                tool_call: Some(tool_call),
                ..
            } => Some(ContentBlock::ToolCall {
                tool_call: tool_call.clone(),
            }),
            Self::ToolResult { tool_result, .. } => Some(ContentBlock::ToolResult {
                tool_result: tool_result.clone(),
            }),
            _ => None,
        }
    }

    fn content_index(&self) -> usize {
        match self {
            Self::Text { content_index, .. }
            | Self::Reasoning { content_index, .. }
            | Self::ToolCall { content_index, .. }
            | Self::ToolResult { content_index, .. } => *content_index,
        }
    }

    fn reconcile(index: usize, old: Option<Self>, block: ContentBlock, cx: &mut App) -> Self {
        match (old, block) {
            (
                Some(Self::Text {
                    ui_id, id, body, ..
                }),
                ContentBlock::Text {
                    text,
                    provider_metadata,
                },
            ) => {
                let mut body = body;
                body.set_text(&text, cx);
                Self::Text {
                    content_index: index,
                    ui_id,
                    id,
                    text,
                    replay: provider_metadata,
                    finished: true,
                    body,
                }
            }
            (
                Some(Self::Reasoning {
                    ui_id,
                    id,
                    trace: Some(mut trace),
                    ..
                }),
                ContentBlock::Reasoning { reasoning },
            ) if !reasoning.display.is_empty() => {
                trace.set_source(&reasoning.display, cx);
                trace.finish();
                Self::Reasoning {
                    content_index: index,
                    ui_id,
                    id,
                    reasoning,
                    finished: true,
                    trace: Some(trace),
                }
            }
            (
                Some(Self::ToolCall {
                    ui_id,
                    index,
                    id,
                    name,
                    ..
                }),
                ContentBlock::ToolCall { tool_call },
            ) => Self::ToolCall {
                content_index: index,
                ui_id,
                index,
                id,
                name: if tool_call.name.is_empty() {
                    name
                } else {
                    tool_call.name.clone()
                },
                tool_call: Some(tool_call),
            },
            (_, block) => Self::from_canonical(index, block, cx),
        }
    }
}

#[derive(Clone)]
pub enum ChatEvent {
    TitleChanged(SharedString),
    SelectionChanged(ModelSelection),
}

pub struct ChatView {
    messages: Vec<Message>,
    input: Entity<InputState>,
    /// Placeholder text currently installed in the input state.  Compared
    /// against the live translation each frame so a language switch updates
    /// the composer without rebuilding it (and without notify loops).
    placeholder: SharedString,
    /// Last measured height of the complete floating composer, including its
    /// outer padding. Used as the transcript's bottom inset.
    composer_height: Pixels,
    /// Composer height measured while the input was empty, i.e. at its
    /// single-row resting size.  The empty state centers against this instead
    /// of the live height so a multi-line draft grows the composer upward
    /// *over* the greeting rather than pushing it up the panel.  Re-measured
    /// whenever the input goes back to empty, so a font or text-size change
    /// recalibrates it on its own.
    base_composer_height: Pixels,
    /// Snapshot maintained by the input subscription. Rendering hooks must not
    /// read the external `InputState` entity while recording layout bounds.
    input_empty: bool,
    scroll_handle: ScrollHandle,
    /// Whether streaming updates may pin the view to the bottom.  A single
    /// upward wheel tick clears it — distance alone isn't enough, because
    /// during fast streaming the next token re-pins before the user can
    /// scroll past [`STICK_THRESHOLD`].  Scrolling back near the bottom, or
    /// sending a message, re-arms it.
    follow: bool,
    selection: Option<ModelSelection>,
    selection_available: bool,
    provider_catalog_revision: u64,
    pending: bool,
    reply_task: Option<assistant::ReplyTask>,
    conversation_id: String,
    next_turn_id: u64,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn view(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::new(window, cx))
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder: SharedString = t!("chat.placeholder").to_string().into();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(placeholder.clone())
        });

        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift, .. } if !shift => {
                        let text = input.read(cx).value().trim().to_string();
                        if this.submit(text, cx) {
                            this.input_empty = true;
                            input.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                    }
                    InputEvent::Change => {
                        // After an edit that lands the caret on the last line
                        // (typing at the end, or pasting a block of text), snap
                        // the composer viewport to the bottom.  The component's
                        // own paste follow-scroll computes against the previous
                        // frame's layout, so pasting many lines into a shorter
                        // document clamps its scroll target to zero and the view
                        // stays stuck at the top.
                        let (input_empty, cursor_line, lines_len, x) = {
                            let state = input.read(cx);
                            (
                                state.value().is_empty(),
                                state.cursor_position().line as usize,
                                state.text().lines_len(),
                                state.scroll_offset().x,
                            )
                        };
                        this.input_empty = input_empty;
                        if lines_len > 1 && cursor_line + 1 == lines_len {
                            input.update(cx, |state, cx| {
                                state.set_scroll_offset(point(x, COMPOSER_SCROLL_TO_END), cx);
                            });
                        }
                    }
                    _ => {}
                },
            );

        // Error cards use TextView's native code blocks, whose syntax colors are
        // fixed when parsed. Chat code blocks read the current theme in their
        // custom renderer and invalidate their own highlight cache.
        let theme_observer = cx.observe_global::<gpui_component::Theme>(|this, cx| {
            this.refresh_error_highlights(cx);
        });

        let selection = providers::last_selection(cx);
        let selection_available = providers::selection_is_available(selection.as_ref(), cx);
        Self {
            messages: Vec::new(),
            input,
            placeholder,
            composer_height: DEFAULT_COMPOSER_HEIGHT,
            base_composer_height: DEFAULT_COMPOSER_HEIGHT,
            input_empty: true,
            scroll_handle: ScrollHandle::new(),
            follow: true,
            selection,
            selection_available,
            provider_catalog_revision: providers::catalog_revision(),
            pending: false,
            reply_task: None,
            conversation_id: format!(
                "conversation-{}",
                NEXT_CONVERSATION_ID.fetch_add(1, Ordering::Relaxed)
            ),
            next_turn_id: 1,
            _subscriptions: vec![subscription, theme_observer],
        }
    }

    /// Re-parse native code blocks in error cards against the active palette.
    fn refresh_error_highlights(&mut self, cx: &mut Context<Self>) {
        let mut refreshed = false;
        for message in &mut self.messages {
            if let Some(error) = &mut message.error {
                refreshed |= error.refresh_highlight(cx);
            }
        }
        if refreshed {
            cx.notify();
        }
    }

    /// Force the newest message into view.  Used right after the user sends a
    /// message, where jumping to the bottom is always the desired behavior.
    pub fn scroll_to_bottom(&self) {
        self.scroll_handle.scroll_to_bottom();
    }

    /// Called on every streaming update.  Keeps the latest tokens in view, but
    /// only while auto-follow is armed and the user is actually reading the
    /// bottom of the transcript — an upward wheel tick disarms it (see
    /// [`ChatView::follow`]), so scrolling up to re-read earlier turns is never
    /// interrupted, no matter how small the scroll was.
    pub fn follow_stream(&self) {
        follow_scroll(&self.scroll_handle, self.follow);
    }

    /// Submit a non-empty message when no reply is already in flight.
    /// Returns whether the message was accepted so callers only clear the
    /// composer after a successful submission.
    fn submit(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        self.sync_selection_availability(cx);
        if self.pending || text.is_empty() || !self.selection_available {
            return false;
        }

        if self.messages.is_empty() {
            cx.emit(ChatEvent::TitleChanged(derive_title(&text)));
        }

        self.messages.push(Message::from_canonical(
            LlmMessage {
                role: crate::llm::Role::User,
                content: vec![ContentBlock::Text {
                    text,
                    provider_metadata: ProviderMetadata::default(),
                }],
                provider_metadata: ProviderMetadata::default(),
            },
            cx,
        ));

        self.messages.push(Message::empty(Role::Assistant));

        self.pending = true;
        let history = self
            .messages
            .iter()
            .take(self.messages.len().saturating_sub(1))
            .map(Message::canonical)
            .filter(is_replayable)
            .collect();
        let turn_id = format!("turn-{}", self.next_turn_id);
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.reply_task = Some(assistant::stream_reply(
            history,
            self.selection.clone(),
            self.conversation_id.clone(),
            turn_id,
            cx,
        ));
        self.follow = true;
        self.scroll_to_bottom();
        cx.notify();
        true
    }

    /// Finish the turn in flight and attach its terminal error, if any. The
    /// error card's state is built here, outside render, and all terminal state
    /// changes are published with one notification.
    pub fn finish_reply(
        &mut self,
        message: Option<IndexedMessage>,
        error: Option<GatewayError>,
        cx: &mut Context<Self>,
    ) {
        let turn_error = error.map(|error| TurnError::new(error, cx));
        if let Some(last) = self.messages.last_mut() {
            if let Some(message) = message {
                // Match pi's message lifecycle: deltas provide a responsive live
                // projection, then the complete message_end snapshot becomes
                // authoritative for both rendering and replay.
                last.replace_with_canonical(message, cx);
            }
            last.error = turn_error;
            // Terminal fallback for a stream that never delivered its explicit
            // `ReasoningFinished` boundary, including cancellation and failure.
            last.finish_reasoning(None);
        }
        self.pending = false;
        self.reply_task = None;
        cx.notify();
    }

    pub fn start_stream_text(&mut self, content_index: usize, id: String, cx: &mut App) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if last
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Text { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id))
        {
            return;
        }
        let ui_id = NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed);
        last.parts.push(MessagePart::Text {
            content_index,
            ui_id,
            id,
            text: String::new(),
            replay: ProviderMetadata::default(),
            finished: false,
            body: MarkdownBody::new("", ui_id, cx),
        });
        last.parts.sort_by_key(MessagePart::content_index);
    }

    pub fn append_stream_text(
        &mut self,
        content_index: usize,
        id: String,
        delta: &str,
        cx: &mut App,
    ) {
        // OpenAI Responses, like pi's adapter, forwards text deltas as received;
        // an empty delta carries no content and must not create a canonical block
        // or alter the independent reasoning lifecycle.
        if delta.is_empty() {
            return;
        }
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        let Some(position) = last.parts.iter().position(
            |part| matches!(part, MessagePart::Text { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id),
        ) else {
            self.start_stream_text(content_index, id.clone(), cx);
            self.append_stream_text(content_index, id, delta, cx);
            return;
        };
        if let MessagePart::Text {
            text,
            finished,
            body,
            ..
        } = &mut last.parts[position]
        {
            if *finished {
                return;
            }
            text.push_str(delta);
            body.push_str(delta, cx);
        }
    }

    pub fn finish_stream_text(
        &mut self,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
    ) {
        let Some(MessagePart::Text {
            replay: current,
            finished,
            ..
        }) = self.messages.last_mut().and_then(|message| {
            message
                .parts
                .iter_mut()
                .find(|part| matches!(part, MessagePart::Text { content_index: current, id: current_id, .. } if *current == content_index && current_id == id))
        })
        else {
            return;
        };
        if let Some(replay) = replay {
            *current = replay;
        }
        *finished = true;
    }

    /// Start the reasoning content block identified by the gateway stream.
    ///
    /// Protocol adapters emit this boundary before starting a different content
    /// block, so the presentation layer never infers one block's lifecycle from
    /// another block's deltas.
    pub fn start_stream_reasoning(&mut self, content_index: usize, id: String) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if last.parts.iter().any(
            |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id),
        ) {
            return;
        }
        last.parts.push(MessagePart::Reasoning {
            content_index,
            ui_id: NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed),
            id,
            reasoning: ReasoningContent {
                display: String::new(),
                replay: None,
            },
            finished: false,
            trace: None,
        });
        last.parts.sort_by_key(MessagePart::content_index);
    }

    pub fn append_stream_reasoning(
        &mut self,
        content_index: usize,
        id: String,
        delta: &str,
        cx: &mut App,
    ) {
        // Empty protocol deltas carry no visible state and must not create a
        // card or start its duration clock.
        if delta.is_empty() {
            return;
        }
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        let Some(position) = last.parts.iter().position(
            |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == &id),
        ) else {
            self.start_stream_reasoning(content_index, id.clone());
            self.append_stream_reasoning(content_index, id, delta, cx);
            return;
        };
        if let MessagePart::Reasoning {
            ui_id,
            reasoning,
            finished,
            trace,
            ..
        } = &mut last.parts[position]
        {
            // A finished block is immutable. Protocol adapters must allocate a
            // new id for later reasoning so its card, timer, disclosure, and
            // canonical position remain independent.
            if *finished {
                return;
            }
            reasoning.display.push_str(delta);
            trace
                .get_or_insert_with(|| ReasoningTrace::new(*ui_id, cx))
                .push(delta, cx);
        }
    }

    pub fn finish_stream_reasoning(
        &mut self,
        content_index: usize,
        id: &str,
        replay: Option<ProviderMetadata>,
    ) {
        let Some(MessagePart::Reasoning {
            reasoning,
            finished,
            trace,
            ..
        }) = self.messages.last_mut().and_then(|message| {
            message.parts.iter_mut().find(
                |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == id),
            )
        })
        else {
            return;
        };
        if let Some(replay) = replay {
            reasoning.replay = Some(replay);
        }
        *finished = true;
        if let Some(trace) = trace {
            trace.finish();
        }
    }

    /// Apply an authoritative provider snapshot without changing this block's
    /// disclosure, timer, or stable GPUI identity.
    pub fn update_stream_reasoning_snapshot(
        &mut self,
        content_index: usize,
        id: &str,
        snapshot: ReasoningContent,
        cx: &mut App,
    ) {
        let Some(MessagePart::Reasoning {
            ui_id,
            reasoning,
            finished,
            trace,
            ..
        }) = self.messages.last_mut().and_then(|message| {
            message.parts.iter_mut().find(
                |part| matches!(part, MessagePart::Reasoning { content_index: current, id: current_id, .. } if *current == content_index && current_id == id),
            )
        })
        else {
            return;
        };
        if !*finished {
            return;
        }
        if snapshot.display.is_empty() {
            *trace = None;
        } else if let Some(trace) = trace.as_mut() {
            trace.set_source(&snapshot.display, cx);
        } else {
            *trace = Some(ReasoningTrace::completed(
                snapshot.display.clone(),
                *ui_id,
                cx,
            ));
        }
        *reasoning = snapshot;
    }

    pub fn start_stream_tool_call(
        &mut self,
        content_index: usize,
        index: usize,
        id: String,
        name: String,
    ) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if last.parts.iter().any(
            |part| matches!(part, MessagePart::ToolCall { content_index: current, .. } if *current == content_index),
        ) {
            return;
        }
        last.parts.push(MessagePart::ToolCall {
            content_index,
            ui_id: NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed),
            index,
            id,
            name,
            tool_call: None,
        });
        last.parts.sort_by_key(MessagePart::content_index);
    }

    pub fn finish_stream_tool_call(
        &mut self,
        content_index: usize,
        index: usize,
        tool_call: ToolCall,
    ) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        if let Some(MessagePart::ToolCall {
            id,
            name,
            tool_call: current,
            ..
        }) = last.parts.iter_mut().find(
            |part| matches!(part, MessagePart::ToolCall { content_index: current, index: current_index, .. } if *current == content_index && *current_index == index),
        ) {
            *id = tool_call.id.clone();
            *name = tool_call.name.clone();
            *current = Some(tool_call);
        } else {
            last.parts.push(MessagePart::ToolCall {
                content_index,
                ui_id: NEXT_MESSAGE_PART_UI_ID.fetch_add(1, Ordering::Relaxed),
                index,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                tool_call: Some(tool_call),
            });
            last.parts.sort_by_key(MessagePart::content_index);
        }
    }

    pub fn finish_stream_batch(&mut self, cx: &mut Context<Self>) {
        self.follow_stream();
        cx.notify();
    }

    pub fn cancel_reply(&mut self) {
        if !self.pending {
            return;
        }
        if let Some(reply) = &self.reply_task {
            reply.cancel();
        }
    }

    pub fn select_model(&mut self, selection: ModelSelection, cx: &mut Context<Self>) {
        if !self.update_selection(selection.clone()) {
            return;
        }
        providers::select_model(selection.clone(), cx);
        cx.emit(ChatEvent::SelectionChanged(selection));
        cx.notify();
    }

    fn update_selection(&mut self, selection: ModelSelection) -> bool {
        if self.selection.as_ref() == Some(&selection) {
            return false;
        }
        self.selection = Some(selection);
        self.selection_available = true;
        self.provider_catalog_revision = providers::catalog_revision();
        true
    }

    pub fn selection(&self) -> Option<ModelSelection> {
        self.selection.clone()
    }

    fn sync_selection_availability(&mut self, cx: &App) {
        let revision = providers::catalog_revision();
        if self.provider_catalog_revision == revision {
            return;
        }
        self.selection_available = providers::selection_is_available(self.selection.as_ref(), cx);
        self.provider_catalog_revision = revision;
    }

    fn on_send_click(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if self.submit(text, cx) {
            self.input_empty = true;
            self.input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    fn on_stop_click(&mut self, _: &ClickEvent, _: &mut Window, _: &mut Context<Self>) {
        self.cancel_reply();
    }
}

impl EventEmitter<ChatEvent> for ChatView {}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_selection_availability(cx);
        // Re-resolve the placeholder so a language switch reaches the
        // already-built input; guarded to avoid a notify cycle.
        let placeholder: SharedString = t!("chat.placeholder").to_string().into();
        if self.placeholder != placeholder {
            self.placeholder = placeholder.clone();
            self.input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
            });
        }

        let has_messages = !self.messages.is_empty();
        let send_disabled = self.pending
            || self.input.read(cx).value().trim().is_empty()
            || !self.selection_available;
        let composer_height = self.composer_height;
        let base_composer_height = self.base_composer_height;
        let view = cx.weak_entity();

        // Full-height message viewport with a floating composer stacked on top
        // (gpui-component absolute overlay pattern).  Scrollbar tracks the right
        // edge all the way to the panel bottom; content padding keeps the last
        // turn clear of the input.
        div()
            .relative()
            .size_full()
            .child(if has_messages {
                self.render_message_list(composer_height, window, cx)
                    .into_any_element()
            } else {
                render_empty_state(base_composer_height, cx).into_any_element()
            })
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(self.render_input_area(send_disabled, cx))
                    .on_prepaint(move |bounds, _, cx| {
                        view.update(cx, |this, cx| {
                            if this.record_composer_height(bounds.size.height) {
                                cx.notify();
                            }
                        })
                        .ok();
                    }),
            )
    }
}

fn is_replayable(message: &LlmMessage) -> bool {
    message.role != crate::llm::Role::Assistant || !message.content.is_empty()
}

impl ChatView {
    /// Fold a fresh composer measurement into the two tracked heights, and
    /// report whether either moved (i.e. whether a re-render is needed).
    ///
    /// The live height follows every frame, but the resting height only
    /// records while the input is empty — and an empty input is exactly one
    /// row tall.  That keeps the greeting anchored when a draft grows the
    /// composer, without hard-coding what one row measures.
    fn record_composer_height(&mut self, height: Pixels) -> bool {
        let mut changed = false;
        if self.composer_height != height {
            self.composer_height = height;
            changed = true;
        }
        if self.input_empty && self.base_composer_height != height {
            self.base_composer_height = height;
            changed = true;
        }
        changed
    }

    fn render_message_list(
        &self,
        composer_height: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Rendered up front rather than inside `.children()`: an error card needs
        // `&mut Window` for its collapse state, which cannot escape the `FnMut`
        // closure that a lazy iterator would be called from.
        let rendered = self
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| render_message(message, index, window, cx).into_any_element())
            .collect::<Vec<_>>();

        // Relative, non-scrolling wrapper so the overlay scrollbar anchors to
        // the full panel height (including under the floating composer).
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("messages")
                    .size_full()
                    .track_scroll(&self.scroll_handle)
                    .overflow_y_scroll()
                    // Wheel intent drives auto-follow: any upward tick disarms
                    // it immediately (no minimum distance), scrolling back to
                    // near the bottom re-arms it.  `delta.y > 0` scrolls toward
                    // earlier content (gpui applies `offset.y += delta.y` with
                    // 0 = top).
                    .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, _| {
                        update_scroll_follow(&mut this.follow, &this.scroll_handle, ev, window);
                    }))
                    .child(
                        v_flex()
                            .w_full()
                            // Match the scrollbar thumb's 4px top inset so the
                            // first message aligns with the top of the thumb.
                            .pt(px(4.))
                            // Leave exactly enough room for the measured floating composer.
                            .pb(composer_height)
                            .gap_5()
                            .children(rendered),
                    ),
            )
            .vertical_scrollbar(&self.scroll_handle)
    }

    fn render_input_area(&self, send_disabled: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        h_flex()
            .w_full()
            .justify_center()
            .px_6()
            .pt_2()
            .pb_3()
            .child(
                v_flex()
                    .w_full()
                    .max_w(CONTENT_MAX_WIDTH)
                    .gap_0p5()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius_lg)
                    .shadow_md()
                    .py_1()
                    // Input consumes wheel events while its viewport moves, but
                    // deliberately propagates them at the top/bottom boundary.
                    // Contain that remainder inside the floating composer so it
                    // cannot scroll the transcript underneath.
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    // The multi-line Input places its overlay scrollbar inside its
                    // own horizontal padding and soft-wraps text 10px short of the
                    // text area (RIGHT_MARGIN, fixed upstream).  The thumb's left
                    // edge sits at `text_right + padding.right − 12px`, so the
                    // glyph→thumb gap works out to `padding.right − 2px`: the
                    // default 12px padding reads as a too-wide 10px gap, 8px
                    // brings it to 6px. Don't go much lower: this is the final
                    // visual separation between the production-shaped line and
                    // the thumb. The card adds no horizontal padding of its own
                    // around the input; the toolbar row below carries its own
                    // inset.
                    //
                    // Bundled fonts remain the product defaults for consistent
                    // cross-platform appearance, but are no longer a wrapping
                    // workaround. gpui-component derives soft-wrap points from
                    // production-shaped widths, including system fallback and
                    // fullwidth punctuation. `cargo run --example wrap_probe`
                    // compares the legacy estimator with production shaping on
                    // real fonts; production lines must remain overflow-free.
                    .child(
                        Input::new(&self.input)
                            .appearance(false)
                            .font_family(fonts::active(cx).family())
                            .pr(px(8.)),
                    )
                    .child(
                        h_flex()
                            .px_1p5()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Plus)
                                    .tooltip(t!("chat.attach").to_string()),
                            )
                            .child(div().flex_1())
                            .when(self.pending, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("chat.generating").to_string()),
                                )
                            })
                            .child(if self.pending {
                                Button::new("stop")
                                    .primary()
                                    .icon(IconName::Close)
                                    .small()
                                    .tooltip(t!("chat.stop_tooltip").to_string())
                                    .on_click(cx.listener(Self::on_stop_click))
                            } else {
                                Button::new("send")
                                    .primary()
                                    .icon(IconName::ArrowUp)
                                    .small()
                                    .disabled(send_disabled)
                                    .tooltip(t!("chat.send_tooltip").to_string())
                                    .on_click(cx.listener(Self::on_send_click))
                            }),
                    ),
            )
    }
}

fn render_message(
    msg: &Message,
    message_index: usize,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> impl IntoElement {
    let (radius_lg, secondary, secondary_foreground, foreground, muted_foreground) = {
        let theme = cx.theme();
        (
            theme.radius_lg,
            theme.secondary,
            theme.secondary_foreground,
            theme.foreground,
            theme.muted_foreground,
        )
    };
    let is_user = msg.role == Role::User;
    let parts = msg.parts.iter().filter_map(|part| {
            match part {
                MessagePart::Text { text, body, .. } if !text.is_empty() => {
                    Some(body.text_view(TextViewStyle::default()).into_any_element())
                }
                MessagePart::Reasoning {
                    ui_id,
                    reasoning,
                    finished,
                    trace: Some(trace),
                    ..
                } => {
                    // Each content block owns independent disclosure and copy
                    // state. `ui_id` survives terminal reconciliation and vector
                    // reordering, so keyed GPUI/Clipboard state remains stable.
                    let ui_id = *ui_id;
                    let content_index = part.content_index();
                    let on_toggle = cx.listener(move |this: &mut ChatView, _, _, cx| {
                        if let Some(MessagePart::Reasoning {
                            trace: Some(trace), ..
                        }) = this
                            .messages
                            .get_mut(message_index)
                            .and_then(|message| {
                                message.parts.iter_mut().find(|part| {
                                    matches!(part, MessagePart::Reasoning { ui_id: current, .. } if *current == ui_id)
                                })
                            })
                        {
                            trace.toggle();
                            cx.notify();
                        }
                    });
                    let on_scroll = cx.listener(
                        move |this: &mut ChatView,
                              event: &ScrollWheelEvent,
                              window,
                              _cx| {
                            if let Some(MessagePart::Reasoning {
                                trace: Some(trace), ..
                            }) = this
                                .messages
                                .get_mut(message_index)
                                .and_then(|message| {
                                    message.parts.iter_mut().find(|part| {
                                        matches!(part, MessagePart::Reasoning { ui_id: current, .. } if *current == ui_id)
                                    })
                                })
                            {
                                trace.handle_scroll(event, window);
                            }
                        },
                    );
                    let view = cx.entity().downgrade();
                    let copy_value = move |_: &mut Window, cx: &mut App| {
                        view.upgrade()
                            .and_then(|view| {
                                let view = view.read(cx);
                                match view
                                    .messages
                                    .get(message_index)
                                    .and_then(|message| {
                                        message.parts.iter().find(|part| {
                                            matches!(part, MessagePart::Reasoning { ui_id: current, .. } if *current == ui_id)
                                        })
                                    })
                                {
                                    Some(MessagePart::Reasoning {
                                        reasoning,
                                        trace: Some(_),
                                        ..
                                    }) => Some(reasoning.display.clone().into()),
                                    _ => None,
                                }
                            })
                            .unwrap_or_default()
                    };
                    Some(reasoning_card::render(
                        trace,
                        &reasoning.display,
                        *finished,
                        reasoning_card::ReasoningCardId {
                            ui_id,
                            content_index,
                        },
                        reasoning_card::ReasoningCardActions {
                            on_toggle: std::rc::Rc::new(on_toggle),
                            copy_value: std::rc::Rc::new(copy_value),
                            on_scroll: std::rc::Rc::new(on_scroll),
                        },
                        window,
                        cx,
                    ))
                }
                MessagePart::ToolCall { name, .. } if !name.is_empty() => Some(
                    div()
                        .text_color(muted_foreground)
                        .child(t!("chat.tool_requested", name = name.clone()).to_string())
                        .into_any_element(),
                ),
                MessagePart::ToolResult { body, .. } => {
                    Some(body.text_view(TextViewStyle::default()).into_any_element())
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();

    let inner: AnyElement = if is_user {
        // Right-aligned bubble for user turns.
        h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .debug_selector(move || format!("user-message-bubble-{message_index}"))
                    // Markdown contributes an intrinsic min-content width. As a
                    // horizontal flex item the bubble must be allowed to shrink
                    // below it when the conversation column narrows.
                    .min_w_0()
                    .max_w(px(560.))
                    .rounded(radius_lg)
                    .bg(secondary)
                    .text_color(secondary_foreground)
                    // Kept tight on purpose: the body inherits the window's
                    // 16px base size, so its line box is already ~24px tall and
                    // generous padding on top of that makes a one-word turn
                    // read as a block.
                    .px_3()
                    .py_1p5()
                    .children(parts),
            )
            .into_any_element()
    } else {
        // Assistant content is rendered in canonical block order. Reasoning
        // cards are normal flex children, so text/tool interleaving is preserved
        // without overlays or a second presentation ordering model.
        v_flex()
            .w_full()
            .gap_3()
            .text_color(foreground)
            .children(parts)
            .when_some(msg.error.as_ref(), |this, error| {
                this.child(error_card::render(error, message_index, window, cx))
            })
            .into_any_element()
    };

    h_flex().w_full().justify_center().px_6().child(
        div()
            .debug_selector(move || format!("assistant-message-content-{message_index}"))
            .w_full()
            .max_w(CONTENT_MAX_WIDTH)
            .child(inner),
    )
}

/// Greeting shown before the first turn.  Takes the composer's *resting*
/// height (see `ChatView::base_composer_height`) so the block stays anchored
/// while a multi-line draft grows the composer over it.
fn render_empty_state(base_composer_height: Pixels, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .pb(base_composer_height)
        .gap_2()
        .child(
            div()
                .text_2xl()
                .font_semibold()
                .text_color(theme.foreground)
                .child(t!("chat.empty_title").to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("chat.empty_hint").to_string()),
        )
}

fn derive_title(text: &str) -> SharedString {
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

#[cfg(test)]
mod tests;
