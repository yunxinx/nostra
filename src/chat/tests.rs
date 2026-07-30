use gpui::{IntoElement as _, TestAppContext, px};
use gpui_component::input::InputEvent;

use crate::llm::{
    ContentBlock, IndexedContentBlock, IndexedMessage, Message as LlmMessage, ProviderMetadata,
    ResponsesReplayMetadata,
};
use crate::preferences;

use super::{
    CONTENT_MAX_WIDTH, ChatView, Message, MessagePart, ReasoningTrace, Role, is_replayable,
};

/// A completed user turn plus the assistant placeholder a reply streams
/// into. Pushed directly rather than through `submit`, which is gated on a
/// configured provider that a unit test has no reason to stand up.
fn seed_turn(chat: &gpui::Entity<ChatView>, cx: &mut gpui::VisualTestContext) {
    cx.update(|_, cx| {
        chat.update(cx, |this, _cx| {
            for role in [Role::User, Role::Assistant] {
                this.messages.push(Message::empty(role));
            }
        });
    });
}

fn reasoning_part(message: &Message) -> Option<&ReasoningTrace> {
    message.parts.iter().find_map(|part| match part {
        MessagePart::Reasoning {
            trace: Some(trace), ..
        } => Some(trace),
        _ => None,
    })
}

fn reasoning_part_mut(message: &mut Message) -> Option<&mut ReasoningTrace> {
    message.parts.iter_mut().find_map(|part| match part {
        MessagePart::Reasoning {
            trace: Some(trace), ..
        } => Some(trace),
        _ => None,
    })
}

fn reasoning_parts(message: &Message) -> Vec<&ReasoningTrace> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Reasoning {
                trace: Some(trace), ..
            } => Some(trace),
            _ => None,
        })
        .collect()
}

fn reasoning_states(message: &Message) -> Vec<(&str, bool)> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Reasoning {
                reasoning,
                finished,
                ..
            } => Some((reasoning.display.as_str(), *finished)),
            _ => None,
        })
        .collect()
}

fn init_app(cx: &mut TestAppContext) {
    let prefs = preferences::Preferences::default();
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::fonts::init(prefs.composer_font, cx);
        preferences::init_global(prefs, cx);
    });
}

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

/// A failed turn must render its upstream error card through a real view
/// pass: the card reads window-keyed collapse state, which is only available
/// with a rendering view on the stack.
#[gpui::test]
fn failed_turn_renders_the_upstream_error_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let mut error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
        .with_upstream_body(
            r#"{"error":{"message":"Rate limit reached","code":"rate_limit_exceeded"}}"#,
        );
    error.request_id = Some("nostra-1".into());
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.finish_reply(None, Some(error), cx));
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        // Attached to the assistant turn, not the user's message.
        let assistant = this.messages.last().expect("assistant turn");
        assert_eq!(assistant.role, Role::Assistant);
        assert!(
            assistant.error.is_some(),
            "card attached to the failed turn"
        );
        assert_eq!(
            assistant
                .error
                .as_ref()
                .and_then(crate::error_card::TurnError::request_id),
            Some("nostra-1"),
            "the visible card retains the correlation id"
        );
        assert!(
            this.messages[0].error.is_none(),
            "the user's own turn carries no error"
        );
        // The provider's error text must not leak into replayable history.
        assert!(
            this.messages
                .iter()
                .all(
                    |message| message.canonical().content.iter().all(|block| !matches!(
                        block,
                        ContentBlock::Text { text, .. } if text.contains("rate_limit_exceeded")
                    ))
                ),
            "error text must stay out of canonical content"
        );
    });

    // Draws the whole view, card included, without panicking.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );

    let error_body_before_theme_switch = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|message| message.error.as_ref())
            .and_then(|error| error.body_entity_id())
            .expect("error body entity")
    });

    // A theme switch fires the global observer, which replaces each card's
    // body while `ChatView` itself is mid-update. Creating the separate body
    // entity here is safe; re-entering `ChatView` through its handle would
    // panic.
    //
    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let error = this
            .messages
            .last()
            .and_then(|message| message.error.as_ref())
            .expect("the card survives a theme switch");
        let error_body_after_theme_switch = error.body_entity_id().expect("error body entity");
        assert_ne!(
            error_body_after_theme_switch, error_body_before_theme_switch,
            "theme changes must replace the parsed markdown state"
        );
    });

    // Re-draws cleanly against the new theme.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
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

