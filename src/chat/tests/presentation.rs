use super::*;

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
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, None, Some(error), cx)
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let assistant_turn = last_turn(this, cx);
        let error = last_error_card(this).expect("card attached to the failed turn");
        assert_eq!(assistant_turn.role, Role::Assistant);
        assert_eq!(
            error.request_id(),
            Some("nostra-1"),
            "the visible card retains the correlation id"
        );
        assert!(
            this.transcript.read(cx).turns()[0].error.is_none(),
            "the user's own turn carries no error"
        );
        // The provider's error text must not leak into replayable history.
        assert!(
            this.transcript
                .read(cx)
                .turns()
                .iter()
                .all(
                    |message| message.to_llm().content.iter().all(|block| !matches!(
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
        last_error_card(chat.read(cx))
            .and_then(|error| error.body_entity_id())
            .expect("error body entity")
    });

    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let error = last_error_card(this).expect("the card survives a theme switch");
        let error_body_after_theme_switch = error.body_entity_id().expect("error body entity");
        assert_eq!(
            error_body_after_theme_switch, error_body_before_theme_switch,
            "theme changes must refresh native highlights without replacing TextView state"
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
    // The windowed helper renders the composer, which reads the font global;
    // init_app installs it alongside the rest of the app state.
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let input = cx.update(|_, cx| chat.read(cx).composer.read(cx).input());

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
            assert!(!this.input_blank);
            assert!(this.record_composer_height(px(168.)));
            assert_eq!(this.composer_height, px(168.));
            assert_eq!(this.base_composer_height, px(96.));
        });
    });

    // Whitespace is not an empty layout state, but it is still not a
    // submittable draft. Both decisions are retained in the view snapshot.
    cx.update(|window, cx| {
        input.update(cx, |state, cx| {
            state.set_value("   ", window, cx);
            cx.emit(InputEvent::Change);
        });
    });
    cx.update(|_, cx| {
        chat.update(cx, |this, _| {
            assert!(!this.input_empty);
            assert!(this.input_blank);
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
            assert!(this.input_blank);
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
            test_support::push_canonical(this,
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
            );
        });
    });
    cx.run_until_parked();

    let draw = |width: f32, cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(width), px(700.)),
            |_, _| chat.clone().into_any_element(),
        );
        cx.debug_bounds("row-userbubble-1-1-bubble")
            .expect("the user bubble was drawn")
    };

    let narrow_viewport_width = 440.;
    let content_inset = px(24.);
    let wide = draw(900., cx);
    eprintln!(
        "DEBUG wide: row={:?}",
        cx.debug_bounds("row-userbubble-1-1")
    );
    let narrow = draw(narrow_viewport_width, cx);
    eprintln!(
        "DEBUG narrow: row={:?} content={:?}",
        cx.debug_bounds("row-userbubble-1-1"),
        cx.debug_bounds("row-userbubble-1-1-bubble")
    );

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
