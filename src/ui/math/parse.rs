//! Pure Markdown math recognition and AST-to-render-segment conversion.
//!
//! This module deliberately has no GPUI window or application dependencies.
//! Keeping delimiter policy and source-offset mapping here makes the parser
//! deterministic and lets regressions be covered without constructing a UI.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ops::Range,
};

use gpui_component::text::markdown_ast;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MathSegment {
    Text {
        text: String,
        marks: Vec<MathMarkRange>,
    },
    Image {
        url: String,
        title: String,
        link: Option<String>,
    },
    Formula {
        source: String,
        plain_text: String,
        start: usize,
        display: bool,
        marks: MathMarks,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MathMarkRange {
    pub(super) range: Range<usize>,
    pub(super) marks: MathMarks,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MathMarks {
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) strikethrough: bool,
    pub(super) code: bool,
    pub(super) link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MathBlockSegment {
    Inline {
        segments: Vec<MathSegment>,
        start: usize,
        breaks_before: usize,
    },
    Formula {
        source: String,
        plain_text: String,
        start: usize,
        marks: MathMarks,
        breaks_before: usize,
    },
}

impl MathBlockSegment {
    pub(super) fn breaks_before(&self) -> usize {
        match self {
            Self::Inline { breaks_before, .. } | Self::Formula { breaks_before, .. } => {
                *breaks_before
            }
        }
    }
}

/// Build a CommonMark-safe parsing view of complete display-math fences.
///
/// `gpui-component` currently parses Markdown with `math_flow` disabled and
/// only offers plugins after mdast construction. Consequently, a TeX line such
/// as a standalone `=` is interpreted as a setext heading underline before the
/// math plugin can see the fence; blank lines and other block constructs can
/// split it for the same reason. Masking ASCII bytes inside each complete body
/// makes the whole fence one ordinary paragraph for mdast purposes.
///
/// This transformation is deliberately length-preserving. Delimiters and all
/// non-ASCII bytes stay untouched, while ASCII body bytes (including newlines)
/// become spaces. The plugin uses the resulting node positions to slice a
/// separately retained copy of the original Markdown, so RaTeX receives the
/// exact TeX source and every document-space cache key remains stable. An
/// incomplete streaming fence is left unchanged until its closing delimiter
/// arrives, preventing partial output from swallowing later prose.
pub(super) fn protect_display_math_for_markdown(source: &str) -> Cow<'_, str> {
    let mut candidates = scan_math(source)
        .tokens
        .into_iter()
        // Inline display delimiters are already ordinary paragraph text to the
        // document parser. Only standalone blocks need protection against
        // setext headings, blank lines, and other block constructs.
        .filter(|token| token.delimiter.is_display() && token.block_range.is_some())
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Cow::Borrowed(source);
    }

    // Protection is safe only when the resulting block is one the math plugin
    // can replace losslessly. A table cell bypasses block plugins, while a
    // paragraph containing a reference link, raw HTML, or a future unknown
    // inline node is deliberately left to the native renderer. Iteratively
    // remove unsafe masks because restoring one token can change the block
    // grouping of another token nearby.
    loop {
        let Some(protected) = mask_display_math_bodies(source, &candidates) else {
            return Cow::Borrowed(source);
        };
        let Ok(root) = markdown::to_mdast(&protected, &markdown::ParseOptions::gfm()) else {
            return Cow::Borrowed(source);
        };
        let retained = candidates
            .iter()
            .filter(|token| claimable_math_block(&root, token.start..token.end))
            .cloned()
            .collect::<Vec<_>>();

        if retained.len() == candidates.len() {
            return Cow::Owned(protected);
        }
        if retained.is_empty() {
            return Cow::Borrowed(source);
        }
        candidates = retained;
    }
}

fn mask_display_math_bodies(source: &str, tokens: &[MathToken]) -> Option<String> {
    let mut protected = source.as_bytes().to_vec();
    for token in tokens {
        for byte in &mut protected[token.body.clone()] {
            if byte.is_ascii() {
                *byte = b' ';
            }
        }
    }

    // Replacing only individual ASCII bytes cannot invalidate an existing
    // UTF-8 sequence. Keep a graceful fallback nevertheless because this is a
    // user-facing parsing path and must not rely on an `unwrap` invariant.
    String::from_utf8(protected).ok()
}

fn claimable_math_block(node: &markdown_ast::Node, token: Range<usize>) -> bool {
    let contains_token = |node: &markdown_ast::Node| {
        node.position().is_some_and(|position| {
            position.start.offset <= token.start && position.end.offset >= token.end
        })
    };

    match node {
        markdown_ast::Node::Paragraph(_) | markdown_ast::Node::Heading(_) => {
            contains_token(node) && supports_inline_math_node(node)
        }
        markdown_ast::Node::Root(root) => root
            .children
            .iter()
            .any(|child| claimable_math_block(child, token.clone())),
        markdown_ast::Node::Blockquote(blockquote) => blockquote
            .children
            .iter()
            .any(|child| claimable_math_block(child, token.clone())),
        markdown_ast::Node::List(list) => list
            .children
            .iter()
            .any(|child| claimable_math_block(child, token.clone())),
        markdown_ast::Node::ListItem(item) => item
            .children
            .iter()
            .any(|child| claimable_math_block(child, token.clone())),
        // gpui-component parses table cells and footnote bodies directly as
        // inline content, without invoking block plugins for their children.
        _ => false,
    }
}

#[derive(Clone, Copy)]
struct CodeFence {
    marker: u8,
    length: usize,
}

/// Update CommonMark fenced-code state and report whether `line` is itself a
/// fence boundary. Math recognition happens before mdast conversion, so it
/// must independently respect this one block construct or a literal `$$`
/// example inside code would be rewritten before the code parser sees it.
fn update_code_fence(line: &str, state: &mut Option<CodeFence>) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    let indentation = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indentation > 3 {
        return false;
    }
    let rest = &line.as_bytes()[indentation..];
    let Some(&marker) = rest.first().filter(|marker| matches!(marker, b'`' | b'~')) else {
        return false;
    };
    let length = rest.iter().take_while(|byte| **byte == marker).count();
    if length < 3 {
        return false;
    }

    match *state {
        None => {
            // Backtick info strings cannot contain a backtick. Tilde fences do
            // not have the corresponding restriction in CommonMark.
            if marker == b'`' && rest[length..].contains(&b'`') {
                return false;
            }
            *state = Some(CodeFence { marker, length });
            true
        }
        Some(open) if open.marker == marker && length >= open.length => {
            // A closing fence permits only trailing whitespace.
            if rest[length..].iter().all(|byte| byte.is_ascii_whitespace()) {
                *state = None;
                true
            } else {
                false
            }
        }
        Some(_) => false,
    }
}

