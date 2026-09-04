use super::*;

#[gpui::test]
fn oversized_display_formula_scrolls_horizontally_and_bubbles_vertical_wheel(
    cx: &mut TestAppContext,
) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(320.), px(900.)));
    let terms = std::iter::repeat_n("x_i^2", 40)
        .collect::<Vec<_>>()
        .join(" + ");
    let markdown = format!("$$\n{terms}\n$$\n\n{}", "tail\n\n".repeat(80));
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
    redraw_settled_math(cx);
    cx.update(|_, cx| {
        chat.read(cx).list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    });
    redraw(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    let formula_selector: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let row_selector: &'static str =
        Box::leak(format!("markdown-math-block-row-{owner_id}-0").into_boxed_str());
    let before = cx
        .debug_bounds(formula_selector)
        .expect("wide formula bounds");
    let row = cx.debug_bounds(row_selector).expect("display row bounds");
    assert!(
        before.size.width > row.size.width,
        "fixture must exceed its viewport: {before:?} vs {row:?}"
    );

    cx.simulate_event(ScrollWheelEvent {
        position: row.center(),
        delta: ScrollDelta::Pixels(point(px(-80.), px(-10.))),
        ..Default::default()
    });
    redraw(cx);

    let after = cx
        .debug_bounds(formula_selector)
        .expect("scrolled formula bounds");
    assert!(
        after.left() < before.left(),
        "horizontal input did not move the oversized formula: {before:?} -> {after:?}"
    );

    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.max_offset_for_scrollbar().y > px(0.)),
        "the transcript fixture must have vertical overflow"
    );
    let transcript_before_vertical =
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y);
    let row = cx.debug_bounds(row_selector).expect("display row bounds");
    cx.simulate_event(ScrollWheelEvent {
        position: row.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
        ..Default::default()
    });
    redraw(cx);

    assert_eq!(
        cx.debug_bounds(formula_selector)
            .expect("vertically scrolled formula bounds")
            .left(),
        after.left(),
        "vertical wheel input must not be remapped into horizontal formula scrolling"
    );
    assert!(
        cx.update(|_, cx| chat.read(cx).list_state.scroll_px_offset_for_scrollbar().y)
            < transcript_before_vertical,
        "vertical wheel input over a display formula must continue to scroll the transcript"
    );
}

#[gpui::test]
fn reasoning_delimiter_ellipsis_stays_on_the_native_text_path(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    seed_turn(&chat, cx);
    cx.simulate_resize(gpui::size(px(210.), px(700.)));
    let reasoning = "我们要求输出一些数学公式，包括块级和内联。块级公式用$$...$$，内联用$...$。随便写点数学公式即可。注意格式。我们输出即可。";
    cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            test_support::append_reasoning(this, 0, "reasoning-math".into(), reasoning, cx);
        });
    });
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let inline_marker = "内联用$...$";
        let inline_start = reasoning.find(inline_marker).expect("inline phrase") + "内联用".len();
        (last_reasoning_id(chat.read(cx)), inline_start)
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_none(),
        "dot-only delimiter examples must remain native text instead of an atomic formula image"
    );
}

#[gpui::test]
fn linked_inline_formula_opens_its_destination(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "[$x^2$](https://example.com/math)";
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
        (ui_id, text.find('$').expect("linked formula"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    let bounds = cx.debug_bounds(formula).expect("linked formula bounds");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/math"));
}

#[gpui::test]
fn native_markdown_image_remains_clickable_beside_a_formula(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown =
        "$x$[![native](https://example.com/image.svg)](https://example.com/native-image)";
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
    let formula: &'static str = Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let formula = cx.debug_bounds(formula).expect("formula bounds");
    let image_center = point(formula.right() + px(4.), formula.center().y);
    cx.simulate_click(image_center, gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/native-image"),
        "math preparation must preserve the adjacent native image and its link"
    );
}

#[gpui::test]
fn reference_image_alt_hazards_keep_native_images_and_chat_rendering(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "$q$[![a $$x][r]](https://click-a.test) ![b y$$][s]\n\n[r]: https://a.test/i.svg\n[s]: https://b.test/i.svg";
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
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        assert_eq!(text, markdown);
        ui_id
    });
    let formula: &'static str = Box::leak(format!("markdown-math-{owner_id}-0").into_boxed_str());
    let formula = cx.debug_bounds(formula).expect("leading formula bounds");
    let image_center = point(formula.right() + px(4.), formula.center().y);
    cx.simulate_click(image_center, gpui::Modifiers::default());
    assert_eq!(cx.opened_url().as_deref(), Some("https://click-a.test"));

    let unclaimed_math_start = markdown.find("$$").expect("image alt delimiter");
    let unclaimed_math: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{unclaimed_math_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(unclaimed_math).is_none(),
        "dollars spanning reference-image alt text must remain native image content"
    );
}

