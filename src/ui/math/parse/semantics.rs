//! Native-Markdown and reference-semantic equivalence checks.

use super::*;

#[derive(Default)]
pub(super) struct ParsedMathNodes {
    pub(super) pairs: Vec<(Range<usize>, Range<usize>)>,
    pub(super) pending_display: Vec<Range<usize>>,
}

pub(super) fn parsed_math_nodes(prepared: &[u8]) -> Option<ParsedMathNodes> {
    let source = std::str::from_utf8(prepared).ok()?;
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(source, &options).ok()?;

    fn collect(
        node: &markdown_ast::Node,
        source: &str,
        parsed: &mut ParsedMathNodes,
    ) -> Option<()> {
        if matches!(
            node,
            markdown_ast::Node::InlineMath(_) | markdown_ast::Node::Math(_)
        ) {
            let position = node.position()?;
            let range = position.start.offset..position.end.offset;
            if let Some(pair) = math_node_delimiter_pair(source, range.clone()) {
                parsed.pairs.push(pair);
            } else if matches!(node, markdown_ast::Node::Math(_))
                && source.get(range.clone())?.starts_with("$$")
            {
                parsed.pending_display.push(range);
            } else {
                return None;
            }
        }
        if let Some(children) = node.children() {
            for child in children {
                collect(child, source, parsed)?;
            }
        }
        Some(())
    }

    let mut parsed = ParsedMathNodes::default();
    collect(&root, source, &mut parsed)?;
    Some(parsed)
}

pub(super) fn math_node_delimiter_pair(
    source: &str,
    node_range: Range<usize>,
) -> Option<(Range<usize>, Range<usize>)> {
    source.get(node_range.clone())?;
    let opening_start = node_range
        .clone()
        .find(|offset| source.as_bytes()[*offset] == b'$' && !is_escaped(source, *offset))?;
    let mut opening_end = opening_start + 1;
    while opening_end < node_range.end && source.as_bytes()[opening_end] == b'$' {
        opening_end += 1;
    }

    let closing_end = node_range
        .clone()
        .rev()
        .find(|offset| source.as_bytes()[*offset] == b'$' && !is_escaped(source, *offset))?
        + 1;
    let mut closing_start = closing_end - 1;
    while closing_start > opening_end && source.as_bytes()[closing_start - 1] == b'$' {
        closing_start -= 1;
    }
    (opening_end <= closing_start)
        .then_some((opening_start..opening_end, closing_start..closing_end))
}

pub(super) fn reference_semantics_match(
    source: &str,
    prepared: &[u8],
    ranges: &MarkdownSourceRanges,
    visible_tokens: &[&MathToken],
    pending_display: Option<&Range<usize>>,
) -> bool {
    let active_formulas = active_formula_ranges(prepared, visible_tokens, pending_display);
    let protected_displays =
        active_protected_display_ranges(prepared, visible_tokens, pending_display);
    let expected = if protected_displays.is_empty() {
        ranges.reference_topology.clone()
    } else {
        native_baseline_root(source, &protected_displays)
            .and_then(|root| reference_resolution_topology_from_root(&root))
    };
    let actual = std::str::from_utf8(prepared)
        .ok()
        .and_then(prepared_reference_resolution_topology)
        .map(|topology| {
            topology
                .into_iter()
                .filter(|resolution| !reference_is_owned_by_formula(resolution, &active_formulas))
                .collect::<Vec<_>>()
        });
    actual == expected
}

pub(super) fn reference_is_owned_by_formula(
    resolution: &ReferenceResolution,
    active_formulas: &[Range<usize>],
) -> bool {
    range_is_covered(active_formulas, &resolution.source)
}

pub(super) fn native_semantics_match(
    source: &str,
    prepared: &[u8],
    ranges: &MarkdownSourceRanges,
    visible_tokens: &[&MathToken],
    pending_display: Option<&Range<usize>>,
) -> bool {
    let active_formulas = active_formula_ranges(prepared, visible_tokens, pending_display);
    let protected_displays =
        active_protected_display_ranges(prepared, visible_tokens, pending_display);
    let synthetic_inline_code = visible_tokens
        .iter()
        .filter(|token| {
            token.delimiter == MathDelimiter::Dollar
                && active_formulas.contains(&(token.start..token.end))
        })
        .map(|token| token.start..token.end)
        .collect::<Vec<_>>();
    let expected = if protected_displays.is_empty() {
        ranges.native_topology.clone()
    } else {
        native_baseline_root(source, &protected_displays)
            .and_then(|root| native_structure_topology_from_root(&root))
            .map(|topology| {
                normalize_native_topology_for_displays(
                    source,
                    topology,
                    &protected_displays,
                    pending_display,
                )
            })
    }
    .map(|topology| {
        topology
            .into_iter()
            .filter(|structure| !native_mark_is_owned_by_formula(structure, &active_formulas))
            .collect::<Vec<_>>()
    });
    let actual = std::str::from_utf8(prepared)
        .ok()
        .and_then(|prepared| prepared_native_structure_topology(prepared, &synthetic_inline_code))
        .map(|topology| {
            topology
                .into_iter()
                .filter(|structure| {
                    !active_formulas.iter().any(|formula| {
                        range_is_covered(std::slice::from_ref(formula), &structure.source)
                    })
                })
                .collect::<Vec<_>>()
        });
    let actual = actual.map(|topology| {
        normalize_native_topology_for_displays(
            source,
            topology,
            &protected_displays,
            pending_display,
        )
    });
    actual == expected
}

