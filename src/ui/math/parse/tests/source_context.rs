use super::*;

#[test]
fn delimiters_cannot_pair_across_native_markdown_nodes() {
    let direct_link = r"[label \(x](https://example.com) y\)";
    let inline_code = r"\(a `code` b\)";
    let hard_break = "\\(a  \nb\\)";

    for source in [direct_link, inline_code, hard_break] {
        let prepared = prepare_math_source(source);
        assert_eq!(prepared, source, "source: {source:?}");
        let (_, formulas) = parse_inline_math(source, &prepared);
        assert!(formulas.is_empty(), "source: {source:?}");
    }

    let (root, _) = parse_inline_math(direct_link, direct_link);
    assert_eq!(
        count_nodes(&root, |node| matches!(node, markdown_ast::Node::Link(_))),
        1
    );
    let (root, _) = parse_inline_math(inline_code, inline_code);
    assert_eq!(
        count_nodes(&root, |node| matches!(
            node,
            markdown_ast::Node::InlineCode(_)
        )),
        1
    );
    let (root, _) = parse_inline_math(hard_break, hard_break);
    assert_eq!(
        count_nodes(&root, |node| matches!(node, markdown_ast::Node::Break(_))),
        1
    );

    for (source, native_kind, expected_count) in [
        ("[label $x](https://example.com) y$", "link", 1usize),
        ("**Cost $5** plain **Cost $5**", "strong", 2),
        (
            "[Cost $5](https://a.test) [Cost $5](https://b.test)",
            "link",
            2,
        ),
        (
            "![Cost $5](https://a.test/i.svg) ![Cost $5](https://b.test/i.svg)",
            "image",
            2,
        ),
    ] {
        let prepared = prepare_math_source(source);
        let (root, formulas) = parse_inline_math(source, &prepared);
        assert!(formulas.is_empty(), "source: {source:?}");
        let actual = match native_kind {
            "link" => count_nodes(&root, |node| matches!(node, markdown_ast::Node::Link(_))),
            "strong" => count_nodes(&root, |node| matches!(node, markdown_ast::Node::Strong(_))),
            "image" => count_nodes(&root, |node| matches!(node, markdown_ast::Node::Image(_))),
            _ => unreachable!(),
        };
        assert_eq!(
            actual, expected_count,
            "source: {source:?}; prepared: {prepared:?}"
        );
    }
}

#[test]
fn streaming_formula_prefixes_never_fail_source_preparation() {
    let response = "先说明结论。\n\n$$\n\\begin{aligned}\nf(x) &= \\begin{cases}\nx^2, & x \\ge 0 \\\\\n-x, & x < 0\n\\end{cases}\n\\end{aligned}\n$$\n\n最后补充内联公式 $E = mc^2$ 与文字。";

    for end in response
        .char_indices()
        .map(|(offset, character)| offset + character.len_utf8())
    {
        let prefix = &response[..end];
        assert!(
            try_prepare_math_source(prefix).is_ok(),
            "streaming prefix must remain renderable: {prefix:?}"
        );
    }
}

#[test]
fn unclosed_display_math_is_one_opaque_tail_in_the_markdown_ast() {
    let source = "[stable][ref]\n\n$$\nx\n=\ny\n\n[ref]: /inside-formula";
    let opener = source.find("$$").expect("pending opener");
    let prepared = try_prepare_math_source(source).expect("renderable pending math");
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(&prepared, &options).expect("pending math AST");
    let position = first_display_math(&root)
        .and_then(|math| math.position.as_ref())
        .expect("one pending display node");
    assert_eq!(
        position.start.offset..position.end.offset,
        opener..source.len()
    );
    assert_eq!(
        count_nodes(&root, |node| matches!(node, markdown_ast::Node::Heading(_))),
        0
    );

    let closed = format!("{source}\n$$\n\n[ref]: /outside-formula");
    let prepared = try_prepare_math_source(&closed).expect("closed display with real definition");
    let root = markdown::to_mdast(&prepared, &options).expect("closed math AST");
    assert_eq!(
        count_nodes(&root, |node| matches!(node, markdown_ast::Node::Math(_))),
        1
    );
    let destinations = prepared_reference_resolution_topology(&prepared)
        .expect("closed reference topology")
        .into_iter()
        .filter_map(|resolution| resolution.destination.map(|destination| destination.url))
        .collect::<Vec<_>>();
    assert_eq!(destinations, ["/outside-formula"]);
}

#[test]
fn incomplete_non_display_delimiters_remain_literal_parse_views() {
    for source in ["$", r"\(", r"\(x", r"\[", r"\[x"] {
        let prepared = try_prepare_math_source(source)
            .unwrap_or_else(|error| panic!("{source:?} must remain renderable: {error}"));
        assert_eq!(prepared.len(), source.len(), "source: {source:?}");

        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        options.math_text_single_dollar = false;
        let root = markdown::to_mdast(&prepared, &options).expect("literal fallback AST");
        assert_eq!(
            count_nodes(&root, |node| matches!(
                node,
                markdown_ast::Node::InlineMath(_) | markdown_ast::Node::Math(_)
            )),
            0,
            "an incomplete formula must stay on the literal path: {source:?} -> {prepared:?}"
        );
    }
}

