use super::*;

#[gpui::test]
fn heading_and_inline_display_math_use_distinct_formula_nodes(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "# 标题 $x^2$\n\n块：$$\\int_0^1 x dx$$ 和 $$y^2$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let starts = [
        markdown.find("$x^2$").expect("heading formula"),
        markdown.find("$$\\int").expect("first inline display"),
        markdown.rfind("$$y^2$$").expect("second inline display"),
    ];
    for start in starts {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "formula at {start} was not rendered"
        );
    }
}

#[gpui::test]
fn formula_rendering_inherits_native_marks_and_heading_weight(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$x+y+z$\n\n***~~$x+y+z$~~***\n\n# $x+y+z$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let starts = [
        markdown.find('$').expect("plain formula"),
        markdown.find("$x+y+z$~~").expect("marked formula"),
        markdown.rfind('$').expect("heading formula") - "$x+y+z".len(),
    ];
    let bounds = starts.map(|start| {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        cx.debug_bounds(selector).expect("rendered formula bounds")
    });

    assert!(
        bounds[1].size != bounds[0].size,
        "bold/italic/strikethrough marks must affect the rendered formula: {bounds:?}"
    );
    assert!(
        bounds[2].size.height > bounds[0].size.height,
        "the native heading size/weight must reach the formula renderer: {bounds:?}"
    );
    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), "$x+y+z$\n$x+y+z$\n$x+y+z$");
}

#[gpui::test]
fn theme_switch_keeps_markdown_state_and_regenerates_formula_color(cx: &mut TestAppContext) {
    init_app(cx);
    cx.update(|cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
    });
    let (chat, cx) = add_chat_window(cx);
    let markdown = "theme-aware $x+y$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start, body_id) = cx.update(|_, cx| {
        let MessagePart::Text {
            ui_id, text, body, ..
        } = &chat.read(cx).messages[0].parts[0]
        else {
            panic!("assistant text part")
        };
        (
            *ui_id,
            text.find("$x+y$").expect("formula"),
            body.entity_id(),
        )
    });
    let before = crate::ui::math::formula_cache_snapshot(owner_id, formula_start)
        .expect("settled dark-theme formula cache");
    assert!(before.ready);
    assert_eq!(before.source, "x+y");
    assert!(before.inline);

    // `Theme::change` avoids persisting into the user's real preferences.
    cx.update(|_, cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
    });
    redraw_settled_math(cx);

    let after = crate::ui::math::formula_cache_snapshot(owner_id, formula_start)
        .expect("settled light-theme formula cache");
    assert!(after.ready);
    assert_eq!(after.source, before.source);
    assert_eq!(after.inline, before.inline);
    assert_ne!(
        after.color, before.color,
        "the resolved text color must change"
    );
    assert!(
        after.generation > before.generation,
        "the keyed formula cache must regenerate after its color fingerprint changes: {before:?} -> {after:?}"
    );
    cx.update(|_, cx| {
        let MessagePart::Text { body, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        assert_eq!(
            body.entity_id(),
            body_id,
            "theme changes must not replace the authoritative Markdown state"
        );
    });
}

#[gpui::test]
fn failed_inline_and_display_formulas_remain_selectable_fallbacks(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = concat!(
        "inline $\\includegraphics{missing.png}$\n\n",
        "$$\n\\includegraphics{missing.png}\n$$",
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, inline_start, display_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (
            *ui_id,
            text.find("$\\includegraphics").expect("inline fallback"),
            text.find("$$\n").expect("display fallback"),
        )
    });
    for start in [inline_start, display_start] {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_none(),
            "an unsupported formula must stay on its selectable text fallback"
        );
    }
    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(
        selected_all.trim(),
        "inline $\\includegraphics{missing.png}$\n$$\n\\includegraphics{missing.png}\n$$"
    );
}

#[gpui::test]
fn currency_closing_context_stays_native_and_selectable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "Cost $5 and$10 today";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, dollar_start, selected_all) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text {
                ui_id, text, body, ..
            } = &this.messages[0].parts[0]
            else {
                panic!("assistant text part")
            };
            (
                *ui_id,
                text.find('$').expect("currency dollar"),
                body.select_all_text(cx),
            )
        })
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{dollar_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_none(),
        "a closing dollar followed by a digit must not become a formula"
    );
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn inline_formula_participates_in_reverse_drag_and_copy(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "before $x^2$ after";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.find("$x^2$").expect("inline formula"))
    });

    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    let formula = cx.debug_bounds(formula_selector).expect("formula bounds");

    let formula_right = point(formula.right() - px(1.), formula.center().y);
    let formula_left = point(formula.left() + px(1.), formula.center().y);
    cx.simulate_mouse_down(formula_right, MouseButton::Left, Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_move(formula_left, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    let selected_inside_formula = cx.update(gpui_base::TextSelection::selected_text);
    assert!(
        selected_inside_formula.contains("$x^2$"),
        "dragging inside the atomic formula did not select it: {selected_inside_formula:?}"
    );
    let text_endpoint = point(formula.left() - px(2.), formula.center().y);
    cx.simulate_mouse_move(text_endpoint, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_up(text_endpoint, MouseButton::Left, Modifiers::default());
    redraw(cx);

    let selected = cx.update(gpui_base::TextSelection::selected_text);
    assert!(
        selected.contains("$x^2$"),
        "reverse drag dropped the atomic formula fallback: {selected:?}"
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), markdown);
}

#[gpui::test]
fn display_formula_drag_preserves_interleaved_markdown_order(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$$\na\n$$\nmiddle\n$$\nb\n$$";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: markdown.into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw_settled_math(cx);

    let (owner_id, second_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        (*ui_id, text.rfind("$$\nb").expect("second formula"))
    });
    let first_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let second_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{second_start}").into_boxed_str());
    let first = cx.debug_bounds(first_selector).expect("first formula");
    let second = cx.debug_bounds(second_selector).expect("second formula");
    let start = point(first.left() + px(1.), first.center().y);
    let end = point(second.right() - px(1.), second.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    redraw(cx);
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    redraw(cx);

    assert_eq!(
        cx.update(gpui_base::TextSelection::selected_text).trim(),
        markdown
    );

    let selected_all = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[0].parts[0] else {
                panic!("assistant text part")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected_all.trim(), markdown);
}