fn line_indentation(line: &str) -> usize {
    let spaces = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    // A leading tab advances to at least the four-column code indentation and
    // can never be part of a valid display-math delimiter line. Returning the
    // sentinel keeps callers' `<= 3` checks explicit and short-circuits before
    // any slice uses it as a byte offset.
    if line.as_bytes().get(spaces) == Some(&b'\t') {
        usize::MAX
    } else {
        spaces
    }
}

pub(super) fn block_math_segments(source: &str, start: usize) -> Option<Vec<MathBlockSegment>> {
    let scan = scan_math(source);
    let standalone_ranges = scan
        .tokens
        .iter()
        .filter_map(|token| {
            token
                .block_range
                .clone()
                .map(|block_range| (token.start, block_range))
        })
        .collect::<HashMap<_, _>>();
    if standalone_ranges.is_empty() {
        return None;
    }

    let parsed = parse_math_segments(source, &scan)?;
    let mut segments = Vec::new();
    let mut inline = Vec::new();
    let mut inline_start = start;
    let mut inline_breaks_before = 0;
    let mut trim_inline_start = false;
    let mut pending_hard_breaks = 0;

    for mut segment in parsed {
        let standalone_range = match &segment {
            MathSegment::Formula { start, .. } => standalone_ranges.get(start).cloned(),
            MathSegment::Text { .. } | MathSegment::Image { .. } => None,
        };
        let Some(block_range) = standalone_range else {
            if trim_inline_start {
                let (visible, hard_breaks) = trim_segment_start(&mut segment);
                pending_hard_breaks += hard_breaks;
                if !visible {
                    continue;
                }
                inline_breaks_before = if segments.is_empty() {
                    0
                } else {
                    pending_hard_breaks.max(1)
                };
                pending_hard_breaks = 0;
                trim_inline_start = false;
            }
            inline.push(offset_math_segment(segment, start));
            continue;
        };

        let trailing_hard_breaks = trim_inline_end(&mut inline);
        push_inline_block_segment(
            &mut segments,
            &mut inline,
            inline_start,
            inline_breaks_before,
        );
        let breaks_before = if segments.is_empty() {
            0
        } else {
            (pending_hard_breaks + trailing_hard_breaks).max(1)
        };
        let MathSegment::Formula {
            source,
            plain_text,
            start: formula_start,
            marks,
            ..
        } = segment
        else {
            unreachable!();
        };
        let formula_end = formula_start + plain_text.len();
        segments.push(MathBlockSegment::Formula {
            source,
            plain_text,
            start: start + formula_start,
            marks,
            breaks_before,
        });
        debug_assert!(block_range.end >= formula_end);
        inline_start = start + block_range.end;
        inline_breaks_before = 0;
        trim_inline_start = true;
        pending_hard_breaks = 0;
    }
    push_inline_block_segment(
        &mut segments,
        &mut inline,
        inline_start,
        inline_breaks_before,
    );
    Some(segments)
}

fn trim_segment_start(segment: &mut MathSegment) -> (bool, usize) {
    let MathSegment::Text { text, marks } = segment else {
        return (true, 0);
    };
    let mut whitespace_end = 0;
    let mut hard_breaks = 0;
    for (byte_ix, byte) in text.bytes().enumerate() {
        if !byte.is_ascii_whitespace() || is_code_marked(marks, byte_ix) {
            break;
        }
        whitespace_end = byte_ix + 1;
        if byte == b'\n' {
            hard_breaks += 1;
        }
    }
    let removed = whitespace_end;
    if removed == 0 {
        return (!text.is_empty(), 0);
    }

    text.drain(..removed);
    let new_len = text.len();
    marks.retain_mut(|mark| {
        mark.range.start = mark.range.start.saturating_sub(removed).min(new_len);
        mark.range.end = mark.range.end.saturating_sub(removed).min(new_len);
        mark.range.start < mark.range.end
    });
    (!text.is_empty(), hard_breaks)
}

fn trim_inline_end(inline: &mut Vec<MathSegment>) -> usize {
    let mut hard_breaks = 0;
    while let Some(segment) = inline.last_mut() {
        let MathSegment::Text { text, marks } = segment else {
            break;
        };
        let mut whitespace_start = text.len();
        for (byte_ix, byte) in text.bytes().enumerate().rev() {
            if !byte.is_ascii_whitespace() || is_code_marked(marks, byte_ix) {
                break;
            }
            whitespace_start = byte_ix;
            if byte == b'\n' {
                hard_breaks += 1;
            }
        }
        let new_len = whitespace_start;
        text.truncate(new_len);
        marks.retain_mut(|mark| {
            mark.range.start = mark.range.start.min(new_len);
            mark.range.end = mark.range.end.min(new_len);
            mark.range.start < mark.range.end
        });
        if text.is_empty() {
            inline.pop();
        } else {
            break;
        }
    }
    hard_breaks
}

fn is_code_marked(marks: &[MathMarkRange], byte_ix: usize) -> bool {
    marks
        .iter()
        .any(|mark| mark.marks.code && mark.range.contains(&byte_ix))
}

fn push_inline_block_segment(
    segments: &mut Vec<MathBlockSegment>,
    inline: &mut Vec<MathSegment>,
    start: usize,
    breaks_before: usize,
) {
    let visible = inline.iter().any(|segment| match segment {
        MathSegment::Text { text, marks } => {
            !text.trim().is_empty()
                || marks
                    .iter()
                    .any(|mark| mark.marks.code && !mark.range.is_empty())
        }
        MathSegment::Image { .. } | MathSegment::Formula { .. } => true,
    });
    if visible {
        segments.push(MathBlockSegment::Inline {
            segments: std::mem::take(inline),
            start,
            breaks_before,
        });
    } else {
        inline.clear();
    }
}