/// A user bubble must obey the conversation column when the window narrows.
/// Its Markdown body has an intrinsic width, so this is asserted through the
/// real flex tree instead of a standalone style value.
#[gpui::test]
fn user_message_bubble_shrinks_with_the_viewport(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Please extract factorio-headless.tar.xz into factorio-2.1.12 without changing its contents."
                            .into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    cx.run_until_parked();

    let draw = |width: f32, cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(width), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
        cx.debug_bounds("user-message-bubble-0")
            .expect("the user bubble was drawn")
    };

    let narrow_viewport_width = 440.;
    let content_inset = px(24.);
    let wide = draw(900., cx);
    let narrow = draw(narrow_viewport_width, cx);

    assert_eq!(wide.size.width, px(560.), "wide bubbles keep their cap");
    assert!(
        narrow.size.width < wide.size.width,
        "the bubble stayed {:?} wide in a {narrow_viewport_width}px viewport",
        narrow.size.width
    );
    assert!(
        narrow.left() >= content_inset,
        "the bubble left edge {:?} escaped the padded content column",
        narrow.left()
    );
    assert!(
        narrow.right() <= px(narrow_viewport_width) - content_inset,
        "the bubble right edge {:?} escaped the padded content column",
        narrow.right()
    );
}

/// The regression this whole module exists for: reasoning deltas were being
/// folded into canonical content and then never rendered. A streaming trace
/// must reach a visible, expanded card *and* stay out of the prose body.
#[gpui::test]
fn streaming_reasoning_reaches_an_expanded_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "Weighing ", cx);
            this.append_stream_reasoning(0, "reasoning-0".into(), "the options.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this.messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("a trace was created");

        assert_eq!(
            reasoning_states(turn)[0].0,
            "Weighing the options.",
            "deltas accumulate into the card's own markdown source"
        );
        assert!(
            reasoning.is_expanded() && !reasoning_states(turn)[0].1,
            "a live trace shows itself without being asked"
        );
        // Canonical content still carries it, for replay across turns.
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "Weighing the options."
        ));
        assert_eq!(turn.parts.len(), 1, "no synthetic empty prose part");
    });

    // Draws the whole view, card included, without panicking.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

/// The canonical reasoning-finished boundary collapses the card. Text remains
/// an independent content event, matching pi's thinking block lifecycle.
#[gpui::test]
fn reasoning_finished_collapses_the_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "Here is the answer.", cx);
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let turn = this.messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");

        assert!(reasoning_states(turn)[0].1, "the block was closed");
        assert!(
            !reasoning.is_expanded(),
            "a finished trace folds down to its trigger"
        );
        assert!(
            reasoning_states(turn)[0].0.contains("thinking"),
            "the reasoning text is retained for re-expansion"
        );
        assert!(matches!(turn.parts.as_slice(), [MessagePart::Reasoning { .. }, MessagePart::Text { text, .. }] if text == "Here is the answer."));
    });
}

/// Exercise the production assistant boundary, not just `ChatView` methods in
/// isolation: an explicit canonical end marker must remain ordered between the
/// final reasoning delta and the first prose delta.
#[gpui::test]
fn canonical_events_close_reasoning_before_prose_reaches_the_view(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        delta: "thinking".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-1".into(),
                        delta: "answer".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn)[0].0, "thinking");
        assert!(
            reasoning_states(turn)[0].1,
            "the explicit boundary was replayed"
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }, ContentBlock::Text { text, .. }]
                if reasoning.display == "thinking" && text == "answer"
        ));
    });
}

/// Content deltas do not own another block's lifecycle. Protocol adapters emit
/// the explicit end boundary before a type transition, and the UI preserves it.
#[gpui::test]
fn prose_does_not_infer_a_reasoning_boundary(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-1".into(),
                        delta: "thinking".into(),
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-1".into(),
                        delta: String::new(),
                    },
                ],
                cx,
            );
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![crate::llm::GenerationEvent::TextDelta {
                    content_index: 1,
                    id: "text-1".into(),
                    delta: "answer".into(),
                }],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert!(
            !reasoning_states(turn)[0].1,
            "prose cannot close a different content block"
        );
    });
}

