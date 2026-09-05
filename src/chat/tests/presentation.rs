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
/// pass: the raw-response body is a lazy `MarkdownBody`, created by the first
/// expand and released by the re-collapse, and a failed turn projects no
/// actions row (AC6).
#[gpui::test]
fn failed_turn_renders_the_upstream_error_row(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    // Long enough to cross the collapse threshold, so the card starts
    // collapsed and the lazy body rule is observable.
    let raw = format!(
        r#"{{"error":{{"message":"{}","code":"rate_limit_exceeded"}}}}"#,
        "x".repeat(4 * 1024)
    );
    let mut error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
        .with_upstream_body(raw);
    error.request_id = Some("nostra-1".into());
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, None, Some(error), cx)
        });
    });

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let assistant_turn = last_turn(this, cx);
        assert_eq!(assistant_turn.role, Role::Assistant);
        let error = last_error_renderer(this).expect("card attached to the failed turn");
        assert_eq!(
            error.request_id(),
            Some("nostra-1"),
            "the visible card retains the correlation id"
        );
        assert!(
            this.transcript.read(cx).turns()[0].error.is_none(),
            "the user's own turn carries no error"
        );
        // AC6: the failed turn projects the error row and no actions row.
        let failed_turn_id = last_turn(this, cx).turn_id;
        let failed_kinds: Vec<crate::chat::projection::RowKind> = this
            .view
            .projection
            .rows()
            .iter()
            .filter(|row| row.id().turn == failed_turn_id)
            .map(|row| row.kind())
            .collect();
        assert!(
            failed_kinds.contains(&crate::chat::projection::RowKind::TurnError),
            "the error row exists: {failed_kinds:?}"
        );
        assert!(
            !failed_kinds.contains(&crate::chat::projection::RowKind::TurnActions),
            "a failed turn projects no actions row: {failed_kinds:?}"
        );
        assert!(
            error.body_entity_id().is_none(),
            "the raw response body is lazy: nothing before the first expand"
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

    // Expanding builds the body entity; a theme change keeps it (colors
    // refresh without replacing the Markdown state).
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            assert!(toggle_error_row(this, cx), "the error row toggles");
        });
    });
    cx.run_until_parked();
    let error_body_before_theme_switch = cx.update(|_, cx| {
        last_error_renderer(chat.read(cx))
            .and_then(|error| error.body_entity_id())
            .expect("the expanded body entity")
    });

    // `Theme::change` rather than `theme::set_mode`: the latter persists to
    // the user's real configuration directory.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        let this = chat.read(cx);
        let error = last_error_renderer(this).expect("the card survives a theme switch");
        let error_body_after_theme_switch = error.body_entity_id().expect("expanded body");
        assert_eq!(
            error_body_after_theme_switch, error_body_before_theme_switch,
            "theme changes must refresh native highlights without replacing the body"
        );
    });

    // Re-draws cleanly against the new theme.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );

    // Re-collapse releases the body entity again.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            assert!(toggle_error_row(this, cx));
        });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let error = last_error_renderer(chat.read(cx)).expect("card");
        assert!(
            error.body_entity_id().is_none(),
            "AC6: collapse releases the raw response body"
        );
    });
}

