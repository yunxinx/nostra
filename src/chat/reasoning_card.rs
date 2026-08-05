//! Streaming reasoning ("chain of thought") card for one assistant content block.
//!
//! Reasoning deltas were already being folded into the turn's canonical content
//! as [`ContentBlock::Reasoning`](crate::llm::ContentBlock::Reasoning), but the
//! transcript only ever rendered the visible-text body — so everything a
//! reasoning model emitted was captured and then dropped on the floor. This
//! module is where it surfaces.
//!
//! Each card owns a [`MarkdownBody`](crate::ui::markdown::MarkdownBody), so
//! reasoning streams as live markdown
//! through the same `push_str` path as prose rather than being flattened into
//! it. Two properties keep it from taking over the transcript:
//!
//! * **A height budget of [`VISIBLE_LINES`] lines.** Past that the card scrolls
//!   internally instead of growing. Because the cap is a fixed pixel height, a
//!   long reasoning stream stops changing the card's outer size entirely — the
//!   prose below it is laid out once and then stays put, no matter how many
//!   tokens still arrive.
//! * **Auto-collapse when reasoning ends.** A finished block reads as a
//!   text-only trigger above it, not as a wall of intermediate thought. The
//!   trigger is a chip sized to its own label rather than a full-width bar.
//!
//! Auto-expand and auto-collapse are both suppressed once the user touches the
//! toggle (see [`ReasoningTrace::user_controlled`]): explicit intent outlives
//! any state the stream would otherwise impose.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

// Imported by name rather than glob: `gpui::*` exports a `test` macro that
// shadows the built-in `#[test]` attribute in this module's test submodule.
use gpui::{
    AnyElement, App, ElementId, FollowMode, InteractiveElement as _, IntoElement, ListState,
    ParentElement as _, Pixels, Point, ScrollHandle, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
    rems,
};
use gpui_component::{
    ActiveTheme, Sizable as _, button::Button, clipboard::Clipboard, h_flex,
    scroll::ScrollableElement as _, text::TextViewStyle, v_flex,
};
use rust_i18n::t;

use crate::ui::markdown::MarkdownBody;

/// Per-block test hook. Stable protocol slots make it possible to drive one
/// card without accidentally matching another card in the same turn.
fn block_selector(kind: &str, content_index: usize) -> String {
    format!("reasoning-{kind}-{content_index}")
}

/// Visible height budget for an expanded card, in lines of body text.
///
/// Applied both while streaming and after the user expands a finished trace, so
/// the card never dominates its surrounding message. Deliberately a *height*
/// rather than a line count the renderer enforces: markdown paragraph
/// gaps count against it, which is what makes the outer size predictable.
const VISIBLE_LINES: f32 = 7.;

/// Tighter than the 1rem markdown default. Reasoning tends to arrive as many
/// short paragraphs, and the default gap spends most of a 7-line budget on
/// whitespace.
const PARAGRAPH_GAP: f32 = 0.5;

/// Source-size gate for the retained block-list path. This avoids laying out a
/// large document merely to decide whether it needs virtualization.
const VIRTUALIZED_SOURCE_BYTES: usize = 4 * 1024;

#[derive(Clone)]
enum ReasoningScroll {
    Natural(ScrollHandle),
    Virtualized(ListState),
}

impl ReasoningScroll {
    fn offset(&self) -> Point<Pixels> {
        match self {
            Self::Natural(scroll) => scroll.offset(),
            Self::Virtualized(scroll) => scroll.scroll_px_offset_for_scrollbar(),
        }
    }

    fn max_offset(&self) -> Point<Pixels> {
        match self {
            Self::Natural(scroll) => scroll.max_offset(),
            Self::Virtualized(scroll) => scroll.max_offset_for_scrollbar(),
        }
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        match self {
            Self::Natural(scroll) => scroll.set_offset(offset),
            Self::Virtualized(scroll) => scroll.set_offset_from_scrollbar(offset),
        }
    }

    fn scroll_to_bottom(&self) {
        match self {
            Self::Natural(scroll) => scroll.scroll_to_bottom(),
            Self::Virtualized(scroll) => scroll.scroll_to_end(),
        }
    }
}