#[test]
fn reference_image_alt_dollars_cannot_form_cross_image_math() {
    for source in [
        "![a $$x][r] ![b y$$][s]\n\n[r]: https://a.test/i.svg\n[s]: https://b.test/i.svg",
        "![a $$x][] ![b y$$][]\n\n[a $$x]: https://a.test/i.svg\n[b y$$]: https://b.test/i.svg",
        "![a $$x] ![b y$$]\n\n[a $$x]: https://a.test/i.svg\n[b y$$]: https://b.test/i.svg",
    ] {
        let prepared = try_prepare_math_source(source)
            .expect("reference image alt text must have a safe parse view");

        assert_eq!(prepared.len(), source.len());
        assert_ne!(prepared, source, "one unsafe delimiter must be masked");

        let mut options = markdown::ParseOptions::gfm();
        options.constructs.math_text = true;
        options.constructs.math_flow = true;
        options.math_text_single_dollar = false;
        let root = markdown::to_mdast(&prepared, &options).expect("prepared Markdown AST");
        assert_eq!(
            count_nodes(&root, |node| matches!(
                node,
                markdown_ast::Node::ImageReference(_)
            )),
            2,
            "source: {source:?}; prepared: {prepared:?}"
        );
        assert_eq!(
            count_nodes(&root, |node| matches!(
                node,
                markdown_ast::Node::InlineMath(_) | markdown_ast::Node::Math(_)
            )),
            0,
            "source: {source:?}; prepared: {prepared:?}"
        );

        let destinations = prepared_reference_resolution_topology(&prepared)
            .expect("reference topology")
            .into_iter()
            .filter_map(|resolution| match resolution.kind {
                ReferenceResolutionKind::ImageFull
                | ReferenceResolutionKind::ImageCollapsed
                | ReferenceResolutionKind::ImageShortcut => {
                    resolution.destination.map(|destination| destination.url)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            destinations,
            ["https://a.test/i.svg", "https://b.test/i.svg"],
            "source: {source:?}; prepared: {prepared:?}"
        );
    }
}

#[test]
fn opaque_scanner_controls_cannot_hide_visible_math() {
    for (source, expected) in [
        (
            r"<span data-v='`'>raw</span> and \(x\)",
            "<span data-v='`'>raw</span> and $$x$$",
        ),
        ("<div>\n\n\\(x\\)", "<div>\n\n$$x$$"),
        (r"unmatched ` then \(x\)", "unmatched ^ then $$x$$"),
    ] {
        let prepared = prepare_math_source(source);
        assert_eq!(prepared, expected);
        let (_, formulas) = parse_inline_math(source, &prepared);
        assert_eq!(formulas, [r"\(x\)"], "source: {source:?}");
    }
}

#[test]
fn html_context_follows_source_order_across_markdown_nesting() {
    let hidden = "**<span>raw** $x$ </span>";
    let hidden_prepared = prepare_math_source(hidden);
    assert!(hidden_prepared.contains("^x^"));

    let visible = "<span> $raw$ **</span>** and $x$";
    let visible_prepared = prepare_math_source(visible);
    assert!(visible_prepared.contains("^raw^"));
    assert!(visible_prepared.ends_with("and `x`"));

    for (source, prepared, expected) in [
        (hidden, hidden_prepared.as_str(), Vec::<&str>::new()),
        (visible, visible_prepared.as_str(), vec!["$x$"]),
    ] {
        let (_, formulas) = parse_inline_math(source, prepared);
        assert_eq!(formulas, expected, "source: {source:?}");
    }
}

#[test]
fn html_only_dollars_use_a_restorable_parse_view() {
    let source = "<kbd>$raw$</kbd>";
    let prepared = prepare_math_source(source);
    assert_eq!(prepared, "<kbd>^raw^</kbd>");
    assert_eq!(
        restore_prepared_literal("$raw$", "^raw^", "^raw^").as_deref(),
        Some("$raw$")
    );

    let (_, formulas) = parse_inline_math(source, &prepared);
    assert!(formulas.is_empty());
}

#[test]
fn implicit_reference_identifiers_follow_the_prepared_label() {
    for (source, expected) in [
        (
            r"[label \(collapsed\)][] and $x$

[label \(collapsed\)]: https://example.com",
            "[label $$collapsed$$][] and `x`\n\n[label $$collapsed$$]: https://example.com",
        ),
        (
            r"[label \(shortcut\)] and $x$

[label \(shortcut\)]: https://example.com",
            "[label $$shortcut$$] and `x`\n\n[label $$shortcut$$]: https://example.com",
        ),
        (
            "[Cost $5][] and $x$\n\n[Cost $5]: https://example.com",
            "[Cost $5][] and `x`\n\n[Cost $5]: https://example.com",
        ),
        (
            r"[Label   \(X\)][] and $y$

[label \(x\)]: https://example.com",
            "[Label   $$X$$][] and `y`\n\n[label $$x$$]: https://example.com",
        ),
        (
            r"[a ` \(x\)][] and $y$

[a ` \(x\)]: https://example.com",
            "[a ^ $$x$$][] and `y`\n\n[a ^ $$x$$]: https://example.com",
        ),
    ] {
        assert_eq!(prepare_math_source(source), expected, "source: {source:?}");
    }

    let identifier_collision = r"[label \(x\)][] and [label $$x$$][] and $y$

[label \(x\)]: https://example.com/slash
[label $$x$$]: https://example.com/dollar";
    assert_eq!(
        prepare_math_source(identifier_collision),
        identifier_collision.replace("and $y$", "and `y`"),
        "different original identifiers must not merge in the prepared parse view"
    );

    for unresolved_collision in [
        r"[label \(x\)][] and [label $$x$$][] and $y$

[label \(x\)]: https://example.com/slash",
        r"[label \(x\)][] and ![label $$x$$][] and $y$

[label \(x\)]: https://example.com/slash",
    ] {
        assert_eq!(
            prepare_math_source(unresolved_collision),
            unresolved_collision.replace("and $y$", "and `y`"),
            "preparation must not resolve latent link/image syntax"
        );
    }

    let currency_collision = r"[Cost $5 and \(x\)][] and [Cost ^5 and $$x$$][] and $y$

[Cost $5 and \(x\)]: https://example.com";
    assert_eq!(
        prepare_math_source(currency_collision),
        r"[Cost $5 and $$x$$][] and [Cost ^5 and $$x$$][] and `y`

[Cost $5 and $$x$$]: https://example.com"
    );

    let currency_only_collision = r"[Cost $5][] and $y$

[Cost ^5]: https://example.com";
    let currency_only_prepared = prepare_math_source(currency_only_collision);
    assert_eq!(
        currency_only_prepared,
        r"[Cost $5][] and `y`

[Cost ^5]: https://example.com"
    );
    let (root, formulas) = parse_inline_math(currency_only_collision, &currency_only_prepared);
    assert_eq!(formulas, ["$y$"]);
    assert_eq!(
        count_nodes(&root, |node| matches!(
            node,
            markdown_ast::Node::LinkReference(_) | markdown_ast::Node::ImageReference(_)
        )),
        0
    );

    let repeated_currency_reference = r"[Cost $5][] [Cost $5][]

[Cost $5]: https://a.test";
    let repeated_currency_prepared = prepare_math_source(repeated_currency_reference);
    let (root, formulas) =
        parse_inline_math(repeated_currency_reference, &repeated_currency_prepared);
    assert!(formulas.is_empty());
    assert_eq!(
        count_nodes(&root, |node| matches!(
            node,
            markdown_ast::Node::LinkReference(_)
        )),
        2,
        "the safe fallback must not publish a known reference-topology mismatch"
    );

    let shared = r"[label \(shared\)][] and [other][label \(shared\)] and $x$

[label \(shared\)]: https://example.com";
    let shared_prepared = prepare_math_source(shared);
    assert_eq!(
        shared_prepared,
        "[label $$shared$$][] and [other][label $$shared$$] and `x`\n\n[label $$shared$$]: https://example.com"
    );
    let (root, _) = parse_inline_math(shared, &shared_prepared);
    assert_eq!(
        count_nodes(&root, |node| matches!(
            node,
            markdown_ast::Node::LinkReference(_)
        )),
        2,
        "both implicit and full references must retain the shared definition"
    );

    let image_collision = r"[label \(shared\)][] and ![label \(shared\)][] and $x$

[label \(shared\)]: https://example.com/image.png";
    assert_eq!(
        prepare_math_source(image_collision),
        image_collision.replace("and $x$", "and `x`"),
        "a visible image association must make the parse view fall back atomically"
    );
}

#[test]
fn reference_identifier_normalization_tracks_markdown_rs() {
    for label in [r"label \(x\)", r"Label   \(X\)", r"Straße \(α\)"] {
        let source = format!("[{label}]: https://example.com");
        let root =
            markdown::to_mdast(&source, &markdown::ParseOptions::gfm()).expect("definition AST");
        let identifier = root
            .children()
            .expect("document children")
            .iter()
            .find_map(|node| match node {
                markdown_ast::Node::Definition(definition) => Some(definition.identifier.as_str()),
                _ => None,
            })
            .expect("reference definition");
        assert_eq!(normalize_reference_identifier(label), identifier);
    }
}