/// Content type transitions are structural boundaries. A later reasoning run
/// gets a new card at its canonical position instead of reopening or appending
/// to the first card.
#[gpui::test]
fn reasoning_after_prose_creates_a_second_ordered_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        delta: "first".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::TextStarted {
                        content_index: 1,
                        id: "text-0".into(),
                    },
                    crate::llm::GenerationEvent::TextDelta {
                        content_index: 1,
                        id: "text-0".into(),
                        delta: "answer".into(),
                    },
                    crate::llm::GenerationEvent::TextFinished {
                        content_index: 1,
                        id: "text-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 2,
                        id: "reasoning-1".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 2,
                        id: "reasoning-1".into(),
                        delta: "second".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn),
            vec![("first", true), ("second", false)]
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [
                ContentBlock::Reasoning { reasoning: first },
                ContentBlock::Text { text, .. },
                ContentBlock::Reasoning { reasoning: second },
            ] if first.display == "first" && text == "answer" && second.display == "second"
        ));
    });

    // Both cards must coexist in the real GPUI element tree with independent
    // element ids and interaction state.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

#[gpui::test]
fn separate_reasoning_cards_keep_independent_state(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "second", cx);
            this.finish_stream_reasoning(2, "reasoning-1", None);

            let turn = this.messages.last_mut().expect("assistant turn");
            assert_eq!(
                reasoning_states(turn),
                vec![("first", true), ("second", true)]
            );
            let reasoning_positions = turn
                .parts
                .iter()
                .enumerate()
                .filter_map(|(index, part)| {
                    matches!(part, MessagePart::Reasoning { .. }).then_some(index)
                })
                .collect::<Vec<_>>();
            let [first, second] = reasoning_positions.as_slice() else {
                panic!("two reasoning cards");
            };
            let (before_second, second_and_after) = turn.parts.split_at_mut(*second);
            let MessagePart::Reasoning {
                trace: Some(first_trace),
                ..
            } = &mut before_second[*first]
            else {
                panic!("first reasoning card");
            };
            let MessagePart::Reasoning {
                trace: Some(second_trace),
                ..
            } = &mut second_and_after[0]
            else {
                panic!("second reasoning card");
            };
            first_trace.toggle();
            assert!(first_trace.is_expanded());
            assert!(!second_trace.is_expanded());
        });
    });
}

/// Drive the real listeners for both cards. Stable element ids are not enough:
/// each closure must also resolve the same block identity when it reaches back
/// into `ChatView` for disclosure and clipboard content.
#[gpui::test]
fn separate_reasoning_cards_toggle_and_copy_independently(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = cx.add_window_view(ChatView::new);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "second", cx);
            this.finish_stream_reasoning(2, "reasoning-1", None);
        });
    });

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };
    draw(cx);

    let first_trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("first reasoning trigger");
    cx.simulate_click(first_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this.messages.last().expect("assistant turn"));
        assert!(traces[0].is_expanded());
        assert!(!traces[1].is_expanded());
    });

    let second_trigger = cx
        .debug_bounds("reasoning-trigger-2")
        .expect("second reasoning trigger");
    cx.simulate_click(second_trigger.center(), gpui::Modifiers::default());
    draw(cx);
    chat.read_with(cx, |this, _| {
        let traces = reasoning_parts(this.messages.last().expect("assistant turn"));
        assert!(traces[0].is_expanded());
        assert!(traces[1].is_expanded());
    });

    let copy_and_read =
        |selector: &'static str, trigger: &'static str, cx: &mut gpui::VisualTestContext| {
            let trigger = cx.debug_bounds(trigger).expect("reasoning trigger");
            cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
            draw(cx);
            let copy = cx.debug_bounds(selector).expect("reasoning copy action");
            cx.simulate_click(copy.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .expect("reasoning copied")
        };

    assert_eq!(
        copy_and_read("reasoning-copy-0", "reasoning-trigger-0", cx),
        "first"
    );
    assert_eq!(
        copy_and_read("reasoning-copy-2", "reasoning-trigger-2", cx),
        "second"
    );
}