pub(super) fn native_mark_is_owned_by_formula(
    structure: &NativeStructure,
    active_formulas: &[Range<usize>],
) -> bool {
    matches!(
        structure.kind,
        NativeStructureKind::Strong | NativeStructureKind::Emphasis | NativeStructureKind::Delete
    ) && range_is_covered(active_formulas, &structure.source)
}

pub(super) fn normalize_native_topology_for_displays(
    source: &str,
    topology: Vec<NativeStructure>,
    protected_displays: &[Range<usize>],
    pending_display: Option<&Range<usize>>,
) -> Vec<NativeStructure> {
    // A flow-math construct interrupts a CommonMark list even when baseline
    // GFM would lazily keep the same source inside its preceding item. Ignore
    // only that container regrouping: retaining every item start still proves
    // item identity and order, while nested marks and links remain exact.
    topology
        .into_iter()
        .filter_map(|mut structure| {
            let precedes_display = protected_displays.iter().any(|display| {
                ranges_overlap(&structure.source, display)
                    || source
                        .get(structure.source.end..display.start)
                        .is_some_and(is_single_line_boundary)
            });
            match structure.kind {
                // A pending flow-math tail owns its source span inside the
                // quote. Its trailing newline can make markdown-rs extend the
                // enclosing Blockquote by one byte compared with the GFM
                // baseline where that tail is blanked. Compare the stable quote
                // prefix; nested native children remain checked independently.
                NativeStructureKind::Blockquote => {
                    if let Some(display_start) = pending_display
                        .filter(|display| ranges_overlap(&structure.source, display))
                        .map(|display| display.start)
                    {
                        structure.source.end = structure.source.end.min(display_start);
                    }
                    Some(structure)
                }
                NativeStructureKind::List
                    if precedes_display
                        || protected_displays.iter().any(|display| {
                            source
                                .get(display.end..structure.source.start)
                                .is_some_and(is_nearby_blank_boundary)
                        }) =>
                {
                    None
                }
                NativeStructureKind::ListItem if precedes_display => {
                    structure.source.end = structure.source.start;
                    Some(structure)
                }
                _ => Some(structure),
            }
        })
        .collect()
}

pub(super) fn is_single_line_boundary(source: &str) -> bool {
    let source = source
        .strip_suffix("\r\n")
        .or_else(|| source.strip_suffix('\n'));
    source.is_some_and(|source| source.bytes().all(|byte| matches!(byte, b' ' | b'\t')))
}

pub(super) fn is_nearby_blank_boundary(source: &str) -> bool {
    source.bytes().all(|byte| byte.is_ascii_whitespace())
        && source.bytes().filter(|byte| *byte == b'\n').count() <= 2
}

pub(super) fn active_protected_display_ranges(
    prepared: &[u8],
    visible_tokens: &[&MathToken],
    pending_display: Option<&Range<usize>>,
) -> Vec<Range<usize>> {
    let mut active = visible_tokens
        .iter()
        .filter(|token| {
            token.delimiter.is_display()
                && token.block_range.is_some()
                && prepared_token_is_active(prepared, token)
        })
        .map(|token| token.start..token.end)
        .collect::<Vec<_>>();
    if let (Some(pending), Some(parsed)) = (pending_display, parsed_math_nodes(prepared))
        && parsed.pending_display.as_slice() == std::slice::from_ref(pending)
    {
        active.push(pending.clone());
    }
    active
}

