//! Markdown AST source ownership, references, and visible-range projection.

use super::*;

pub(super) fn preparable_text_ranges(source: &str) -> MarkdownSourceRanges {
    preparable_text_ranges_from_view(source, source)
}

pub(super) fn preparable_text_ranges_from_view(
    source: &str,
    parse_view: &str,
) -> MarkdownSourceRanges {
    if source.len() != parse_view.len() {
        return MarkdownSourceRanges::default();
    }
    let Ok(root) = markdown::to_mdast(parse_view, &markdown::ParseOptions::gfm()) else {
        return MarkdownSourceRanges::default();
    };
    let mut ranges = MarkdownSourceRanges::default();
    collect_preparable_text(&root, source, &mut ranges);
    ranges.reference_topology = reference_resolution_topology_from_root(&root);
    ranges.native_topology = native_structure_topology_from_root(&root);
    ranges
}

pub(super) fn prepared_reference_resolution_topology(
    source: &str,
) -> Option<Vec<ReferenceResolution>> {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(source, &options).ok()?;
    reference_resolution_topology_from_root(&root)
}

pub(super) fn reference_resolution_topology_from_root(
    root: &markdown_ast::Node,
) -> Option<Vec<ReferenceResolution>> {
    fn collect_definitions(
        node: &markdown_ast::Node,
        definitions: &mut HashMap<String, ReferenceDestination>,
    ) {
        if let markdown_ast::Node::Definition(definition) = node {
            definitions
                .entry(definition.identifier.clone())
                .or_insert_with(|| ReferenceDestination {
                    url: definition.url.clone(),
                    title: definition.title.clone(),
                });
        }
        if let Some(children) = node.children() {
            for child in children {
                collect_definitions(child, definitions);
            }
        }
    }

    fn collect(
        node: &markdown_ast::Node,
        definitions: &HashMap<String, ReferenceDestination>,
        resolutions: &mut Vec<ReferenceResolution>,
    ) -> Option<()> {
        let resolution = match node {
            markdown_ast::Node::LinkReference(reference) => Some((
                match reference.reference_kind {
                    markdown_ast::ReferenceKind::Full => ReferenceResolutionKind::LinkFull,
                    markdown_ast::ReferenceKind::Collapsed => {
                        ReferenceResolutionKind::LinkCollapsed
                    }
                    markdown_ast::ReferenceKind::Shortcut => ReferenceResolutionKind::LinkShortcut,
                },
                definitions.get(&reference.identifier).cloned(),
            )),
            markdown_ast::Node::ImageReference(reference) => Some((
                match reference.reference_kind {
                    markdown_ast::ReferenceKind::Full => ReferenceResolutionKind::ImageFull,
                    markdown_ast::ReferenceKind::Collapsed => {
                        ReferenceResolutionKind::ImageCollapsed
                    }
                    markdown_ast::ReferenceKind::Shortcut => ReferenceResolutionKind::ImageShortcut,
                },
                definitions.get(&reference.identifier).cloned(),
            )),
            _ => None,
        };
        if let Some((kind, destination)) = resolution {
            let position = node.position()?;
            resolutions.push(ReferenceResolution {
                source: position.start.offset..position.end.offset,
                kind,
                destination,
            });
        }
        if let Some(children) = node.children() {
            for child in children {
                collect(child, definitions, resolutions)?;
            }
        }
        Some(())
    }

    let mut definitions = HashMap::new();
    collect_definitions(root, &mut definitions);
    let mut resolutions = Vec::new();
    collect(root, &definitions, &mut resolutions)?;
    Some(resolutions)
}

pub(super) fn prepared_native_structure_topology(
    source: &str,
    synthetic_inline_code: &[Range<usize>],
) -> Option<Vec<NativeStructure>> {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(source, &options).ok()?;
    let mut topology = native_structure_topology_from_root(&root)?;
    topology.retain(|structure| {
        structure.kind != NativeStructureKind::InlineCode
            || !synthetic_inline_code.contains(&structure.source)
    });
    Some(topology)
}

