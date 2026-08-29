use super::*;

#[test]
fn visible_container_text_uses_native_inline_math_flow() {
    for (source, expected) in [
        (
            "| value |\n| --- |\n| \\(cell\\) |\n\n$x$",
            vec![r"\(cell\)", "$x$"],
        ),
        (
            "| value |\n| --- |\n| $cell$ |\n\n$x$",
            vec!["$cell$", "$x$"],
        ),
    ] {
        let prepared = prepare_math_source(source);
        let (root, formulas) = parse_inline_math(source, &prepared);
        assert_eq!(formulas, expected, "source: {source:?}");
        assert!(
            count_nodes(&root, |node| matches!(node, markdown_ast::Node::Table(_))) > 0,
            "native table was replaced: {source:?}"
        );
    }

    for (source, expected) in [
        (
            "[label $raw$][id] and $x$\n\n[id]: https://example.com",
            vec!["$raw$", "$x$"],
        ),
        (
            "[label $raw$][] and $x$\n\n[label $raw$]: https://example.com",
            vec!["$raw$", "$x$"],
        ),
        (
            "[label $raw$] and $x$\n\n[label $raw$]: https://example.com",
            vec!["$raw$", "$x$"],
        ),
        (
            r"[label \(collapsed\)][] and $x$

[label \(collapsed\)]: https://example.com",
            vec![r"\(collapsed\)", "$x$"],
        ),
        (
            r"[label \(shortcut\)] and $x$

[label \(shortcut\)]: https://example.com",
            vec![r"\(shortcut\)", "$x$"],
        ),
        (
            "[Cost $5][] and $x$\n\n[Cost $5]: https://example.com",
            vec!["$x$"],
        ),
    ] {
        let prepared = prepare_math_source(source);
        let (root, formulas) = parse_inline_math(source, &prepared);
        assert_eq!(formulas, expected, "source: {source:?}");
        assert!(
            count_nodes(&root, |node| matches!(
                node,
                markdown_ast::Node::LinkReference(_)
            )) > 0,
            "native reference link was replaced: {source:?}"
        );
    }

    for (source, expected) in [
        (
            r"[label \(direct\)](https://example.com) and $x$",
            vec![r"\(direct\)", "$x$"],
        ),
        (
            "[label $direct$](https://example.com) and $x$",
            vec!["$direct$", "$x$"],
        ),
    ] {
        let prepared = prepare_math_source(source);
        let (root, formulas) = parse_inline_math(source, &prepared);
        assert_eq!(formulas, expected, "source: {source:?}");
        assert!(
            count_nodes(&root, |node| matches!(node, markdown_ast::Node::Link(_))) > 0,
            "native direct link was replaced: {source:?}"
        );
    }

    for (source, expected) in [
        (
            "note[^n]\n\n[^n]: prose \\(foot\\)\n\n$x$",
            vec![r"\(foot\)", "$x$"],
        ),
        (
            "note[^n]\n\n[^n]: prose $foot$\n\n$x$",
            vec!["$foot$", "$x$"],
        ),
    ] {
        let prepared = prepare_math_source(source);
        let (root, formulas) = parse_inline_math(source, &prepared);
        assert_eq!(formulas, expected, "source: {source:?}");
        assert!(
            count_nodes(&root, |node| matches!(
                node,
                markdown_ast::Node::FootnoteDefinition(_)
            )) > 0,
            "native footnote definition was replaced: {source:?}"
        );
    }
}

#[test]
fn invalid_currency_cannot_hide_a_later_formula() {
    for (source, expected) in [
        ("Cost $5; equation $x$", "Cost $5; equation `x`"),
        ("Cost $5; equation $$x$$", "Cost $5; equation $$x$$"),
        (r"Cost $5; equation \(x\)", "Cost $5; equation $$x$$"),
        (r"Cost $5; equation \[x\]", "Cost $5; equation $$x$$"),
        (
            r"escaped \* cost $5; equation \(x\)",
            r"escaped \* cost $5; equation $$x$$",
        ),
    ] {
        assert_eq!(prepare_math_source(source), expected, "source: {source:?}");
    }

    let source = "Cost $5; equation $x$";
    let prepared = prepare_math_source(source);

    let (_, formulas) = parse_inline_math(source, &prepared);
    assert_eq!(formulas, ["$x$"]);
    assert_eq!(
        restore_prepared_literal(
            "Cost $5; equation ",
            "Cost ^5; equation ",
            "Cost ^5; equation "
        )
        .as_deref(),
        Some("Cost $5; equation ")
    );
    assert!(restore_prepared_literal("$5", "^5", "decoded").is_none());
    assert_eq!(
        gfm_visible_text(r"escaped \* cost ^5; equation ").as_deref(),
        Some("escaped * cost ^5; equation ")
    );
    assert_eq!(
        gfm_visible_text(r"escaped \* cost $5; equation ").as_deref(),
        Some("escaped * cost $5; equation ")
    );
    assert_eq!(
        restore_prepared_literal(
            r"escaped \* cost $5; equation ",
            r"escaped \* cost ^5; equation ",
            "escaped * cost ^5; equation ",
        )
        .as_deref(),
        Some("escaped * cost $5; equation ")
    );
}

