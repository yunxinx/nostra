use super::*;

fn inline_math_sources<'source>(
    node: &markdown_ast::Node,
    source: &'source str,
    out: &mut Vec<&'source str>,
) {
    if matches!(
        node,
        markdown_ast::Node::InlineMath(_) | markdown_ast::Node::InlineCode(_)
    ) {
        let position = node.position().expect("inline math position");
        let range = position.start.offset..position.end.offset;
        if inline_formula_in_context(source, range.clone()).is_some() {
            out.push(&source[range]);
        }
    }
    if let Some(children) = node.children() {
        for child in children {
            inline_math_sources(child, source, out);
        }
    }
}

fn parse_inline_math<'a>(source: &'a str, prepared: &str) -> (markdown_ast::Node, Vec<&'a str>) {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(prepared, &options).expect("math AST");
    let mut formulas = Vec::new();
    inline_math_sources(&root, source, &mut formulas);
    (root, formulas)
}

fn first_display_math(node: &markdown_ast::Node) -> Option<&markdown_ast::Math> {
    if let markdown_ast::Node::Math(math) = node {
        return Some(math);
    }
    node.children()?.iter().find_map(first_display_math)
}

fn count_nodes(node: &markdown_ast::Node, predicate: fn(&markdown_ast::Node) -> bool) -> usize {
    usize::from(predicate(node))
        + node.children().map_or(0, |children| {
            children
                .iter()
                .map(|child| count_nodes(child, predicate))
                .sum()
        })
}

mod recognition;
mod semantics;
mod source_context;
