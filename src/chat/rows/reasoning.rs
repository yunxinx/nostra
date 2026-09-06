//! Row renderer for one reasoning ("chain of thought") part.
//!
//! Two-phase form (PRD R1). While the part streams, the row is a fixed-height
//! tail-following preview: [`typography::PREVIEW_LINES`] lines of body text in
//! an outlined card, with a top fade into the pane background once content has
//! scrolled above the viewport. The outer height never changes during the
//! stream, so the prose below the row is laid out once and stays put (AC1),
//! and the preview itself is a scrollable `TextView` from the first delta —
//! there is no Natural ↔ Virtualized scroll migration anywhere in this
//! renderer.
//!
//! When the stream ends the row folds to a trigger row ("Thought for Ns" /
//! the localized fallback) with a copy action. Expanding gives a body at
//! `min(natural height, cap)` where the cap is
//! `max(BUDGET_MIN_LINES lines, viewport × 45%)` (PRD R3): short traces render
//! at their own height, longer ones scroll inside a viewport of exactly the
//! cap. The natural height converges through the painted-body measurement
//! ([`RowAction::BodyMeasured`]) plus the body's layout observers. Auto
//! collapse yields to the user the first time they work the toggle
//! (`user_controlled`).
//!
//! The trigger's "Thought for Ns" duration comes from the part content
//! (`ReasoningContent::duration_ms`): the coalescer stamps it at event
//! arrival (R10) and the gateway banks it on the persisted assistant message
//! (R7), so a restored session shows the real thinking time. The renderer
//! owns no timer of its own.
//!
//! Wheel input inside the preview or the clamped viewport is forwarded to
//! the view through [`RowAction::ReplayNestedScroll`], which owns the easing
//! constants, the painted-frame anchor restore, the window-activation check,
//! and the nested scroll boundary; the renderer owns only the follow flag and
//! the queued distance ([`NestedScrollReplay`]).

use std::{rc::Rc, time::Duration};

use gpui::{
    AnyElement, App, ElementId, FollowMode, InteractiveElement as _, IntoElement, KeyDownEvent,
    ListState, ParentElement as _, Pixels, Role, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, linear_color_stop, linear_gradient,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, Icon, IconName, clipboard::Clipboard, h_flex, v_flex,
};
use rust_i18n::t;

use crate::chat::projection::{DisclosureState, ReasoningDisclosure, RowKind};
use crate::chat::transcript::PartSource;
use crate::chat::{STICK_THRESHOLD, SmoothScrollState};
use crate::ui::markdown::{MarkdownBody, MarkdownPresentation};

use super::{
    DisclosureTarget, MaterializeContext, NestedScrollReplay, RowAction, RowChange,
    RowRenderContext, RowRenderer, typography,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReasoningPhase {
    Streaming,
    /// The banked stream duration, read from the part content
    /// (`ReasoningContent::duration_ms`), never measured here.
    Finished {
        elapsed: Option<Duration>,
    },
}

/// "N s" under a minute, "M m S s" under an hour, "H h M m" beyond — whole
/// seconds, ASCII digits, trailing units dropped when their value is zero
/// (PRD R7). A burst-backfilled block whose duration is ~0 renders as "0 s".
pub(crate) fn format_reasoning_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        let minutes = seconds / 60;
        let remainder = seconds % 60;
        if remainder == 0 {
            format!("{minutes} m")
        } else {
            format!("{minutes} m {remainder} s")
        }
    } else {
        let hours = seconds / 3600;
        let minutes = (seconds % 3600) / 60;
        if minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {minutes} m")
        }
    }
}