#[test]
fn currency_and_delimiter_examples_remain_native() {
    for source in [
        "Costs $5 and $10 today",
        "inline uses $...$",
        "inline uses $…$",
        "inline uses $⋯$",
        "display uses $$...$$",
    ] {
        assert!(!contains_math_syntax(source), "unexpected math: {source:?}");
    }
    assert!(contains_math_syntax("The value is $5$ exactly"));
    assert!(contains_math_syntax("placeholder $...$, formula $x$"));
}

#[test]
fn code_escapes_and_incomplete_streams_are_not_math() {
    for source in [
        "```text\n$x$ $$y$$ \\\\[z\\]\n```",
        "    $x$ $$y$$",
        "`$x$ $$y$$`",
        r"\$x$",
        r"\\(x\)",
        "$x",
        "$$\nx^2",
        r"\[x^2",
        "$$open `$$`",
        r"\[open `\]`",
        "$$open\n```text\n$$\n```",
    ] {
        assert!(!contains_math_syntax(source), "unexpected math: {source:?}");
    }

    assert!(contains_math_syntax("$x$"));
    assert!(contains_math_syntax("$$\nx^2\n$$"));
    assert!(contains_math_syntax(r"\[x^2\]"));
    assert!(contains_math_syntax(r"escaped \` marker and $x$"));
}

#[test]
fn multiline_display_math_is_one_opaque_markdown_node() {
    let source = "$$\n\\begin{bmatrix}\n1 & 2 \\\\\n3 & 4\n\\end{bmatrix}\n\n=\n0\n$$";
    let prepared = prepare_math_source(source);
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(&prepared, &options).expect("math AST");
    let children = root.children().expect("root children");
    assert!(matches!(children.as_slice(), [markdown_ast::Node::Math(_)]));
    let formula = display_formula(source).expect("display formula");
    assert!(formula.source.contains("\n=\n"));
}

#[test]
fn inline_preparation_does_not_hide_later_display_blocks() {
    for (source, expected_displays) in [
        ("$$\na\n$$", 1),
        ("prose\n\n$$\na\n$$", 1),
        ("inline \\(x\\)\n\n$$\na\n$$", 1),
        ("inline \\(x\\)\n\n$$\na\n$$\n\n$$\nb\n$$", 2),
    ] {
        let prepared = prepare_math_source(source);
        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        options.math_text_single_dollar = false;
        let root = markdown::to_mdast(&prepared, &options).expect("math AST");
        assert_eq!(
            count_nodes(&root, |node| matches!(node, markdown_ast::Node::Math(_))),
            expected_displays,
            "source: {source:?}; prepared: {prepared:?}"
        );
    }
}

#[test]
fn list_labels_followed_by_display_math_keep_all_formulas_active() {
    let source = "好的，这里有一些数学公式。\n\n**内联公式：**\n- 欧拉公式：$e^{i\\pi} + 1 = 0$\n- 质能方程：$E = mc^2$\n- 勾股定理：$a^2 + b^2 = c^2$\n\n**块级公式：**\n- 二次方程求根公式：\n$$\nx = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}\n$$\n\n- 高斯积分：\n$$\n\\int_{-\\infty}^{\\infty} e^{-x^2} \\, dx = \\sqrt{\\pi}\n$$\n\n- 泰勒级数展开（以 $e^x$ 为例）：\n$$\ne^x = \\sum_{n=0}^{\\infty} \\frac{x^n}{n!}\n$$\n\n- 傅里叶变换：\n$$\n\\hat{f}(\\xi) = \\int_{-\\infty}^{\\infty} f(x) e^{-2\\pi i x \\xi} \\, dx\n$$";
    let prepared = try_prepare_math_source(source).expect("safe math parse view");
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(&prepared, &options).expect("math AST");

    let mut inline_formulas = Vec::new();
    inline_math_sources(&root, source, &mut inline_formulas);
    assert_eq!(inline_formulas.len(), 4, "prepared: {prepared:?}");
    assert_eq!(
        count_nodes(&root, |node| matches!(node, markdown_ast::Node::Math(_))),
        4,
        "prepared: {prepared:?}"
    );
}

