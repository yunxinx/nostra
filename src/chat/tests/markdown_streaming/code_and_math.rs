use super::super::*;

#[test]
fn complete_syntax_registry_supports_common_fence_aliases() {
    let registry = gpui_component::highlighter::LanguageRegistry::singleton();
    for language in [
        "rust",
        "rs",
        "python",
        "py",
        "javascript",
        "js",
        "typescript",
        "ts",
        "bash",
        "sh",
    ] {
        assert!(
            registry.language(language).is_some(),
            "{language} must resolve through the story crate's language set"
        );
    }

    let code = "fn main() { println!(\"highlighted\"); }";
    let rope = gpui_component::Rope::from(code);
    let mut highlighter = gpui_component::highlighter::SyntaxHighlighter::new("rust");
    assert!(highlighter.update(None, &rope, None));
    assert!(
        !highlighter
            .styles(
                &(0..code.len()),
                gpui_component::highlighter::HighlightTheme::default_dark().as_ref(),
            )
            .is_empty(),
        "a registered grammar must produce token styles, not just monospace text"
    );
}

#[gpui::test]
fn streamed_fences_never_fall_back_to_the_native_code_block_renderer(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(1600.), px(3000.)));
    seed_turn(&chat, cx);

    let first = "这里为你提供三段符合要求的 Python 代码：\n\n### 第一段：正常输出\n```python\n# 这是一段简单的打印代码\nname = \"世界\"\nprint(f\"你好，{name}！\")\n```";
    let second = "\n\n### 第二段：内容很长\n```python\n# 这是一段通过拼接长字符串实现的代码，模拟数据量较大的处理场景\nlong_data = \"数据节点\" * 1000 + \"，处理过程正在进行中...\" + \"状态码：200\" * 500\nresult = f\"完整日志信息如下：{long_data}\"\nprint(f\"输出字符串长度为：{len(result)}\")\nprocess_log = [f\"Item_{i}\" for i in range(1000)]\nprint(f\"已处理列表长度：{len(process_log)}\")\n```";
    let third = "\n\n### 第三段：每行都很长\n```python\nextremely_long_variable_name_to_demonstrate_the_horizontal_length_of_the_code_block = \"这是一段非常长的字符串\" + \"重复拼接\" * 20\ndef extremely_complex_and_redundant_function_with_too_many_parameters_to_fit_on_a_standard_screen(param1_that_is_very_long, param2_that_is_also_very_long): return [f\"value_{i}\" for i in range(10)]\nprint(extremely_long_variable_name_to_demonstrate_the_horizontal_length_of_the_code_block)\n```";
    let id = "streamed-python".to_string();

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    // The first draw installs the custom extensions. Before the regression
    // fix, a later background append resumed from its older native parse.
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    let (owner_id, first_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, text.find("```").expect("first fence"))
    });
    let first_header: &'static str =
        Box::leak(format!("markdown-code-header-{owner_id}-{first_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(first_header).is_some(),
        "the first draw must already install the custom fenced-code renderer"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id.clone(), second, cx);
        });
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds(first_header).is_some(),
        "later appends must not replace the first custom fence"
    );
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, third, cx);
        });
    });
    redraw(cx);

    let (owner_id, source) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, text.clone())
    });
    let fences = source
        .match_indices("```")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    assert_eq!(fences.len(), 6);

    for start in fences.into_iter().step_by(2) {
        let header: &'static str =
            Box::leak(format!("markdown-code-header-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(header).is_some(),
            "every streamed fence must use Nostra's renderer, including the first at {start}"
        );
    }
}

#[gpui::test]
fn streamed_indented_code_keeps_the_native_renderer(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-indented-code".to_string();

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), "    print('first')", cx);
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        *ui_id
    });
    let header: &'static str =
        Box::leak(format!("markdown-code-header-{owner_id}-0").into_boxed_str());
    assert!(
        cx.debug_bounds(header).is_none(),
        "four-space indented code must not receive the fenced-code toolbar"
    );

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, "\n    print('second')", cx);
        });
    });
    redraw(cx);
    assert!(
        cx.debug_bounds(header).is_none(),
        "later appends must keep indented code on the native renderer"
    );
    let rendered_text = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { body, .. } = &this.messages[1].parts[0] else {
                panic!("streamed assistant text")
            };
            body.select_all_text(cx)
        })
    });
    assert_eq!(
        rendered_text.trim(),
        "print('first')\nprint('second')",
        "native rendering must preserve every streamed code line"
    );
}