/// A copy button hidden until the pointer enters `hover_group`.
///
/// `id` must be unique within the window: [`Clipboard`] keys its Copy→Check
/// feedback state by it, so a stable id keeps that state across
/// reconciliation and list reordering. `value_fn` runs at click time rather
/// than capturing a render snapshot, so the clipboard always reflects the
/// latest state.
fn hidden_until_hover_copy(
    id: impl Into<ElementId>,
    hover_group: SharedString,
    tooltip: impl Into<SharedString>,
    value_fn: impl Fn(&mut Window, &mut App) -> SharedString + 'static,
    debug_selector: impl FnOnce() -> String,
) -> impl IntoElement {
    div()
        .flex_none()
        .debug_selector(debug_selector)
        .invisible()
        .group_hover(hover_group, |this| this.visible())
        .child(Clipboard::new(id).value_fn(value_fn).tooltip(tooltip))
}

/// Per-part test hook. Stable protocol slots make it possible to drive one
/// row without accidentally matching another reasoning row in the same turn.
fn block_selector(kind: &str, content_index: usize) -> String {
    format!("reasoning-{kind}-{content_index}")
}

pub(crate) struct ReasoningRenderer {
    /// Stable element-id base: the part's ui id survives list splices.
    ui_id: u64,
    content_index: usize,
    display: String,
    body: Option<MarkdownBody>,
    /// Handle onto the body's retained list, captured when the body is
    /// created. A clone shares the retained state, so follow, easing, and
    /// test observations all drive the same viewport the TextView renders.
    scroll: Option<ListState>,
    phase: ReasoningPhase,
    disclosure: ReasoningDisclosure,
    /// Set once the user works a toggle. From then on neither auto-collapse
    /// nor terminal reconciliation overrides their choice.
    user_controlled: bool,
    /// Whether streaming updates may pin the preview to its tail. Upward
    /// wheel input disarms it; a downward gesture at the end re-arms it.
    follow: bool,
    smooth: SmoothScrollState,
    owner_id: u64,
    presentation: Option<MarkdownPresentation>,
    materialized: bool,
    /// Painted height of the expanded body in its natural-height form, as
    /// reported through [`RowAction::BodyMeasured`]. While it stays at or
    /// below the cap the body renders at natural height; the first report
    /// above the cap switches the form to the clamped viewport. `None` until
    /// the natural form has painted once.
    natural_height: Option<Pixels>,
}

impl ReasoningRenderer {
    pub(crate) fn new() -> Self {
        Self {
            ui_id: 0,
            content_index: 0,
            display: String::new(),
            body: None,
            scroll: None,
            phase: ReasoningPhase::Streaming,
            disclosure: ReasoningDisclosure::Collapsed,
            user_controlled: false,
            follow: true,
            smooth: SmoothScrollState::default(),
            owner_id: 0,
            presentation: None,
            materialized: false,
            natural_height: None,
        }
    }

    /// Build the body from the accumulated display text. A streaming part
    /// streams through `push_str` afterwards; a terminal part gets the
    /// authoritative document.
    fn build_body(&mut self, cx: &mut App) {
        if self.display.is_empty() {
            self.body = None;
            return;
        }
        let presentation = self.presentation.clone();
        let Some(presentation) = presentation else {
            return;
        };
        let body = if matches!(self.phase, ReasoningPhase::Streaming) {
            let body = MarkdownBody::new_streaming_with_presentation(
                &self.display,
                self.owner_id,
                &presentation,
                cx,
            );
            // Tail-follow while streaming, but only while follow is armed:
            // a re-materialized row whose follow the user disarmed keeps
            // their viewport instead of being snapped back to the tail.
            if self.follow {
                body.scroll_state(cx).set_follow_mode(FollowMode::Tail);
            }
            body
        } else {
            MarkdownBody::new_with_presentation(&self.display, self.owner_id, &presentation, cx)
        };
        self.scroll = Some(body.scroll_state(cx));
        self.body = Some(body);
    }

    /// The body whose layout the expanded row depends on — the
    /// natural-height measurement dependency. Both expanded forms (natural
    /// and clamped) declare it: parse and layout completion gate the row's
    /// `Settled` confidence either way.
    fn natural_height_body(&self) -> Option<&MarkdownBody> {
        if matches!(self.phase, ReasoningPhase::Finished { .. })
            && self.disclosure == ReasoningDisclosure::Expanded
        {
            self.body.as_ref()
        } else {
            None
        }
    }