/// AC6: the "copy raw response" button writes the captured upstream body —
/// not the re-indented display form — to the clipboard.
#[gpui::test]
fn the_error_row_copies_the_raw_response(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);

    let raw = r#"{"error":{"message":"Rate limit reached","code":"rate_limit_exceeded"}}"#;
    let mut error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
        .with_upstream_body(raw);
    error.request_id = Some("nostra-1".into());
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, None, Some(error), cx)
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let copy = cx
        .debug_bounds("row-turnerror-2-0-copy")
        .expect("the copy raw response control is in the tree");
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("something was written to the clipboard");
    assert_eq!(
        copied, raw,
        "the clipboard carries the verbatim captured response"
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

/// The `user_message_markdown` preference chooses the user bubble's
/// projection at materialize time: off renders the keyed plain text view with
/// no Markdown entity, on builds the `MarkdownBody`, and flipping the
/// preference releases the entity the row no longer needs — one entity per
/// text, never two (R6).
#[gpui::test]
fn toggling_user_message_markdown_swaps_the_bubble_projection(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let text = "**bold** <em>html</em> and $x$ over\nseveral lines";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::User,
                    content: vec![ContentBlock::Text {
                        text: text.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw_settled(cx);

    // Default preference: plain projection, no Markdown entity.
    cx.update(|_, cx| {
        let bubble = last_user_bubble_renderer(chat.read(cx)).expect("user bubble row");
        assert!(
            bubble.body_entity_for_test().is_none(),
            "the plain projection must not build a MarkdownBody"
        );
    });

    // Enabling the preference re-materializes the row with a MarkdownBody.
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.user_message_markdown = true);
    });
    redraw_settled(cx);
    let markdown_body = cx.update(|_, cx| {
        last_user_bubble_renderer(chat.read(cx))
            .expect("user bubble row")
            .body_entity_for_test()
            .expect("the markdown projection builds a MarkdownBody")
    });

    // And back off: the entity is released again; canonical text is untouched.
    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| prefs.user_message_markdown = false);
    });
    redraw_settled(cx);
    cx.update(|_, cx| {
        let bubble = last_user_bubble_renderer(chat.read(cx)).expect("user bubble row");
        assert!(
            bubble.body_entity_for_test().is_none(),
            "disabling the preference releases the MarkdownBody"
        );
        assert_ne!(
            last_user_bubble_renderer(chat.read(cx))
                .and_then(|bubble| bubble.body_entity_for_test()),
            Some(markdown_body),
        );
        let message = last_turn(chat.read(cx), cx).to_llm();
        let ContentBlock::Text {
            text: canonical, ..
        } = &message.content[0]
        else {
            panic!("the user turn carries a text block");
        };
        assert_eq!(canonical.as_str(), text, "toggling never rewrites content");
    });

    // Both projections draw cleanly.
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(900.), px(700.)),
        |_, _| chat.clone().into_any_element(),
    );
}

/// Regression (P3 dual-axis review): re-materializing the error row with an
/// unchanged raw body — the window-re-entry path — must keep the user's
/// expansion instead of collapsing it again (`seed` only resets the form for
/// a genuinely different body).
#[gpui::test]
fn the_error_row_keeps_its_expansion_across_rematerialization(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    seed_turn(&chat, cx);

    let raw = format!(
        r#"{{"error":{{"message":"{}","code":"rate_limit_exceeded"}}}}"#,
        "x".repeat(4 * 1024)
    );
    let error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
        .with_upstream_body(raw);
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, None, Some(error), cx);
        });
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            assert!(toggle_error_row(this, cx), "the error row toggles");
        });
    });
    cx.update(|_, cx| {
        let expanded = last_error_renderer(chat.read(cx))
            .expect("the error renderer")
            .is_expanded();
        assert!(expanded, "the row is expanded after the toggle");
    });

    // Re-materialize the same row with the same error (what a row leaving
    // and re-entering the materialization window experiences).
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let row_id = this
                .view
                .projection
                .rows()
                .iter()
                .find(|row| row.kind() == crate::chat::projection::RowKind::TurnError)
                .map(|row| row.id())
                .expect("the error row");
            let ix = this
                .view
                .projection
                .row_index(row_id)
                .expect("the error row is in the window");
            let presentation = crate::ui::markdown::MarkdownPresentation::for_test(cx);
            this.view.materialize_row(
                ix,
                &this.transcript.clone(),
                &presentation,
                false,
                false,
                cx,
            );
        });
    });

    cx.update(|_, cx| {
        let expanded = last_error_renderer(chat.read(cx))
            .expect("the error renderer survives re-materialization")
            .is_expanded();
        assert!(
            expanded,
            "re-materialization with the same body keeps the user's expansion"
        );
    });
}