#[test]
fn multiple_prepared_inline_formulas_preserve_display_offsets_and_values() {
    let source = "以下是一些数学公式：\n\n内联公式示例：勾股定理 \\(a^2+b^2=c^2\\)，欧拉公式 \\(e^{i\\pi}+1=0\\)，以及极限 \\(\\lim_{x\\to 0}\\frac{\\sin x}{x}=1\\)。\n\n块级公式示例：\n\n$$\n\\int_{-\\infty}^{\\infty} e^{-x^2}\\,dx = \\sqrt{\\pi}\n$$\n\n$$\n\\sum_{n=1}^{\\infty} \\frac{1}{n^2} = \\frac{\\pi^2}{6}\n$$\n\n$$\n\\begin{pmatrix}\n1 & 2 \\\\\n3 & 4\n\\end{pmatrix}\n\\begin{pmatrix}\nx \\\\ y\n\\end{pmatrix}\n=\n\\begin{pmatrix}\n5 \\\\ 6\n\\end{pmatrix}\n$$";
    markdown::to_mdast(source, &markdown::ParseOptions::gfm()).expect("baseline GFM AST");
    let prepared = prepare_math_source(source);
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(&prepared, &options).expect("math AST");

    fn collect_display<'a>(
        node: &'a markdown_ast::Node,
        displays: &mut Vec<&'a markdown_ast::Math>,
    ) {
        if let markdown_ast::Node::Math(math) = node {
            displays.push(math);
        }
        if let Some(children) = node.children() {
            for child in children {
                collect_display(child, displays);
            }
        }
    }

    let mut displays = Vec::new();
    collect_display(&root, &mut displays);
    let expected_starts = source
        .match_indices("$$")
        .enumerate()
        .filter_map(|(index, (start, _))| index.is_multiple_of(2).then_some(start))
        .collect::<Vec<_>>();
    assert_eq!(
        displays.len(),
        expected_starts.len(),
        "prepared: {prepared:?}"
    );
    for (math, expected_start) in displays.into_iter().zip(expected_starts) {
        let position = math.position.as_ref().expect("display position");
        assert_eq!(position.start.offset, expected_start);
        let original = &source[position.start.offset..position.end.offset];
        let formula = display_formula_from_ast(original, &math.value).expect("display formula");
        assert!(!formula.source.is_empty());
        assert_eq!(formula.relative_start, 0);
    }
}

#[test]
fn nested_display_math_uses_container_free_ast_value() {
    for (source, expected_plain_text) in [
        ("> $$\n> x\n> $$", "$$\nx\n$$"),
        ("- $$\n  x\n  $$", "$$\nx\n$$"),
        (
            r"> \[
> x
> \]",
            "\\[\nx\n\\]",
        ),
    ] {
        let prepared = prepare_math_source(source);
        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        options.math_text_single_dollar = false;
        let root = markdown::to_mdast(&prepared, &options).expect("math AST");
        let math = first_display_math(&root).unwrap_or_else(|| {
            panic!("display math node for {source:?}, prepared as {prepared:?}")
        });
        assert_eq!(math.value, "x", "source: {source:?}");

        let position = math.position.as_ref().expect("math position");
        let raw = &source[position.start.offset..position.end.offset];
        assert!(
            raw.contains('>') || raw.contains("  "),
            "fixture must expose container bytes in the raw node span: {raw:?}"
        );
        let formula = display_formula_from_ast(raw, &math.value).expect("display formula");
        assert_eq!(formula.source, "x");
        assert_eq!(formula.plain_text, expected_plain_text);
        assert!(!formula.plain_text.contains("> "));
    }

    for (source, ast_value) in [
        ("> $$\n> \n> $$", ""),
        ("> $$\n> ...\n> $$", "..."),
        ("- $$\n  …\n  $$", "…"),
        ("- $$\n  ⋯\n  $$", "⋯"),
    ] {
        let raw = source
            .strip_prefix("> ")
            .or_else(|| source.strip_prefix("- "))
            .expect("container prefix");
        assert!(
            display_formula_from_ast(raw, ast_value).is_none(),
            "container-only placeholder must remain native: {source:?}"
        );
    }
}