#[gpui::test]
fn raw_html_dollars_remain_literal_and_selectable(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let source = "<kbd>$raw$</kbd>";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, "raw-html-math".to_string(), cx);
            this.append_stream_text(0, "raw-html-math".to_string(), source, cx);
        });
    });
    redraw(cx);

    let (owner_id, start, selected) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text {
                ui_id, text, body, ..
            } = &this.messages[1].parts[0]
            else {
                panic!("streamed assistant text")
            };
            (
                *ui_id,
                text.find("$raw$").expect("literal dollars"),
                body.select_all_text(cx),
            )
        })
    });
    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula_selector).is_none(),
        "raw HTML text must not be claimed by the math renderer"
    );
    assert_eq!(selected.trim(), "$raw$");
}

#[gpui::test]
fn streamed_math_keeps_custom_nodes_after_later_appends(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-math".to_string();
    let first = "根据定义：\n$$\nE = mc^2\n$$";
    let second = "\n继续说明 $x^2$ 的含义。";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    redraw_settled_math(cx);

    let (owner_id, display_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, text.find("$$").expect("display formula"))
    });
    let display: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{display_start}").into_boxed_str());
    assert!(cx.debug_bounds(display).is_some());

    // The append parser runs from its own document snapshot. This second
    // delta verifies that installing the math extensions on the first draw
    // cannot later restore a native paragraph and erase the custom formula.
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, second, cx);
        });
    });
    redraw_settled_math(cx);

    let inline_start = cx.update(|_, cx| {
        let MessagePart::Text { text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        text.find("$x^2$").expect("inline formula")
    });
    let inline: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{inline_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(display).is_some(),
        "a later append must preserve the earlier display formula"
    );
    assert!(
        cx.debug_bounds(inline).is_some(),
        "the appended inline formula must use the custom renderer"
    );
}

#[gpui::test]
fn incomplete_streamed_math_keeps_stable_text_visible_and_recovers(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-incomplete-math".to_string();
    let chunks = [
        "先说明结论。\n\n",
        "$$",
        "\nE = mc^2",
        "\n$$",
        "\n\n最后补充内联公式 $E = mc^2$ 与文字。",
    ];
    let mut expected = String::new();

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
        });
    });

    for chunk in chunks {
        expected.push_str(chunk);
        cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                this.append_stream_text(0, id.clone(), chunk, cx);
            });
        });
        redraw_settled_math(cx);

        let selected = cx.update(|_, cx| {
            chat.update(cx, |this, cx| {
                let MessagePart::Text { body, .. } = &this.messages[1].parts[0] else {
                    panic!("streamed assistant text")
                };
                body.select_all_text(cx)
            })
        });
        for line in expected.lines().filter(|line| !line.is_empty()) {
            assert!(
                selected.contains(line),
                "every streamed prefix must remain visible: missing {line:?} in {selected:?}"
            );
        }
    }

    let (owner_id, display_start, inline_start) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, text, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (
            *ui_id,
            text.find("$$").expect("display formula"),
            text.rfind("$E = mc^2$").expect("inline formula"),
        )
    });
    for start in [display_start, inline_start] {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "the completed formula at {start} must recover its custom renderer"
        );
    }
}