/// One reasoning content block, prepared for rendering.
///
/// Holds a markdown state entity, so — like
/// [`TurnError`](crate::chat::error_card::TurnError) — it must be constructed from an
/// update, never from inside a render pass.
pub(crate) struct ReasoningTrace {
    /// Live markdown state, appended to with `push_str` so the card streams
    /// instead of re-parsing on every delta.
    body: MarkdownBody,
    /// Whether the body is currently visible.
    expanded: bool,
    /// Set once the user works the toggle. From then on neither auto-expand nor
    /// auto-collapse overrides them.
    user_controlled: bool,
    /// Time spent streaming this reasoning block.
    elapsed: Option<Duration>,
    /// Start of this block's stream, cleared exactly once by [`Self::finish`].
    started_at: Option<Instant>,
    /// Keeps the newest reasoning in view while the card is capped and
    /// streaming, as long as the user has not scrolled away from the end.
    scroll: ReasoningScroll,
    source_bytes: usize,
    /// Whether streaming updates may pin the card to its end. This follows the
    /// transcript's behavior so reading earlier reasoning is not interrupted.
    follow: bool,
    /// Discrete wheel distance waiting for the same easing used by the transcript.
    smooth_scroll: super::SmoothScrollState,
}

impl ReasoningTrace {
    /// Open a trace for a block that just started reasoning. Auto-expanded: the
    /// point of streaming reasoning is that it is visible as it arrives.
    pub(crate) fn new(owner_id: u64, cx: &mut App) -> Self {
        let body = MarkdownBody::new("", owner_id, cx);
        Self {
            body,
            expanded: true,
            user_controlled: false,
            elapsed: None,
            started_at: Some(Instant::now()),
            scroll: ReasoningScroll::Natural(ScrollHandle::new()),
            source_bytes: 0,
            follow: true,
            smooth_scroll: super::SmoothScrollState::default(),
        }
    }

    /// Build a closed trace from an authoritative terminal message.
    pub(crate) fn completed(source: String, owner_id: u64, cx: &mut App) -> Self {
        let body = MarkdownBody::new(&source, owner_id, cx);
        let scroll = if source.len() >= VIRTUALIZED_SOURCE_BYTES {
            ReasoningScroll::Virtualized(body.scroll_state(cx))
        } else {
            ReasoningScroll::Natural(ScrollHandle::new())
        };
        Self {
            body,
            expanded: false,
            user_controlled: false,
            // A terminal-only block was not timed on this client.
            elapsed: None,
            started_at: None,
            scroll,
            source_bytes: source.len(),
            follow: true,
            smooth_scroll: super::SmoothScrollState::default(),
        }
    }

    /// Append a reasoning delta, and keep the newest text in view.
    ///
    /// Follow the end only while the user is still reading there. Once the
    /// card is scrolled upward, later deltas preserve that position until a
    /// downward gesture brings it back to the end.
    pub(crate) fn push(&mut self, delta: &str, cx: &mut App) {
        if delta.is_empty() {
            return;
        }
        self.body.push_str(delta, cx);
        self.source_bytes = self.source_bytes.saturating_add(delta.len());
        self.ensure_scroll_mode(cx);
        self.follow_scroll();
    }

    /// Close this block and bank its duration. Collapses the card unless the
    /// user has taken manual control.
    ///
    /// Idempotent: called on the explicit reasoning-block boundary and again as
    /// a terminal fallback for cancellation or malformed/incomplete streams.
    pub(crate) fn finish(&mut self) {
        if let Some(started_at) = self.started_at.take() {
            self.elapsed = Some(started_at.elapsed());
        }
        if !self.user_controlled {
            self.expanded = false;
        }
    }

    /// Toggle the body, and hand control of it to the user for good.
    pub(crate) fn toggle(&mut self) {
        self.user_controlled = true;
        self.expanded = !self.expanded;
    }

    /// Apply the terminal message's complete reasoning snapshot while retaining
    /// disclosure, timing, and keyed markdown entity accumulated during
    /// streaming.
    pub(crate) fn set_source(&mut self, source: &str, cx: &mut App) {
        self.body.set_text(source, cx);
        self.source_bytes = source.len();
        self.ensure_scroll_mode(cx);
        self.follow_scroll();
    }

