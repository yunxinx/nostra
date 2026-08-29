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
        inline_formula(&source[node_range.clone()]).is_some(),
        "the isolated node demonstrates why the following digit must remain visible"
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