pub(super) fn native_structure_topology_from_root(
    root: &markdown_ast::Node,
) -> Option<Vec<NativeStructure>> {
    fn collect(node: &markdown_ast::Node, structures: &mut Vec<NativeStructure>) -> Option<()> {
        let kind = match node {
            markdown_ast::Node::Blockquote(_) => Some(NativeStructureKind::Blockquote),
            markdown_ast::Node::List(_) => Some(NativeStructureKind::List),
            markdown_ast::Node::ListItem(_) => Some(NativeStructureKind::ListItem),
            markdown_ast::Node::Heading(_) => Some(NativeStructureKind::Heading),
            markdown_ast::Node::Strong(_) => Some(NativeStructureKind::Strong),
            markdown_ast::Node::Emphasis(_) => Some(NativeStructureKind::Emphasis),
            markdown_ast::Node::Delete(_) => Some(NativeStructureKind::Delete),
            markdown_ast::Node::Link(link) => Some(NativeStructureKind::Link(
                link.url.clone(),
                link.title.clone(),
            )),
            markdown_ast::Node::Image(image) => Some(NativeStructureKind::Image(
                image.url.clone(),
                image.title.clone(),
            )),
            markdown_ast::Node::LinkReference(_) => Some(NativeStructureKind::LinkReference),
            markdown_ast::Node::ImageReference(_) => Some(NativeStructureKind::ImageReference),
            markdown_ast::Node::Definition(definition) => Some(NativeStructureKind::Definition(
                definition.url.clone(),
                definition.title.clone(),
            )),
            markdown_ast::Node::FootnoteDefinition(_) => {
                Some(NativeStructureKind::FootnoteDefinition)
            }
            markdown_ast::Node::FootnoteReference(_) => {
                Some(NativeStructureKind::FootnoteReference)
            }
            markdown_ast::Node::InlineCode(_) => Some(NativeStructureKind::InlineCode),
            markdown_ast::Node::Code(_) => Some(NativeStructureKind::Code),
            markdown_ast::Node::Break(_) => Some(NativeStructureKind::Break),
            markdown_ast::Node::Html(_) => Some(NativeStructureKind::Html),
            markdown_ast::Node::Table(_) => Some(NativeStructureKind::Table),
            markdown_ast::Node::TableRow(_) => Some(NativeStructureKind::TableRow),
            markdown_ast::Node::TableCell(_) => Some(NativeStructureKind::TableCell),
            _ => None,
        };
        if let Some(kind) = kind {
            let position = node.position()?;
            structures.push(NativeStructure {
                source: position.start.offset..position.end.offset,
                kind,
            });
        }
        if let Some(children) = node.children() {
            for child in children {
                collect(child, structures)?;
            }
        }
        Some(())
    }

    let mut structures = Vec::new();
    collect(root, &mut structures)?;
    Some(structures)
}

pub(super) fn record_reference_association(
    node: &markdown_ast::Node,
    source: &str,
    identifier: &str,
    label_index: usize,
    role: ReferenceAssociationRole,
    ranges: &mut MarkdownSourceRanges,
) {
    let Some(label) = reference_label_ranges(node, source)
        .into_iter()
        .nth(label_index)
    else {
        ranges
            .unsafe_reference_identifiers
            .insert(identifier.to_string());
        return;
    };
    ranges.reference_associations.push(ReferenceAssociation {
        identifier: identifier.to_string(),
        source: label,
        role,
    });
}

pub(super) fn reference_label_ranges(node: &markdown_ast::Node, source: &str) -> Vec<Range<usize>> {
    let Some(position) = node.position() else {
        return Vec::new();
    };
    let node_range = position.start.offset..position.end.offset;
    if source.get(node_range.clone()).is_none() {
        return Vec::new();
    }

    let mut labels = Vec::new();
    let mut depth = 0usize;
    let mut label_start = None;
    for offset in node_range {
        match source.as_bytes()[offset] {
            b'[' if !is_escaped(source, offset) => {
                if depth == 0 {
                    label_start = Some(offset + 1);
                }
                depth += 1;
            }
            b']' if depth > 0 && !is_escaped(source, offset) => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = label_start.take() {
                        labels.push(start..offset);
                    }
                }
            }
            _ => {}
        }
    }
    labels
}

pub(super) fn record_link_reference(
    node: &markdown_ast::Node,
    link: &markdown_ast::LinkReference,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    let (label_index, role) = match link.reference_kind {
        markdown_ast::ReferenceKind::Full => (1, ReferenceAssociationRole::Opaque),
        markdown_ast::ReferenceKind::Collapsed | markdown_ast::ReferenceKind::Shortcut => {
            (0, ReferenceAssociationRole::Implicit)
        }
    };
    record_reference_association(node, source, &link.identifier, label_index, role, ranges);
}

