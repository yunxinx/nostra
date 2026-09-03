use super::*;

#[test]
fn recognizes_supported_delimiters_and_document_relative_starts() {
    for (source, display, body) in [
        ("$x^2$", false, "x^2"),
        (r"\(x + 1\)", false, "x + 1"),
        ("$$x^2$$", true, "x^2"),
        (r"\[x^2\]", true, "x^2"),
    ] {
        let formula = inline_formula(source).expect("inline formula");
        assert_eq!(formula.source, body);
        assert_eq!(formula.plain_text, source);
        assert_eq!(formula.relative_start, 0);
        assert_eq!(formula.display, display);
    }

    let block = display_formula("  $$\n x^2 \n$$").expect("display formula");
    assert_eq!(block.source, "x^2");
    assert_eq!(block.relative_start, 2);
    assert!(block.display);
    assert!(display_formula("plain $$x$$ text").is_none());
}

#[test]
fn source_preparation_preserves_utf8_length_and_character_boundaries() {
    let source = "中文 \\(x + 1\\)\n\n\\[\nα = β\n\\]";
    let prepared = prepare_math_source(source);
    assert_eq!(prepared, "中文 $$x + 1$$\n\n$$\nα = β\n$$");
    assert_eq!(prepared.len(), source.len());
    assert_eq!(
        prepared
            .char_indices()
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>(),
        source
            .char_indices()
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>()
    );
}

#[test]
fn chinese_quoted_strong_remains_one_native_strong_node() {
    let source = "**“神奇魔法球”**，公式 $x$";
    let prepared = prepare_math_source(source);
    let (root, formulas) = parse_inline_math(source, &prepared);
    assert_eq!(formulas, ["$x$"]);
    assert_eq!(
        count_nodes(&root, |node| matches!(node, markdown_ast::Node::Strong(_))),
        1
    );
}

#[test]
fn same_line_double_dollar_formula_remains_an_inline_math_ast_node() {
    let source = "before\n\n$$x^2$$\n\nafter";
    let prepared = prepare_math_source(source);
    assert_eq!(prepared, source);

    let (root, formulas) = parse_inline_math(source, &prepared);
    assert_eq!(formulas, ["$$x^2$$"]);
    assert_eq!(
        count_nodes(&root, |node| matches!(
            node,
            markdown_ast::Node::InlineMath(_)
        )),
        1
    );
}

#[test]
fn inline_formula_recognition_keeps_closing_currency_context() {
    let source = "Cost $5 and$10 today";
    let prepared = prepare_math_source(source);
    assert_eq!(prepared, source);
    let (_, formulas) = parse_inline_math(source, &prepared);
    assert!(formulas.is_empty());

    let node_range = source.find('$').unwrap()..source.rfind('$').unwrap() + 1;
    assert!(
        inline_formula(&source[node_range.clone()]).is_none(),
        "an amount-then-prose body is currency even without a following digit"
    );
    assert!(
        inline_formula_in_context(source, node_range).is_none(),
        "the complete source must reject a closing dollar followed by a digit"
    );
}

#[test]
fn preparation_leaves_code_html_and_destinations_unchanged() {
    for source in [
        "`\\(code\\)` and $x$",
        "```text\n\\[code\\]\n```\n\n$x$",
        "<kbd>\\(raw\\)</kbd> and $x$",
        "<kbd>$raw$</kbd> and $x$",
        "[target](<https://example.com/\\(path\\)>) and $x$",
        "![plot](<https://example.com/\\[path\\]>) and $x$",
        "[target][ref]\n\n[ref]: <https://example.com/$path>\n\n$x$",
        "[target][label $raw$]\n\n[label $raw$]: https://example.com\n\n$x$",
    ] {
        let prepared = prepare_math_source(source);
        assert!(
            prepared.contains(r"\(code\)")
                || prepared.contains(r"\[code\]")
                || prepared.contains(r"\(raw\)")
                || prepared.contains("^raw^")
                || prepared.contains(r"\(path\)")
                || prepared.contains(r"\[path\]")
                || prepared.contains("$path")
                || prepared.contains("$raw$"),
            "opaque source changed: {source:?} -> {prepared:?}"
        );

        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        options.math_text_single_dollar = false;
        let root = markdown::to_mdast(&prepared, &options).expect("math AST");
        let mut formulas = Vec::new();
        inline_math_sources(&root, source, &mut formulas);
        assert_eq!(formulas, ["$x$"], "source: {source:?}");
    }

    for source in [
        "Cost $5 [docs](https://e.test/$path) and $x$",
        "Cost $5 <https://e.test/$path> and $x$",
        "Cost $5 https://e.test/$path and $x$",
        "Cost $5 <span data-v='$path'>raw</span> and $x$",
    ] {
        let prepared = prepare_math_source(source);
        assert!(prepared.starts_with("Cost $5"), "source: {source:?}");
        assert!(prepared.contains("$path"), "source: {source:?}");
        assert!(prepared.ends_with("and `x`"), "source: {source:?}");
    }
}

#[test]
fn spaced_dollars_are_formulas_and_currency_stays_text() {
    for source in ["$ x $", "$x$", r"\(x\)"] {
        let formula = inline_formula(source).expect("accepted formula");
        assert_eq!(formula.plain_text, source, "{source}");
        assert!(!formula.display, "{source}");
    }

    for source in ["$5 and $10", "$ 5 and $ 10", "$2000~$5000", "$...$"] {
        assert!(
            scan_math(source).tokens.is_empty(),
            "currency/placeholder became a formula: {source:?} -> {:?}",
            scan_math(source).tokens
        );
        let prepared = prepare_math_source(source);
        let (_, formulas) = parse_inline_math(source, &prepared);
        assert!(
            formulas.is_empty(),
            "prepared formulas for {source:?}: {formulas:?}"
        );
    }
}

#[test]
fn later_formula_survives_a_rejected_currency_opener() {
    let source = "$5; equation $x$";
    let prepared = prepare_math_source(source);
    let (_, formulas) = parse_inline_math(source, &prepared);
    assert_eq!(formulas, ["$x$"]);
}

#[test]
fn transport_splits_are_repaired_only_inside_math() {
    let tabbed = "$\\text{\tlabel}$";
    let formula = inline_formula(tabbed).expect("tabbed formula");
    assert_eq!(formula.source, r"\text{\tlabel}");
    assert_eq!(formula.plain_text, tabbed);

    let split = "$a\neq b$";
    let formula = inline_formula(split).expect("split neq");
    assert_eq!(formula.source, r"a\neq b");
    assert_eq!(formula.plain_text, split);

    let prose = "line\neq remains prose $x$";
    let prepared = prepare_math_source(prose);
    assert!(prepared.contains("line\neq remains prose"));
    let (_, formulas) = parse_inline_math(prose, &prepared);
    assert_eq!(formulas, ["$x$"]);
}

#[test]
fn unclosed_tex_tail_looks_like_pending_math() {
    let (prefix, pending) = split_pending_math(r"intro $\frac{a}{b").expect("pending");
    assert_eq!(prefix, "intro ");
    assert_eq!(pending, r"$\frac{a}{b");
    assert!(looks_like_math(r"\frac{a}{b"));
    assert!(split_pending_math("$   ").is_none());
    assert!(split_pending_math("just text").is_none());
}
