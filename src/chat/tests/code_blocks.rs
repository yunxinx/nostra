use super::*;

#[gpui::test]
fn fenced_blocks_show_language_and_copy_their_own_code(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "Before\n\n```rust\nfn first() {}\n```\n\n```unknown-language-tag-that-must-truncate-cleanly\nsecond\n```\n\n```\nplain\n```";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);

    let (owner_id, body_id_before) = cx.update(|_, cx| {
        let (ui_id, _, body) = prose_at(chat.read(cx), 0, 0);
        (ui_id, body.entity_id())
    });
    let fences = markdown
        .match_indices("```")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let first_copy: &'static str =
        Box::leak(format!("markdown-code-copy-{owner_id}-{}", fences[0]).into_boxed_str());
    let second_copy: &'static str =
        Box::leak(format!("markdown-code-copy-{owner_id}-{}", fences[2]).into_boxed_str());
    let third_copy: &'static str =
        Box::leak(format!("markdown-code-copy-{owner_id}-{}", fences[4]).into_boxed_str());
    let first_language: &'static str =
        Box::leak(format!("markdown-code-language-{owner_id}-{}", fences[0]).into_boxed_str());
    let second_language: &'static str =
        Box::leak(format!("markdown-code-language-{owner_id}-{}", fences[2]).into_boxed_str());
    let third_language: &'static str =
        Box::leak(format!("markdown-code-language-{owner_id}-{}", fences[4]).into_boxed_str());
    let second_block: &'static str =
        Box::leak(format!("markdown-code-block-{owner_id}-{}", fences[2]).into_boxed_str());
    let second_header: &'static str =
        Box::leak(format!("markdown-code-header-{owner_id}-{}", fences[2]).into_boxed_str());
    let second_line: &'static str =
        Box::leak(format!("markdown-code-line-{owner_id}-{}-0", fences[2]).into_boxed_str());
    let second_wrap: &'static str =
        Box::leak(format!("markdown-code-wrap-{owner_id}-{}", fences[2]).into_boxed_str());

    assert!(cx.debug_bounds(first_language).is_some(), "rust label");
    assert!(cx.debug_bounds(second_language).is_some(), "unknown label");
    assert!(
        cx.debug_bounds(third_language).is_none(),
        "an untagged block must not render an empty label"
    );

    cx.simulate_resize(gpui::size(px(320.), px(600.)));
    redraw(cx);
    let block_bounds = cx.debug_bounds(second_block).expect("code block");
    let header_bounds = cx.debug_bounds(second_header).expect("code header");
    let language_bounds = cx.debug_bounds(second_language).expect("unknown label");
    let line_bounds = cx.debug_bounds(second_line).expect("first code line");
    let wrap_bounds = cx.debug_bounds(second_wrap).expect("wrap button");
    let copy_bounds = cx.debug_bounds(second_copy).expect("code copy button");
    assert_eq!(
        line_bounds.top() - header_bounds.bottom(),
        px(6.),
        "the code content must sit 6px below the header"
    );
    assert_eq!(
        (
            header_bounds.top(),
            header_bounds.left(),
            header_bounds.right()
        ),
        (
            block_bounds.top(),
            block_bounds.left(),
            block_bounds.right()
        ),
        "the header background must reach the code block's top and side edges"
    );
    assert_eq!(
        language_bounds.left(),
        line_bounds.left(),
        "the language label must align with the code content inside the full-width header"
    );
    assert_eq!(
        wrap_bounds.top() - block_bounds.top(),
        px(6.),
        "the header controls must sit 6px below the block top"
    );
    assert_eq!(
        header_bounds.bottom() - wrap_bounds.bottom(),
        px(6.),
        "the header controls must have 6px of bottom padding"
    );
    assert!(
        language_bounds.right() <= wrap_bounds.left()
            && wrap_bounds.right() <= copy_bounds.left()
            && copy_bounds.right() <= header_bounds.right(),
        "the language must stay left while wrap and copy stay right"
    );
    assert!(
        copy_bounds.right() <= px(320.),
        "the copy action must stay inside a narrow chat window"
    );

    for (selector, expected) in [
        (first_copy, "fn first() {}"),
        (second_copy, "second"),
        (third_copy, "plain"),
    ] {
        let bounds = cx.debug_bounds(selector).expect("code copy button");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(expected)
        );
    }

    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let (_, _, body) = prose_at(chat.read(cx), 0, 0);
        assert_eq!(
            body.entity_id(),
            body_id_before,
            "the renderer updates its palette without replacing message state"
        );
    });
}

