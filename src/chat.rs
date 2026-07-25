//! Single conversation view: message list, streaming assistant turn, and
//! composer input.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, RopeExt as _},
    scroll::ScrollableElement as _,
    text::{TextView, TextViewState},
    v_flex,
};

use crate::assistant;
use crate::fonts;

const CONTENT_MAX_WIDTH: Pixels = px(760.);

/// While streaming, the view re-pins to the bottom only when it is already
/// within this distance of the end — used both as the pin guard and as the
/// "scrolled back down" re-arm threshold for [`ChatView::follow`].
const STICK_THRESHOLD: Pixels = px(48.);

/// Bottom inset reserved for the floating composer so the last message stays
/// readable and scroll-to-bottom lands above the input.  Sized for the default
/// single-line auto-grow height plus outer padding; taller multi-line input may
/// briefly overlap until the user scrolls.
const COMPOSER_RESERVE: Pixels = px(120.);

/// Deliberately over-scrolled deferred target for the composer viewport.
/// `InputState::set_scroll_offset` applies it after the next layout pass and
/// clamps it to the fresh content size, so this lands exactly at the bottom.
const COMPOSER_SCROLL_TO_END: Pixels = px(-1_000_000.);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub body: Entity<TextViewState>,
}

pub struct ChatView {
    title: SharedString,
    messages: Vec<Message>,
    input: Entity<InputState>,
    scroll_handle: ScrollHandle,
    /// Whether streaming updates may pin the view to the bottom.  A single
    /// upward wheel tick clears it — distance alone isn't enough, because
    /// during fast streaming the next token re-pins before the user can
    /// scroll past [`STICK_THRESHOLD`].  Scrolling back near the bottom, or
    /// sending a message, re-arms it.
    follow: bool,
    pending: bool,
    _reply_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn view(title: impl Into<SharedString>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::new(title, window, cx))
    }

    fn new(title: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Send a message.  Enter to send, Shift+Enter for newline.")
        });

        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift, .. } if !shift => {
                        let text = input.read(cx).value().trim().to_string();
                        if text.is_empty() {
                            return;
                        }
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                        this.submit(text, cx);
                    }
                    InputEvent::Change => {
                        // After an edit that lands the caret on the last line
                        // (typing at the end, or pasting a block of text), snap
                        // the composer viewport to the bottom.  The component's
                        // own paste follow-scroll computes against the previous
                        // frame's layout, so pasting many lines into a shorter
                        // document clamps its scroll target to zero and the view
                        // stays stuck at the top.
                        let (cursor_line, lines_len, x) = {
                            let state = input.read(cx);
                            (
                                state.cursor_position().line as usize,
                                state.text().lines_len(),
                                state.scroll_offset().x,
                            )
                        };
                        if lines_len > 1 && cursor_line + 1 == lines_len {
                            input.update(cx, |state, cx| {
                                state.set_scroll_offset(point(x, COMPOSER_SCROLL_TO_END), cx);
                            });
                        }
                    }
                    _ => {}
                },
            );

        Self {
            title: title.into(),
            messages: Vec::new(),
            input,
            scroll_handle: ScrollHandle::new(),
            follow: true,
            pending: false,
            _reply_task: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn title(&self) -> &SharedString {
        &self.title
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

    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }

        if self.messages.is_empty() {
            self.title = derive_title(&text);
        }

        let user_body = cx.new(|cx| TextViewState::markdown(&text, cx));
        self.messages.push(Message {
            role: Role::User,
            body: user_body,
        });

        let assistant_body = cx.new(|cx| TextViewState::markdown("", cx));
        self.messages.push(Message {
            role: Role::Assistant,
            body: assistant_body.clone(),
        });

        self.pending = true;
        self._reply_task = Some(assistant::stream_reply(&text, assistant_body, cx));
        self.follow = true;
        self.scroll_to_bottom();
        cx.notify();
    }

    pub fn finish_reply(&mut self, cx: &mut Context<Self>) {
        self.pending = false;
        self._reply_task = None;
        cx.notify();
    }

    fn on_send_click(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.submit(text, cx);
    }
}

impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_messages = !self.messages.is_empty();
        let send_disabled = self.pending || self.input.read(cx).value().trim().is_empty();

        // Full-height message viewport with a floating composer stacked on top
        // (gpui-component absolute overlay pattern).  Scrollbar tracks the right
        // edge all the way to the panel bottom; content padding keeps the last
        // turn clear of the input.
        div()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .child(if has_messages {
                self.render_message_list(cx).into_any_element()
            } else {
                render_empty_state(cx).into_any_element()
            })
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(self.render_input_area(send_disabled, cx)),
            )
    }
}

impl ChatView {
    fn render_message_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            // Leave room so the last message clears the floating composer.
                            .pb(COMPOSER_RESERVE)
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
                                    .tooltip("Attach"),
                            )
                            .child(div().flex_1())
                            .when(self.pending, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Generating..."),
                                )
                            })
                            .child(
                                Button::new("send")
                                    .primary()
                                    .icon(IconName::ArrowUp)
                                    .small()
                                    .disabled(send_disabled)
                                    .tooltip("Send (Enter)")
                                    .on_click(cx.listener(Self::on_send_click)),
                            ),
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
                    .px_4()
                    .py_2p5()
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

fn render_empty_state(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .pb(COMPOSER_RESERVE)
        .gap_2()
        .child(
            div()
                .text_2xl()
                .font_semibold()
                .text_color(theme.foreground)
                .child("How can I help you today?"),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Ask me anything.  Enter to send, Shift+Enter for newline."),
        )
}

fn derive_title(text: &str) -> SharedString {
    let mut cleaned = text.replace('\n', " ");
    if cleaned.chars().count() > 40 {
        cleaned = cleaned.chars().take(37).collect::<String>() + "...";
    }
    if cleaned.trim().is_empty() {
        "New chat".into()
    } else {
        cleaned.into()
    }
}
