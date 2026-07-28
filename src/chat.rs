//! Single conversation view: message list, streaming assistant turn, and
//! composer input.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, ElementExt as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, RopeExt as _},
    scroll::ScrollableElement as _,
    text::{TextView, TextViewState},
    v_flex,
};
use rust_i18n::t;

use crate::assistant;
use crate::fonts;
use crate::llm::{
    ContentBlock, Message as LlmMessage, ModelSelection, ProviderMetadata, ReasoningContent,
    ToolCall,
};
use crate::providers;

use std::sync::atomic::{AtomicU64, Ordering};

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

static NEXT_CONVERSATION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub body: Entity<TextViewState>,
    pub canonical: LlmMessage,
}

#[derive(Clone)]
pub enum ChatEvent {
    TitleChanged(SharedString),
    StateChanged {
        selection: Option<ModelSelection>,
        pending: bool,
    },
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
            _subscriptions: vec![subscription],
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
        if self.follow && self.is_near_bottom() {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    /// True when the viewport sits at (or within `STICK_THRESHOLD` of) the
    /// bottom.  `offset.y` is `<= 0` and reaches `-max_offset.y` at the bottom,
    /// so their sum is the remaining distance to the end of the content.
    fn is_near_bottom(&self) -> bool {
        let offset = self.scroll_handle.offset().y;
        let max = self.scroll_handle.max_offset().y;
        max + offset <= STICK_THRESHOLD
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

        let user_body = cx.new(|cx| TextViewState::markdown(&text, cx));
        self.messages.push(Message {
            role: Role::User,
            body: user_body,
            canonical: LlmMessage {
                role: crate::llm::Role::User,
                content: vec![ContentBlock::Text {
                    text: text.clone(),
                    provider_metadata: ProviderMetadata::default(),
                }],
                provider_metadata: ProviderMetadata::default(),
            },
        });

        let assistant_body = cx.new(|cx| TextViewState::markdown("", cx));
        self.messages.push(Message {
            role: Role::Assistant,
            body: assistant_body.clone(),
            canonical: LlmMessage {
                role: crate::llm::Role::Assistant,
                content: Vec::new(),
                provider_metadata: ProviderMetadata::default(),
            },
        });

        self.pending = true;
        let history = self
            .messages
            .iter()
            .take(self.messages.len().saturating_sub(1))
            .filter(|message| is_replayable(&message.canonical))
            .map(|message| message.canonical.clone())
            .collect();
        let turn_id = format!("turn-{}", self.next_turn_id);
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.reply_task = Some(assistant::stream_reply(
            history,
            self.selection.clone(),
            self.conversation_id.clone(),
            turn_id,
            assistant_body,
            cx,
        ));
        self.follow = true;
        self.scroll_to_bottom();
        self.emit_state(cx);
        cx.notify();
        true
    }

    pub fn finish_reply(&mut self, message: Option<LlmMessage>, cx: &mut Context<Self>) {
        if let (Some(message), Some(last)) = (message, self.messages.last_mut()) {
            last.canonical = message;
        }
        self.pending = false;
        self.reply_task = None;
        self.emit_state(cx);
        cx.notify();
    }

    pub fn append_stream_text(&mut self, delta: &str) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        match last.canonical.content.last_mut() {
            Some(ContentBlock::Text { text, .. }) => text.push_str(delta),
            _ => last.canonical.content.push(ContentBlock::Text {
                text: delta.to_string(),
                provider_metadata: ProviderMetadata::default(),
            }),
        }
    }

    pub fn append_stream_reasoning(&mut self, delta: &str) {
        let Some(last) = self.messages.last_mut() else {
            return;
        };
        match last.canonical.content.last_mut() {
            Some(ContentBlock::Reasoning { reasoning }) => reasoning.display.push_str(delta),
            _ => last.canonical.content.push(ContentBlock::Reasoning {
                reasoning: ReasoningContent {
                    display: delta.to_string(),
                    replay: None,
                },
            }),
        }
    }