#[test]
fn code_block_labels_resolve_in_every_locale() {
    for locale in ["en", "zh-CN"] {
        for key in ["chat.code.copy", "chat.code.wrap"] {
            let resolved = rust_i18n::t!(key, locale = locale).to_string();
            assert!(!resolved.contains(key), "{key} unresolved for {locale}");
        }
    }
}

#[gpui::test]
fn code_block_display_controls_apply_at_their_own_scopes(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let long_line = "long code line ".repeat(40);
    let markdown = format!("```rust\n{long_line}\nsecond line\n```\n\n```text\n{long_line}\n```");
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    let fences = markdown
        .match_indices("```")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let selectors = |kind: &str, block: usize, suffix: &str| -> &'static str {
        Box::leak(
            format!(
                "markdown-code-{kind}-{owner_id}-{}{suffix}",
                fences[block * 2]
            )
            .into_boxed_str(),
        )
    };
    let first_block = selectors("block", 0, "");
    let first_header = selectors("header", 0, "");
    let first_line = selectors("line", 0, "-0");
    let first_scroll = selectors("scroll", 0, "");
    let first_wrap = selectors("wrap", 0, "");
    let first_copy = selectors("copy", 0, "");
    let first_number = selectors("line-number", 0, "-0");
    let second_line = selectors("line", 1, "-0");
    let second_wrap = selectors("wrap", 1, "");
    let second_number = selectors("line-number", 1, "-0");

    cx.update(|_, cx| {
        assert!(!crate::ui::markdown::global_wrap_enabled(cx));
        assert!(!crate::ui::markdown::line_numbers_enabled(cx));
    });
    assert!(cx.debug_bounds(first_number).is_none());
    assert!(cx.debug_bounds(second_number).is_none());

    let block_bounds = cx.debug_bounds(first_block).expect("first code block");
    let header_bounds = cx.debug_bounds(first_header).expect("code header");
    let nowrap_line = cx.debug_bounds(first_line).expect("first code line");
    let nowrap_second = cx.debug_bounds(second_line).expect("second code block");
    let scroll_bounds = cx.debug_bounds(first_scroll).expect("horizontal viewport");
    let wrap_bounds = cx.debug_bounds(first_wrap).expect("wrap control");
    let copy_bounds = cx.debug_bounds(first_copy).expect("copy control");
    assert!(
        nowrap_line.size.width > scroll_bounds.size.width,
        "nowrap code must retain its intrinsic width inside a horizontal viewport"
    );
    assert_eq!(
        nowrap_line.top() - header_bounds.bottom(),
        px(6.),
        "the header and code content must be separated by exactly 6px"
    );
    assert!(
        wrap_bounds.right() <= copy_bounds.left()
            && copy_bounds.right() <= header_bounds.right()
            && header_bounds.right() <= block_bounds.right(),
        "actions must remain ordered at the right side of the header"
    );

    cx.update(|_, cx| {
        preferences::update_in_memory(cx, |prefs| {
            prefs.code_block_line_numbers = true;
        });
        cx.refresh_windows();
    });
    redraw(cx);

    cx.update(|_, cx| {
        assert!(!crate::ui::markdown::global_wrap_enabled(cx));
        assert!(crate::ui::markdown::line_numbers_enabled(cx));
    });
    let number_bounds = cx.debug_bounds(first_number).expect("fixed line number");
    let numbered_scroll_bounds = cx
        .debug_bounds(first_scroll)
        .expect("numbered horizontal viewport");
    assert!(
        number_bounds.right() <= numbered_scroll_bounds.left(),
        "line numbers must stay outside the horizontal scrolling viewport"
    );
    assert!(cx.debug_bounds(second_number).is_some());

    let first_wrap_bounds = cx.debug_bounds(first_wrap).expect("first wrap control");
    cx.simulate_click(first_wrap_bounds.center(), gpui::Modifiers::default());
    redraw(cx);

    let locally_wrapped_first = cx.debug_bounds(first_line).expect("locally wrapped block");
    let unchanged_second = cx
        .debug_bounds(second_line)
        .expect("unchanged second block");
    assert!(locally_wrapped_first.size.height > nowrap_line.size.height);
    assert_eq!(unchanged_second.size.height, nowrap_second.size.height);
    cx.update(|_, cx| assert!(!crate::ui::markdown::global_wrap_enabled(cx)));

    cx.update(|_, cx| crate::ui::markdown::set_global_wrap_in_memory(true, cx));
    redraw(cx);

    cx.update(|_, cx| {
        assert!(crate::ui::markdown::global_wrap_enabled(cx));
        assert!(crate::ui::markdown::line_numbers_enabled(cx));
    });
    let wrapped_first = cx.debug_bounds(first_line).expect("wrapped first block");
    let wrapped_second = cx.debug_bounds(second_line).expect("wrapped second block");
    assert!(
        wrapped_first.size.height > nowrap_line.size.height
            && wrapped_second.size.height > nowrap_line.size.height,
        "a global change must reset every code block to the new value"
    );

    cx.update(|_, cx| crate::ui::markdown::set_global_wrap_in_memory(false, cx));
    redraw(cx);

    let reset_first = cx
        .debug_bounds(first_line)
        .expect("globally reset first block");
    let reset_second = cx
        .debug_bounds(second_line)
        .expect("globally reset second block");
    assert_eq!(reset_first.size.height, nowrap_line.size.height);
    assert_eq!(reset_second.size.height, nowrap_second.size.height);
    cx.update(|_, cx| assert!(!crate::ui::markdown::global_wrap_enabled(cx)));

    cx.update(|_, cx| crate::ui::markdown::set_global_wrap_in_memory(true, cx));
    redraw(cx);

    let first_wrap_bounds = cx.debug_bounds(first_wrap).expect("first wrap control");
    cx.simulate_click(first_wrap_bounds.center(), gpui::Modifiers::default());
    redraw(cx);

    let locally_unwrapped_first = cx
        .debug_bounds(first_line)
        .expect("locally unwrapped block");
    let still_wrapped_second = cx
        .debug_bounds(second_line)
        .expect("still wrapped second block");
    assert_eq!(locally_unwrapped_first.size.height, nowrap_line.size.height);
    assert!(still_wrapped_second.size.height > nowrap_second.size.height);
    assert!(cx.debug_bounds(second_wrap).is_some());
    cx.update(|_, cx| assert!(crate::ui::markdown::global_wrap_enabled(cx)));
}