    /// Record wheel intent for the card's own follow state. The surrounding
    /// transcript remains a separate scroll boundary.
    pub(crate) fn handle_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut App,
    ) {
        let dy = event.delta.pixel_delta(window.line_height()).y;
        if dy > px(0.) {
            self.follow = false;
        } else if dy < px(0.) && self.scroll_is_near_bottom() {
            self.follow = true;
            // A terminal stream may never append again. Migrate as part of the
            // gesture that returns to the tail instead of relying on `push` or
            // `set_source` to provide a later update boundary.
            self.ensure_scroll_mode(cx);
        }
    }

    pub(crate) fn current_scroll_offset(&self) -> gpui::Point<gpui::Pixels> {
        self.scroll.offset()
    }

    pub(crate) fn enqueue_smooth_scroll(
        &mut self,
        anchor: gpui::Point<gpui::Pixels>,
        distance: gpui::Pixels,
    ) {
        self.scroll.set_offset(anchor);
        self.smooth_scroll.enqueue(distance);
    }

    pub(crate) fn cancel_smooth_scroll(&mut self) {
        self.smooth_scroll.cancel_motion();
    }

    pub(crate) fn cancel_smooth_scroll_frame(&mut self) {
        self.smooth_scroll.frame_scheduled = false;
        self.smooth_scroll.cancel_motion();
    }

    pub(crate) fn mark_smooth_frame_scheduled(&mut self) -> bool {
        if self.smooth_scroll.frame_scheduled {
            return false;
        }
        self.smooth_scroll.frame_scheduled = true;
        true
    }

    pub(crate) fn advance_smooth_scroll(&mut self) -> Option<bool> {
        self.smooth_scroll.frame_scheduled = false;
        let step = self.smooth_scroll.next_step()?;
        let offset = self.scroll.offset();
        let max_offset = self.scroll.max_offset();
        let next_y = (offset.y + step).clamp(-max_offset.y, gpui::Pixels::ZERO);
        self.scroll.set_offset(gpui::point(offset.x, next_y));
        if next_y == offset.y {
            self.smooth_scroll.cancel_motion();
        }
        Some(self.smooth_scroll.remaining != gpui::Pixels::ZERO)
    }

    fn ensure_scroll_mode(&mut self, cx: &mut App) {
        if self.source_bytes >= VIRTUALIZED_SOURCE_BYTES
            && self.follow
            && matches!(self.scroll, ReasoningScroll::Natural(_))
        {
            self.scroll = ReasoningScroll::Virtualized(self.body.scroll_state(cx));
            if let ReasoningScroll::Virtualized(scroll) = &self.scroll {
                scroll.set_follow_mode(FollowMode::Tail);
            }
            self.scroll.scroll_to_bottom();
        }
    }

    fn scroll_is_near_bottom(&self) -> bool {
        self.scroll.max_offset().y + self.scroll.offset().y <= super::STICK_THRESHOLD
    }

    fn follow_scroll(&self) {
        if self.follow && self.scroll_is_near_bottom() {
            self.scroll.scroll_to_bottom();
        }
    }

    fn is_virtualized(&self) -> bool {
        matches!(self.scroll, ReasoningScroll::Virtualized(_))
    }

    /// Localized trigger text: a live label while thinking, the banked duration
    /// once done.
    fn label(&self, finished: bool) -> String {
        if !finished {
            return t!("chat.reasoning.streaming").to_string();
        }
        let Some(elapsed) = self.elapsed else {
            return t!("chat.reasoning.completed").to_string();
        };
        // One decimal, floored at 0.1s: a sub-100ms trace is real but reads as
        // "0 seconds", which looks like a bug rather than a fast model.
        let seconds = elapsed.as_secs_f64().max(0.1);
        t!(
            "chat.reasoning.finished",
            duration = format!("{seconds:.1}")
        )
        .to_string()
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self) -> bool {
        self.expanded
    }

    #[cfg(test)]
    pub(crate) fn body_entity_id(&self) -> gpui::EntityId {
        self.body.entity_id()
    }

    #[cfg(test)]
    pub(crate) fn elapsed(&self) -> Option<Duration> {
        self.elapsed
    }

    /// How far the card's own viewport can scroll, i.e. how much content the
    /// height budget is currently hiding. Non-zero means the cap has engaged.
    #[cfg(test)]
    pub(crate) fn scroll_max_offset(&self) -> gpui::Pixels {
        self.scroll.max_offset().y
    }

    #[cfg(test)]
    pub(crate) fn is_following(&self) -> bool {
        self.follow
    }

    #[cfg(test)]
    pub(crate) fn uses_virtualized_scroll(&self) -> bool {
        self.is_virtualized()
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> gpui::Point<gpui::Pixels> {
        self.scroll.offset()
    }

    #[cfg(test)]
    pub(crate) fn scroll_max(&self) -> gpui::Point<gpui::Pixels> {
        self.scroll.max_offset()
    }

    #[cfg(test)]
    pub(crate) fn smooth_scroll_remaining(&self) -> gpui::Pixels {
        self.smooth_scroll.remaining
    }
}