    pub fn append_stream_tool_call(&mut self, tool_call: ToolCall) {
        if let Some(last) = self.messages.last_mut() {
            last.canonical
                .content
                .push(ContentBlock::ToolCall { tool_call });
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
        if self.pending || self.selection.as_ref() == Some(&selection) {
            return;
        }
        self.selection = Some(selection.clone());
        self.selection_available = true;
        self.provider_catalog_revision = providers::catalog_revision();
        providers::select_model(selection, cx);
        self.emit_state(cx);
        cx.notify();
    }

    pub fn selection(&self) -> Option<ModelSelection> {
        self.selection.clone()
    }

    fn emit_state(&self, cx: &mut Context<Self>) {
        cx.emit(ChatEvent::StateChanged {
            selection: self.selection.clone(),
            pending: self.pending,
        });
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
                self.render_message_list(composer_height, cx)
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
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                        let dy = ev.delta.pixel_delta(window.line_height()).y;
                        if dy > px(0.) {
                            this.follow = false;
                        } else if dy < px(0.) && this.is_near_bottom() {
                            this.follow = true;
                        }
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
                            .children(self.messages.iter().map(|m| render_message(m, cx))),
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
                    // brings it to 6px.  Don't go much lower — the gap is only
                    // guaranteed because the bundled fonts wrap with zero
                    // estimate drift (see the font note below).  The card adds
                    // no horizontal padding of its own around the input; the
                    // toolbar row below carries its own inset.
                    //
                    // The bundled font is load-bearing, not cosmetic: upstream
                    // gpui-component (bc174a7) computes soft-wrap points from
                    // per-char isolated measurements.  Under a proportional
                    // system font, fullwidth punctuation (，。) measures ~0.5em
                    // alone but paints at 1em inside a CJK run, so wrapped lines
                    // overflow and the rightmost glyph clips.  Both bundled
                    // choices avoid that: Maple Mono CN covers Latin + CJK +
                    // fullwidth forms itself (no fallback at all), and JetBrains
                    // Mono carries no fullwidth glyphs whatsoever, so both
                    // measurement paths fall back identically.  Verify with
                    // `cargo run --example wrap_probe`.
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

fn render_message(msg: &Message, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let is_user = msg.role == Role::User;

    let inner: AnyElement = if is_user {
        // Right-aligned bubble for user turns.
        h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .max_w(px(560.))
                    .rounded(theme.radius_lg)
                    .bg(theme.secondary)
                    .text_color(theme.secondary_foreground)
                    // Kept tight on purpose: the body inherits the window's
                    // 16px base size, so its line box is already ~24px tall and
                    // generous padding on top of that makes a one-word turn
                    // read as a block.
                    .px_3()
                    .py_1p5()
                    .child(TextView::new(&msg.body).selectable(true)),
            )
            .into_any_element()
    } else {
        // Assistant turn: flat markdown, no avatar, full width.
        div()
            .w_full()
            .text_color(theme.foreground)
            .child(TextView::new(&msg.body).selectable(true))
            .into_any_element()
    };

    h_flex()
        .w_full()
        .justify_center()
        .px_6()
        .child(div().w_full().max_w(CONTENT_MAX_WIDTH).child(inner))
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
mod tests {
    use gpui::{TestAppContext, px};
    use gpui_component::input::InputEvent;

    use crate::llm::{ContentBlock, Message as LlmMessage, ProviderMetadata};
    use crate::preferences;

    use super::{ChatView, is_replayable};

    #[test]
    fn empty_assistant_placeholders_are_not_replayed() {
        let empty_assistant = LlmMessage {
            role: crate::llm::Role::Assistant,
            content: Vec::new(),
            provider_metadata: ProviderMetadata::default(),
        };
        let user = LlmMessage {
            role: crate::llm::Role::User,
            content: vec![ContentBlock::Text {
                text: "hi".into(),
                provider_metadata: ProviderMetadata::default(),
            }],
            provider_metadata: ProviderMetadata::default(),
        };

        assert!(!is_replayable(&empty_assistant));
        assert!(is_replayable(&user));
    }

    /// The greeting is laid out against the *resting* composer height, so a
    /// growing draft must not move that number — otherwise the empty state
    /// gets pushed up the panel one row at a time.
    #[gpui::test]
    fn growing_draft_leaves_the_resting_composer_height_alone(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            preferences::init_global(preferences::Preferences::default(), cx);
        });
        let cx = cx.add_empty_window();
        let chat = cx.update(ChatView::view);
        let input = cx.update(|_, cx| chat.read(cx).input.clone());

        // First measurement of an empty composer sets both heights.
        cx.update(|_, cx| {
            chat.update(cx, |this, _| {
                assert!(this.record_composer_height(px(96.)));
                assert_eq!(this.composer_height, px(96.));
                assert_eq!(this.base_composer_height, px(96.));
            });
        });

        // A draft grows the composer: the live height tracks it, the resting
        // height stays where the greeting was placed.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("line\nline\nline", window, cx);
                cx.emit(InputEvent::Change);
            });
        });
        cx.update(|_, cx| {
            chat.update(cx, |this, _| {
                assert!(!this.input_empty);
                assert!(this.record_composer_height(px(168.)));
                assert_eq!(this.composer_height, px(168.));
                assert_eq!(this.base_composer_height, px(96.));
            });
        });

        // Clearing the draft re-measures the resting height, which is how a
        // font or text-size change recalibrates it.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                cx.emit(InputEvent::Change);
            });
        });
        cx.update(|_, cx| {
            chat.update(cx, |this, _| {
                assert!(this.input_empty);
                assert!(this.record_composer_height(px(104.)));
                assert_eq!(this.base_composer_height, px(104.));
                // Idempotent: the same measurement asks for no re-render.
                assert!(!this.record_composer_height(px(104.)));
            });
        });
    }
}
