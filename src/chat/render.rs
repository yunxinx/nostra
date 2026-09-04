//! Rendering and scroll behavior for the conversation transcript.

use gpui::{
    AnyElement, App, Context, ElementId, InteractiveElement as _, IntoElement, ListOffset,
    ParentElement as _, Pixels, Render, ScrollWheelEvent, SharedString, Styled as _, Window, div,
    list, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, StyledExt as _, h_flex,
    scroll::ScrollableElement as _,
    shimmer::ShimmerText,
    text::{TextView, TextViewStyle},
    v_flex,
};
use rust_i18n::t;

use crate::appearance::contrast;

use super::hover_reveal::hover_reveal_copy;
#[cfg(test)]
use super::scrolling::record_reasoning_smooth_invalidation;
use super::scrolling::smooth_scroll_animation_enabled;
use super::*;

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-resolve the placeholder so a language switch reaches the
        // already-built input; guarded to avoid a notify cycle.
        let placeholder: SharedString = if self.references_enabled {
            t!("reference_picker.composer_placeholder").to_string()
        } else {
            t!("chat.placeholder").to_string()
        }
        .into();
        if self.placeholder != placeholder {
            self.placeholder = placeholder.clone();
            self.composer.update(cx, |composer, cx| {
                composer.set_placeholder(placeholder, window, cx)
            });
        }

        let has_messages = !self.mirrors.is_empty();
        let send_disabled = self.runtime_snapshot.is_generating()
            || self.runtime_snapshot.persistence_pending()
            || self.runtime_snapshot.deletion_pending()
            || self.runtime_snapshot.shutdown_requested()
            || self.input_blank
            || !self.selection_available;
        let composer_height = self.composer_height;
        let base_composer_height = self.base_composer_height;
        self.composer_status
            .set(crate::ui::reference_picker::ComposerStatus {
                pending: self.runtime_snapshot.is_generating(),
                send_disabled,
            });
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
                    .child(self.composer.clone())
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