#[gpui::test]
fn a_finished_reasoning_id_cannot_be_reused(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_reasoning(0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(reasoning_part(turn).is_some());
        assert_eq!(reasoning_states(turn)[0].0, "first");
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }] if reasoning.display == "first"
        ));
    });
}

#[gpui::test]
fn replay_only_reasoning_is_closed_without_allocating_a_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);
    let replay = ProviderMetadata {
        chat: Some(crate::llm::ChatReplayMetadata {
            reasoning_field: Some(crate::llm::ChatReasoningField::ReasoningContent),
            reasoning_details: None,
        }),
        responses: None,
    };

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_reasoning(0, "reasoning-0".into());
            this.finish_stream_reasoning(0, "reasoning-0", Some(replay.clone()));
            this.append_stream_reasoning(0, "reasoning-0".into(), "late", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(
            reasoning_part(turn).is_none(),
            "no visible body was streamed"
        );
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { reasoning }]
                if reasoning.display.is_empty() && reasoning.replay.as_ref() == Some(&replay)
        ));
    });
}

#[gpui::test]
fn terminal_snapshot_preserves_separate_reasoning_cards(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "partial first", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "partial answer", cx);
            this.append_stream_reasoning(2, "reasoning-1".into(), "partial second", cx);
            let first = this
                .messages
                .last_mut()
                .and_then(reasoning_part_mut)
                .expect("first reasoning card");
            first.toggle();
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "first".into(),
                                replay: None,
                            },
                        },
                        ContentBlock::Text {
                            text: "answer".into(),
                            provider_metadata: ProviderMetadata::default(),
                        },
                        ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "second".into(),
                                replay: None,
                            },
                        },
                    ],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let traces = reasoning_parts(turn);
        assert_eq!(traces.len(), 2);
        assert_eq!(
            reasoning_states(turn),
            vec![("first", true), ("second", true)]
        );
        assert!(
            traces[0].is_expanded(),
            "terminal reconciliation preserves the first card's disclosure"
        );
        assert!(
            !traces[1].is_expanded(),
            "the second card retains its independent automatic disclosure"
        );
    });
}

/// Terminal canonical content may omit an unfinished tool placeholder. The
/// later reasoning block must retain its GPUI identity even though its vector
/// position changes when that placeholder disappears.
#[gpui::test]
fn terminal_filter_preserves_reasoning_identity_by_content_index(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let (ui_id, body_id) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_tool_call(0, 0, "call-0".into(), "lookup".into());
            this.append_stream_reasoning(1, "reasoning-0".into(), "partial", cx);
            this.finish_stream_reasoning(1, "reasoning-0", None);
            let turn = this.messages.last_mut().expect("assistant turn");
            let MessagePart::Reasoning {
                ui_id,
                trace: Some(trace),
                ..
            } = turn
                .parts
                .iter_mut()
                .find(|part| part.content_index() == 1)
                .expect("reasoning slot")
            else {
                panic!("reasoning part")
            };
            trace.toggle();
            (*ui_id, trace.body_entity_id())
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![IndexedContentBlock {
                        content_index: 1,
                        block: ContentBlock::Reasoning {
                            reasoning: crate::llm::ReasoningContent {
                                display: "authoritative".into(),
                                replay: None,
                            },
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                }),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(
            turn.parts.len(),
            1,
            "unfinished tool placeholder was filtered"
        );
        let MessagePart::Reasoning {
            ui_id: current_ui_id,
            reasoning,
            trace: Some(trace),
            ..
        } = &turn.parts[0]
        else {
            panic!("terminal reasoning part")
        };
        assert_eq!(*current_ui_id, ui_id);
        assert_eq!(trace.body_entity_id(), body_id);
        assert!(
            trace.is_expanded(),
            "manual disclosure survives reconciliation"
        );
        assert_eq!(reasoning.display, "authoritative");
    });
}

