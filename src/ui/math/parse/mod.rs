//! Pure Markdown math recognition and length-preserving source preparation.
//!
//! This module deliberately has no GPUI window or application dependencies.
//! Nostra owns the accepted TeX delimiters, while gpui-component owns the
//! Markdown AST, native marks/links/images, mixed inline flow, and selection.

mod rewrite;
mod scanner;
mod semantics;
mod source;

use self::{rewrite::*, scanner::*, semantics::*, source::*};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
};

use gpui_component::text::markdown_ast;

const PREPARED_DOLLAR_MASK: u8 = b'^';
const PREPARED_INLINE_MATH_DELIMITER: u8 = b'`';
const PREPARED_INLINE_MATH_BODY_MASK: u8 = b'^';
const PREPARED_TABLE_PIPE_MASK: u8 = b':';
const SCAN_CONTROL_MASK: u8 = b' ';
pub(super) const PREPARATION_CHANGED_NATIVE_SEMANTICS: &str =
    "nostra.math.preparation.changed-native-semantics";
pub(super) const PREPARATION_PRODUCED_INVALID_UTF8: &str = "nostra.math.preparation.invalid-utf8";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecognizedFormula {
    pub(super) source: String,
    pub(super) plain_text: String,
    pub(super) relative_start: usize,
    pub(super) display: bool,
}

