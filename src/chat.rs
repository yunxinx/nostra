//! Single conversation view: message list, streaming assistant turn, and
//! composer input.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonRounded, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    text::{TextView, TextViewState},
    v_flex,
};

use crate::assistant;

const CONTENT_MAX_WIDTH: Pixels = px(760.);

/// While streaming, keep pinning to the bottom only when the viewport is within
/// this distance of the end — roughly one line of slack so tiny layout jitter
/// doesn't disengage the auto-follow.
const STICK_THRESHOLD: Pixels = px(48.);

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
                    _ => {}
                },
            );

        Self {
            title: title.into(),
            messages: Vec::new(),
            input,
            scroll_handle: ScrollHandle::new(),
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
    /// only while the user is already reading the bottom of the transcript —
    /// so scrolling up to re-read earlier turns isn't interrupted.
    pub fn follow_stream(&self) {
        if self.is_near_bottom() {
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

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(div().flex_1().min_h_0().map(|this| {
                if has_messages {
                    this.child(self.render_message_list(cx))
                } else {
                    this.child(render_empty_state(cx))
                }
            }))
            .child(self.render_input_area(send_disabled, cx))
    }
}

impl ChatView {
    fn render_message_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Relative, non-scrolling wrapper so the overlay scrollbar anchors to
        // the viewport instead of scrolling away with the content.
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("messages")
                    .size_full()
                    .track_scroll(&self.scroll_handle)
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .py_5()
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
            .pb_5()
            .child(
                v_flex()
                    .w_full()
                    .max_w(CONTENT_MAX_WIDTH)
                    .gap_1()
                    .bg(theme.input_background())
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius_lg)
                    .px_3()
                    .py_2()
                    .child(Input::new(&self.input).appearance(false))
                    .child(
                        h_flex()
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
                                    .rounded(ButtonRounded::Large)
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