/// A late Responses `output_item.done` updates the already-closed card in place.
/// It must not restart timing, reset disclosure, or replace the markdown entity.
#[gpui::test]
fn late_reasoning_snapshot_preserves_card_state_and_identity(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let (ui_id, body_id, elapsed) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                        delta: "streamed draft".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0-0".into(),
                        replay: None,
                    },
                ],
                cx,
            );
            let turn = this.messages.last_mut().expect("assistant turn");
            let MessagePart::Reasoning {
                ui_id,
                trace: Some(trace),
                ..
            } = &mut turn.parts[0]
            else {
                panic!("reasoning part")
            };
            trace.toggle();
            (*ui_id, trace.body_entity_id(), trace.elapsed())
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![crate::llm::GenerationEvent::ReasoningSnapshotUpdated {
                    content_index: 0,
                    id: "reasoning-0-0".into(),
                    reasoning: crate::llm::ReasoningContent {
                        display: "authoritative summary".into(),
                        replay: Some(ProviderMetadata {
                            chat: None,
                            responses: Some(ResponsesReplayMetadata {
                                item_id: Some("rs_1".into()),
                                encrypted_reasoning: Some("opaque".into()),
                                ..Default::default()
                            }),
                        }),
                    },
                }],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let MessagePart::Reasoning {
            ui_id: current_ui_id,
            reasoning,
            finished,
            trace: Some(trace),
            ..
        } = &turn.parts[0]
        else {
            panic!("reasoning part")
        };
        assert_eq!(*current_ui_id, ui_id);
        assert_eq!(trace.body_entity_id(), body_id);
        assert_eq!(trace.elapsed(), elapsed);
        assert!(trace.is_expanded() && *finished);
        assert_eq!(reasoning.display, "authoritative summary");
        assert_eq!(
            reasoning
                .replay
                .as_ref()
                .and_then(|metadata| metadata.responses.as_ref())
                .and_then(|metadata| metadata.encrypted_reasoning.as_deref()),
            Some("opaque")
        );
    });
}

#[gpui::test]
fn reasoning_after_a_tool_call_creates_a_second_ordered_card(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let tool_call = crate::llm::ToolCall {
        id: "call-0".into(),
        name: "lookup".into(),
        arguments: serde_json::json!({"query": "Nostra"}),
        raw_arguments: r#"{"query":"Nostra"}"#.into(),
        provider_metadata: ProviderMetadata::default(),
    };
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            crate::assistant::apply_generation_events_for_test(
                this,
                vec![
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 0,
                        id: "reasoning-0".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        delta: "before tool".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningFinished {
                        content_index: 0,
                        id: "reasoning-0".into(),
                        replay: None,
                    },
                    crate::llm::GenerationEvent::ToolCallStarted {
                        content_index: 1,
                        index: 0,
                        id: "call-0".into(),
                        name: "lookup".into(),
                    },
                    crate::llm::GenerationEvent::ToolCallFinished {
                        content_index: 1,
                        index: 0,
                        tool_call: Box::new(tool_call.clone()),
                    },
                    crate::llm::GenerationEvent::ReasoningStarted {
                        content_index: 2,
                        id: "reasoning-1".into(),
                    },
                    crate::llm::GenerationEvent::ReasoningDelta {
                        content_index: 2,
                        id: "reasoning-1".into(),
                        delta: "after tool".into(),
                    },
                ],
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(reasoning_parts(turn).len(), 2);
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [
                ContentBlock::Reasoning { reasoning: first },
                ContentBlock::ToolCall { tool_call: middle },
                ContentBlock::Reasoning { reasoning: second },
            ] if first.display == "before tool"
                && middle == &tool_call
                && second.display == "after tool"
        ));
    });
}

/// Responses providers may forward empty text deltas. They carry no content and
/// cannot stand in for the separate reasoning-finished lifecycle event.
#[gpui::test]
fn empty_text_delta_does_not_finish_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.append_stream_text(1, "text-0".into(), "", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn)[0].1 && reasoning.is_expanded());
        assert!(matches!(
            turn.canonical().content.as_slice(),
            [ContentBlock::Reasoning { .. }]
        ));
    });
}

/// Direct view updates obey the same structural rule as the event bridge: a
/// text delta cannot close an independently identified reasoning block.
#[gpui::test]
fn visible_text_does_not_finish_an_independent_reasoning_block(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            this.append_stream_text(1, "text-0".into(), "interleaved text", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(!reasoning_states(turn)[0].1 && reasoning.is_expanded());
    });
}