#[gpui::test]
fn nowrap_code_block_exposes_a_horizontal_scrollbar(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    let long_line = "horizontal overflow ".repeat(80);
    let markdown = format!("```rust\n{long_line}\n```\n\n{}", "tail\n\n".repeat(80));
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    let scrollbar: &'static str =
        Box::leak(format!("markdown-code-scrollbar-{owner_id}-0").into_boxed_str());
    let first_line: &'static str =
        Box::leak(format!("markdown-code-line-{owner_id}-0-0").into_boxed_str());
    let block: &'static str =
        Box::leak(format!("markdown-code-block-{owner_id}-0").into_boxed_str());
    let viewport: &'static str =
        Box::leak(format!("markdown-code-scroll-{owner_id}-0").into_boxed_str());

    let scrollbar_bounds = cx
        .debug_bounds(scrollbar)
        .expect("horizontal scrollbar layer");
    let line_bounds = cx.debug_bounds(first_line).expect("first code line");
    let block_bounds = cx.debug_bounds(block).expect("code block");
    let viewport_bounds = cx.debug_bounds(viewport).expect("code viewport");
    assert_eq!(
        scrollbar_bounds.size.height,
        px(16.),
        "the horizontal scrollbar must have a stable interaction track"
    );
    assert!(
        scrollbar_bounds.top() >= line_bounds.bottom(),
        "the horizontal scrollbar must sit below the code instead of covering it: scrollbar={scrollbar_bounds:?}, line={line_bounds:?}"
    );
    assert_eq!(
        block_bounds.bottom() - scrollbar_bounds.bottom(),
        px(2.),
        "the horizontal scrollbar must sit exactly 2px above the code block bottom"
    );
    assert_eq!(
        viewport_bounds.size.width - scrollbar_bounds.size.width,
        px(2.),
        "the horizontal scrollbar must be 2px narrower than its viewport"
    );
}