pub(super) fn record_image_reference(
    node: &markdown_ast::Node,
    image: &markdown_ast::ImageReference,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    let (label_index, role) = match image.reference_kind {
        markdown_ast::ReferenceKind::Full => (1, ReferenceAssociationRole::Opaque),
        markdown_ast::ReferenceKind::Collapsed | markdown_ast::ReferenceKind::Shortcut => {
            (0, ReferenceAssociationRole::Implicit)
        }
    };
    record_reference_association(node, source, &image.identifier, label_index, role, ranges);
}

pub(super) fn record_authoritative_image_alt(
    node: &markdown_ast::Node,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    if let Some(alt) = reference_label_ranges(node, source).into_iter().next() {
        ranges.authoritative_image_alt.push(alt);
    }
    mark_opaque(node, ranges);
}

pub(super) fn collect_preparable_text(
    node: &markdown_ast::Node,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    match node {
        markdown_ast::Node::Text(text) => collect_text_range(text, source, ranges, true),
        markdown_ast::Node::Paragraph(paragraph) => {
            collect_inline_root(&paragraph.children, source, ranges)
        }
        markdown_ast::Node::Heading(heading) => {
            collect_inline_root(&heading.children, source, ranges)
        }
        markdown_ast::Node::Delete(delete) => collect_inline_root(&delete.children, source, ranges),
        markdown_ast::Node::Emphasis(emphasis) => {
            collect_inline_root(&emphasis.children, source, ranges)
        }
        markdown_ast::Node::Strong(strong) => collect_inline_root(&strong.children, source, ranges),
        markdown_ast::Node::Link(link) if is_explicit_link(link, source) => {
            collect_inline_root(&link.children, source, ranges)
        }
        markdown_ast::Node::LinkReference(link) => {
            record_link_reference(node, link, source, ranges);
            collect_inline_root(&link.children, source, ranges)
        }
        markdown_ast::Node::TableCell(cell) => collect_inline_root(&cell.children, source, ranges),
        markdown_ast::Node::ImageReference(image) => {
            record_image_reference(node, image, source, ranges);
            record_authoritative_image_alt(node, source, ranges);
        }
        markdown_ast::Node::Image(_) => record_authoritative_image_alt(node, source, ranges),
        markdown_ast::Node::Definition(definition) => {
            record_reference_association(
                node,
                source,
                &definition.identifier,
                0,
                ReferenceAssociationRole::Definition,
                ranges,
            );
            mark_opaque(node, ranges);
        }
        // These nodes either own opaque syntax/destinations or render through
        // a structure where Nostra intentionally leaves math-looking text to
        // gpui-component's native path.
        markdown_ast::Node::Code(_)
        | markdown_ast::Node::InlineCode(_)
        | markdown_ast::Node::Html(_)
        | markdown_ast::Node::Link(_)
        | markdown_ast::Node::FootnoteReference(_)
        | markdown_ast::Node::MdxJsxFlowElement(_)
        | markdown_ast::Node::MdxJsxTextElement(_)
        | markdown_ast::Node::MdxFlowExpression(_)
        | markdown_ast::Node::MdxTextExpression(_)
        | markdown_ast::Node::MdxjsEsm(_) => {
            mark_opaque(node, ranges);
        }
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    collect_preparable_text(child, source, ranges);
                }
            }
        }
    }
}

pub(super) fn collect_inline_root(
    children: &[markdown_ast::Node],
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    let mut open_html_elements = Vec::new();
    collect_inline_children(children, source, ranges, &mut open_html_elements);
}

pub(super) fn collect_inline_children(
    children: &[markdown_ast::Node],
    source: &str,
    ranges: &mut MarkdownSourceRanges,
    open_html_elements: &mut Vec<String>,
) {
    for child in children {
        collect_inline_node(child, source, ranges, open_html_elements);
    }
}