/// Reasoning that runs to the end of a turn with no text after it — a failed
/// or cancelled turn, or a model that reasons and then stops — is closed by
/// `finish_reply` instead.
#[gpui::test]
fn terminating_a_turn_closes_an_open_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "interrupted mid-thought", cx);
            this.finish_reply(None, None, cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn)[0].1);
        assert!(!reasoning.is_expanded());
    });
}

/// As in pi's `message_end` handling, the complete terminal message is the
/// rendering authority even when it differs from the live delta projection.
#[gpui::test]
fn terminal_message_replaces_streamed_reasoning_projection(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "partial", cx);
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "authoritative terminal reasoning".into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert_eq!(
            reasoning_states(turn)[0].0,
            "authoritative terminal reasoning"
        );
    });
}

/// Some providers backfill omitted intermediate events in their terminal
/// object. The terminal snapshot must still create the presentation state.
#[gpui::test]
fn terminal_message_can_create_a_reasoning_trace(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.finish_reply(
                Some(IndexedMessage::from_message(LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Reasoning {
                        reasoning: crate::llm::ReasoningContent {
                            display: "backfilled reasoning".into(),
                            replay: None,
                        },
                    }],
                    provider_metadata: ProviderMetadata::default(),
                })),
                None,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("terminal trace");
        assert_eq!(reasoning_states(turn)[0], ("backfilled reasoning", true));
        assert!(!reasoning.is_expanded());
    });
}

/// Once the user works the toggle, the stream stops deciding for them: a
/// trace deliberately opened during streaming stays open when it ends.
#[gpui::test]
fn manual_toggle_survives_the_auto_collapse(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "thinking", cx);
            let reasoning = this
                .messages
                .last_mut()
                .and_then(|turn| reasoning_part_mut(turn))
                .expect("trace");
            // Collapse it by hand mid-stream, then re-open it.
            reasoning.toggle();
            assert!(!reasoning.is_expanded());
            reasoning.toggle();
            assert!(reasoning.is_expanded());

            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "answer", cx);
        });
    });

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("trace");
        assert!(reasoning_states(turn)[0].1, "the stream still ended");
        assert!(
            reasoning.is_expanded(),
            "explicit user intent outlives the auto-collapse"
        );
    });
}

/// The collapsed trigger is a chip, not a bar: it must lay out at its own
/// label width rather than stretching across the content column. Asserted
/// against the transcript's real geometry, since "does a flex child stretch"
/// depends on the container it ends up in and is easy to regress by moving
/// the element.
#[gpui::test]
fn the_collapsed_trigger_hugs_its_label(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    // Reason, then answer: that collapses the card down to its trigger.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "a thought", cx);
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "The answer.", cx);
        });
    });
    cx.run_until_parked();
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );

    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the collapsed trigger was drawn");

    assert!(
        trigger.size.width < CONTENT_MAX_WIDTH,
        "the trigger stretched to the content column ({:?}) instead of hugging its label",
        trigger.size.width
    );
    assert!(
        trigger.size.width > px(0.),
        "the trigger must still be wide enough to hit"
    );
}

/// Clicking the copy button puts the turn's complete reasoning on the
/// clipboard — the accumulated source, not the seven lines the card happens
/// to be showing.
#[gpui::test]
fn the_copy_button_copies_the_whole_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    // `ChatView` is the window's own root view here, rather than
    // `add_empty_window` plus a manual `cx.draw`: a click has to route through
    // the window's real element tree, and a hand-drawn element is not part of
    // it. Deliberately not wrapped in `Root` (which the app does): `Root::new`
    // installs a macOS hit-test forwarder behind `not(test)`, and that cfg is
    // false for gpui-component compiled as a dependency, so it reaches for a
    // real platform window and panics under a `TestWindow`. Nothing in this
    // test needs the overlay layers `Root` provides.
    let (chat, cx) = cx.add_window_view(ChatView::new);
    seed_turn(&chat, cx);

    // Far more than the visible budget, so the copy would be lossy if it read
    // the rendered view instead of the source.
    let mut expected = String::new();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..30 {
                let delta = format!("Reasoning line {line}.\n\n");
                expected.push_str(&delta);
                this.append_stream_reasoning(0, "reasoning-0".into(), &delta, cx);
            }
            this.finish_stream_reasoning(0, "reasoning-0", None);
            this.append_stream_text(1, "text-0".into(), "The answer.", cx);
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let copy = cx
        .debug_bounds("reasoning-copy-0")
        .expect("the copy button is in the tree once there is reasoning");
    let trigger = cx
        .debug_bounds("reasoning-trigger-0")
        .expect("the reasoning trigger is in the tree");
    // The copy action is intentionally hidden from hit testing and keyboard
    // focus until its group is hovered. Exercise that real interaction
    // instead of clicking the hidden element by debug bounds.
    cx.simulate_mouse_move(trigger.center(), None, gpui::Modifiers::default());
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, expected,
        "the clipboard must carry the complete reasoning source"
    );

    // Copying is not a disclosure gesture. The button sits beside the trigger
    // rather than inside it, and `Clipboard` stops propagation, so a copy must
    // leave the card exactly as the user left it.
    assert!(
        chat.read_with(cx, |this, _| this
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .is_some_and(|reasoning| !reasoning.is_expanded())),
        "copying must not toggle the card open"
    );
}