#[gpui::test]
fn nowrap_code_block_hides_the_horizontal_scrollbar_without_overflow(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    let markdown = "```rust\nlet short = true;\n```";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    let scrollbar: &'static str =
        Box::leak(format!("markdown-code-scrollbar-{owner_id}-0").into_boxed_str());

    assert!(
        cx.debug_bounds(scrollbar).is_none(),
        "a nowrap code block without horizontal overflow must not render a scrollbar track"
    );
}

#[gpui::test]
fn nowrap_code_block_updates_scrollbar_visibility_after_resize(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(1000.), px(420.)));
    let medium_line = "medium-width-code ".repeat(4);
    let markdown = format!("```rust\n{medium_line}\n```");
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    let scrollbar: &'static str =
        Box::leak(format!("markdown-code-scrollbar-{owner_id}-0").into_boxed_str());

    assert!(
        cx.debug_bounds(scrollbar).is_none(),
        "a wide viewport must not render the scrollbar track"
    );

    cx.simulate_resize(gpui::size(px(360.), px(420.)));
    redraw(cx);
    redraw(cx);
    assert!(
        cx.debug_bounds(scrollbar).is_some(),
        "narrowing the viewport past the content width must reveal the scrollbar track"
    );

    cx.simulate_resize(gpui::size(px(1000.), px(420.)));
    redraw(cx);
    redraw(cx);
    redraw(cx);
    assert!(
        cx.debug_bounds(scrollbar).is_none(),
        "widening the viewport again must remove the stale scrollbar track"
    );
}