fn offset_math_segment(segment: MathSegment, offset: usize) -> MathSegment {
    match segment {
        MathSegment::Formula {
            source,
            plain_text,
            start,
            display,
            marks,
        } => MathSegment::Formula {
            source,
            plain_text,
            start: offset + start,
            display,
            marks,
        },
        MathSegment::Text { text, marks } => MathSegment::Text { text, marks },
        MathSegment::Image { url, title, link } => MathSegment::Image { url, title, link },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MathDelimiter {
    Dollar,
    Parenthesized,
    DisplayDollar,
    DisplayBracket,
}

impl MathDelimiter {
    fn opening_len(self) -> usize {
        match self {
            Self::Dollar => 1,
            Self::Parenthesized | Self::DisplayDollar | Self::DisplayBracket => 2,
        }
    }

    fn closing_len(self) -> usize {
        self.opening_len()
    }

    fn is_display(self) -> bool {
        matches!(self, Self::DisplayDollar | Self::DisplayBracket)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MathToken {
    delimiter: MathDelimiter,
    start: usize,
    end: usize,
    body: Range<usize>,
    block_range: Option<Range<usize>>,
}

#[derive(Default)]
struct MathScan {
    tokens: Vec<MathToken>,
    escapable_dollars: Vec<usize>,
    empty_display_blocks: Vec<EmptyDisplayBlock>,
}

struct EmptyDisplayBlock {
    token_range: Range<usize>,
    block_range: Range<usize>,
}

/// Markdown prepared for a single AST pass with Nostra's math delimiter policy.
///
/// `markdown-rs` deliberately accepts every matching dollar run, including
/// currency. The product syntax is narrower, so the scanner first selects the
/// formulas we intend to render. Accepted `$...$` and `\(...\)` delimiters are
/// rewritten to `$$...$$`; all other unescaped dollars are escaped. The offset
/// table maps positions in that transformed source back to the user's original
/// Markdown, including the extra bytes inserted around single-dollar formulas.
struct PreparedInlineMath {
    source: String,
    original_offsets: Vec<usize>,
    formulas: HashMap<usize, PreparedFormula>,
}

struct PreparedFormula {
    source: String,
    display: bool,
    plain_text: String,
}

impl PreparedInlineMath {
    fn original_offset(&self, transformed_offset: usize) -> Option<usize> {
        self.original_offsets.get(transformed_offset).copied()
    }

    fn formula(&self, original_start: usize) -> Option<&PreparedFormula> {
        self.formulas.get(&original_start)
    }
}

struct RichText {
    text: String,
    marks: Vec<MathMarkRange>,
}

pub(super) fn inline_math_segments(source: &str) -> Option<Vec<MathSegment>> {
    let scan = scan_math(source);
    parse_math_segments(source, &scan)
}

fn parse_math_segments(source: &str, scan: &MathScan) -> Option<Vec<MathSegment>> {
    if scan.tokens.is_empty() {
        return None;
    }
    let prepared = prepare_inline_math(source, scan);
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_flow = false;
    options.constructs.math_text = true;
    // Every accepted formula is normalized to a two-dollar delimiter. Turning
    // off single-dollar parsing is the final guard that prevents a currency
    // dollar missed by future scanner changes from pairing with later text.
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(&prepared.source, &options).ok()?;

    let mut collector = MathSegmentCollector::new(&prepared);
    collector.collect(&root, MathMarks::default());
    collector.finish(true)
}

/// Whether replacing this native Markdown node with `InlineFlow` can preserve
/// every inline child it currently contains.
///
/// This check runs against the document-level mdast received by the plugin,
/// before the paragraph is reparsed in isolation. That distinction matters for
/// reference links: markdown-rs only recognizes them when their definition is
/// present elsewhere in the document. If a node needs context the custom flow
/// does not own, leave it on gpui-component's native rendering path.
pub(super) fn supports_inline_math_node(node: &markdown_ast::Node) -> bool {
    match node {
        markdown_ast::Node::Root(root) => root.children.iter().all(supports_inline_math_node),
        markdown_ast::Node::Paragraph(paragraph) => {
            paragraph.children.iter().all(supports_inline_math_node)
        }
        markdown_ast::Node::Heading(heading) => {
            heading.children.iter().all(supports_inline_math_node)
        }
        markdown_ast::Node::Strong(strong) => strong.children.iter().all(supports_inline_math_node),
        markdown_ast::Node::Emphasis(emphasis) => {
            emphasis.children.iter().all(supports_inline_math_node)
        }
        markdown_ast::Node::Delete(delete) => delete.children.iter().all(supports_inline_math_node),
        markdown_ast::Node::Link(link) => link.children.iter().all(supports_inline_math_node),
        markdown_ast::Node::InlineCode(_)
        | markdown_ast::Node::InlineMath(_)
        | markdown_ast::Node::Text(_)
        | markdown_ast::Node::Break(_)
        | markdown_ast::Node::Image(_) => true,
        _ => false,
    }
}

pub(super) fn contains_math_syntax(source: &str) -> bool {
    !scan_math(source).tokens.is_empty()
}

fn scan_math(source: &str) -> MathScan {
    let mut scan = MathScan::default();
    let mut ix = 0;
    let mut line_start = 0;
    let mut code_ticks = None;
    let mut code_fence = None;

    while ix < source.len() {
        if ix == line_start && code_ticks.is_none() {
            let line_end = source[ix..]
                .find('\n')
                .map_or(source.len(), |offset| ix + offset + 1);
            let line = &source[ix..line_end];
            let fence_boundary = update_code_fence(line, &mut code_fence);
            if fence_boundary || code_fence.is_some() || line_indentation(line) > 3 {
                ix = line_end;
                line_start = line_end;
                continue;
            }
        }

        if let Some(ticks) =
            count_run(source, ix, b'`').filter(|_| code_ticks.is_some() || !is_escaped(source, ix))
        {
            if code_ticks == Some(ticks) {
                code_ticks = None;
            } else if code_ticks.is_none() {
                code_ticks = Some(ticks);
            }
            ix += ticks;
            continue;
        }

        if code_ticks.is_none()
            && let Some(delimiter) = math_delimiter_at(source, ix)
            && let Some(closing_start) =
                find_math_close(source, ix + delimiter.opening_len(), delimiter)
        {
            let body = ix + delimiter.opening_len()..closing_start;
            let trimmed = source[body.clone()].trim();
            let end = closing_start + delimiter.closing_len();
            if !trimmed.is_empty() && !is_ellipsis_placeholder(trimmed) {
                scan.tokens.push(MathToken {
                    delimiter,
                    start: ix,
                    end,
                    body,
                    block_range: delimiter
                        .is_display()
                        .then(|| standalone_block_range(source, ix, end))
                        .flatten(),
                });
                ix = end;
                line_start = source[..ix].rfind('\n').map_or(0, |offset| offset + 1);
                continue;
            }

            if trimmed.is_empty()
                && delimiter.is_display()
                && let Some(block_range) = standalone_block_range(source, ix, end)
            {
                scan.empty_display_blocks.push(EmptyDisplayBlock {
                    token_range: ix..end,
                    block_range,
                });
            }

            // Empty and dot-only examples are deliberately literal. Consume
            // the matched pair as a unit so its closing delimiter cannot be
            // reinterpreted as the opener of a later formula.
            for offset in ix..end {
                if source.as_bytes()[offset] == b'$' && !is_escaped(source, offset) {
                    scan.escapable_dollars.push(offset);
                }
            }
            ix = end;
            line_start = source[..ix].rfind('\n').map_or(0, |offset| offset + 1);
            continue;
        }

        if code_ticks.is_none() && source.as_bytes()[ix] == b'$' && !is_escaped(source, ix) {
            scan.escapable_dollars.push(ix);
        }

        let character = source[ix..].chars().next();
        let char_len = character.map_or(1, char::len_utf8);
        if character == Some('\n') {
            line_start = ix + char_len;
        }
        ix += char_len;
    }

    scan
}

fn math_delimiter_at(source: &str, ix: usize) -> Option<MathDelimiter> {
    if exact_dollar_run(source, ix, 2) && !is_escaped(source, ix) {
        Some(MathDelimiter::DisplayDollar)
    } else if source[ix..].starts_with(r"\[") && !is_escaped(source, ix) {
        Some(MathDelimiter::DisplayBracket)
    } else if source[ix..].starts_with(r"\(") && !is_escaped(source, ix) {
        Some(MathDelimiter::Parenthesized)
    } else if source.as_bytes()[ix] == b'$' && is_valid_dollar_opener(source, ix) {
        Some(MathDelimiter::Dollar)
    } else {
        None
    }
}

fn find_math_close(source: &str, start: usize, delimiter: MathDelimiter) -> Option<usize> {
    let mut ix = start;
    let mut line_start = source[..start].rfind('\n').map_or(0, |offset| offset + 1);
    let mut code_ticks = None;

    while ix < source.len() {
        if ix == line_start && code_ticks.is_none() {
            let line_end = source[ix..]
                .find('\n')
                .map_or(source.len(), |offset| ix + offset + 1);
            let line = &source[ix..line_end];
            let mut fence = None;
            if update_code_fence(line, &mut fence)
                || (!delimiter.is_display() && line_indentation(line) > 3)
            {
                // A formula opener cannot claim a closing delimiter from a
                // later Markdown code block. Display bodies may themselves use
                // TeX indentation, so only real fences terminate those scans.
                return None;
            }
        }

        if let Some(ticks) =
            count_run(source, ix, b'`').filter(|_| code_ticks.is_some() || !is_escaped(source, ix))
        {
            if code_ticks == Some(ticks) {
                code_ticks = None;
            } else if code_ticks.is_none() {
                code_ticks = Some(ticks);
            }
            ix += ticks;
            continue;
        }

        if code_ticks.is_none() {
            match delimiter {
                MathDelimiter::Dollar
                    if source.as_bytes()[ix] == b'$'
                        && !is_escaped(source, ix)
                        && source.as_bytes().get(ix.wrapping_sub(1)) != Some(&b'$')
                        && source.as_bytes().get(ix + 1) != Some(&b'$') =>
                {
                    let preceded_by_non_whitespace = source[..ix]
                        .chars()
                        .next_back()
                        .is_some_and(|previous| !previous.is_whitespace());
                    let followed_by_digit = source[ix + 1..]
                        .chars()
                        .next()
                        .is_some_and(|next| next.is_ascii_digit());
                    if preceded_by_non_whitespace && !followed_by_digit {
                        return Some(ix);
                    }

                    // Do not search through an invalid currency boundary: a
                    // later real formula must remain available to the scanner.
                    return None;
                }
                MathDelimiter::Parenthesized
                    if source[ix..].starts_with(r"\)") && !is_escaped(source, ix) =>
                {
                    return Some(ix);
                }
                MathDelimiter::DisplayBracket
                    if source[ix..].starts_with(r"\]") && !is_escaped(source, ix) =>
                {
                    return Some(ix);
                }
                MathDelimiter::DisplayDollar
                    if exact_dollar_run(source, ix, 2) && !is_escaped(source, ix) =>
                {
                    return Some(ix);
                }
                _ => {}
            }
        }

        let character = source[ix..].chars().next();
        let char_len = character.map_or(1, char::len_utf8);
        if character == Some('\n') {
            line_start = ix + char_len;
        }
        ix += char_len;
    }

    None
}

fn exact_dollar_run(source: &str, ix: usize, length: usize) -> bool {
    count_run(source, ix, b'$') == Some(length)
}

fn standalone_block_range(source: &str, start: usize, end: usize) -> Option<Range<usize>> {
    let opening_line_start = source[..start].rfind('\n').map_or(0, |offset| offset + 1);
    let closing_line_end = source[end..]
        .find('\n')
        .map_or(source.len(), |offset| end + offset + 1);
    let opening_line_end = source[start..]
        .find('\n')
        .map_or(source.len(), |offset| start + offset + 1);
    let opening_line = &source[opening_line_start..opening_line_end];

    (line_indentation(opening_line) <= 3
        && source[opening_line_start..start].trim().is_empty()
        && source[end..closing_line_end].trim().is_empty())
    .then_some(opening_line_start..closing_line_end)
}

fn is_ellipsis_placeholder(body: &str) -> bool {
    // `$...$` is commonly used in prose to demonstrate delimiter syntax, as
    // in "inline math uses $...$". It carries no mathematical operand and
    // turning it into an atomic image needlessly replaces native text layout.
    // Restrict the exception to dot-only ellipses so genuine expressions such
    // as `$x+\cdots+y$` continue through RaTeX.
    !body.is_empty()
        && body
            .chars()
            .all(|character| matches!(character, '.' | '…' | '⋯'))
}

fn is_valid_dollar_opener(source: &str, ix: usize) -> bool {
    if is_escaped(source, ix)
        || source.as_bytes().get(ix.wrapping_sub(1)) == Some(&b'$')
        || source.as_bytes().get(ix + 1) == Some(&b'$')
    {
        return false;
    }

    let Some(next) = source[ix + 1..].chars().next() else {
        return false;
    };
    !next.is_whitespace()
}

fn prepare_inline_math(source: &str, scan: &MathScan) -> PreparedInlineMath {
    let mut prepared = PreparedInlineMath {
        source: String::with_capacity(source.len() + scan.tokens.len() * 2),
        original_offsets: vec![0],
        formulas: HashMap::new(),
    };
    let mut cursor = 0;

    enum PrepareEvent<'a> {
        Formula(&'a MathToken),
        Empty(&'a EmptyDisplayBlock),
    }

    let mut events = scan
        .tokens
        .iter()
        .map(PrepareEvent::Formula)
        .chain(scan.empty_display_blocks.iter().map(PrepareEvent::Empty))
        .collect::<Vec<_>>();
    events.sort_by_key(|event| match event {
        PrepareEvent::Formula(token) => token.start,
        PrepareEvent::Empty(block) => block.token_range.start,
    });

    for event in events {
        let event_start = match &event {
            PrepareEvent::Formula(token) => token.start,
            PrepareEvent::Empty(block) => block.token_range.start,
        };
        append_literal_text(
            &mut prepared,
            source,
            cursor,
            event_start,
            &scan.escapable_dollars,
        );

        match event {
            PrepareEvent::Formula(token) => {
                append_replacement(&mut prepared, "$$", token.start, token.body.start);
                append_flattened_formula_body(
                    &mut prepared,
                    source,
                    token.body.start,
                    token.body.end,
                );
                append_replacement(&mut prepared, "$$", token.body.end, token.end);
                prepared.formulas.insert(
                    token.start,
                    PreparedFormula {
                        source: source[token.body.clone()].trim().to_string(),
                        display: token.delimiter.is_display(),
                        plain_text: source[token.start..token.end].to_string(),
                    },
                );
                cursor = token.end;
            }
            PrepareEvent::Empty(block) => {
                debug_assert!(block.block_range.start <= block.token_range.start);
                debug_assert!(block.block_range.end >= block.token_range.end);
                append_masked_spaces(
                    &mut prepared,
                    block.token_range.start,
                    block.token_range.end,
                );
                cursor = block.token_range.end;
            }
        }
    }

    append_literal_text(
        &mut prepared,
        source,
        cursor,
        source.len(),
        &scan.escapable_dollars,
    );
    debug_assert_eq!(prepared.original_offsets.len(), prepared.source.len() + 1);
    prepared
}

fn append_masked_spaces(prepared: &mut PreparedInlineMath, start: usize, end: usize) {
    prepared
        .source
        .extend(std::iter::repeat_n(' ', end - start));
    prepared.original_offsets.extend(start + 1..=end);
}

fn append_flattened_formula_body(
    prepared: &mut PreparedInlineMath,
    source: &str,
    start: usize,
    end: usize,
) {
    let mut flattened = source.as_bytes()[start..end].to_vec();
    for byte in &mut flattened {
        if matches!(*byte, b'\r' | b'\n') {
            *byte = b' ';
        }
    }
    let Ok(flattened) = std::str::from_utf8(&flattened) else {
        // Only ASCII line-ending bytes were replaced, so valid UTF-8 input
        // should stay valid. Preserve a readable native fallback if that
        // invariant ever changes instead of panicking in a user-facing path.
        append_original(prepared, source, start, end);
        return;
    };
    prepared.source.push_str(flattened);
    prepared.original_offsets.extend(start + 1..=end);
}

fn append_literal_text(
    prepared: &mut PreparedInlineMath,
    source: &str,
    start: usize,
    end: usize,
    escapable_dollars: &[usize],
) {
    let mut cursor = start;
    while cursor < end {
        let char_len = source[cursor..].chars().next().map_or(1, char::len_utf8);
        if escapable_dollars.binary_search(&cursor).is_ok() {
            append_replacement(prepared, r"\$", cursor, cursor + 1);
        } else {
            append_original(prepared, source, cursor, cursor + char_len);
        }
        cursor += char_len;
    }
}

fn append_original(prepared: &mut PreparedInlineMath, source: &str, start: usize, end: usize) {
    prepared.source.push_str(&source[start..end]);
    prepared.original_offsets.extend(start + 1..=end);
}

fn append_replacement(
    prepared: &mut PreparedInlineMath,
    replacement: &str,
    original_start: usize,
    original_end: usize,
) {
    prepared.source.push_str(replacement);
    // Only the outer boundaries are semantically observable. Interior bytes
    // introduced by normalization map to the opening boundary, while the final
    // byte maps to the end of the replaced delimiter.
    for byte_ix in 1..=replacement.len() {
        prepared
            .original_offsets
            .push(if byte_ix == replacement.len() {
                original_end
            } else {
                original_start
            });
    }
}

fn count_run(source: &str, ix: usize, needle: u8) -> Option<usize> {
    if source.as_bytes().get(ix) != Some(&needle) {
        return None;
    }
    let mut end = ix + 1;
    while source.as_bytes().get(end) == Some(&needle) {
        end += 1;
    }
    Some(end - ix)
}

struct MathSegmentCollector<'a> {
    prepared: &'a PreparedInlineMath,
    segments: Vec<MathSegment>,
    pending: RichText,
    consumed_formulas: HashSet<usize>,
    unsupported: bool,
}

impl<'a> MathSegmentCollector<'a> {
    fn new(prepared: &'a PreparedInlineMath) -> Self {
        Self {
            prepared,
            segments: Vec::new(),
            pending: RichText {
                text: String::new(),
                marks: Vec::new(),
            },
            consumed_formulas: HashSet::new(),
            unsupported: false,
        }
    }

    fn collect(&mut self, node: &markdown_ast::Node, marks: MathMarks) {
        match node {
            markdown_ast::Node::Root(root) => {
                for (child_ix, child) in root.children.iter().enumerate() {
                    if child_ix > 0 {
                        // A blank-line block boundary is not present in mdast's
                        // child values. Retain one logical break so suppressing
                        // an empty display fence cannot concatenate the prose
                        // on its two sides.
                        self.append_text("\n", marks.clone());
                    }
                    self.collect(child, marks.clone());
                }
            }
            markdown_ast::Node::Paragraph(paragraph) => {
                self.collect_children(&paragraph.children, marks)
            }
            markdown_ast::Node::Heading(heading) => self.collect_children(&heading.children, marks),
            markdown_ast::Node::Strong(strong) => {
                let mut marks = marks;
                marks.bold = true;
                self.collect_children(&strong.children, marks);
            }
            markdown_ast::Node::Emphasis(emphasis) => {
                let mut marks = marks;
                marks.italic = true;
                self.collect_children(&emphasis.children, marks);
            }
            markdown_ast::Node::Delete(delete) => {
                let mut marks = marks;
                marks.strikethrough = true;
                self.collect_children(&delete.children, marks);
            }
            markdown_ast::Node::Link(link) => {
                let mut marks = marks;
                marks.link = Some(link.url.clone());
                self.collect_children(&link.children, marks);
            }
            markdown_ast::Node::InlineCode(code) => {
                let mut marks = marks;
                marks.code = true;
                self.append_text(&code.value, marks);
            }
            markdown_ast::Node::InlineMath(math) => {
                let Some(transformed_start) =
                    math.position.as_ref().map(|position| position.start.offset)
                else {
                    self.unsupported = true;
                    return;
                };
                let Some(start) = self.prepared.original_offset(transformed_start) else {
                    self.unsupported = true;
                    return;
                };
                let Some(formula) = self.prepared.formula(start) else {
                    self.unsupported = true;
                    return;
                };
                if !self.consumed_formulas.insert(start) {
                    self.unsupported = true;
                    return;
                }
                self.flush_text();
                self.segments.push(MathSegment::Formula {
                    source: formula.source.clone(),
                    plain_text: formula.plain_text.clone(),
                    start,
                    display: formula.display,
                    marks,
                });
            }
            markdown_ast::Node::Text(text) => {
                // A CommonMark soft line ending is collapsible whitespace, not
                // an explicit line break. GPUI's `shape_line` accepts exactly
                // one line, so normalize it before custom flow measurement.
                if text.value.contains('\n') {
                    self.append_text(&text.value.replace('\n', " "), marks);
                } else {
                    self.append_text(&text.value, marks);
                }
            }
            // Hard breaks remain real newlines. The flow layout splits these
            // into logical lines before calling `shape_line`.
            markdown_ast::Node::Break(_) => self.append_text("\n", marks),
            markdown_ast::Node::Image(image) => {
                self.flush_text();
                self.segments.push(MathSegment::Image {
                    url: image.url.clone(),
                    title: image.title.clone().unwrap_or_else(|| image.alt.clone()),
                    link: marks.link,
                });
            }
            // gpui-component gives these nodes behavior that a math-specific
            // flow cannot faithfully reproduce without the document's link
            // definitions or its HTML converter. Decline the whole paragraph
            // instead of silently dropping or literalizing native Markdown
            // content; the ordinary renderer then retains its original
            // capability and the math delimiter remains readable text.
            markdown_ast::Node::Html(_)
            | markdown_ast::Node::LinkReference(_)
            | markdown_ast::Node::FootnoteReference(_) => self.unsupported = true,
            _ => self.unsupported = true,
        }
    }

    fn collect_children(&mut self, children: &[markdown_ast::Node], marks: MathMarks) {
        for child in children {
            self.collect(child, marks.clone());
        }
    }

    fn append_text(&mut self, text: &str, marks: MathMarks) {
        if text.is_empty() {
            return;
        }
        let start = self.pending.text.len();
        self.pending.text.push_str(text);
        self.pending.marks.push(MathMarkRange {
            range: start..self.pending.text.len(),
            marks,
        });
    }

    fn flush_text(&mut self) {
        if self.pending.text.is_empty() {
            return;
        }
        self.segments.push(MathSegment::Text {
            text: std::mem::take(&mut self.pending.text),
            marks: std::mem::take(&mut self.pending.marks),
        });
    }

    fn finish(mut self, require_formula: bool) -> Option<Vec<MathSegment>> {
        self.flush_text();
        let all_formulas_consumed = self.consumed_formulas.len() == self.prepared.formulas.len();
        ((!require_formula || !self.consumed_formulas.is_empty())
            && all_formulas_consumed
            && !self.unsupported)
            .then_some(self.segments)
    }
}

fn is_escaped(source: &str, ix: usize) -> bool {
    let mut backslashes = 0;
    let mut cursor = ix;
    while cursor > 0 && source.as_bytes()[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_dollar_and_bracket_delimiters() {
        for source in ["$$\n x^2 \n$$", r"\[ x^2 \]"] {
            assert!(matches!(
                block_math_segments(source, 0).as_deref(),
                Some([MathBlockSegment::Formula {
                    source,
                    plain_text,
                    start: 0,
                    ..
                }]) if source == "x^2"
                    && (plain_text.starts_with("$$") || plain_text.starts_with(r"\["))
            ));
        }
        assert!(block_math_segments("plain $$ x $$ text", 0).is_none());
    }

    #[test]
    fn splits_block_math_from_surrounding_markdown() {
        let source = "**Title**\n$$\nx^2\n$$\n\nAfter";
        let segments = block_math_segments(source, 10).expect("block math");
        assert_eq!(segments.len(), 3);
        assert!(
            matches!(
                &segments[0],
                MathBlockSegment::Inline {
                    segments, start, ..
                }
                    if *start == 10
                        && matches!(segments.as_slice(), [MathSegment::Text { text, marks }]
                            if text.trim() == "Title"
                                && marks.iter().all(|mark| mark.marks.bold))
            ),
            "{segments:#?}"
        );
        assert!(matches!(
            &segments[1],
            MathBlockSegment::Formula {
                source,
                plain_text,
                start,
                marks,
                ..
            } if source == "x^2"
                && plain_text == "$$\nx^2\n$$"
                && *start == 20
                && *marks == MathMarks::default()
        ));
        assert!(matches!(
            &segments[2],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, .. }]
                    if text.trim() == "After")
        ));
    }

    #[test]
    fn protects_markdown_block_constructs_inside_display_math_without_moving_offsets() {
        let source = "before\n\n$$\n\\begin{pmatrix}\n1 & 2 \\\\\n3 & 4\n\\end{pmatrix}\n=\n\\begin{pmatrix}\n5 \\\\ 6\n\\end{pmatrix}\n$$\n\nafter";
        let protected = protect_display_math_for_markdown(source);
        assert_eq!(protected.len(), source.len());
        assert_eq!(protected.find("$$"), source.find("$$"));
        assert_eq!(protected.rfind("$$"), source.rfind("$$"));

        let root = markdown::to_mdast(&protected, &markdown::ParseOptions::gfm())
            .expect("protected Markdown should parse");
        let markdown_ast::Node::Root(root) = root else {
            panic!("markdown root")
        };
        let opening = source.find("$$").expect("opening fence");
        let closing_end = source.rfind("$$").expect("closing fence") + 2;
        assert!(root.children.iter().any(|node| {
            node.position().is_some_and(|position| {
                position.start.offset <= opening && position.end.offset >= closing_end
            })
        }));
    }

    #[test]
    fn display_math_protection_ignores_fenced_and_indented_code() {
        for source in [
            "```text\n$$\n=\n$$\n```",
            "~~~text\n$$\n=\n$$\n~~~",
            "    $$\n    =\n    $$",
            "    $$x^2$$",
            "\t$$x^2$$",
        ] {
            assert!(matches!(
                protect_display_math_for_markdown(source),
                Cow::Borrowed(_)
            ));
            assert!(block_math_segments(source, 0).is_none());
        }
    }

    #[test]
    fn display_math_protection_never_masks_a_block_the_plugin_cannot_claim() {
        for source in [
            "| value |\n| --- |\n| $$x$$ |",
            "[reference][target]\n$$\n=\n$$\n\n[target]: https://example.com",
            "before <kbd>key</kbd>\n$$\n=\n$$",
            "[link](https://example.com/$$value$$)",
            "![plot](https://example.com/$$plot$$)",
        ] {
            let protected = protect_display_math_for_markdown(source);
            assert!(matches!(protected, Cow::Borrowed(_)), "source: {source}");
            assert_eq!(protected, source);
        }
    }

    #[test]
    fn skips_empty_block_math() {
        assert!(block_math_segments("$$\n\n$$", 0).is_none());
    }

    #[test]
    fn empty_block_math_is_suppressed_when_real_math_claims_the_paragraph() {
        let segments =
            block_math_segments("before\n$$\n$$\n$$\nx^2\n$$\nafter", 0).expect("real block math");
        assert_eq!(segments.len(), 3);
        assert!(matches!(
            &segments[0],
            MathBlockSegment::Inline {
                segments, start, ..
            }
                if *start == 0
                    && matches!(segments.as_slice(), [MathSegment::Text { text, .. }]
                        if text.trim() == "before")
        ));
        assert!(matches!(
            &segments[1],
            MathBlockSegment::Formula {
                source,
                plain_text,
                start: 13,
                ..
            } if source == "x^2" && plain_text == "$$\nx^2\n$$"
        ));
        assert!(matches!(
            &segments[2],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, .. }]
                    if text.trim() == "after")
        ));
    }

    #[test]
    fn suppressed_empty_display_keeps_the_prose_boundary_around_it() {
        let segments = block_math_segments("before\n$$\n$$\nafter\n$$x$$", 0)
            .expect("real display math after empty display");
        assert!(matches!(
            &segments[0],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, .. }]
                    if text == "before\nafter")
        ));
        assert!(matches!(
            &segments[1],
            MathBlockSegment::Formula {
                source,
                breaks_before: 1,
                ..
            } if source == "x"
        ));
    }

    #[test]
    fn display_math_preserves_marks_that_span_its_block_boundary() {
        let source = "**_~~[before\n$$x$$\nafter](https://example.com)~~_**";
        let segments = block_math_segments(source, 100).expect("marked display math");
        assert_eq!(segments.len(), 3);

        let has_all_marks = |marks: &MathMarks| {
            marks.bold
                && marks.italic
                && marks.strikethrough
                && marks.link.as_deref() == Some("https://example.com")
        };
        assert!(matches!(
            &segments[0],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, marks }]
                    if text.trim() == "before"
                        && marks.iter().all(|mark| has_all_marks(&mark.marks)))
        ));
        assert!(matches!(
            &segments[1],
            MathBlockSegment::Formula { source, marks, .. }
                if source == "x" && has_all_marks(marks)
        ));
        assert!(matches!(
            &segments[2],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, marks }]
                    if text.trim() == "after"
                        && marks.iter().all(|mark| has_all_marks(&mark.marks)))
        ));
    }

    #[test]
    fn parses_inline_math_inside_markdown_marks() {
        let segments = inline_math_segments("**before $x^2$ after**").expect("math");
        assert!(matches!(
            &segments[0],
            MathSegment::Text { text, marks }
                if text == "before " && marks.iter().all(|mark| mark.marks.bold)
        ));
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, marks, .. } if source == "x^2" && marks.bold
        ));
        assert!(matches!(
            &segments[2],
            MathSegment::Text { text, marks }
                if text == " after" && marks.iter().all(|mark| mark.marks.bold)
        ));
    }

    #[test]
    fn preserves_native_inline_images_around_math() {
        let segments = inline_math_segments(
            "before [![plot](https://example.com/plot.png \"Plot\")](https://example.com) $x$",
        )
        .expect("math");
        assert!(matches!(
            &segments[1],
            MathSegment::Image { url, title, link }
                if url == "https://example.com/plot.png"
                    && title == "Plot"
                    && link.as_deref() == Some("https://example.com")
        ));
        assert!(segments.iter().any(|segment| matches!(
            segment,
            MathSegment::Formula { source, .. } if source == "x"
        )));
    }

    #[test]
    fn declines_nodes_whose_native_behavior_cannot_be_reproduced() {
        assert!(inline_math_segments("<kbd>key</kbd> and $x$").is_none());
        assert!(inline_math_segments("[target](https://example.com/$path$) and $x$").is_none());
        assert!(inline_math_segments("![plot](https://example.com/$path$.png) and $x$").is_none());

        let root = markdown::to_mdast(
            "[reference][target] and $x$\n\n[target]: https://example.com",
            &markdown::ParseOptions::gfm(),
        )
        .expect("document");
        let markdown_ast::Node::Root(root) = root else {
            panic!("Markdown root")
        };
        assert!(!supports_inline_math_node(&root.children[0]));
    }

    #[test]
    fn skips_escaped_dollars_and_code_spans() {
        assert!(inline_math_segments(r"`$ignored$` and \$amount").is_none());
        let segments = inline_math_segments(r"`$ignored$` and $x$").expect("math");
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, .. } if source == "x"
        ));
        assert!(matches!(
            &segments[0],
            MathSegment::Text { text, marks }
                if text == "$ignored$ and " && marks.iter().any(|mark| mark.marks.code)
        ));
    }

    #[test]
    fn backslash_does_not_escape_a_closing_code_span_tick() {
        let segments = inline_math_segments("`code \\` and $x$").expect("formula after code span");
        assert!(segments.iter().any(|segment| matches!(
            segment,
            MathSegment::Formula { source, .. } if source == "x"
        )));
        assert!(contains_math_syntax("$x + `code \\` + y$"));
    }

    #[test]
    fn multiline_display_body_is_flattened_only_for_mdast_recognition() {
        let source = "$$\n\\begin{aligned}\nx &= 1\n\ny &= 2\n\\end{aligned}\n$$";
        let segments = block_math_segments(source, 0).expect("multiline display math");
        assert!(matches!(
            segments.as_slice(),
            [MathBlockSegment::Formula {
                source: formula,
                plain_text,
                start: 0,
                ..
            }] if formula == "\\begin{aligned}\nx &= 1\n\ny &= 2\n\\end{aligned}"
                && plain_text == source
        ));
    }

    #[test]
    fn block_boundary_trimming_preserves_code_spaces_and_consecutive_hard_breaks() {
        let trailing = block_math_segments("`trailing `\n$$x$$", 0).expect("trailing code");
        assert!(matches!(
            &trailing[0],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, marks }]
                    if text == "trailing "
                        && marks.iter().all(|mark| mark.marks.code))
        ));

        let leading = block_math_segments("$$x$$\n` leading`", 0).expect("leading code");
        assert!(matches!(
            &leading[1],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, marks }]
                    if text == " leading"
                        && marks.iter().all(|mark| mark.marks.code))
        ));

        let breaks = block_math_segments("before\\\n\\\n$$x$$", 0).expect("hard breaks");
        assert!(matches!(
            &breaks[0],
            MathBlockSegment::Inline { segments, .. }
                if matches!(segments.as_slice(), [MathSegment::Text { text, .. }]
                    if text == "before")
        ));
        assert!(matches!(
            &breaks[1],
            MathBlockSegment::Formula {
                breaks_before: 2,
                ..
            }
        ));
    }

    #[test]
    fn identical_code_and_math_tokens_keep_their_own_positions() {
        let segments = inline_math_segments("写 `$x$` 表示代码，例如 $x$ 表示公式").expect("math");
        assert!(matches!(
            &segments[0],
            MathSegment::Text { text, marks }
                if text == "写 $x$ 表示代码，例如 "
                    && marks.iter().any(|mark| mark.marks.code)
        ));
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, start, .. } if source == "x" && *start == 32
        ));
        assert!(matches!(
            &segments[2],
            MathSegment::Text { text, .. } if text == " 表示公式"
        ));
    }

    #[test]
    fn currency_dollars_do_not_consume_a_later_formula() {
        let segments = inline_math_segments("Costs $5 and $10; equation $x$ works").expect("math");
        assert!(matches!(
            &segments[0],
            MathSegment::Text { text, .. } if text == "Costs $5 and $10; equation "
        ));
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, .. } if source == "x"
        ));
        assert!(inline_math_segments("Costs $5 and $10 today").is_none());
        assert!(matches!(
            &inline_math_segments("The value is $5$ exactly").expect("numeric formula")[1],
            MathSegment::Formula { source, .. } if source == "5"
        ));
    }

    #[test]
    fn parenthesized_inline_math_remains_supported() {
        let segments = inline_math_segments(r"before \(x + 1\) after").expect("math");
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, start, .. } if source == "x + 1" && *start == 7
        ));
    }

    #[test]
    fn inline_display_delimiters_are_distinct_atomic_formulas() {
        for source in [
            r"块：$$\int_0^1 x dx$$",
            r"文字 $$\frac{1}{2}$$ 文字",
            r"前 \[x^2\] 后",
        ] {
            let segments = inline_math_segments(source).expect("inline display math");
            let formulas = segments
                .iter()
                .filter_map(|segment| match segment {
                    MathSegment::Formula {
                        source, display, ..
                    } => Some((source.as_str(), *display)),
                    MathSegment::Text { .. } | MathSegment::Image { .. } => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(formulas.len(), 1, "source: {source}");
            assert!(formulas[0].1, "display style was lost: {source}");
        }

        let source = "$$a$$ 和 $$b$$";
        assert!(block_math_segments(source, 0).is_none());
        let formulas = inline_math_segments(source)
            .expect("two formulas")
            .into_iter()
            .filter_map(|segment| match segment {
                MathSegment::Formula {
                    source, display, ..
                } => Some((source, display)),
                MathSegment::Text { .. } | MathSegment::Image { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            formulas,
            vec![("a".to_string(), true), ("b".to_string(), true)]
        );
    }

    #[test]
    fn heading_math_uses_the_same_inline_lexer() {
        let segments = inline_math_segments("# 标题 $x^2$").expect("heading formula");
        assert!(matches!(
            &segments[0],
            MathSegment::Text { text, .. } if text == "标题 "
        ));
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, display: false, .. } if source == "x^2"
        ));
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
    fn delimiter_ellipsis_examples_remain_native_text() {
        assert!(inline_math_segments("内联公式使用 $...$ 表示").is_none());
        assert!(inline_math_segments("块公式使用 $$...$$ 表示").is_none());
        let segments = inline_math_segments("占位 $...$，公式 $x^2$ 保留").expect("real formula");
        assert!(matches!(
            &segments[0],
            MathSegment::Text { text, .. } if text == "占位 $...$，公式 "
        ));
        assert!(matches!(
            &segments[1],
            MathSegment::Formula { source, .. } if source == "x^2"
        ));
    }

    #[test]
    fn soft_breaks_collapse_but_explicit_breaks_remain_logical_lines() {
        let soft = inline_math_segments("first\nsecond $x$").expect("math");
        assert!(matches!(
            &soft[0],
            MathSegment::Text { text, .. } if text == "first second "
        ));

        let hard = inline_math_segments("first  \nsecond $x$").expect("math");
        assert!(matches!(
            &hard[0],
            MathSegment::Text { text, .. } if text == "first\nsecond "
        ));
    }
}