/// Nothing to copy before the first delta lands, so the button stays out of
/// the tree rather than offering to copy an empty string.
#[gpui::test]
fn the_copy_button_appears_only_once_there_is_reasoning(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = cx.add_window_view(ChatView::new);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };

    // An empty protocol delta is not evidence that reasoning started.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "", cx)
        });
    });
    draw(cx);
    assert!(
        chat.read_with(cx, |this, _| this
            .messages
            .last()
            .is_some_and(|turn| reasoning_part(turn).is_none())),
        "empty deltas must not allocate a trace"
    );
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_none(),
        "no copy button while there is nothing to copy"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "a thought", cx)
        });
    });
    draw(cx);
    assert!(
        cx.debug_bounds("reasoning-copy-0").is_some(),
        "the button becomes available as soon as reasoning arrives"
    );
}

/// A non-reasoning assistant turn has no synthetic placeholder part. Its first
/// visible text event creates the exact part that render consumes.
#[gpui::test]
fn a_turn_without_reasoning_creates_only_its_text_part(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, "text-0".into(), "answer", cx);
        });
    });
    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        assert!(matches!(
            turn.parts.as_slice(),
            [MessagePart::Text { text, .. }] if text == "answer"
        ));
    });
}

/// The design claim behind rendering the card inline instead of floating it:
/// once a reasoning stream saturates the height budget, the card stops
/// growing, so everything laid out below it holds still no matter how many
/// tokens still arrive. Asserted against the transcript's own content height,
/// which is what a reflow would move.
#[gpui::test]
fn a_saturated_card_stops_moving_the_content_below_it(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
    };
    let transcript_content_height = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| chat.read(cx).scroll_handle.max_offset().y)
    };

    // Well past a seven-line budget, so the cap is already engaged.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 0..40 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    let saturated = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .expect("trace")
            .scroll_max_offset()
    });
    assert!(
        saturated > px(0.),
        "the card must be hiding content behind its own scroll, not growing to fit it"
    );
    let before = transcript_content_height(cx);

    // Another 40 paragraphs of reasoning: all of it lands inside the card.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for line in 40..80 {
                this.append_stream_reasoning(
                    0,
                    "reasoning-0".into(),
                    &format!("Reasoning line {line}.\n\n"),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    assert_eq!(
        transcript_content_height(cx),
        before,
        "a capped card must not change the transcript's layout as it streams"
    );
}

/// Reasoning is markdown, so a fenced block in it bakes in the palette it was
/// parsed against — the same trap the error card's body falls into.
#[gpui::test]
fn theme_switch_reparses_the_reasoning_body(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_reasoning(0, "reasoning-0".into(), "```json\n{\"a\":1}\n```", cx);
        });
    });
    let before = cx.update(|_, cx| {
        chat.read(cx)
            .messages
            .last()
            .and_then(|turn| reasoning_part(turn))
            .expect("trace")
            .body_entity_id()
    });

    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let turn = chat.read(cx).messages.last().expect("assistant turn");
        let reasoning = reasoning_part(turn).expect("the trace survives a theme switch");
        assert_ne!(
            reasoning.body_entity_id(),
            before,
            "theme changes must replace the parsed markdown state"
        );
        assert!(
            reasoning_states(turn)[0].0.contains("json"),
            "re-parsing must not lose what already streamed"
        );
    });

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}
