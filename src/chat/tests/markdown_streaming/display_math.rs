use super::super::*;

#[gpui::test]
fn block_math_after_markdown_text_is_rendered_as_its_own_block(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(1200.)));
    let markdown = "上文\n\n$$\n\\sum_{n=1}^{\\infty} \\frac{1}{n^2}\n$$\n\n下文";
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
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$$").expect("block math delimiter"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "block math must produce a dedicated rendered formula element"
    );
    let formula_bounds = cx.debug_bounds(formula).expect("formula bounds");
    assert!(
        formula_bounds.size.width > px(0.) && formula_bounds.size.height > px(0.),
        "block formula must have visible layout bounds: {formula_bounds:?}"
    );
    let row: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-{formula_start}").into_boxed_str());
    let row_bounds = cx.debug_bounds(row).expect("display formula row bounds");
    assert!(
        (formula_bounds.center().x - row_bounds.center().x).abs() < px(1.),
        "a display formula narrower than its viewport must stay centered: {formula_bounds:?} vs {row_bounds:?}"
    );

    let content: &'static str = Box::leak("row-prose-1-1".to_string().into_boxed_str());
    let content_bounds = cx.debug_bounds(content).expect("assistant content bounds");
    assert!(
        formula_bounds.top() >= content_bounds.top()
            && formula_bounds.bottom() <= content_bounds.bottom(),
        "display formula must be contained by the assistant content: {formula_bounds:?} vs {content_bounds:?}"
    );
}

#[gpui::test]
fn standalone_same_line_display_math_is_centered_as_a_block(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(900.)));
    let markdown = "上文\n\n$$x^2 + y^2$$\n\n下文";
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
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    let row: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-{formula_start}").into_boxed_str());
    let formula_bounds = cx.debug_bounds(formula).expect("display formula bounds");
    let row_bounds = cx
        .debug_bounds(row)
        .expect("standalone display formula must use the block row");
    assert!(
        (formula_bounds.center().x - row_bounds.center().x).abs() < px(1.),
        "standalone display math must be centered: {formula_bounds:?} vs {row_bounds:?}"
    );
}

#[gpui::test]
fn blockquote_display_math_uses_container_free_formula_source(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "> $$\n> x^2 + y^2\n> $$";
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
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "blockquote container markers must not be passed to RaTeX"
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 0, 0);
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "$$\nx^2 + y^2\n$$");
}

#[gpui::test]
fn list_display_math_uses_container_free_formula_source(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "- $$\n  x^2 + y^2\n  $$";
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
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "list display math must render from its container-free AST value"
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 0, 0);
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "$$\nx^2 + y^2\n$$");
}

#[gpui::test]
fn display_formula_participates_in_reverse_drag_and_copy(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$$\nx^2 + y^2\n$$";
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
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let formula = cx.debug_bounds(formula_selector).expect("formula bounds");
    let right = point(formula.right() - px(1.), formula.center().y);
    let left = point(formula.left() + px(1.), formula.center().y);
    cx.simulate_mouse_down(right, MouseButton::Left, Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_move(left, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_up(left, MouseButton::Left, Modifiers::default());
    redraw(cx);

    let selected = cx.update(gpui_base::TextSelection::selected_text);
    assert_eq!(selected.trim(), markdown);

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 0, 0);
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn adjacent_markdown_and_display_math_keep_their_ordered_inline_flow(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "根据**定义**：\n$$\nE = mc^2\n$$";
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
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$$").expect("display formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "native Markdown blocks must retain the adjacent display formula"
    );

    // A second draw exercises the same TextView and display-flow state rather
    // than rebuilding nested Markdown around the formula.
    redraw(cx);
    assert!(cx.debug_bounds(formula).is_some());

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 0, 0);
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "根据定义：\n$$\nE = mc^2\n$$");
}

#[gpui::test]
fn inline_math_does_not_turn_markdown_marks_into_literal_text(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(240.), px(1200.)));
    let markdown = "**Bold** before $x^2$ and after";
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
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$x^2$").expect("inline math"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "inline math must produce a dedicated rendered formula element"
    );
    let formula_bounds = cx.debug_bounds(formula).expect("formula bounds");
    let content_bounds = cx
        .debug_bounds("row-prose-1-1")
        .expect("assistant content bounds");
    assert!(
        formula_bounds.size.width > px(0.) && formula_bounds.size.height > px(0.),
        "inline formula must have visible layout bounds: {formula_bounds:?}"
    );
    assert!(
        formula_bounds.left() >= content_bounds.left()
            && formula_bounds.right() <= content_bounds.right(),
        "wrapped inline formula must remain inside native Markdown content: {formula_bounds:?} vs {content_bounds:?}"
    );
}

#[gpui::test]
fn streamed_list_continuation_keeps_surrounding_math_renderable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let markdown = "before $a$\n\n- item\n    $$\n    x^2\n    $$\n\nafter $b$";
    let id = "streamed-list-continuation-math".to_string();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::start_text(this, 0, id.clone(), cx)
        });
    });
    for chunk in markdown.split_inclusive('\n') {
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                test_support::append_text(this, 0, id.clone(), chunk, cx);
            });
        });
        redraw(cx);
    }
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 1, 0);
        ui_id
    });
    let snapshots = crate::ui::math::formula_cache_snapshots(owner_id);
    assert_eq!(
        snapshots
            .iter()
            .map(|(_, snapshot)| (snapshot.source.as_str(), snapshot.inline, snapshot.ready))
            .collect::<Vec<_>>(),
        vec![("a", true, true), ("x^2", false, true), ("b", true, true)],
        "every formula around a four-space list continuation must settle: {snapshots:#?}",
    );
}

#[gpui::test]
fn pending_quoted_display_does_not_downgrade_stable_math(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(2000.)));
    seed_turn(&chat, cx);
    let id = "quoted-pending-math".to_string();
    let source = "$$\na\n$$\n\nbefore $s_0$\n\n> quote\n> $$\n> y^2\n> $$\n";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::start_text(this, 0, id.clone(), cx)
        });
    });
    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 1, 0);
        ui_id
    });
    for chunk in source.split_inclusive('\n') {
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                test_support::append_text(this, 0, id.clone(), chunk, cx);
            });
        });
        redraw(cx);
    }
    redraw_settled_math(cx);

    let terminal = IndexedMessage::from_message(LlmMessage {
        role: crate::llm::Role::Assistant,
        content: vec![ContentBlock::Text {
            text: source.to_string(),
            provider_metadata: ProviderMetadata::default(),
        }],
        provider_metadata: ProviderMetadata::default(),
    });
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::finish_reply(this, Some(terminal), None, cx)
        });
    });
    redraw_settled_math(cx);

    let terminal_owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 1, 0);
        ui_id
    });
    assert_eq!(terminal_owner_id, owner_id);
    let start = source.find("$s_0$").expect("stable inline formula");
    let selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
    assert!(
        cx.debug_bounds(selector).is_some(),
        "the terminal snapshot must retain stable math rendered before a quoted pending display"
    );
}