#[derive(Default)]
struct MarkdownSourceRanges {
    preparable: Vec<Range<usize>>,
    restorable: Vec<Range<usize>>,
    authoritative_image_alt: Vec<Range<usize>>,
    opaque: Vec<Range<usize>>,
    reference_associations: Vec<ReferenceAssociation>,
    unsafe_reference_identifiers: HashSet<String>,
    reference_topology: Option<Vec<ReferenceResolution>>,
    native_topology: Option<Vec<NativeStructure>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceAssociationRole {
    /// A shortcut/collapsed label whose prepared source determines the
    /// reference identifier. Link text may contain formulas; image alt text is
    /// only masked through gpui-component's authoritative-alt recovery seam.
    Implicit,
    /// A definition whose identifier must follow every associated reference.
    Definition,
    /// Association-only source that is safe to mirror in the parse view.
    Opaque,
}

#[derive(Clone, Debug)]
struct ReferenceAssociation {
    identifier: String,
    source: Range<usize>,
    role: ReferenceAssociationRole,
}

struct ReferenceIdentifierRewrite {
    original_identifier: String,
    prepared_identifier: String,
    implicit_sources: Vec<Range<usize>>,
    replacements: Vec<(Range<usize>, String)>,
    restore_implicit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceResolutionKind {
    LinkFull,
    LinkCollapsed,
    LinkShortcut,
    ImageFull,
    ImageCollapsed,
    ImageShortcut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReferenceResolution {
    source: Range<usize>,
    kind: ReferenceResolutionKind,
    destination: Option<ReferenceDestination>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReferenceDestination {
    url: String,
    title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NativeStructureKind {
    Blockquote,
    List,
    ListItem,
    Heading,
    Strong,
    Emphasis,
    Delete,
    Link(String, Option<String>),
    Image(String, Option<String>),
    LinkReference,
    ImageReference,
    Definition(String, Option<String>),
    FootnoteDefinition,
    FootnoteReference,
    InlineCode,
    Code,
    Break,
    Html,
    Table,
    TableRow,
    TableCell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeStructure {
    source: Range<usize>,
    kind: NativeStructureKind,
}

struct MathParsePreparation {
    ranges: MarkdownSourceRanges,
    scan: MathScan,
    table_pipe_formulas: Vec<MathToken>,
}

/// Build the length-preserving parse view consumed by markdown-rs.
///
/// Accepted single-dollar formulas are encoded as equal-length backtick spans
/// while markdown-rs's permissive single-dollar mode stays disabled. Accepted
/// `\(...\)` and `\[...\]` delimiters become equivalent two-dollar constructs;
/// bare pipes inside a scanner-approved table formula are masked before GFM
/// can mistake them for cell separators;
/// native literal backticks that could capture a synthetic span are masked in
/// the parse view only. Rejected dollars are adaptively masked when math parsing
/// would otherwise consume native links, images, marks, references, or currency
/// contexts. A scanner-approved unclosed `$$` tail is deliberately left as
/// markdown-rs's single flow-math node: gpui-component can then show the
/// authoritative source fallback while keeping formula-body block markers
/// opaque until the close arrives. Raw-HTML `Text` siblings use the same
/// restorable sentinel seam.
///
/// Every candidate is checked against the baseline native-structure and
/// resolved-reference fingerprints. If no candidate preserves those semantics,
/// preparation fails instead of publishing a parse view already known to be
/// unsafe. Authoritative source, selection, copying, and node offsets never use
/// these private substitutions.
pub(super) fn try_prepare_math_source(source: &str) -> Result<String, &'static str> {
    // Avoid the baseline GFM pass for ordinary prose. This trigger deliberately
    // does not use the math scanner: scanner control bytes inside an opaque
    // Markdown region must not be able to hide a later visible formula.
    if !source.as_bytes().contains(&b'$') && !source.contains(r"\(") && !source.contains(r"\[") {
        return Ok(source.to_string());
    }

    let MathParsePreparation {
        ranges,
        scan,
        table_pipe_formulas,
    } = math_parse_preparation(source)?;
    let mut prepared = source.as_bytes().to_vec();

    mask_table_formula_pipes(source, &table_pipe_formulas, &mut prepared);

    let visible_tokens = scan
        .tokens
        .iter()
        .filter(|token| math_token_is_visible(source, &ranges, token))
        .collect::<Vec<_>>();
    let pending_display = scan
        .pending_display
        .as_ref()
        .filter(|pending| pending_display_is_visible(source, &ranges, pending))
        .cloned();
    for token in &visible_tokens {
        prepare_visible_math_token(source, token, &mut prepared);
    }
    mask_literal_backticks(source, &ranges, &mut prepared);

    let mut dollars_to_mask = HashSet::new();

    // Text between paired raw HTML tags is still emitted as Markdown `Text`
    // siblings. Mask every dollar there so enabling markdown-rs math globally
    // cannot claim it; the inline fallback restores the original text leaf.
    for range in &ranges.restorable {
        for offset in range.clone() {
            if source.as_bytes()[offset] == b'$'
                && !range_is_covered(&ranges.preparable, &(offset..offset + 1))
            {
                dollars_to_mask.insert(offset);
            }
        }
    }
    for offset in dollars_to_mask {
        prepared[offset] = PREPARED_DOLLAR_MASK;
    }

    // The scanner can recognize a syntactically valid dollar pair that is not
    // contained by one native Text leaf. Mask one safe delimiter before
    // markdown-rs sees it; otherwise the math tokenizer can consume the link,
    // image, mark, or break that made the token ineligible in the first place.
    let rejected_tokens_are_suppressed = scan
        .tokens
        .iter()
        .filter(|token| {
            matches!(token.delimiter, MathDelimiter::DisplayDollar)
                && !math_token_is_visible(source, &ranges, token)
        })
        .all(|token| {
            let (opening, closing) = token.delimiter_ranges();
            mask_unclaimed_delimiter_pair(&opening, &closing, &ranges, &mut prepared).is_some()
        });

    let prepared_without_reference_sync = prepared.clone();
    let initial_is_safe = rejected_tokens_are_suppressed
        && stabilize_math_parse_view(
            source,
            &ranges,
            &visible_tokens,
            pending_display.as_ref(),
            false,
            true,
            &mut prepared,
        );

    // A prepared identifier can make previously unresolved shortcut or
    // collapsed syntax resolve (or make an existing reference disappear).
    // Associations collected from the original AST cannot describe those
    // latent candidates, so compare the complete reference topology and fall
    // back atomically if source preparation changed native link/image meaning.
    if !initial_is_safe
        || !reference_semantics_match(
            source,
            &prepared,
            &ranges,
            &visible_tokens,
            pending_display.as_ref(),
        )
    {
        // Preserve unrelated formula conversion and currency masks, but make
        // implicit identifiers conservative: retain rejected-dollar masking
        // while restoring slash-delimited formulas, then mirror that exact
        // view into associated definitions/full-reference labels.
        let mut fallback = prepared_without_reference_sync;
        let fallback_labels_are_valid = ranges
            .reference_associations
            .iter()
            .filter(|association| association.role == ReferenceAssociationRole::Implicit)
            .all(|association| {
                let Some(original) = source.get(association.source.clone()) else {
                    return false;
                };
                let candidate = mirror_reference_identifier(original, false, false);
                fallback[association.source.clone()].copy_from_slice(candidate.as_bytes());
                true
            });
        let fallback_is_safe = fallback_labels_are_valid
            && stabilize_math_parse_view(
                source,
                &ranges,
                &visible_tokens,
                pending_display.as_ref(),
                false,
                false,
                &mut fallback,
            )
            && reference_semantics_match(
                source,
                &fallback,
                &ranges,
                &visible_tokens,
                pending_display.as_ref(),
            );
        if !fallback_is_safe {
            let recovered = [true, false]
                .into_iter()
                .find_map(|preserve_reference_labels| {
                    let mut candidate = conservative_reference_fallback(
                        source,
                        &ranges,
                        &visible_tokens,
                        preserve_reference_labels,
                    );
                    (stabilize_math_parse_view(
                        source,
                        &ranges,
                        &visible_tokens,
                        pending_display.as_ref(),
                        false,
                        false,
                        &mut candidate,
                    ) && reference_semantics_match(
                        source,
                        &candidate,
                        &ranges,
                        &visible_tokens,
                        pending_display.as_ref(),
                    ))
                    .then_some(candidate)
                });
            fallback = if let Some(recovered) = recovered {
                recovered
            } else {
                // Disable every formula as a final native-Markdown fallback.
                // This candidate is still accepted only after both the
                // unclaimed-math and reference-semantic invariants hold.
                let mut native = source.as_bytes().to_vec();
                let no_visible_tokens = Vec::new();
                let is_safe = stabilize_math_parse_view(
                    source,
                    &ranges,
                    &no_visible_tokens,
                    pending_display.as_ref(),
                    false,
                    false,
                    &mut native,
                ) && reference_semantics_match(
                    source,
                    &native,
                    &ranges,
                    &no_visible_tokens,
                    pending_display.as_ref(),
                );
                if !is_safe {
                    return Err(PREPARATION_CHANGED_NATIVE_SEMANTICS);
                }
                native
            };
        }
        prepared = fallback;
    }

    if prepared.as_slice() == source.as_bytes() {
        return Ok(source.to_string());
    }

    // Only one- or two-byte ASCII substitutions are performed, so valid UTF-8
    // and every character boundary are preserved. Refuse to publish a parse
    // view if that invariant is ever changed accidentally.
    String::from_utf8(prepared).map_err(|_| PREPARATION_PRODUCED_INVALID_UTF8)
}

fn math_parse_preparation(source: &str) -> Result<MathParsePreparation, &'static str> {
    let mut ranges = preparable_text_ranges(source);
    let mut table_pipe_formulas = table_formula_tokens_split_by_pipes(source, &ranges);
    while !table_pipe_formulas.is_empty() {
        let mut table_parse_view = source.as_bytes().to_vec();
        mask_table_formula_pipes(source, &table_pipe_formulas, &mut table_parse_view);
        let table_parse_view =
            String::from_utf8(table_parse_view).map_err(|_| PREPARATION_PRODUCED_INVALID_UTF8)?;
        let candidate_ranges = preparable_text_ranges_from_view(source, &table_parse_view);
        let retained = table_pipe_formulas
            .iter()
            .filter(|token| math_token_is_visible(source, &candidate_ranges, token))
            .cloned()
            .collect::<Vec<_>>();
        ranges = candidate_ranges;
        if retained.len() == table_pipe_formulas.len() {
            break;
        }
        table_pipe_formulas = retained;
        if table_pipe_formulas.is_empty() {
            ranges = preparable_text_ranges(source);
        }
    }

    let mut scan_view = delimiter_safe_scan_view(source, &ranges).into_bytes();
    mask_table_formula_pipes(source, &table_pipe_formulas, &mut scan_view);
    let scan_view = String::from_utf8(scan_view).map_err(|_| PREPARATION_PRODUCED_INVALID_UTF8)?;
    let scan = scan_math_in_parse_view(&scan_view);
    Ok(MathParsePreparation {
        ranges,
        scan,
        table_pipe_formulas,
    })
}

fn table_formula_tokens_split_by_pipes(
    source: &str,
    ranges: &MarkdownSourceRanges,
) -> Vec<MathToken> {
    let scan = scan_math_in_parse_view(source);
    scan.tokens
        .into_iter()
        .filter(|token| {
            !source[token.start..token.end].contains(['\n', '\r'])
                && token
                    .body
                    .clone()
                    .any(|offset| source.as_bytes()[offset] == b'|' && !is_escaped(source, offset))
                && ranges.native_topology.as_ref().is_some_and(|structures| {
                    structures.iter().any(|structure| {
                        structure.kind == NativeStructureKind::TableRow
                            && structure.source.start <= token.start
                            && structure.source.end > token.start
                    })
                })
                && [token.delimiter_ranges().0, token.delimiter_ranges().1]
                    .into_iter()
                    .all(|delimiter| range_is_covered(&ranges.preparable, &delimiter))
        })
        .collect()
}

fn mask_table_formula_pipes(source: &str, tokens: &[MathToken], prepared: &mut [u8]) {
    for token in tokens {
        for offset in token.body.clone() {
            if source.as_bytes()[offset] == b'|' && !is_escaped(source, offset) {
                prepared[offset] = PREPARED_TABLE_PIPE_MASK;
            }
        }
    }
}

#[cfg(test)]
fn prepare_math_source(source: &str) -> String {
    try_prepare_math_source(source).expect("test math source must have a safe parse view")
}

/// Restore a literal text leaf changed only by [`try_prepare_math_source`].
pub(super) fn restore_prepared_literal(
    original: &str,
    prepared: &str,
    value: &str,
) -> Option<String> {
    if original == prepared || original.len() != prepared.len() {
        return None;
    }

    let mut changed = false;
    for (&original, &prepared) in original.as_bytes().iter().zip(prepared.as_bytes()) {
        if original == prepared {
            continue;
        }
        if !matches!(
            (original, prepared),
            (b'$', PREPARED_DOLLAR_MASK)
                | (b'|', PREPARED_TABLE_PIPE_MASK)
                | (
                    PREPARED_INLINE_MATH_DELIMITER,
                    PREPARED_INLINE_MATH_BODY_MASK
                )
        ) {
            return None;
        }
        changed = true;
    }

    if !changed {
        return None;
    }

    if prepared.as_bytes() == value.as_bytes() {
        return Some(original.to_string());
    }

    // A baseline Text leaf can contain other CommonMark escapes or entities,
    // so its raw source need not equal the decoded value passed to the inline
    // parser. Decode both equal-length variants through ordinary GFM and only
    // restore when the prepared projection exactly matches that parser value.
    let prepared_value = gfm_visible_text(prepared)?;
    if prepared_value != value {
        return None;
    }
    gfm_visible_text(original)
}

pub(super) fn is_prepared_single_dollar_formula(original: &str, prepared: &str) -> bool {
    original.len() == prepared.len()
        && original.starts_with('$')
        && original.ends_with('$')
        && !original.starts_with("$$")
        && !original.ends_with("$$")
        && prepared.starts_with(PREPARED_INLINE_MATH_DELIMITER as char)
        && prepared.ends_with(PREPARED_INLINE_MATH_DELIMITER as char)
}

fn gfm_visible_text(source: &str) -> Option<String> {
    // Wrapping keeps leading/trailing spaces inside a paragraph; parsing the
    // fragment alone would trim them and no longer match the Text node value.
    const SENTINEL: char = '\u{e000}';
    let wrapped = format!("{SENTINEL}{source}{SENTINEL}");
    let root = markdown::to_mdast(&wrapped, &markdown::ParseOptions::gfm()).ok()?;
    let mut text = String::new();
    append_gfm_visible_text(&root, &mut text)?;
    Some(
        text.strip_prefix(SENTINEL)?
            .strip_suffix(SENTINEL)?
            .to_string(),
    )
}

fn append_gfm_visible_text(node: &markdown_ast::Node, text: &mut String) -> Option<()> {
    match node {
        markdown_ast::Node::Text(node) => text.push_str(&node.value),
        markdown_ast::Node::InlineCode(node) => text.push_str(&node.value),
        markdown_ast::Node::Break(_) => text.push('\n'),
        markdown_ast::Node::Image(node) => text.push_str(&node.alt),
        markdown_ast::Node::ImageReference(node) => text.push_str(&node.alt),
        _ => {
            let children = node.children()?;
            for child in children {
                append_gfm_visible_text(child, text)?;
            }
        }
    }
    Some(())
}

#[cfg(test)]
pub(super) fn inline_formula(source: &str) -> Option<RecognizedFormula> {
    recognized_formula(source, false)
}

/// Recognize one inline mdast node against its complete Markdown fragment.
///
/// markdown-rs decides where an `InlineMath` node ends before Nostra's stricter
/// currency policy runs. Re-scanning only the node slice loses the byte after
/// its closing dollar (for example the `1` in `Cost $5 and$10`) and can turn a
/// rejected currency span into a formula. Requiring the node range to match a
/// visible token from the complete fragment preserves that closing context.
pub(super) fn inline_formula_in_context(
    source: &str,
    node_range: Range<usize>,
) -> Option<RecognizedFormula> {
    source.get(node_range.clone())?;
    let MathParsePreparation { ranges, scan, .. } = math_parse_preparation(source).ok()?;
    let token = scan.tokens.iter().find(|token| {
        token.start == node_range.start
            && token.end == node_range.end
            && math_token_is_visible(source, &ranges, token)
    })?;
    let mut formula = recognized_formula_from_token(source, token);
    formula.relative_start = 0;
    Some(formula)
}

#[cfg(test)]
pub(super) fn display_formula(source: &str) -> Option<RecognizedFormula> {
    recognized_formula(source, true)
}

/// Recognize a flow-math node while taking its container-free TeX body from
/// markdown-rs. The raw positional span can contain blockquote markers or list
/// indentation on continuation lines, which must never reach RaTeX or copied
/// fallback text.
pub(super) fn display_formula_from_ast(source: &str, ast_value: &str) -> Option<RecognizedFormula> {
    let scan = scan_math_in_parse_view(source);
    let [token] = scan.tokens.as_slice() else {
        return None;
    };
    if !token.delimiter.is_display() {
        return None;
    }
    let mut formula = recognized_formula_from_token(source, token);
    let opening = &source[token.start..token.body.start];
    let closing = &source[token.body.end..token.end];
    let raw_body = &source[token.body.clone()];
    let ast_value = ast_value.trim_matches(['\r', '\n']);
    let formula_source = ast_value.trim();
    if formula_source.is_empty() || is_ellipsis_placeholder(formula_source) {
        return None;
    }

    formula.source = formula_source.to_string();
    formula.plain_text = if raw_body.contains('\n') {
        let line_ending = if raw_body.starts_with("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        format!("{opening}{line_ending}{ast_value}{line_ending}{closing}")
    } else {
        format!("{opening}{ast_value}{closing}")
    };
    Some(formula)
}

#[cfg(test)]
fn contains_math_syntax(source: &str) -> bool {
    !scan_math(source).tokens.is_empty()
}

#[cfg(test)]
fn recognized_formula(source: &str, require_standalone_display: bool) -> Option<RecognizedFormula> {
    recognized_formula_with_token(source, require_standalone_display).map(|(formula, _)| formula)
}

#[cfg(test)]
fn recognized_formula_with_token(
    source: &str,
    require_standalone_display: bool,
) -> Option<(RecognizedFormula, MathToken)> {
    let scan = scan_math(source);
    let [token] = scan.tokens.as_slice() else {
        return None;
    };

    if require_standalone_display {
        if !token.delimiter.is_display()
            || token
                .block_range
                .as_ref()
                .is_none_or(|range| range.start != 0 || range.end != source.len())
        {
            return None;
        }
    } else if token.start != 0 || token.end != source.len() {
        return None;
    }

    Some((recognized_formula_from_token(source, token), token.clone()))
}

fn recognized_formula_from_token(source: &str, token: &MathToken) -> RecognizedFormula {
    RecognizedFormula {
        source: source[token.body.clone()].trim().to_string(),
        plain_text: source[token.start..token.end].to_string(),
        relative_start: token.start,
        display: token.delimiter.is_display(),
    }
}

#[cfg(test)]
mod tests;
