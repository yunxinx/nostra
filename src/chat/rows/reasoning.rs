//! Row renderer for one reasoning ("chain of thought") part.
//!
//! Two-phase form (PRD R1). While the part streams, the row is a fixed-height
//! tail-following preview: [`typography::PREVIEW_LINES`] lines of body text on
//! a left rail, no frame, no fill, with a top fade into the pane background
//! once content has scrolled above the viewport. The outer height never
//! changes during the stream, so the prose below the row is laid out once and
//! stays put (AC1), and the preview itself is a scrollable `TextView` from the
//! first delta — there is no Natural ↔ Virtualized scroll migration anywhere
//! in this renderer.
//!
//! When the stream ends the row folds to a trigger line ("Thought for Ns" /
//! the localized fallback) with a copy action. Expanding gives a viewport
//! bounded by `max(BUDGET_MIN_LINES lines, viewport × 45%)`; a secondary
//! toggle switches to natural full height with no inner scrollbar. Auto
//! collapse yields to the user the first time they work either toggle
//! (`user_controlled`).
//!
//! Wheel input inside the preview or the budgeted viewport is forwarded to
//! the view through [`RowAction::ReplayNestedScroll`], which owns the easing
//! constants, the painted-frame anchor restore, the window-activation check,
//! and the nested scroll boundary; the renderer owns only the follow flag and
//! the queued distance ([`NestedScrollReplay`]).

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, ElementId, FollowMode, InteractiveElement as _, IntoElement, ListState,
    ParentElement as _, Pixels, ScrollWheelEvent, SharedString, Styled as _, Window, div,
    linear_color_stop, linear_gradient, prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    clipboard::Clipboard,
    h_flex, v_flex,
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
    Finished { elapsed: Option<Duration> },
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
    /// Start of this block's stream, cleared exactly once when the phase
    /// turns `Finished`.
    started_at: Option<Instant>,
    smooth: SmoothScrollState,
    owner_id: u64,
    presentation: Option<MarkdownPresentation>,
    materialized: bool,
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
            started_at: None,
            smooth: SmoothScrollState::default(),
            owner_id: 0,
            presentation: None,
            materialized: false,
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

    fn turn_finished(&mut self) {
        if let ReasoningPhase::Streaming = self.phase {
            let elapsed = self.started_at.take().map(|started| started.elapsed());
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
        // One decimal, floored at 0.1s: a sub-100ms trace is real but reads as
        // "0 seconds", which looks like a bug rather than a fast model.
        let seconds = elapsed.as_secs_f64().max(0.1);
        t!(
            "chat.reasoning.finished",
            duration = format!("{seconds:.1}")
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
        self.disclosure = if self.disclosure == ReasoningDisclosure::Budgeted {
            ReasoningDisclosure::Collapsed
        } else {
            ReasoningDisclosure::Budgeted
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
    /// height budget is hiding. Non-zero means the budget engaged.
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
            if let PartSource::Reasoning { reasoning, .. } = &part.source {
                // A live insert seeds empty: stream batches publish Insert
                // then Append after the model already carries the delta, and
                // the following Append replays the accumulated source (P1
                // empty-seed rule). A late materialization (cold restore
                // mid-stream, first layout) re-reads the accumulated content
                // so no prefix is lost.
                self.display = if part.finished || !ctx.append_replays_part {
                    reasoning.display.clone()
                } else {
                    String::new()
                };
            }
            if part.finished {
                self.phase = ReasoningPhase::Finished { elapsed: None };
                self.started_at = None;
            } else if matches!(self.phase, ReasoningPhase::Finished { .. }) {
                self.phase = ReasoningPhase::Streaming;
            }
            if matches!(self.phase, ReasoningPhase::Streaming) && self.started_at.is_none() {
                self.started_at = Some(Instant::now());
            }
        }
        self.build_body(cx);
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.body = None;
        self.scroll = None;
        self.smooth.cancel_motion();
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
                self.turn_finished();
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
                    self.turn_finished();
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
        match target {
            DisclosureTarget::Reasoning => {
                self.user_controlled = true;
                self.disclosure = if self.disclosure == ReasoningDisclosure::Budgeted {
                    ReasoningDisclosure::Collapsed
                } else {
                    ReasoningDisclosure::Budgeted
                };
            }
            DisclosureTarget::ReasoningFull => {
                self.user_controlled = true;
                self.disclosure = if self.disclosure == ReasoningDisclosure::Full {
                    ReasoningDisclosure::Budgeted
                } else {
                    ReasoningDisclosure::Full
                };
            }
            _ => {}
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

    fn is_windowed(&self, cx: &App) -> bool {
        // Only the Full disclosure renders a natural-height body; the preview
        // and the budgeted viewport are scrollable, where the fork ignores
        // the windowed flag.
        self.disclosure == ReasoningDisclosure::Full
            && self.body.as_ref().is_some_and(|body| {
                typography::windowed_body(self.display.len(), body.block_count(cx))
            })
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
        let rail = crate::appearance::contrast::pane_outline(theme.border, cx);
        let text_color =
            crate::appearance::contrast::text_on(theme.group_box_foreground, background, cx);
        let line_height = window.line_height();
        let preview_height = line_height * typography::PREVIEW_LINES;
        // The painted-frame native anchor is captured at render time, the
        // same way the transcript listener does, so the eased replay never
        // skips. It is negative as soon as content sits above the viewport —
        // exactly when the top fade belongs.
        let anchor = body.scroll_state(cx).scroll_px_offset_for_scrollbar();
        let show_fade = anchor.y < Pixels::ZERO;

        let row_id = ctx.row_id;
        let dispatch = ctx.dispatch.clone();
        let content_index = self.content_index;

        div()
            .w_full()
            .border_l_2()
            .border_color(rail)
            .pl_3()
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
                                    .text_color(text_color),
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
        let rail = crate::appearance::contrast::pane_outline(theme.border, cx);
        let text_color =
            crate::appearance::contrast::text_on(theme.group_box_foreground, background, cx);
        let line_height = window.line_height();
        let budget_height = (line_height * typography::BUDGET_MIN_LINES)
            .max(ctx.viewport_height * typography::BUDGET_VIEWPORT_RATIO);
        let expanded = self.disclosure != ReasoningDisclosure::Collapsed;
        let full = self.disclosure == ReasoningDisclosure::Full;

        let ui_id = self.ui_id;
        let content_index = self.content_index;
        let hover_group: SharedString = format!("turn-reasoning-{ui_id}").into();
        let label = self.label();

        let dispatch_toggle = ctx.dispatch.clone();
        let row_id = ctx.row_id;
        let on_toggle = Rc::new(
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                dispatch_toggle.send(
                    RowAction::ToggleDisclosure {
                        row_id,
                        target: DisclosureTarget::Reasoning,
                    },
                    window,
                    cx,
                );
            },
        );

        let dispatch_full = ctx.dispatch.clone();
        let on_toggle_full = Rc::new(
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                dispatch_full.send(
                    RowAction::ToggleDisclosure {
                        row_id,
                        target: DisclosureTarget::ReasoningFull,
                    },
                    window,
                    cx,
                );
            },
        );

        type CopyValue = Rc<dyn Fn(&mut Window, &mut App) -> SharedString>;
        let dispatch_copy = ctx.dispatch.clone();
        let copy_value: CopyValue = Rc::new(move |_, cx| dispatch_copy.clipboard_value(row_id, cx));

        let dispatch_scroll = ctx.dispatch.clone();
        let anchor = body.scroll_state(cx).scroll_px_offset_for_scrollbar();
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
                        // The trigger is a flex item on the main axis, so it
                        // stays at its intrinsic width instead of stretching
                        // across the column. Ceiling for a label some locale
                        // makes long enough to reach the column edge — it
                        // truncates there instead of widening the message.
                        Button::new(ElementId::NamedInteger(
                            "turn-reasoning-toggle".into(),
                            ui_id,
                        ))
                        .ghost()
                        .small()
                        .max_w_full()
                        .min_w_0()
                        .overflow_hidden()
                        .debug_selector(move || block_selector("trigger", content_index))
                        .child(div().min_w_0().text_ellipsis().child(label))
                        .tooltip(if expanded {
                            t!("chat.reasoning.collapse").to_string()
                        } else {
                            t!("chat.reasoning.expand").to_string()
                        })
                        .on_click(move |event, window, cx| on_toggle(event, window, cx)),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger(
                            "turn-reasoning-full-toggle".into(),
                            ui_id,
                        ))
                        .ghost()
                        .small()
                        .compact()
                        .label(if full {
                            t!("chat.reasoning.collapse_all").to_string()
                        } else {
                            t!("chat.reasoning.expand_all").to_string()
                        })
                        .tooltip(if full {
                            t!("chat.reasoning.collapse_all").to_string()
                        } else {
                            t!("chat.reasoning.expand_all").to_string()
                        })
                        .debug_selector(move || block_selector("full", content_index))
                        .on_click(move |event, window, cx| on_toggle_full(event, window, cx)),
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
            .when(self.disclosure == ReasoningDisclosure::Budgeted, |this| {
                // `relative` is load-bearing: the scrollable TextView attaches
                // an absolutely-positioned scrollbar overlay that needs a
                // positioning context, exactly like the transcript's own
                // scrollbar host.
                this.child(
                    div()
                        .id(ElementId::NamedInteger(
                            "turn-reasoning-body".into(),
                            self.ui_id,
                        ))
                        .debug_selector(move || block_selector("body", content_index))
                        .relative()
                        .w_full()
                        .border_l_2()
                        .border_color(rail)
                        .pl_3()
                        .child(
                            div()
                                .h(budget_height)
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
                                                .text_color(text_color),
                                        ),
                                ),
                        ),
                )
            })
            .when(full, |this| {
                // Natural height, no inner scrollbar; long sources route
                // through the fork's windowed block layout (P4 PRD R5).
                this.child(
                    div()
                        .w_full()
                        .border_l_2()
                        .border_color(rail)
                        .pl_3()
                        .text_sm()
                        .text_color(text_color)
                        .debug_selector(move || block_selector("body", content_index))
                        .child(body.text_view(typography::reasoning(cx)).windowed(
                            typography::windowed_body(self.display.len(), body.block_count(cx)),
                        )),
                )
            })
            .into_any_element()
    }
}