    /// Whether the expanded body renders inside the clamped, internally
    /// scrollable viewport rather than at natural height.
    ///
    /// Three inputs decide, all conservative in the same direction — never
    /// lay out more than the cap in one frame, never clamp content that
    /// plausibly fits:
    ///
    /// - sources past the windowed thresholds are always clamped: the windowed
    ///   block layout cannot combine with an internal-scroll viewport, and the
    ///   clamped viewport bounds its own per-frame layout work;
    /// - a painted natural height above the cap clamps exactly;
    /// - before the first paint, a source-length estimate over twice the cap
    ///   avoids one full-height layout frame for long traces; the 2× slack
    ///   keeps the coarse characters-per-line heuristic from clamping content
    ///   that actually fits.
    fn expanded_uses_clamped_viewport(
        &self,
        body: &MarkdownBody,
        cap: Pixels,
        line_height: Pixels,
    ) -> bool {
        if typography::windowed_body(self.display.len(), body.block_count()) {
            return true;
        }
        if self.natural_height.is_some_and(|height| height > cap) {
            return true;
        }
        let estimated_lines = (self.display.len() as f32 / typography::ESTIMATE_CHARS_PER_LINE)
            .ceil()
            .max(1.);
        estimated_lines * line_height > cap * 2.
    }

    /// Scroll the body to its end when tail follow is armed and the user has
    /// not moved away from the end. Belt-and-braces next to
    /// `FollowMode::Tail`, which already keeps growing content pinned.
    fn follow_tail(&self) {
        if !self.follow {
            return;
        }
        let Some(scroll) = self.scroll.as_ref() else {
            return;
        };
        if scroll.max_offset_for_scrollbar().y + scroll.scroll_px_offset_for_scrollbar().y
            <= STICK_THRESHOLD
        {
            scroll.scroll_to_end();
        }
    }

    /// The banked stream duration carried by `ctx`'s part content, or `None`
    /// for a block this client never saw stream (e.g. replay-only reasoning).
    fn banked_duration(ctx: &MaterializeContext) -> Option<Duration> {
        let part = ctx.part?;
        let PartSource::Reasoning { reasoning, .. } = &part.source else {
            return None;
        };
        reasoning.duration_ms.map(Duration::from_millis)
    }

    /// Fold to the finished phase with `elapsed` as the banked duration. The
    /// value is set unconditionally: a terminal reconciliation may arrive
    /// after the stream's own finish event and carries the authoritative
    /// duration.
    fn turn_finished(&mut self, elapsed: Option<Duration>) {
        if let ReasoningPhase::Finished { elapsed: current } = &mut self.phase {
            *current = elapsed;
        } else {
            self.phase = ReasoningPhase::Finished { elapsed };
        }
        if !self.user_controlled {
            self.disclosure = ReasoningDisclosure::Collapsed;
        }
    }

    /// Localized trigger text: the banked duration once done, the fallback
    /// for a terminal block that was never timed on this client.
    fn label(&self) -> String {
        let ReasoningPhase::Finished { elapsed } = self.phase else {
            return t!("chat.reasoning.completed").to_string();
        };
        let Some(elapsed) = elapsed else {
            return t!("chat.reasoning.completed").to_string();
        };
        t!(
            "chat.reasoning.finished",
            duration = format_reasoning_duration(elapsed)
        )
        .to_string()
    }
}

impl ReasoningRenderer {
    #[cfg(test)]
    pub(crate) fn is_expanded(&self) -> bool {
        match self.phase {
            ReasoningPhase::Streaming => true,
            ReasoningPhase::Finished { .. } => self.disclosure != ReasoningDisclosure::Collapsed,
        }
    }