#[gpui::test]
fn horizontal_code_scroll_does_not_move_the_transcript(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(640.), px(420.)));
    let long_line = "horizontal overflow ".repeat(80);
    let markdown = format!(
        "Intro\n\nIntro\n\n```rust\n{long_line}\n```\n\n{}",
        "tail\n\n".repeat(80)
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::push_canonical(
                this,
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.clone(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            );
        });
    });
    redraw(cx);
    cx.update(|_, cx| {
        chat.read(cx)
            .view
            .list_state
            .set_offset_from_scrollbar(point(px(0.), px(0.)));
    });
    redraw(cx);

    let (owner_id, fence_start) = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, markdown.find("```").expect("code fence"))
    });
    let selector = |kind: &str, suffix: &str| -> &'static str {
        Box::leak(format!("markdown-code-{kind}-{owner_id}-{fence_start}{suffix}").into_boxed_str())
    };
    let viewport = selector("scroll", "");
    let first_line = selector("line", "-0");

    assert!(
        cx.update(|_, cx| chat.read(cx).view.list_state.max_offset_for_scrollbar().y > px(0.)),
        "the transcript fixture must have vertical overflow"
    );
    let line_before = cx.debug_bounds(first_line).expect("first code line");
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(-80.), px(0.))),
        ..Default::default()
    });
    redraw(cx);
    let line_after_right = cx.debug_bounds(first_line).expect("scrolled code line");
    assert!(
        line_after_right.left() < line_before.left(),
        "horizontal wheel input must move the code content"
    );

    cx.update(|_, cx| {
        chat.read(cx)
            .view
            .list_state
            .set_offset_from_scrollbar(point(px(0.), px(-20.)));
    });
    redraw(cx);
    let transcript_before_left = cx.update(|_, cx| {
        chat.read(cx)
            .view
            .list_state
            .scroll_px_offset_for_scrollbar()
            .y
    });
    let line_before_left = cx
        .debug_bounds(first_line)
        .expect("right-scrolled code line");
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(40.), px(0.))),
        ..Default::default()
    });
    redraw(cx);

    let line_after_left = cx
        .debug_bounds(first_line)
        .expect("left-scrolled code line");
    assert!(
        line_after_left.left() > line_before_left.left(),
        "leftward navigation must move the code content back toward its origin"
    );
    assert_eq!(
        cx.update(|_, cx| chat
            .read(cx)
            .view
            .list_state
            .scroll_px_offset_for_scrollbar()
            .y),
        transcript_before_left,
        "horizontal scrolling inside a code block must never move the transcript"
    );

    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(1000.), px(0.))),
        ..Default::default()
    });
    redraw(cx);
    let line_at_left_boundary = cx.debug_bounds(first_line).expect("left-aligned code line");
    assert_eq!(line_at_left_boundary.left(), line_before.left());

    let transcript_at_left_boundary = cx.update(|_, cx| {
        chat.read(cx)
            .view
            .list_state
            .scroll_px_offset_for_scrollbar()
            .y
    });
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(40.), px(0.))),
        ..Default::default()
    });
    redraw(cx);
    assert_eq!(
        cx.update(|_, cx| chat
            .read(cx)
            .view
            .list_state
            .scroll_px_offset_for_scrollbar()
            .y),
        transcript_at_left_boundary,
        "continuing left at the code boundary must still not scroll the transcript"
    );

    let code_x_before_vertical = line_at_left_boundary.left();
    let transcript_before_vertical = cx.update(|_, cx| {
        chat.read(cx)
            .view
            .list_state
            .scroll_px_offset_for_scrollbar()
            .y
    });
    let viewport_bounds = cx.debug_bounds(viewport).expect("horizontal viewport");
    cx.simulate_event(ScrollWheelEvent {
        position: viewport_bounds.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
        ..Default::default()
    });
    redraw(cx);

    assert_eq!(
        cx.debug_bounds(first_line)
            .expect("vertically scrolled code line")
            .left(),
        code_x_before_vertical,
        "vertical wheel input must not be remapped into horizontal code scrolling"
    );
    assert!(
        cx.update(|_, cx| chat
            .read(cx)
            .view
            .list_state
            .scroll_px_offset_for_scrollbar()
            .y)
            < transcript_before_vertical,
        "vertical wheel input over a code block must continue to scroll the transcript"
    );
}