pub(super) fn collect_inline_node(
    node: &markdown_ast::Node,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
    open_html_elements: &mut Vec<String>,
) {
    match node {
        markdown_ast::Node::Html(html) => {
            mark_opaque(node, ranges);
            update_html_context(&html.value, open_html_elements);
        }
        markdown_ast::Node::Text(text) => {
            collect_text_range(text, source, ranges, open_html_elements.is_empty())
        }
        // A link label is visible inline content, but an autolink's child is
        // also its destination. Only recurse into the explicit `[label](url)`
        // form so preparation can never rewrite a URL.
        markdown_ast::Node::Link(link) if is_explicit_link(link, source) => {
            collect_inline_children(&link.children, source, ranges, open_html_elements);
        }
        markdown_ast::Node::LinkReference(link) => {
            record_link_reference(node, link, source, ranges);
            collect_inline_children(&link.children, source, ranges, open_html_elements);
        }
        markdown_ast::Node::ImageReference(image) => {
            record_image_reference(node, image, source, ranges);
            record_authoritative_image_alt(node, source, ranges);
        }
        markdown_ast::Node::Image(_) => record_authoritative_image_alt(node, source, ranges),
        markdown_ast::Node::Delete(delete) => {
            collect_inline_children(&delete.children, source, ranges, open_html_elements);
        }
        markdown_ast::Node::Emphasis(emphasis) => {
            collect_inline_children(&emphasis.children, source, ranges, open_html_elements);
        }
        markdown_ast::Node::Strong(strong) => {
            collect_inline_children(&strong.children, source, ranges, open_html_elements);
        }
        markdown_ast::Node::Break(_) => {}
        _ => mark_opaque(node, ranges),
    }
}

pub(super) fn collect_text_range(
    text: &markdown_ast::Text,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
    preparable: bool,
) {
    let Some(position) = text.position.as_ref() else {
        return;
    };
    let range = position.start.offset..position.end.offset;
    if source.get(range.clone()).is_none() {
        return;
    }
    ranges.restorable.push(range.clone());
    if !preparable {
        ranges.opaque.push(range);
        return;
    }

    // At the start of a text leaf CommonMark can exclude the escape
    // backslash from the mdast position (`\(` becomes visible `(x)`). Include
    // that byte in the scan-only range so Nostra can still recognize its own
    // delimiter without broadening the node's restorable source span.
    let preparable_start = range
        .start
        .checked_sub(1)
        .filter(|start| source.as_bytes()[*start] == b'\\' && !is_escaped(source, *start))
        .unwrap_or(range.start);
    ranges.preparable.push(preparable_start..range.end);
}

pub(super) fn is_explicit_link(link: &markdown_ast::Link, source: &str) -> bool {
    let Some(position) = link.position.as_ref() else {
        return false;
    };
    source
        .get(position.start.offset..position.end.offset)
        .is_some_and(|source| source.trim_start().starts_with('['))
}

pub(super) fn mark_opaque(node: &markdown_ast::Node, ranges: &mut MarkdownSourceRanges) {
    if let Some(position) = node.position() {
        ranges
            .opaque
            .push(position.start.offset..position.end.offset);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum HtmlTag {
    Open(String),
    Close(String),
    Neutral,
}

pub(super) fn update_html_context(source: &str, open_elements: &mut Vec<String>) {
    match classify_html_tag(source) {
        HtmlTag::Open(name) => open_elements.push(name),
        HtmlTag::Close(name) => {
            if let Some(index) = open_elements.iter().rposition(|open| open == &name) {
                open_elements.truncate(index);
            }
        }
        HtmlTag::Neutral => {}
    }
}

pub(super) fn classify_html_tag(source: &str) -> HtmlTag {
    let source = source.trim();
    let bytes = source.as_bytes();
    if bytes.first() != Some(&b'<') || html_tag_end(source) != Some(source.len() - 1) {
        return HtmlTag::Neutral;
    }

    let mut inner = source[1..source.len() - 1].trim();
    if inner.starts_with(['!', '?']) {
        return HtmlTag::Neutral;
    }

    let closing = inner.starts_with('/');
    if closing {
        inner = inner[1..].trim_start();
    }
    let name_len = inner
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b':' | b'_'))
        .count();
    if name_len == 0 {
        return HtmlTag::Neutral;
    }
    let name = inner[..name_len].to_ascii_lowercase();
    if closing {
        return HtmlTag::Close(name);
    }

    let self_closing = inner[name_len..].trim_end().ends_with('/');
    if self_closing || is_void_html_element(&name) {
        HtmlTag::Neutral
    } else {
        HtmlTag::Open(name)
    }
}

pub(super) fn html_tag_end(source: &str) -> Option<usize> {
    let mut quote = None;
    for (offset, character) in source.char_indices().skip(1) {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => return Some(offset),
            _ => {}
        }
    }
    None
}

pub(super) fn is_void_html_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

pub(super) fn range_is_covered(ranges: &[Range<usize>], candidate: &Range<usize>) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= candidate.start && range.end >= candidate.end)
}