    #[cfg(test)]
    pub(crate) fn toggle_for_test(&mut self) {
        self.user_controlled = true;
        self.disclosure = if self.disclosure == ReasoningDisclosure::Expanded {
            ReasoningDisclosure::Collapsed
        } else {
            ReasoningDisclosure::Expanded
        };
    }

    #[cfg(test)]
    pub(crate) fn elapsed(&self) -> Option<Duration> {
        match self.phase {
            ReasoningPhase::Streaming => None,
            ReasoningPhase::Finished { elapsed } => elapsed,
        }
    }

    #[cfg(test)]
    pub(crate) fn label_for_test(&self) -> String {
        self.label()
    }

    #[cfg(test)]
    pub(crate) fn body_entity_id(&self) -> Option<gpui::EntityId> {
        self.body.as_ref().map(MarkdownBody::entity_id)
    }

    #[cfg(test)]
    pub(crate) fn owner_id(&self) -> u64 {
        self.owner_id
    }

    #[cfg(test)]
    pub(crate) fn body_for_test(&self) -> Option<&MarkdownBody> {
        self.body.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn body_state(&self) -> Option<ListState> {
        self.scroll.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_following(&self) -> bool {
        self.follow
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> gpui::Point<gpui::Pixels> {
        self.body_state()
            .map(|scroll| scroll.scroll_px_offset_for_scrollbar())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn scroll_max(&self) -> gpui::Point<gpui::Pixels> {
        self.body_state()
            .map(|scroll| scroll.max_offset_for_scrollbar())
            .unwrap_or_default()
    }

    /// How far the body's own viewport can scroll, i.e. how much content the
    /// height cap is hiding. Non-zero means the cap engaged.
    #[cfg(test)]
    pub(crate) fn scroll_max_offset(&self) -> gpui::Pixels {
        self.scroll_max().y
    }

    #[cfg(test)]
    pub(crate) fn smooth_scroll_remaining(&self) -> gpui::Pixels {
        self.smooth.remaining
    }

    /// Whether the current form renders a scrollable (retained-list) body.
    #[cfg(test)]
    pub(crate) fn is_scrollable(&self) -> bool {
        self.scroll.is_some()
    }

    /// The painted natural height of the expanded body, once its natural
    /// form has painted. A trace pre-clamped from its source length never
    /// paints natural, so this stays `None` there.
    #[cfg(test)]
    pub(crate) fn natural_height_for_test(&self) -> Option<gpui::Pixels> {
        self.natural_height
    }
}

impl RowRenderer for ReasoningRenderer {
    fn kind(&self) -> RowKind {
        RowKind::Reasoning
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.owner_id = ctx.owner_id;
        self.presentation = Some(ctx.presentation.clone());
        self.ui_id = ctx.row_id.part.as_u64();
        if let Some(part) = ctx.part {
            self.content_index = part.content_index;
            // A live insert seeds empty: stream batches publish Insert
            // then Append after the model already carries the delta, and
            // the following Append replays the accumulated source (P1
            // empty-seed rule). A late materialization (cold restore
            // mid-stream, first layout) re-reads the accumulated content
            // so no prefix is lost.
            let duration = if let PartSource::Reasoning { reasoning, .. } = &part.source {
                if part.finished || !ctx.append_replays_part {
                    self.display = reasoning.display.clone();
                } else {
                    self.display = String::new();
                }
                reasoning.duration_ms.map(Duration::from_millis)
            } else {
                None
            };
            if part.finished {
                // Historical content: the duration banked on the persisted
                // message is the only timing there is (R7).
                self.phase = ReasoningPhase::Finished { elapsed: duration };
            } else if matches!(self.phase, ReasoningPhase::Finished { .. }) {
                self.phase = ReasoningPhase::Streaming;
            }
        }
        // A fresh body has no painted natural height yet.
        self.natural_height = None;
        self.build_body(cx);
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.body = None;
        self.scroll = None;
        self.smooth.cancel_motion();
        self.natural_height = None;
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App) {
        match change {
            RowChange::Append { delta } => {
                self.display.push_str(delta);
                if let Some(body) = self.body.as_mut() {
                    body.push_str(delta, cx);
                } else if !self.display.is_empty() {
                    // First content on a row that materialized empty.
                    self.owner_id = ctx.owner_id;
                    self.presentation = Some(ctx.presentation.clone());
                    self.build_body(cx);
                }
                self.follow_tail();
            }
            RowChange::Finished => {
                if let Some(body) = self.body.as_mut() {
                    body.finish(cx);
                }
                // The transcript already banked the coalescer's duration on
                // the part before publishing this event, so re-reading the
                // part is the same read the renderer would have done at
                // materialize time (R10).
                self.turn_finished(Self::banked_duration(ctx));
            }
            RowChange::Replace => {
                // Reuse semantics: keep the markdown entity when the part
                // survived reconciliation so keyed caches and the retained
                // list state persist.
                let next = ctx
                    .reasoning_display()
                    .map(str::to_string)
                    .unwrap_or_default();
                let now_finished = ctx.part.is_some_and(|part| part.finished);
                self.owner_id = ctx.owner_id;
                self.presentation = Some(ctx.presentation.clone());
                if let Some(body) = self.body.as_mut() {
                    if self.display != next {
                        body.set_text(&next, cx);
                        // The replacement's natural height is unknown again.
                        self.natural_height = None;
                    }
                    if now_finished {
                        body.finish(cx);
                    }
                } else {
                    self.display = next.clone();
                    self.build_body(cx);
                }
                self.display = next;
                if now_finished {
                    // Terminal reconciliation: the authoritative message
                    // carries the gateway-banked duration and it wins; a
                    // message without timing leaves whatever the stream
                    // already banked on the part.
                    let banked = Self::banked_duration(ctx);
                    let elapsed = match self.phase {
                        ReasoningPhase::Streaming => banked,
                        ReasoningPhase::Finished { elapsed } => banked.or(elapsed),
                    };
                    self.turn_finished(elapsed);
                }
                self.follow_tail();
            }
        }
    }

    fn render(&self, ctx: &RowRenderContext, window: &mut Window, cx: &mut App) -> AnyElement {
        let Some(body) = self.body.as_ref() else {
            // Replay-only or not-yet-streamed part: nothing visible.
            return div().into_any_element();
        };
        match self.phase {
            ReasoningPhase::Streaming => self.render_preview(ctx, body, window, cx),
            ReasoningPhase::Finished { .. } => self.render_finished(ctx, body, window, cx),
        }
    }

    fn copy_source(
        &self,
        _transcript: &crate::chat::transcript::Transcript,
    ) -> Option<gpui::SharedString> {
        Some(self.display.clone().into())
    }

    fn disclosure(&self) -> DisclosureState {
        DisclosureState {
            reasoning: self.disclosure,
            reasoning_user_controlled: self.user_controlled,
            ..DisclosureState::default()
        }
    }

    fn sync_disclosure(&mut self, disclosure: DisclosureState) {
        self.disclosure = disclosure.reasoning;
        // Sticky once set: a re-materialized row must not lose the fact that
        // the user overrode the auto behavior (PRD R1 user-intent rule).
        self.user_controlled = self.user_controlled || disclosure.reasoning_user_controlled;
    }

    fn toggle_disclosure(&mut self, target: DisclosureTarget, _cx: &mut App) {
        if target == DisclosureTarget::Reasoning {
            self.user_controlled = true;
            self.disclosure = if self.disclosure == ReasoningDisclosure::Expanded {
                ReasoningDisclosure::Collapsed
            } else {
                ReasoningDisclosure::Expanded
            };
        }
    }

    fn nested_scroll_replay(&mut self) -> Option<NestedScrollReplay<'_>> {
        let scroll = self.scroll.clone()?;
        Some(NestedScrollReplay {
            scroll,
            follow: &mut self.follow,
            smooth: &mut self.smooth,
        })
    }

    fn visit_layout_dependencies(&self, visit: &mut dyn FnMut(&MarkdownBody)) {
        if let Some(body) = self.natural_height_body() {
            visit(body);
        }
    }

    fn note_body_height(&mut self, height: Pixels, cap: Pixels) -> bool {
        let was_clamped = self.natural_height.is_some_and(|previous| previous > cap);
        let now_clamped = height > cap;
        self.natural_height = Some(height);
        now_clamped != was_clamped
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl ReasoningRenderer {
    fn render_preview(
        &self,
        ctx: &RowRenderContext,
        body: &MarkdownBody,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = cx.theme();
        let background = theme.background;
        let card_outline = crate::appearance::contrast::pane_outline(theme.border, cx);
        let text_color =
            crate::appearance::contrast::text_on(theme.group_box_foreground, background, cx);
        let line_height = window.line_height();
        let preview_height = line_height * typography::PREVIEW_LINES;
        // The painted-frame native anchor is captured at render time, the
        // same way the transcript listener does, so the eased replay never
        // skips. It is negative as soon as content sits above the viewport —
        // exactly when the top fade belongs.
        let anchor = self
            .scroll
            .as_ref()
            .map(|scroll| scroll.scroll_px_offset_for_scrollbar())
            .unwrap_or_default();
        let show_fade = anchor.y < Pixels::ZERO;

        let row_id = ctx.row_id;
        let dispatch = ctx.dispatch.clone();
        let content_index = self.content_index;

        // The streaming preview keeps the pre-refactor card: outlined, no
        // fill, clipped (R9). The horizontal padding lives on the scrollable
        // TextView itself so its absolutely positioned scrollbar is measured
        // against the full card width and lands in the right-hand gutter
        // instead of over the text. Vertical padding is deliberately absent:
        // the preview is a window on the tail-following stream, and a padded
        // tail viewport defeats the native nested-scroll replay (the wheel
        // gesture re-pins to the padding-extended tail instead of moving).
        div()
            .relative()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(card_outline)
            .overflow_hidden()
            .debug_selector(move || block_selector("card", content_index))
            .child(
                div()
                    .id(ElementId::NamedInteger(
                        "turn-reasoning-body".into(),
                        self.ui_id,
                    ))
                    .debug_selector(move || block_selector("body", content_index))
                    .relative()
                    .w_full()
                    .h(preview_height)
                    .on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
                        dispatch.send(
                            RowAction::ReplayNestedScroll {
                                row_id,
                                anchor,
                                dy: event.delta.pixel_delta(window.line_height()).y,
                                precise: event.delta.precise(),
                            },
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .size_full()
                            .min_w_0()
                            .debug_selector(move || block_selector("viewport", content_index))
                            .child(
                                body.scrollable_text_view(typography::reasoning(cx))
                                    .text_sm()
                                    .text_color(text_color)
                                    .px_3(),
                            ),
                    )
                    .when(show_fade, |this| {
                        this.child(
                            // The fade is one line tall and fades the pane
                            // background out over the oldest visible text.
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(line_height)
                                .bg(linear_gradient(
                                    180.,
                                    linear_color_stop(background, 0.),
                                    linear_color_stop(background.opacity(0.), 1.),
                                )),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_finished(
        &self,
        ctx: &RowRenderContext,
        body: &MarkdownBody,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = cx.theme();
        let background = theme.background;
        let card_outline = crate::appearance::contrast::pane_outline(theme.border, cx);
        let text_color =
            crate::appearance::contrast::text_on(theme.group_box_foreground, background, cx);
        // The disclosure's secondary tier: derived like the sidebar's quiet
        // labels — the colour first clears the body-text floor on the pane,
        // then the strength lowers it below the body tier so it reads as
        // visibly "faded" in both light and dark themes (R4). Hovering the
        // trigger lifts the tier back to the floor-cleared base (R8).
        let header_base =
            crate::appearance::contrast::text_on(theme.muted_foreground, background, cx);
        let header_text = crate::appearance::contrast::transcript_muted_text(cx, 0.6);
        let hover_text = header_base;
        let line_height = window.line_height();
        let cap = typography::reasoning_cap(line_height, ctx.viewport_height);
        let expanded = self.disclosure != ReasoningDisclosure::Collapsed;
        let clamped = self.expanded_uses_clamped_viewport(body, cap, line_height);

        let ui_id = self.ui_id;
        let content_index = self.content_index;
        let hover_group: SharedString = format!("turn-reasoning-{ui_id}").into();
        let label = self.label();
        let focus_ring = theme.ring.opacity(0.2);

        let dispatch_toggle = ctx.dispatch.clone();
        let row_id = ctx.row_id;
        type ToggleFn = Rc<dyn Fn(&mut Window, &mut App)>;
        let on_toggle: ToggleFn = Rc::new(move |window: &mut Window, cx: &mut App| {
            dispatch_toggle.send(
                RowAction::ToggleDisclosure {
                    row_id,
                    target: DisclosureTarget::Reasoning,
                },
                window,
                cx,
            );
        });

        type CopyValue = Rc<dyn Fn(&mut Window, &mut App) -> SharedString>;
        let dispatch_copy = ctx.dispatch.clone();
        let copy_value: CopyValue = Rc::new(move |_, cx| dispatch_copy.clipboard_value(row_id, cx));

        let dispatch_scroll = ctx.dispatch.clone();
        let anchor = self
            .scroll
            .as_ref()
            .map(|scroll| scroll.scroll_px_offset_for_scrollbar())
            .unwrap_or_default();
        let on_scroll = Rc::new(
            move |event: &ScrollWheelEvent, window: &mut Window, cx: &mut App| {
                dispatch_scroll.send(
                    RowAction::ReplayNestedScroll {
                        row_id,
                        anchor,
                        dy: event.delta.pixel_delta(window.line_height()).y,
                        precise: event.delta.precise(),
                    },
                    window,
                    cx,
                );
                cx.stop_propagation();
            },
        );

        // A keyed tab-stop focus handle for the trigger row, stable across
        // list splices through the part's ui id (Custom Clickable Rows).
        let focus_handle = window
            .use_keyed_state(
                ElementId::NamedInteger("turn-reasoning-toggle".into(), ui_id),
                cx,
                |_, cx| cx.focus_handle(),
            )
            .read(cx)
            .clone();
        let toggle_id = ElementId::NamedInteger("turn-reasoning-toggle".into(), ui_id);
        let aria_label: SharedString = if expanded {
            t!("chat.reasoning.collapse").to_string()
        } else {
            t!("chat.reasoning.expand").to_string()
        }
        .into();
        let on_toggle_key = on_toggle.clone();
        let on_toggle_click = on_toggle.clone();

        v_flex()
            // Hover scope for the copy button, covering the trigger row and
            // the expanded body — hovering either reveals it.
            .group(hover_group.clone())
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(
                        // The disclosure trigger (R4): a quiet custom
                        // clickable row — the "Thought for Ns" label with a
                        // trailing chevron, like the sidebar's section
                        // headers, in the pane's muted secondary tier —
                        // instead of a button. `h_flex` is load-bearing: a
                        // bare `div()` defaults to block layout in this fork
                        // and stacks its children vertically. The trigger is
                        // a flex item on the main axis, so it stays at its
                        // intrinsic width instead of stretching across the
                        // column; the label truncates if some locale makes it
                        // long enough to reach the column edge.
                        h_flex()
                            .id(toggle_id)
                            .debug_selector(move || block_selector("trigger", content_index))
                            .role(Role::Button)
                            .aria_label(aria_label)
                            .aria_expanded(expanded)
                            .track_focus(&focus_handle.tab_stop(true))
                            .focus_visible(move |this| this.border_1().border_color(focus_ring))
                            .items_center()
                            .gap_0p5()
                            .min_w_0()
                            .max_w_full()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(header_text)
                            // The trigger reads as clickable while hovered:
                            // the quiet tier lifts to its floor-cleared base
                            // colour — text highlight, not a background tint
                            // (R8). No padding — the label stays flush with
                            // the prose column (R4).
                            .hover(move |this| this.text_color(hover_text))
                            // Desktop default cursor, per the Custom Clickable
                            // Rows contract (the arrow every other custom row
                            // uses, not a link hand).
                            .cursor_default()
                            .on_key_down(
                                move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                                    if crate::ui::consume_button_key(event, window, cx) {
                                        on_toggle_key(window, cx);
                                    }
                                },
                            )
                            .on_click(move |_, window: &mut Window, cx: &mut App| {
                                on_toggle_click(window, cx)
                            })
                            .flex_none()
                            .child(div().min_w_0().text_ellipsis().child(label))
                            .child(Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })),
                    )
                    // Nothing to put on the clipboard until the block's
                    // stream ends: a copy offered mid-stream would freeze a
                    // partial thought.
                    .when(!self.display.trim().is_empty(), |this| {
                        this.child(hidden_until_hover_copy(
                            ElementId::NamedInteger("turn-reasoning-copy".into(), ui_id),
                            hover_group.clone(),
                            t!("chat.reasoning.copy").to_string(),
                            move |window, cx| copy_value(window, cx),
                            move || block_selector("copy", content_index),
                        ))
                    }),
            )
            .when(expanded && clamped, |this| {
                // The cap engaged: a fixed-height viewport with the body's
                // own internal scrollbar, inside the outlined card (R9). The
                // horizontal padding lives on the scrollable TextView so its
                // scrollbar is measured against the full card width and
                // lands in the right-hand gutter instead of over the text.
                this.child(
                    div()
                        .relative()
                        .w_full()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(card_outline)
                        .overflow_hidden()
                        .debug_selector(move || block_selector("card", content_index))
                        .child(
                            div()
                                .id(ElementId::NamedInteger(
                                    "turn-reasoning-body".into(),
                                    self.ui_id,
                                ))
                                .debug_selector(move || block_selector("body", content_index))
                                .w_full()
                                .h(cap)
                                .on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
                                    on_scroll(event, window, cx);
                                })
                                .child(
                                    div()
                                        .size_full()
                                        .min_w_0()
                                        .debug_selector(move || {
                                            block_selector("viewport", content_index)
                                        })
                                        .child(
                                            body.scrollable_text_view(typography::reasoning(cx))
                                                .text_sm()
                                                .text_color(text_color)
                                                .px_3()
                                                .py_2(),
                                        ),
                                ),
                        ),
                )
            })
            .when(expanded && !clamped, |this| {
                // Natural height: content at or below the cap renders at its
                // own height with no inner scrollbar, inside the outlined
                // card (R9). The wrapper reports its painted height back so
                // the form can clamp the moment the content outgrows the cap.
                let dispatch_measure = ctx.dispatch.clone();
                this.child(
                    div()
                        .relative()
                        .w_full()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(card_outline)
                        .overflow_hidden()
                        .debug_selector(move || block_selector("card", content_index))
                        .child(
                            div()
                                .w_full()
                                .text_sm()
                                .text_color(text_color)
                                .debug_selector(move || block_selector("body", content_index))
                                .on_prepaint(
                                    move |bounds: gpui::Bounds<Pixels>,
                                          window: &mut Window,
                                          cx: &mut App| {
                                        dispatch_measure.send(
                                            RowAction::BodyMeasured {
                                                row_id,
                                                height: bounds.size.height,
                                            },
                                            window,
                                            cx,
                                        );
                                    },
                                )
                                .child(body.text_view(typography::reasoning(cx)).px_3().py_2()),
                        ),
                )
            })
            .into_any_element()
    }
}