pub(super) fn native_baseline_root(
    source: &str,
    protected_displays: &[Range<usize>],
) -> Option<markdown_ast::Node> {
    let mut baseline = source.as_bytes().to_vec();
    for range in protected_displays {
        for byte in baseline.get_mut(range.clone())? {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    let baseline = String::from_utf8(baseline).ok()?;
    markdown::to_mdast(&baseline, &markdown::ParseOptions::gfm()).ok()
}

pub(super) fn active_formula_ranges(
    prepared: &[u8],
    visible_tokens: &[&MathToken],
    pending_display: Option<&Range<usize>>,
) -> Vec<Range<usize>> {
    let mut active = visible_tokens
        .iter()
        .filter(|token| prepared_token_is_active(prepared, token))
        .map(|token| token.start..token.end)
        .collect::<Vec<_>>();
    if let (Some(pending), Some(parsed)) = (pending_display, parsed_math_nodes(prepared))
        && parsed.pending_display.as_slice() == std::slice::from_ref(pending)
    {
        active.push(pending.clone());
    }
    active
}

pub(super) fn prepared_token_is_active(prepared: &[u8], token: &MathToken) -> bool {
    let (opening, closing) = token.delimiter_ranges();
    let expected = match token.delimiter {
        MathDelimiter::Dollar => PREPARED_INLINE_MATH_DELIMITER,
        MathDelimiter::Parenthesized
        | MathDelimiter::DisplayDollar
        | MathDelimiter::DisplayBracket => b'$',
    };
    let Some(opening) = prepared.get(opening) else {
        return false;
    };
    let Some(closing) = prepared.get(closing) else {
        return false;
    };
    !opening.is_empty()
        && !closing.is_empty()
        && opening.iter().chain(closing).all(|byte| *byte == expected)
}

/// Hide scanner control bytes outside native visible-text leaves before
/// pairing openers and closers. Offsets stay identical to the original source.
pub(super) fn delimiter_safe_scan_view(source: &str, ranges: &MarkdownSourceRanges) -> String {
    let mut view = source.as_bytes().to_vec();
    let mut ix = 0;
    while ix < source.len() {
        let delimiter_len = if source.as_bytes()[ix] == b'$' {
            Some(if exact_dollar_run(source, ix, 2) {
                2
            } else {
                1
            })
        } else if source.as_bytes()[ix] == b'\\'
            && matches!(
                source.as_bytes().get(ix + 1),
                Some(b'(' | b')' | b'[' | b']')
            )
        {
            Some(2)
        } else {
            None
        };

        if let Some(delimiter_len) = delimiter_len {
            let delimiter = ix..ix + delimiter_len;
            if !delimiter_is_visible(source, ranges, &delimiter) {
                view[delimiter.clone()].fill(SCAN_CONTROL_MASK);
            }
            ix += delimiter_len;
            continue;
        }

        // Baseline GFM has already classified real inline/fenced code as
        // opaque. Any remaining backtick or fence tilde is literal text and
        // must not open scanner-only code state. A backslash is retained only
        // in safe visible text, where it may be TeX or a real escape.
        match source.as_bytes()[ix] {
            b'`' | b'~' => view[ix] = SCAN_CONTROL_MASK,
            b'\\' if !range_is_covered(&ranges.preparable, &(ix..ix + 1)) => {
                view[ix] = SCAN_CONTROL_MASK;
            }
            _ => {}
        }
        ix += source[ix..].chars().next().map_or(1, char::len_utf8);
    }

    String::from_utf8(view).unwrap_or_else(|_| source.to_string())
}

pub(super) fn is_standalone_display_delimiter(
    source: &str,
    delimiter: &Range<usize>,
    ranges: &MarkdownSourceRanges,
) -> bool {
    if ranges
        .opaque
        .iter()
        .any(|range| ranges_overlap(range, delimiter))
    {
        return false;
    }

    let line_start = source[..delimiter.start]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let line_end = source[delimiter.end..]
        .find('\n')
        .map_or(source.len(), |offset| delimiter.end + offset);
    matches!(&source[delimiter.clone()], "$$" | r"\[" | r"\]")
        && source[delimiter.end..line_end].trim().is_empty()
        && is_markdown_container_prefix(&source[line_start..delimiter.start])
}

pub(super) fn is_markdown_container_prefix(mut prefix: &str) -> bool {
    loop {
        prefix = prefix.trim_start_matches([' ', '\t']);
        if prefix.is_empty() {
            return true;
        }

        if let Some(rest) = prefix.strip_prefix('>') {
            prefix = rest.strip_prefix(' ').unwrap_or(rest);
            continue;
        }

        let bytes = prefix.as_bytes();
        let marker_len = if matches!(bytes.first(), Some(b'-' | b'+' | b'*')) {
            1
        } else {
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 0 && matches!(bytes.get(digits), Some(b'.' | b')')) {
                digits + 1
            } else {
                0
            }
        };
        if marker_len == 0
            || !bytes
                .get(marker_len)
                .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            return false;
        }
        prefix = &prefix[marker_len + 1..];
    }
}