pub(super) fn range_is_covered_by_union(
    ranges: impl IntoIterator<Item = Range<usize>>,
    candidate: &Range<usize>,
) -> bool {
    let mut ranges = ranges
        .into_iter()
        .filter(|range| ranges_overlap(range, candidate))
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| (range.start, range.end));

    let mut covered_until = candidate.start;
    for range in ranges {
        if range.end <= covered_until {
            continue;
        }
        if range.start > covered_until {
            return false;
        }
        covered_until = range.end;
        if covered_until >= candidate.end {
            return true;
        }
    }
    candidate.is_empty()
}

pub(super) fn inline_token_crosses_only_owned_marks(
    ranges: &MarkdownSourceRanges,
    token: &Range<usize>,
) -> bool {
    let Some(topology) = ranges.native_topology.as_ref() else {
        return false;
    };
    let mut coverage = ranges.preparable.clone();

    for structure in topology
        .iter()
        .filter(|structure| ranges_overlap(&structure.source, token))
    {
        let structure_contains_token =
            structure.source.start <= token.start && structure.source.end >= token.end;
        if structure_contains_token {
            continue;
        }

        let token_contains_structure =
            token.start <= structure.source.start && token.end >= structure.source.end;
        if token_contains_structure
            && matches!(
                structure.kind,
                NativeStructureKind::Strong
                    | NativeStructureKind::Emphasis
                    | NativeStructureKind::Delete
            )
        {
            coverage.push(structure.source.clone());
            continue;
        }

        return false;
    }

    range_is_covered_by_union(coverage, token)
}

pub(super) fn delimiter_is_visible(
    source: &str,
    ranges: &MarkdownSourceRanges,
    delimiter: &Range<usize>,
) -> bool {
    range_is_covered(&ranges.preparable, delimiter)
        || is_standalone_display_delimiter(source, delimiter, ranges)
}

pub(super) fn math_token_is_visible(
    source: &str,
    ranges: &MarkdownSourceRanges,
    token: &MathToken,
) -> bool {
    let (opening, closing) = token.delimiter_ranges();
    // An inline formula must stay inside one native Text leaf. Merely finding
    // visible delimiters on both sides is insufficient: they may otherwise
    // pair across a link destination, inline code, a hard break, or another
    // Markdown node and swallow its native semantics.
    let inline_token_is_visible = ranges
        .preparable
        .iter()
        .any(|range| range.start <= token.start && range.end >= token.end);
    // GFM can interpret TeX control bytes such as `_` as native marks before
    // Nostra gets a chance to claim a scanner-approved formula. Permit that
    // formula to span only complete mark nodes that it wholly owns. Native
    // containers around the formula remain intact, while links, code, HTML,
    // images, breaks, and partially crossing marks remain hard boundaries.
    let inline_token_owns_marks = !source[token.start..token.end].contains(['\n', '\r'])
        && range_is_covered(&ranges.preparable, &opening)
        && range_is_covered(&ranges.preparable, &closing)
        && inline_token_crosses_only_owned_marks(ranges, &(token.start..token.end));
    // Root and container display blocks span multiple native leaves by design.
    // Keep that established path only when both delimiters occupy standalone
    // container-aware lines and no opaque node crosses the formula.
    let standalone_display_is_visible = token.delimiter.is_display()
        && is_standalone_display_delimiter(source, &opening, ranges)
        && is_standalone_display_delimiter(source, &closing, ranges);
    inline_token_is_visible || inline_token_owns_marks || standalone_display_is_visible
}

pub(super) fn pending_display_is_visible(
    source: &str,
    ranges: &MarkdownSourceRanges,
    pending: &Range<usize>,
) -> bool {
    let opening = pending.start..pending.start.saturating_add(2);
    if pending.end != source.len() || source.get(opening.clone()) != Some("$$") {
        return false;
    }

    let inline_tail_is_visible = ranges
        .preparable
        .iter()
        .any(|range| range.start <= pending.start && range.end >= pending.end);
    // Once an explicit standalone opener is visible, Markdown-looking body
    // text belongs to the pending formula. Baseline GFM may classify links,
    // definitions, code, or HTML inside that tail as opaque; using those body
    // classifications to reject the opener would recreate the very semantic
    // leakage this boundary prevents.
    let standalone_tail_is_visible = is_standalone_display_delimiter(source, &opening, ranges);
    inline_tail_is_visible || standalone_tail_is_visible
}

pub(super) fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}