/// Stable identities used by one reasoning card.
pub(crate) struct ReasoningCardId {
    pub(crate) ui_id: u64,
    pub(crate) content_index: usize,
}

type ToggleHandler = Rc<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>;
type CopyValue = Rc<dyn Fn(&mut Window, &mut App) -> SharedString>;
type ScrollHandler = Rc<dyn Fn(&ScrollWheelEvent, &mut Window, &mut App)>;

/// Actions owned by one reasoning card.
pub(crate) struct ReasoningCardActions {
    pub(crate) on_toggle: ToggleHandler,
    pub(crate) copy_value: CopyValue,
    pub(crate) on_scroll: ScrollHandler,
}

/// Render one reasoning card. `id.ui_id` disambiguates every block across the
/// transcript so cards scroll and toggle independently.
///
/// Takes `&Entity<ChatView>`-free state on purpose: the toggle mutates the
/// trace through the supplied `on_toggle` listener, which the transcript builds
/// with `cx.listener`, so this function never re-enters the view that owns it.
pub(crate) fn render(
    trace: &ReasoningTrace,
    source: &str,
    finished: bool,
    id: ReasoningCardId,
    actions: ReasoningCardActions,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let ReasoningCardId {
        ui_id,
        content_index,
    } = id;
    let ReasoningCardActions {
        on_toggle,
        copy_value,
        on_scroll,
    } = actions;
    let theme = cx.theme();
    let (body_foreground, border, radius) = (
        // `GroupBox`'s own foreground, which falls back to `theme.foreground`.
        // Deliberately not `muted_foreground`: against the Nostra dark
        // background that pairing measures 3.9:1, under the 4.5:1 WCAG AA floor
        // for body text — and the reasoning body is prose meant to be read. The
        // smaller text size and the outlined frame carry the "secondary" signal
        // instead, rather than spending legibility on it.
        theme.group_box_foreground,
        theme.border,
        theme.radius,
    );

    let expanded = trace.expanded;
    // The budget is resolved against the live line height rather than a
    // hard-coded pixel value, so it still means seven lines after a text-size
    // or font change.
    let max_height = window.line_height() * VISIBLE_LINES;
    let virtualized = trace.is_virtualized();
    // Per-block group name. A shared constant would tie every card in the
    // transcript together, so hovering one would reveal every copy button.
    let hover_group: SharedString = format!("turn-reasoning-{ui_id}").into();

    // Same disclosure shape as the provider settings' "compatibility" section: a
    // small outline button that stays put in both states, with an outlined card
    // appearing *below* it when open. The trigger is never part of what expands,
    // so opening a trace does not restyle or resize the thing just clicked.
    v_flex()
        // Hover scope for the copy button, covering the trigger row *and* the
        // expanded card — hovering either reveals it, which is why the group sits
        // on the common ancestor rather than on the row.
        .group(hover_group.clone())
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .child(
                    // The trigger is a flex item on the main axis, so it stays at
                    // its intrinsic width instead of stretching across the column.
                    Button::new(ElementId::NamedInteger(
                        "turn-reasoning-toggle".into(),
                        ui_id,
                    ))
                    .outline()
                    // `compact` trims the small size's 12px inset to 6px: this is
                    // a quiet content marker, not a call to action.
                    .small()
                    .compact()
                    // Ceiling for a label some locale makes long enough to reach
                    // the column edge — it truncates there instead of widening the
                    // message. `min_w_0` is what lets it shrink past its content at
                    // all; `Button` wraps `.label()` in a `flex_none` div that
                    // cannot, so the text goes in as a child to stay truncatable.
                    .max_w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .child(div().min_w_0().text_ellipsis().child(trace.label(finished)))
                    .tooltip(if expanded {
                        t!("chat.reasoning.collapse").to_string()
                    } else {
                        t!("chat.reasoning.expand").to_string()
                    })
                    .on_click(move |event, window, cx| on_toggle(event, window, cx))
                    .debug_selector(move || block_selector("trigger", content_index)),
                )
                // Nothing to put on the clipboard until the first delta lands.
                .when(!source.is_empty(), |this| {
                    this.child(
                        // Revealed on group hover rather than conditionally
                        // rendered, so appearing costs no layout: the row already
                        // reserves the space. `invisible` also removes the nested
                        // button from hit testing and keyboard focus while hidden.
                        //
                        // `Clipboard` is `RenderOnce` and carries no `Styled`, so
                        // the group style goes on this wrapper.
                        div()
                            .flex_none()
                            .invisible()
                            .group_hover(hover_group.clone(), |this| this.visible())
                            .debug_selector(move || block_selector("copy", content_index))
                            .child(
                                Clipboard::new(ElementId::NamedInteger(
                                    "turn-reasoning-copy".into(),
                                    ui_id,
                                ))
                                .value_fn(move |window, cx| copy_value(window, cx))
                                .tooltip(t!("chat.reasoning.copy").to_string()),
                            ),
                    )
                }),
        )
        .when(expanded, |this| {
            this.child(
                // `relative` is load-bearing: `vertical_scrollbar` attaches an
                // absolutely-positioned, fully-inset overlay as a child of this
                // element, so it needs a positioning context here or the thumb
                // resolves against the nearest positioned ancestor instead —
                // which is the transcript, not the card. Same shape as the
                // transcript's own scrollbar host in `render_message_list`.
                //
                // Outline only, no fill, matching `GroupBox::outline()`.
                div()
                    .relative()
                    .w_full()
                    .rounded(radius)
                    .border_1()
                    .border_color(border)
                    .overflow_hidden()
                    .child(
                        div()
                            .id(ElementId::NamedInteger("turn-reasoning-body".into(), ui_id))
                            .debug_selector(move || block_selector("body", content_index))
                            .w_full()
                            // Short traces keep natural height. Long traces use a
                            // definite-height TextView so its retained list only
                            // materializes visible Markdown blocks.
                            .when(virtualized, |this| this.h(max_height))
                            .when(!virtualized, |this| {
                                this.max_h(max_height).overflow_y_scroll()
                            })
                            // This nested viewport must occlude the transcript's
                            // list hitbox. Otherwise the list's native wheel
                            // handler runs before this card's bubble handler.
                            .occlude()
                            .when(!virtualized, |this| {
                                let ReasoningScroll::Natural(scroll) = &trace.scroll else {
                                    return this;
                                };
                                this.track_scroll(scroll)
                            })
                            .px_3()
                            .py_2()
                            .text_sm()
                            .text_color(body_foreground)
                            // The card is a scroll container inside the
                            // transcript's own scroll container. Without this, a
                            // wheel tick the card cannot consume (already at its
                            // top or bottom) falls through and scrolls the
                            // transcript out from under the pointer. Same
                            // containment the floating composer applies.
                            .on_scroll_wheel(move |event, window, cx| {
                                on_scroll(event, window, cx);
                                cx.stop_propagation();
                            })
                            .child(if virtualized {
                                trace
                                    .body
                                    .scrollable_text_view(
                                        TextViewStyle::default().paragraph_gap(rems(PARAGRAPH_GAP)),
                                    )
                                    .into_any_element()
                            } else {
                                trace
                                    .body
                                    .text_view(
                                        TextViewStyle::default().paragraph_gap(rems(PARAGRAPH_GAP)),
                                    )
                                    .into_any_element()
                            }),
                    )
                    .map(|this| match &trace.scroll {
                        ReasoningScroll::Natural(scroll) => this.vertical_scrollbar(scroll),
                        ReasoningScroll::Virtualized(_) => this,
                    }),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both label states must resolve in every shipped locale — a missing key
    /// renders as the raw dotted path in the UI.
    #[test]
    fn label_keys_resolve_in_every_locale() {
        for locale in ["en", "zh-CN"] {
            for key in [
                "chat.reasoning.streaming",
                "chat.reasoning.completed",
                "chat.reasoning.collapse",
                "chat.reasoning.expand",
                "chat.reasoning.copy",
            ] {
                let resolved = t!(key, locale = locale).to_string();
                assert!(!resolved.contains(key), "{key} unresolved for {locale}");
            }
            let finished =
                t!("chat.reasoning.finished", locale = locale, duration = "3.2").to_string();
            assert!(finished.contains("3.2"), "{finished} for {locale}");
        }
    }

    /// WCAG relative luminance, per the sRGB definition.
    fn luminance(color: gpui::Hsla) -> f32 {
        let rgb = gpui::Rgba::from(color);
        let channel = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
    }

    fn contrast_ratio(a: gpui::Hsla, b: gpui::Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// The expanded body is prose the user is expected to actually read, so its
    /// colour pairing has to clear the WCAG AA floor for body text in every
    /// bundled theme — not just the one that happened to be active while this was
    /// being built.
    ///
    /// This is a real regression test: pairing `muted_foreground` with the card
    /// surface measured 2.8:1 under Nostra Dark, and the card is outlined rather
    /// than filled precisely so the body sits on `background` where the contrast
    /// budget is widest.
    #[gpui::test]
    fn reasoning_body_clears_wcag_aa_in_every_bundled_theme(cx: &mut gpui::TestAppContext) {
        use gpui_component::{Theme, ThemeMode};

        const AA_BODY_TEXT: f32 = 4.5;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::preferences::init_global(crate::preferences::Preferences::default(), cx);
            crate::appearance::theme::init(&crate::preferences::Preferences::default(), cx);

            for dark in [true, false] {
                // `Theme::change` rather than `theme::set_mode`, which would
                // persist to the user's real configuration directory.
                Theme::change(
                    if dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                    None,
                    cx,
                );

                for name in crate::appearance::theme::theme_names(dark, cx) {
                    // Palette coverage must not pass through the production
                    // preference writer: tests may run beside the real app or
                    // in parallel with other theme tests.
                    crate::appearance::theme::select_theme_for_test(name.as_ref(), cx);

                    // What `render` actually pairs: the body's own colour against
                    // the surface it sits on, which for an outline-only card is
                    // the window background.
                    let ratio =
                        contrast_ratio(cx.theme().group_box_foreground, cx.theme().background);
                    assert!(
                        ratio >= AA_BODY_TEXT,
                        "{name}: reasoning body contrast is {ratio:.2}:1, below the \
                         {AA_BODY_TEXT}:1 WCAG AA floor for body text"
                    );
                }
            }
        });
    }

    #[gpui::test]
    fn label_reports_thinking_while_streaming_then_the_banked_duration(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();
        let mut trace = window.update(|_, cx| ReasoningTrace::new(1, cx));

        assert_eq!(
            trace.label(false),
            t!("chat.reasoning.streaming").to_string()
        );

        trace.finish();
        let finished = trace.label(true);
        assert_ne!(finished, t!("chat.reasoning.streaming").to_string());
        assert!(
            finished.contains("0.1"),
            "a sub-100ms trace floors at 0.1 rather than reading as zero: {finished}"
        );
    }

    #[gpui::test]
    fn finish_is_idempotent_and_preserves_user_disclosure(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();
        let mut trace = window.update(|_, cx| ReasoningTrace::new(1, cx));

        trace.finish();
        let elapsed = trace.elapsed;
        assert!(trace.started_at.is_none(), "the clock was stopped");

        // Closing an already-closed trace must not bank more time, and must not
        // re-collapse a card the user has since opened.
        trace.toggle();
        trace.finish();
        assert_eq!(trace.elapsed, elapsed);
        assert!(trace.is_expanded(), "user intent is preserved");
    }

    #[gpui::test]
    fn terminal_only_block_uses_an_untimed_completion_label(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();
        let trace =
            window.update(|_, cx| ReasoningTrace::completed("complete block".into(), 1, cx));

        assert_eq!(
            trace.label(true),
            t!("chat.reasoning.completed").to_string()
        );
    }

    #[gpui::test]
    fn empty_deltas_do_not_start_or_change_timing(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();
        let mut trace = window.update(|_, cx| ReasoningTrace::new(1, cx));
        trace.finish();
        let elapsed = trace.elapsed;

        window.update(|_, cx| trace.push("", cx));
        assert!(
            trace.started_at.is_none(),
            "an empty delta has no lifecycle meaning"
        );
        assert_eq!(trace.elapsed, elapsed);
    }
}