#[gpui::test]
fn reference_linked_formula_keeps_native_link_behavior(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = r"[$x^2$][math]
[label \(y\)][]
[shortcut \(z\)]

[math]: https://example.com/reference-math
[label \(y\)]: https://example.com/collapsed-math
[shortcut \(z\)]: https://example.com/shortcut-math";
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
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        assert_eq!(text, markdown);
        ui_id
    });
    for (formula_source, destination) in [
        ("$x^2$", "https://example.com/reference-math"),
        (r"\(y\)", "https://example.com/collapsed-math"),
        (r"\(z\)", "https://example.com/shortcut-math"),
    ] {
        let formula_start = markdown
            .find(formula_source)
            .expect("reference-linked formula");
        let formula: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
        let bounds = cx.debug_bounds(formula).expect("reference formula bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some(destination));
    }
    let selected = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 0, 0);
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected.trim(), "$x^2$\nlabel \\(y\\)\nshortcut \\(z\\)");
}

#[gpui::test]
fn colliding_prepared_reference_identifiers_keep_distinct_destinations(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = r"[label \(x\)][] and [$a$][label \(x\)]
[label $$x$$][]

[label \(x\)]: https://example.com/slash
[label $$x$$]: https://example.com/dollar";
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
    for (formula_source, destination) in [
        ("$a$", "https://example.com/slash"),
        ("$$x$$", "https://example.com/dollar"),
    ] {
        let formula_start = markdown.find(formula_source).expect("linked formula");
        let formula: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
        let bounds = cx.debug_bounds(formula).expect("linked formula bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some(destination));
    }

    let selected = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (_, _, body) = prose_at(this, 0, 0);
            body.select_all_text(cx)
        })
    });
    assert_eq!(selected.trim(), "label (x) and $a$\nlabel $$x$$");
}

#[gpui::test]
fn table_cell_formula_uses_native_table_flow(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let markdown = "| 描述 | 数学表达式 |\n| :--- | :--- |\n| 模长大于 R | $|z| > R$ |";
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

    let (owner_id, formula_start, selected) = cx.update(|_, cx| {
        chat.update(cx, |this, cx| {
            let (ui_id, text, body) = prose_at(this, 0, 0);
            (
                ui_id,
                text.find("$|z| > R$").expect("table formula"),
                body.select_all_text(cx),
            )
        })
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(
        cx.debug_bounds(formula).is_some(),
        "the native table cell must retain its custom inline formula"
    );
    assert_eq!(selected.trim(), "描述 数学表达式\n模长大于 R $|z| > R$");
}

#[gpui::test]
fn long_inline_riemann_formulas_all_render(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    let formulas = [
        r"$R^{\rho}_{\sigma\mu\nu}$",
        r"$R^{\rho}_{\sigma\mu\nu} = \partial_{\mu} \Gamma^{\rho}_{\nu\sigma} - \partial_{\nu} \Gamma^{\rho}_{\mu\sigma}$",
        r"$R^{\rho}_{\sigma\mu\nu} = \partial_{\mu} \Gamma^{\rho}_{\nu\sigma} - \partial_{\nu} \Gamma^{\rho}_{\mu\sigma} + \Gamma^{\rho}_{\mu\lambda} \Gamma^{\lambda}_{\nu\sigma} - \Gamma^{\rho}_{\nu\lambda} \Gamma^{\lambda}_{\mu\sigma}$",
        r"$\displaystyle R^{\rho}_{\sigma\mu\nu} = \partial_{\mu} \Gamma^{\rho}_{\nu\sigma} - \partial_{\nu} \Gamma^{\rho}_{\mu\sigma}$",
        r"$R^{\rho}_{\sigma\,\mu\,\nu}$",
    ];
    let markdown = format!(
        "好的，这是你需要渲染的黎曼几何算子公式，直接输出如下：\n\n{}",
        formulas.join("\n\n")
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
    redraw_settled_math(cx);

    let owner_id = cx.update(|_, cx| {
        let (ui_id, _, _) = prose_at(chat.read(cx), 0, 0);
        ui_id
    });
    for formula in formulas {
        let start = markdown.find(formula).expect("formula source");
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "inline formula did not render: {formula}; caches: {:?}",
            crate::ui::math::formula_cache_snapshots(owner_id),
        );
    }
}

#[gpui::test]
fn multiline_inline_math_paragraph_lays_out_without_panicking(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(360.), px(1200.)));
    let markdown = "第一行\n第二行 $x^2$ 结尾";
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

    // The first draw exercises custom flow measurement. The old implementation
    // forwarded the embedded newline to `shape_line`, whose single-line
    // contract deliberately panics in debug builds.
    redraw_settled_math(cx);

    let (owner_id, formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (ui_id, text.find("$x^2$").expect("inline math"))
    });
    let formula: &'static str =
        Box::leak(format!("markdown-math-{owner_id}-{formula_start}").into_boxed_str());
    assert!(cx.debug_bounds(formula).is_some());
}