#[gpui::test]
fn streamed_reference_definitions_prepare_later_formula_labels(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-reference-math".to_string();
    let first = r"[label \(x\)]: https://example.com/collapsed
[shortcut \(y\)]: https://example.com/shortcut
[full-math]: https://example.com/full
[Cost $5 and \(w\)]: https://example.com/currency

retained";
    let second = r"

[label \(x\)][]
[shortcut \(y\)]
[full \(z\)][full-math]
[Cost $5 and \(w\)][]";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    redraw(cx);

    let (owner_id, body_id) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, body, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, body.entity_id())
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, second, cx);
        });
    });
    redraw_settled_math(cx);

    for (formula_source, destination) in [
        (r"\(x\)", "https://example.com/collapsed"),
        (r"\(y\)", "https://example.com/shortcut"),
        (r"\(z\)", "https://example.com/full"),
        (r"\(w\)", "https://example.com/currency"),
    ] {
        let formula_start = first.len()
            + second
                .find(formula_source)
                .expect("appended reference formula");
        let formula: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
        let bounds = cx
            .debug_bounds(formula)
            .expect("appended reference formula bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some(destination));
    }

    let selected = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let MessagePart::Text { text, body, .. } = &this.messages[1].parts[0] else {
                panic!("streamed assistant text")
            };
            assert_eq!(body.entity_id(), body_id);
            assert_eq!(text, &format!("{first}{second}"));
            body.select_all_text(cx)
        })
    });
    assert_eq!(
        selected.trim(),
        "retained\nlabel \\(x\\)\nshortcut \\(y\\)\nfull \\(z\\)\nCost $5 and \\(w\\)"
    );
}

#[gpui::test]
fn streaming_does_not_resolve_a_latent_prepared_reference(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    let id = "streamed-latent-reference".to_string();
    let first = "[label \\(x\\)]: https://example.com/slash\n\nretained";
    let second = "\n\n[label \\(x\\)][] and [$a$][label \\(x\\)] and [label $$x$$][]";

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.start_stream_text(0, id.clone(), cx);
            this.append_stream_text(0, id.clone(), first, cx);
        });
    });
    redraw(cx);
    let (owner_id, body_id) = cx.update(|_, cx| {
        let MessagePart::Text { ui_id, body, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        (*ui_id, body.entity_id())
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.append_stream_text(0, id, second, cx);
        });
    });
    redraw_settled_math(cx);

    let latent_start = first.len() + second.find("$$x$$").expect("latent formula");
    let latent: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{latent_start}").into_boxed_str());
    let latent_bounds = cx.debug_bounds(latent).expect("latent formula bounds");
    cx.simulate_click(latent_bounds.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url(),
        None,
        "preparation must not turn unresolved reference syntax into a link"
    );

    let linked_start = first.len() + second.find("$a$").expect("linked formula");
    let linked: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{linked_start}").into_boxed_str());
    let linked_bounds = cx.debug_bounds(linked).expect("linked formula bounds");
    cx.simulate_click(linked_bounds.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/slash")
    );

    cx.update(|_, cx| {
        let MessagePart::Text { text, body, .. } = &chat.read(cx).messages[1].parts[0] else {
            panic!("streamed assistant text")
        };
        assert_eq!(body.entity_id(), body_id);
        assert_eq!(text, &format!("{first}{second}"));
    });
}

#[gpui::test]
fn streaming_does_not_block_the_next_turn_model_selection(cx: &mut TestAppContext) {
    init_app(cx);
    let cx = cx.add_empty_window();
    let chat = cx.update(ChatView::view);
    let next = crate::llm::ModelSelection {
        profile_id: "next-provider".into(),
        model_id: "next-model".into(),
    };
    let observed = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        let observed = observed.clone();
        cx.subscribe(&chat, move |_, event: &ChatEvent, _| {
            if let ChatEvent::SelectionChanged(selection) = event {
                observed.borrow_mut().push(selection.clone());
            }
        })
    });

    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            this.mark_generating_for_test(cx);
            // Drive the public command path, not the state projection helper:
            // this also proves persistence and SelectionChanged emission stay
            // enabled while the current generation remains pending.
            this.select_model(next.clone(), cx);
            assert!(
                this.runtime_snapshot_for_test(cx).is_generating(),
                "switching models must not alter the active reply"
            );
            assert_eq!(this.selection.as_ref(), Some(&next));
        });
    });
    cx.run_until_parked();

    cx.update(|_, cx| {
        assert_eq!(crate::providers::last_selection(cx), Some(next.clone()));
    });
    assert_eq!(
        observed.borrow().as_slice(),
        std::slice::from_ref(&next),
        "the owning ChatApp receives exactly one selection synchronization event"
    );
}