#[gpui::test]
fn long_formula_transcript_materializes_only_viewport_and_overdraw(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(760.), px(640.)));

    let initial_scroll_height_estimate = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            for index in 0..100 {
                let text = if index % 3 == 0 {
                    format!("$x_{{{index}}}^2 + y_{{{index}}}^2$")
                } else {
                    format!("message {index}")
                };
                test_support::push_canonical(
                    this,
                    LlmMessage {
                        role: crate::llm::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text,
                            provider_metadata: ProviderMetadata::default(),
                        }],
                        provider_metadata: ProviderMetadata::default(),
                    },
                    cx,
                );
            }

            this.view.list_state.max_offset_for_scrollbar().y
        })
    });
    assert!(
        initial_scroll_height_estimate > px(15_000.),
        "unmeasured rows must contribute their height hints to the first-frame scrollbar: {initial_scroll_height_estimate:?}"
    );
    redraw_settled_math(cx);

    let (last_owner, bottom_materialized) = cx.update(|_, cx| {
        let chat = chat.read(cx);
        let owner = |index: usize| prose_at(chat, index, 0).0;
        (owner(99), chat.view.materialized_row_indices().len())
    });
    // Row granularity: the materialize/retain zones are screen-based, so the
    // bound is expressed in rows (two rows per message) plus chrome.
    assert!(
        bottom_materialized <= 96,
        "virtual list materialized {bottom_materialized} of 200 projected rows"
    );
    assert!(
        cx.debug_bounds(Box::leak(
            format!("markdown-math-{last_owner}-0").into_boxed_str()
        ))
        .is_some(),
        "tail formula must render while following the bottom"
    );

    cx.update(|_, cx| {
        chat.read(cx).view.list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    });
    redraw_settled_math(cx);
    // The head row re-materializes on demand; its markdown owner (and with
    // it the formula selector root) is captured from the settled renderer,
    // then one more settled draw renders the regenerated formula.
    let first_owner = cx.update(|_, cx| prose_at(chat.read(cx), 0, 0).0);
    redraw_settled_math(cx);
    let top_materialized = cx.update(|_, cx| chat.read(cx).view.materialized_row_indices().len());
    assert!(
        top_materialized <= 96,
        "top materialized {top_materialized} of 200 projected rows"
    );
    assert!(
        cx.debug_bounds(Box::leak(
            format!("markdown-math-{first_owner}-0").into_boxed_str()
        ))
        .is_some(),
        "head formula must regenerate after scrolling to the top; materialized={:?}, offset={:?}",
        cx.update(|_, cx| chat.read(cx).view.materialized_row_indices()),
        cx.update(|_, cx| chat.read(cx).view.list_state.logical_scroll_top())
    );
    let head_cache = crate::ui::math::formula_cache_snapshot(first_owner, 0)
        .expect("head formula cache after top render");
    assert!(head_cache.active && head_cache.ready, "{head_cache:?}");

    let top_before_stream = cx.update(|_, cx| {
        let chat = chat.read(cx);
        assert!(!chat.view.list_state.is_following_tail());
        chat.view.list_state.logical_scroll_top()
    });
    cx.update(|_, cx| {
        chat.update(cx, |chat, _cx| test_support::finish_stream_batch(chat));
    });
    redraw(cx);
    cx.update(|_, cx| {
        let chat = chat.read(cx);
        assert!(
            !chat.view.list_state.is_following_tail(),
            "a streaming update must not re-arm follow while the user is reading the top"
        );
        let after_stream = chat.view.list_state.logical_scroll_top();
        assert_eq!(after_stream.item_ix, top_before_stream.item_ix);
        assert_eq!(
            after_stream.offset_in_item,
            top_before_stream.offset_in_item
        );
    });

    let messages = cx
        .debug_bounds(Box::leak(
            format!("markdown-math-{first_owner}-0").into_boxed_str(),
        ))
        .expect("visible head formula bounds");
    cx.simulate_event(ScrollWheelEvent {
        position: messages.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-100_000.))),
        ..Default::default()
    });
    redraw_settled_math(cx);
    assert!(
        cx.update(|_, cx| chat.read(cx).view.list_state.is_following_tail()),
        "scrolling back to the true bottom must re-arm tail following"
    );
    // The tail row re-materializes with a fresh markdown owner; recapture it
    // from the settled renderer before asserting the regenerated formula.
    let last_owner = cx.update(|_, cx| prose_at(chat.read(cx), 99, 0).0);
    redraw_settled_math(cx);
    assert!(
        cx.debug_bounds(Box::leak(
            format!("markdown-math-{last_owner}-0").into_boxed_str()
        ))
        .is_some(),
        "tail formula must regenerate after a complete round trip"
    );
    let released = crate::ui::math::formula_cache_snapshot(first_owner, 0)
        .expect("released head formula probe");
    assert!(
        !released.active,
        "offscreen formula cache stayed active: {released:?}"
    );
    assert_eq!(released.release_count, 1, "{released:?}");
    assert_eq!(released.image_drop_count, 1, "{released:?}");
}
