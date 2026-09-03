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

/// Inline formulas of different heights on one line must sit on one shared
/// baseline (the RaTeX baseline reported through `InlineMetrics`), not be
/// vertically centered independently of each other.
#[gpui::test]
fn inline_formulas_on_one_line_share_the_text_baseline(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "前 $x$ 中 $\\int_0^1 f$ 后";
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
        markdown.find("$x$").expect("short formula"),
        markdown.find("$\\int").expect("tall formula"),
    ];
    let [short, tall] = starts.map(|start| {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        let bounds = cx.debug_bounds(selector).expect("rendered formula bounds");
        let snapshot =
            crate::ui::math::formula_cache_snapshot(owner_id, start).expect("formula cache probe");
        let ascent = snapshot.ascent.expect("installed formula ascent");
        let descent = snapshot.descent.expect("installed formula descent");
        (bounds, ascent, descent)
    });

    assert!(
        tall.1 + tall.2 > short.1 + short.2,
        "the integral must be taller than the variable: {tall:?} vs {short:?}"
    );
    let short_baseline = short.0.top() + short.1;
    let tall_baseline = tall.0.top() + tall.1;
    assert!(
        (short_baseline - tall_baseline).abs() < px(0.5),
        "inline formulas must share one baseline: short {short_baseline:?} vs tall {tall_baseline:?}"
    );
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
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let mid = crate::ui::math::formula_cache_snapshot(owner_id, formula_start)
        .expect("theme-switch in-flight formula cache");
    assert!(
        mid.has_displayed,
        "theme switch must keep the previous formula image instead of a blank frame: {mid:?}"
    );

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
            cx.debug_bounds(selector).is_some(),
            "an unsupported formula must keep a visible monospace fallback"
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

#[gpui::test]
fn italic_aligned_environment_renders_without_mathit_wrap(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = r"*$\begin{aligned}a\\b\end{aligned}$*";
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
    let start = markdown.find('$').expect("formula");
    let selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
    assert!(
        cx.debug_bounds(selector).is_some(),
        "aligned environment in italic must render as a formula"
    );
}

#[gpui::test]
fn streaming_unclosed_tex_is_pending_until_close_or_finish(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "pending-frac".to_string();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| this.start_stream_text(0, id.clone(), cx));
    });
    for character in r"$\frac{a}{b".chars() {
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                this.append_stream_text(0, id.clone(), &character.to_string(), cx);
            });
        });
        redraw(cx);
    }
    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        *ui_id
    });
    let pending: &'static str =
        Box::leak(format!("markdown-math-pending-{owner_id}-0").into_boxed_str());
    assert!(
        cx.debug_bounds(pending).is_some(),
        "unclosed TeX must render as a pending formula node"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id.clone(), "}$", cx);
        });
    });
    redraw_settled_math(cx);
    let ready: &'static str = Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    assert!(
        cx.debug_bounds(ready).is_some(),
        "closing the formula must promote the pending node"
    );

    let unfinished_id = "unclosed-until-finish".to_string();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(1, unfinished_id.clone(), cx);
            this.append_stream_text(1, unfinished_id.clone(), r"$\frac{a}{b", cx);
            this.finish_stream_text(1, &unfinished_id, None, cx);
        });
    });
    redraw(cx);
    let unfinished_owner = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[1] else {
            panic!("second streamed text")
        };
        *ui_id
    });
    let unfinished_pending: &'static str =
        Box::leak(format!("markdown-math-pending-{unfinished_owner}-0").into_boxed_str());
    assert!(
        cx.debug_bounds(unfinished_pending).is_none(),
        "finishing an unclosed opener must fall back to ordinary text"
    );
}

#[gpui::test]
fn formula_render_debounce_coalesces_bursts(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    crate::ui::math::reset_formula_background_submits();
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.messages.push(Message::from_canonical(
                LlmMessage {
                    role: crate::llm::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "$$a$$".into(),
                        provider_metadata: ProviderMetadata::default(),
                    }],
                    provider_metadata: ProviderMetadata::default(),
                },
                cx,
            ));
        });
    });
    redraw(cx);
    assert_eq!(
        crate::ui::math::formula_background_submits(),
        1,
        "a formula that appears once must render immediately"
    );

    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    crate::ui::math::reset_formula_background_submits();
    for body in ["a", "ab", "abc", "abcd", "abcde"] {
        let markdown = format!("$${body}$$");
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                let MessagePart::Text {
                    body: markdown_body,
                    ..
                } = &mut this.messages[0].parts[0]
                else {
                    panic!("assistant text");
                };
                markdown_body.set_text(&markdown, cx);
            });
        });
        redraw(cx);
    }
    assert_eq!(
        crate::ui::math::formula_background_submits(),
        1,
        "the first change after a quiet gap submits immediately; later burst changes wait"
    );
    cx.executor()
        .advance_clock(crate::ui::math::FORMULA_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(
        crate::ui::math::formula_background_submits(),
        2,
        "five changes inside the debounce window coalesce to one extra submit"
    );

    crate::ui::math::reset_formula_background_submits();
    for body in ["x", "xy"] {
        let markdown = format!("$${body}$$");
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                let MessagePart::Text {
                    body: markdown_body,
                    ..
                } = &mut this.messages[0].parts[0]
                else {
                    panic!("assistant text");
                };
                markdown_body.set_text(&markdown, cx);
            });
        });
        redraw(cx);
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
    }
    assert_eq!(
        crate::ui::math::formula_background_submits(),
        2,
        "changes spaced by 200ms each submit"
    );
}

#[gpui::test]
fn display_formula_placeholder_height_stays_within_one_line(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$$\n\\sum_{n=1}^{N} n\n$$";
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
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[0].parts[0] else {
            panic!("assistant text part")
        };
        *ui_id
    });
    let row: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-0").into_boxed_str());
    let pending_height = cx
        .debug_bounds(row)
        .expect("placeholder display row")
        .size
        .height;
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let ready_height = cx
        .debug_bounds(row)
        .expect("settled display row")
        .size
        .height;
    let line_height = cx.update(|window, _| window.line_height());
    assert!(
        (ready_height - pending_height).abs() <= line_height,
        "display height jumped from {pending_height:?} to {ready_height:?} (line {line_height:?})"
    );
}