impl ChatView {
    /// Fold a fresh composer measurement into the two tracked heights, and
    /// report whether either moved (i.e. whether a re-render is needed).
    ///
    /// The live height follows every frame, but the resting height only
    /// records while the input is empty — and an empty input is exactly one
    /// row tall.  That keeps the greeting anchored when a draft grows the
    /// composer, without hard-coding what one row measures.
    pub(super) fn record_composer_height(&mut self, height: Pixels) -> bool {
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
        &mut self,
        composer_height: Pixels,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.sync_message_list_count();
        #[cfg(test)]
        self.materialized_message_indices.clear();
        let message_count = self.mirrors.len();
        let is_generating = self.runtime_snapshot.is_generating();
        let wheel_scroll_anchor = self.list_state.logical_scroll_top();
        let render_item = cx.processor(move |this, index, window, cx| {
            #[cfg(test)]
            this.materialized_message_indices.insert(index);
            let Some(mirror) = this.mirrors.get(index) else {
                return div().into_any_element();
            };
            let row = render_turn(
                mirror,
                index,
                this.preference_snapshot.user_message_markdown,
                is_generating && index + 1 == message_count,
                window,
                cx,
            );
            div()
                .w_full()
                .when(index + 1 < message_count, |this| this.pb_5())
                .child(row)
                .into_any_element()
        });

        // Relative, non-scrolling wrapper so the overlay scrollbar anchors to
        // the full panel height (including under the floating composer).
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("messages")
                    .size_full()
                    .on_scroll_wheel(cx.listener(move |this, event, window, cx| {
                        this.handle_message_scroll_wheel(event, wheel_scroll_anchor, window, cx);
                    }))
                    .child(
                        list(self.list_state.clone(), render_item)
                            .size_full()
                            .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                            // Match the scrollbar thumb's 4px top inset so the
                            // first message aligns with the top of the thumb.
                            .pt(px(4.))
                            // Leave exactly enough room for the measured floating composer.
                            .pb(composer_height),
                    ),
            )
            .vertical_scrollbar(&self.list_state)
    }

    fn handle_message_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        native_anchor: ListOffset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Inactive macOS windows may still receive wheel hit-tests, but their
        // frame delivery is throttled. Keep those events on GPUI's native
        // path instead of queueing an animation that cannot advance smoothly.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
            || event.delta.precise()
        {
            self.smooth_scroll.cancel_motion();
            return;
        }

        let distance = -event.delta.pixel_delta(window.line_height()).y;
        if distance == Pixels::ZERO {
            return;
        }

        // GPUI's list handles the wheel event first during bubbling. Restore
        // the pre-event anchor before starting the eased movement so the
        // native jump never reaches a painted frame.
        self.list_state.scroll_to(native_anchor);
        self.smooth_scroll.enqueue(distance);
        self.schedule_smooth_scroll_frame(window, cx);
        cx.stop_propagation();
    }

    fn schedule_smooth_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.smooth_scroll.frame_scheduled {
            return;
        }
        self.smooth_scroll.frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.advance_smooth_scroll(window, cx);
        });
    }

    fn advance_smooth_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.smooth_scroll.frame_scheduled = false;
        // A window can lose activation after the wheel event but before its
        // scheduled frame. Drop the queued motion rather than invalidating a
        // throttled, inactive window on every frame.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
        {
            self.smooth_scroll.cancel_motion();
            return;
        }

        let Some(step) = self.smooth_scroll.next_step() else {
            return;
        };
        let before = self.list_state.logical_scroll_top();
        self.list_state.scroll_by(step);
        let after = self.list_state.logical_scroll_top();
        if before.item_ix == after.item_ix && before.offset_in_item == after.offset_in_item {
            self.smooth_scroll.cancel_motion();
        }

        cx.notify();
        if self.smooth_scroll.remaining != Pixels::ZERO {
            self.schedule_smooth_scroll_frame(window, cx);
        }
    }

    fn schedule_reasoning_scroll_frame(
        &mut self,
        turn_id: TurnId,
        part_id: PartId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(trace) = self.reasoning_trace_mut(turn_id, part_id) else {
            return;
        };
        if !trace.mark_smooth_frame_scheduled() {
            return;
        }
        cx.on_next_frame(window, move |this, window, cx| {
            this.advance_reasoning_scroll(turn_id, part_id, window, cx);
        });
    }

    fn advance_reasoning_scroll(
        &mut self,
        turn_id: TurnId,
        part_id: PartId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Reasoning cards share the same inactive-window frame throttling as
        // the transcript, so cancel pending card easing when focus moves to a
        // different window.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
        {
            if let Some(trace) = self.reasoning_trace_mut(turn_id, part_id) {
                trace.cancel_smooth_scroll_frame();
            }
            return;
        }
        let Some(trace) = self.reasoning_trace_mut(turn_id, part_id) else {
            return;
        };
        let Some(has_remaining) = trace.advance_smooth_scroll() else {
            return;
        };
        #[cfg(test)]
        record_reasoning_smooth_invalidation();
        cx.notify();
        if has_remaining {
            self.schedule_reasoning_scroll_frame(turn_id, part_id, window, cx);
        }
    }

    pub(super) fn reasoning_trace_mut(
        &mut self,
        turn_id: TurnId,
        part_id: PartId,
    ) -> Option<&mut ReasoningTrace> {
        self.mirrors
            .iter_mut()
            .find(|mirror| mirror.turn_id == turn_id)
            .and_then(|mirror| {
                mirror.parts.iter_mut().find_map(|part| match part {
                    PartMirror::Reasoning {
                        part_id: current,
                        trace: Some(trace),
                        ..
                    } if *current == part_id => Some(trace),
                    _ => None,
                })
            })
    }

    pub(super) fn sync_message_list_count(&self) {
        let current = self.list_state.item_count();
        let target = self.mirrors.len();
        if current == 0 && target > 0 {
            self.list_state
                .reset_with_uniform_height(target, MESSAGE_HEIGHT_HINT);
        } else if current < target {
            self.list_state.splice_with_uniform_height(
                current..current,
                target - current,
                MESSAGE_HEIGHT_HINT,
            );
        } else if current > target {
            self.list_state.splice(target..current, 0);
        }
    }

    pub(super) fn remeasure_latest_message(&self) {
        if let Some(index) = self.mirrors.len().checked_sub(1) {
            self.list_state.remeasure_items(index..index + 1);
        }
    }
}

