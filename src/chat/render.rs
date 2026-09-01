//! Rendering and scroll behavior for the conversation transcript.

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

        let has_messages = !self.messages.is_empty();
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
        let message_count = self.messages.len();
        let wheel_scroll_anchor = self.list_state.logical_scroll_top();
        let render_item = cx.processor(move |this, index, window, cx| {
            #[cfg(test)]
            this.materialized_message_indices.insert(index);
            let Some(message) = this.messages.get(index) else {
                return div().into_any_element();
            };
            let row = render_message(
                message,
                index,
                this.preference_snapshot.user_message_markdown,
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
        message_ui_id: u64,
        part_ui_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(trace) = self.reasoning_trace_mut(message_ui_id, part_ui_id) else {
            return;
        };
        if !trace.mark_smooth_frame_scheduled() {
            return;
        }
        cx.on_next_frame(window, move |this, window, cx| {
            this.advance_reasoning_scroll(message_ui_id, part_ui_id, window, cx);
        });
    }

    fn advance_reasoning_scroll(
        &mut self,
        message_ui_id: u64,
        part_ui_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Reasoning cards share the same inactive-window frame throttling as
        // the transcript, so cancel pending card easing when focus moves to a
        // different window.
        if !smooth_scroll_animation_enabled(window, self.preference_snapshot.smooth_chat_scrolling)
        {
            if let Some(trace) = self.reasoning_trace_mut(message_ui_id, part_ui_id) {
                trace.cancel_smooth_scroll_frame();
            }
            return;
        }
        let Some(trace) = self.reasoning_trace_mut(message_ui_id, part_ui_id) else {
            return;
        };
        let Some(has_remaining) = trace.advance_smooth_scroll() else {
            return;
        };
        #[cfg(test)]
        record_reasoning_smooth_invalidation();
        cx.notify();
        if has_remaining {
            self.schedule_reasoning_scroll_frame(message_ui_id, part_ui_id, window, cx);
        }
    }

    pub(super) fn reasoning_trace_mut(
        &mut self,
        message_ui_id: u64,
        part_ui_id: u64,
    ) -> Option<&mut ReasoningTrace> {
        self.messages
            .iter_mut()
            .find(|message| message.ui_id == message_ui_id)
            .and_then(|message| {
                message.parts.iter_mut().find_map(|part| match part {
                    MessagePart::Reasoning {
                        ui_id,
                        trace: Some(trace),
                        ..
                    } if *ui_id == part_ui_id => Some(trace),
                    _ => None,
                })
            })
    }

    pub(super) fn sync_message_list_count(&self) {
        let current = self.list_state.item_count();
        let target = self.messages.len();
        if current == 0 && target > 0 {
            self.list_state
                .reset_with_uniform_height(target, MESSAGE_HEIGHT_HINT);
        } else if current < target {
            self.list_state.splice(current..current, target - current);
        } else if current > target {
            self.list_state.splice(target..current, 0);
        }
    }

    pub(super) fn remeasure_latest_message(&self) {
        if let Some(index) = self.messages.len().checked_sub(1) {
            self.list_state.remeasure_items(index..index + 1);
        }
    }
}

fn render_message(
    msg: &Message,
    message_index: usize,
    render_user_markdown: bool,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> impl IntoElement {
    let message_ui_id = msg.ui_id;
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
                MessagePart::Text {
                    ui_id, text, body, ..
                } if !text.is_empty() => {
                    Some(if is_user && !render_user_markdown {
                        TextView::plain(("user-message-plain", *ui_id), text.clone())
                            .selectable(true)
                            .style(TextViewStyle::default())
                            .into_any_element()
                    } else {
                        body.text_view(TextViewStyle::default()).into_any_element()
                    })
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
                    let native_scroll_anchor = trace.current_scroll_offset();
                    let on_toggle = cx.listener(move |this: &mut ChatView, _, window, cx| {
                        if let Some(MessagePart::Reasoning {
                            trace: Some(trace), ..
                        }) = this
                            .messages
                            .iter_mut()
                            .find(|message| message.ui_id == message_ui_id)
                            .and_then(|message| {
                                message.parts.iter_mut().find(|part| {
                                    matches!(part, MessagePart::Reasoning { ui_id: current, .. } if *current == ui_id)
                                })
                            })
                        {
                            if let Some(position) = trace.toggle_with_cx(cx) {
                                // The first frame gives the retained TextView a
                                // definite viewport and block-height estimates.
                                // Restore the reader's relative position on the
                                // following frame, once its scroll range is real.
                                cx.on_next_frame(window, move |_, window, cx| {
                                    cx.on_next_frame(window, move |this, _, cx| {
                                        if let Some(trace) =
                                            this.reasoning_trace_mut(message_ui_id, ui_id)
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
                        move |this: &mut ChatView,
                              event: &ScrollWheelEvent,
                              window,
                              cx| {
                            // A gesture over a nested reasoning viewport takes
                            // ownership of the pointer; do not let a previously
                            // queued transcript animation keep moving underneath it.
                            this.smooth_scroll.cancel_motion();
                            // Do not start card easing from an inactive window:
                            // AppKit throttles its animation frames.
                            let smooth = smooth_scroll_animation_enabled(
                                window,
                                this.preference_snapshot.smooth_chat_scrolling,
                            )
                                && !event.delta.precise();
                            let Some(trace) = this.reasoning_trace_mut(message_ui_id, ui_id) else {
                                return;
                            };
                            trace.handle_scroll(event, window, cx);
                            if !smooth {
                                trace.cancel_smooth_scroll();
                                return;
                            }

                            // The card's native scroll listener runs before this
                            // callback. Restore its painted-frame anchor, then
                            // replay the same distance through eased frames.
                            let distance = event.delta.pixel_delta(window.line_height()).y;
                            if distance == Pixels::ZERO {
                                return;
                            }
                            trace.enqueue_smooth_scroll(native_scroll_anchor, distance);
                            this.schedule_reasoning_scroll_frame(
                                message_ui_id,
                                ui_id,
                                window,
                                cx,
                            );
                        },
                    );
                    let view = cx.entity().downgrade();
                    let copy_value = move |_: &mut Window, cx: &mut App| {
                        view.upgrade()
                            .and_then(|view| {
                                view.read(cx).reasoning_copy_source(message_ui_id, ui_id)
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
                this.child(error_card::render(error, message_ui_id, window, cx))
            })
            .into_any_element()
    };

    // A hover-revealed action row beneath the message: a single copy button
    // right-aligned under user bubbles, left-aligned under assistant content.
    // `hover_reveal_copy` owns the invisible-until-group-hover wrapper, so
    // revealing it spends no layout and the bubble's bounds never shift when
    // the pointer enters.
    //
    // Suppressed on assistant turns that failed: the error card already carries
    // its own "copy raw response" button, and a second message-level copy would
    // either duplicate it or copy the partial prose above — neither of which is
    // what a failed turn should offer. Also suppressed while the turn is still
    // streaming and until it actually has prose: a copy must never freeze a
    // partial answer mid-stream, and a reasoning- or tool-only stream must not
    // offer to copy an empty string onto the clipboard.
    let hover_group: SharedString = format!("turn-message-{message_ui_id}").into();
    let body_with_actions = if msg.error.is_none() && stream_ended(msg) && has_copyable_text(msg) {
        // Read the live message at click time rather than capturing a snapshot
        // from render, so the clipboard always reflects the message's current
        // state.
        let view = cx.entity().downgrade();
        let actions = h_flex()
            .w_full()
            // Same side the bubble/heading sits on, so the button reads as
            // belonging to that message rather than floating in the column.
            .when(is_user, |this| this.justify_end())
            .mt_1()
            .child(hover_reveal_copy(
                ElementId::NamedInteger("turn-message-copy".into(), message_ui_id),
                hover_group.clone(),
                t!("chat.copy_message").to_string(),
                move |_: &mut Window, cx: &mut App| {
                    view.upgrade()
                        .and_then(|view| view.read(cx).copyable_message_text(message_ui_id))
                        .unwrap_or_default()
                },
                move || format!("message-copy-{message_index}"),
            ));

        // Wrap body + actions so both share one hover group: hovering anywhere
        // over the message — including a nested reasoning card — reveals the
        // row.
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

/// Iterates the non-whitespace text parts of `message` in canonical order. Both
/// `has_copyable_text` and [`copyable_text`] consume this same iterator, so the
/// "should the button appear" and "what lands on the clipboard" rules cannot
/// drift apart.
fn text_parts(message: &Message) -> impl Iterator<Item = &str> {
    message.parts.iter().filter_map(|part| match part {
        MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

/// Whether every streamed block of `message` has ended. User turns, tool
/// calls, and tool results have no streaming lifecycle and read as ended. The
/// message-level copy gate uses this so a copy is never offered for a
/// still-streaming turn.
fn stream_ended(message: &Message) -> bool {
    message.parts.iter().all(|part| match part {
        MessagePart::Text { finished, .. } | MessagePart::Reasoning { finished, .. } => *finished,
        MessagePart::ToolCall { .. } | MessagePart::ToolResult { .. } => true,
    })
}

/// Whether `message` has any prose worth offering a copy of.
fn has_copyable_text(message: &Message) -> bool {
    text_parts(message).next().is_some()
}

/// Plain text a reader would expect on the clipboard for `message`: the
/// concatenated source of every visible-text part, in canonical order.
///
/// The parts carry the raw Markdown the model produced, so the clipboard holds
/// that source verbatim rather than the rendered prose. Reasoning and tool
/// blocks are deliberately excluded — reasoning has its own per-card copy
/// affordance, and tool calls are structured data rather than prose.
pub(super) fn copyable_text(message: &Message) -> SharedString {
    text_parts(message)
        .fold(String::new(), |mut text, part| {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
            text
        })
        .into()
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