#[gpui::test]
fn multiple_display_formulas_stack_without_overlap(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(1600.)));
    let markdown = "before\n\n$$x^2$$\n\nmiddle\n\n\\[\\frac{1}{2}\\]\n\nafter";
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

    let (owner_id, dollar_formula_start, bracket_formula_start) = cx.update(|_, cx| {
        let (ui_id, text, _) = prose_at(chat.read(cx), 0, 0);
        (
            ui_id,
            text.find("$$").expect("dollar formula"),
            text.find(r"\[").expect("bracket formula"),
        )
    });
    let formula_starts = [dollar_formula_start, bracket_formula_start];
    let bounds = formula_starts
        .into_iter()
        .map(|start| {
            let selector: &'static str =
                Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
            cx.debug_bounds(selector).expect("display formula bounds")
        })
        .collect::<Vec<_>>();
    assert!(bounds[0].size.width > px(0.) && bounds[0].size.height > px(0.));
    assert!(bounds[1].size.width > px(0.) && bounds[1].size.height > px(0.));
    assert!(
        bounds[0].bottom() <= bounds[1].top() || bounds[1].bottom() <= bounds[0].top(),
        "multiple display formulas must be vertically ordered without overlap: {bounds:?}"
    );
}

#[gpui::test]
fn matrix_display_formula_survives_markdown_block_tokenization(cx: &mut TestAppContext) {
    init_app(cx);
    let (chat, cx) = add_chat_window(cx);
    cx.simulate_resize(gpui::size(px(900.), px(1800.)));
    let markdown = "以下是一些数学公式：\n\n内联公式示例：勾股定理 \\(a^2+b^2=c^2\\)，欧拉公式 \\(e^{i\\pi}+1=0\\)，以及极限 \\(\\lim_{x\\to 0}\\frac{\\sin x}{x}=1\\)。\n\n块级公式示例：\n\n$$\n\\int_{-\\infty}^{\\infty} e^{-x^2}\\,dx = \\sqrt{\\pi}\n$$\n\n$$\n\\sum_{n=1}^{\\infty} \\frac{1}{n^2} = \\frac{\\pi^2}{6}\n$$\n\n$$\n\\begin{pmatrix}\n1 & 2 \\\\\n3 & 4\n\\end{pmatrix}\n\\begin{pmatrix}\nx \\\\ y\n\\end{pmatrix}\n=\n\\begin{pmatrix}\n5 \\\\ 6\n\\end{pmatrix}\n$$";
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
    let opening_fences = markdown
        .match_indices("$$")
        .enumerate()
        .filter_map(|(index, (start, _))| index.is_multiple_of(2).then_some(start))
        .collect::<Vec<_>>();
    assert_eq!(
        opening_fences.len(),
        3,
        "test fixture must contain three blocks"
    );

    for (formula_ix, start) in opening_fences.into_iter().enumerate() {
        let selector: &'static str =
            Box::leak(format!("markdown-math-{owner_id}-{start}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "display formula {} must be rendered as an image-backed math node; caches: {:?}",
            formula_ix + 1,
            crate::ui::math::formula_cache_snapshots(owner_id),
        );
    }
}