fn render_turn(
    mirror: &TurnMirror,
    message_index: usize,
    render_user_markdown: bool,
    in_flight: bool,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> impl IntoElement {
    let turn_id = mirror.turn_id;
    let message_ui_id = turn_id.as_u64();
    let (radius_lg, secondary, secondary_foreground, foreground, muted, muted_foreground) = {
        let theme = cx.theme();
        (
            theme.radius_lg,
            theme.secondary,
            theme.secondary_foreground,
            theme.foreground,
            theme.muted,
            theme.muted_foreground,
        )
    };
    let is_user = mirror.role == Role::User;
    let is_tool = mirror.role == Role::Tool;
    let parts = mirror
        .parts
        .iter()
        .filter_map(|part| match part {
            PartMirror::Prose {
                text,
                body,
                part_id,
                ..
            } if !text.is_empty() => {
                if is_user && !render_user_markdown {
                    Some(
                        TextView::plain(
                            ("user-message-plain", part_id.as_u64()),
                            SharedString::from(text.as_str()),
                        )
                        .selectable(true)
                        .style(TextViewStyle::default())
                        .into_any_element(),
                    )
                } else {
                    Some(body.text_view(TextViewStyle::default()).into_any_element())
                }
            }
            PartMirror::Reasoning {
                part_id,
                content_index,
                display,
                finished,
                trace: Some(trace),
            } => {
                let part_id = *part_id;
                let ui_id = part_id.as_u64();
                let content_index = *content_index;
                let native_scroll_anchor = trace.current_scroll_offset();
                let on_toggle = cx.listener(move |this: &mut ChatView, _, window, cx| {
                    if let Some(trace) = this.reasoning_trace_mut(turn_id, part_id) {
                        if let Some(position) = trace.toggle_with_cx(cx) {
                            cx.on_next_frame(window, move |_, window, cx| {
                                cx.on_next_frame(window, move |this, _, cx| {
                                    if let Some(trace) = this.reasoning_trace_mut(turn_id, part_id)
                                    {
                                        trace.apply_virtualized_position(position);
                                        cx.notify();
                                    }
                                });
                                cx.notify();
                            });
                        }
                        cx.notify();
                    }
                });
                let on_scroll = cx.listener(
                    move |this: &mut ChatView, event: &ScrollWheelEvent, window, cx| {
                        this.smooth_scroll.cancel_motion();
                        let smooth = smooth_scroll_animation_enabled(
                            window,
                            this.preference_snapshot.smooth_chat_scrolling,
                        ) && !event.delta.precise();
                        let Some(trace) = this.reasoning_trace_mut(turn_id, part_id) else {
                            return;
                        };
                        trace.handle_scroll(event, window, cx);
                        if !smooth {
                            trace.cancel_smooth_scroll();
                            return;
                        }

                        let distance = event.delta.pixel_delta(window.line_height()).y;
                        if distance == Pixels::ZERO {
                            return;
                        }
                        trace.enqueue_smooth_scroll(native_scroll_anchor, distance);
                        this.schedule_reasoning_scroll_frame(turn_id, part_id, window, cx);
                    },
                );
                let view = cx.entity().downgrade();
                let copy_value = move |_: &mut Window, cx: &mut App| {
                    view.upgrade()
                        .and_then(|view| view.read(cx).reasoning_copy_source(turn_id, part_id, cx))
                        .unwrap_or_default()
                };
                Some(reasoning_card::render(
                    trace,
                    display,
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
            PartMirror::ToolCall { name, .. } if !name.is_empty() => Some(
                div()
                    .text_color(muted_foreground)
                    .child(t!("chat.tool_requested", name = name.clone()).to_string())
                    .into_any_element(),
            ),
            PartMirror::ToolResult { body, .. } => {
                Some(body.text_view(TextViewStyle::default()).into_any_element())
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    let waiting = mirror.role == Role::Assistant
        && in_flight
        && mirror.error.is_none()
        && !has_wait_ending_content(mirror);
    let inner: AnyElement = if is_user {
        let bubble = contrast::pane_block(secondary, cx);
        let bubble_text = contrast::text_on(secondary_foreground, bubble, cx);
        h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .debug_selector(move || format!("user-message-bubble-{message_index}"))
                    .min_w_0()
                    .max_w(px(560.))
                    .rounded(radius_lg)
                    .bg(bubble)
                    .text_color(bubble_text)
                    .px_3()
                    .py_1p5()
                    .children(parts),
            )
            .into_any_element()
    } else if is_tool {
        let card = contrast::pane_block(muted, cx);
        let card_text = contrast::text_on(muted_foreground, card, cx);
        h_flex()
            .w_full()
            .justify_start()
            .child(
                div()
                    .debug_selector(move || format!("tool-result-{message_index}"))
                    .min_w_0()
                    .max_w(px(560.))
                    .rounded(radius_lg)
                    .bg(card)
                    .text_color(card_text)
                    .px_3()
                    .py_1p5()
                    .children(parts),
            )
            .into_any_element()
    } else if waiting {
        div()
            .w_full()
            .debug_selector(move || format!("assistant-waiting-{message_index}"))
            .child(
                ShimmerText::new(t!("chat.generating").to_string())
                    .id(("assistant-waiting", message_ui_id))
                    .text_color(muted_foreground),
            )
            .into_any_element()
    } else {
        v_flex()
            .w_full()
            .gap_3()
            .text_color(foreground)
            .children(parts)
            .when_some(mirror.error.as_ref(), |this, error| {
                this.child(error_card::render(error, message_ui_id, window, cx))
            })
            .into_any_element()
    };

    let hover_group: SharedString = format!("turn-message-{message_ui_id}").into();
    let body_with_actions = if mirror.error.is_none() && mirror.copyable {
        let view = cx.entity().downgrade();
        let actions = h_flex()
            .w_full()
            .when(is_user, |this| this.justify_end())
            .mt_1()
            .child(hover_reveal_copy(
                ElementId::NamedInteger("turn-message-copy".into(), message_ui_id),
                hover_group.clone(),
                t!("chat.copy_message").to_string(),
                move |_: &mut Window, cx: &mut App| {
                    view.upgrade()
                        .and_then(|view| view.read(cx).copyable_message_text(turn_id, cx))
                        .unwrap_or_default()
                },
                move || format!("message-copy-{message_index}"),
            ));

        v_flex()
            .w_full()
            .gap_0()
            .child(inner)
            .child(actions)
            .into_any_element()
    } else {
        inner
    };

    div().group(hover_group).w_full().child(
        h_flex().w_full().justify_center().px_6().child(
            div()
                .debug_selector(move || format!("assistant-message-content-{message_index}"))
                .w_full()
                .max_w(CONTENT_MAX_WIDTH)
                .child(body_with_actions),
        ),
    )
}

fn has_wait_ending_content(mirror: &TurnMirror) -> bool {
    mirror.parts.iter().any(|part| match part {
        PartMirror::Prose { text, .. } => !text.is_empty(),
        PartMirror::Reasoning {
            display,
            trace: Some(_),
            ..
        } => !display.is_empty(),
        PartMirror::ToolCall { name, .. } => !name.is_empty(),
        PartMirror::ToolResult { .. } => false,
        _ => false,
    })
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
