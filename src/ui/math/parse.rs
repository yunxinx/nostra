//! Pure Markdown math recognition and length-preserving source preparation.
//!
//! This module deliberately has no GPUI window or application dependencies.
//! Nostra owns the accepted TeX delimiters, while gpui-component owns the
//! Markdown AST, native marks/links/images, mixed inline flow, and selection.

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

/// Keep definition lookup stable when a shortcut/collapsed link label needs a
/// length-preserving parse-only rewrite. markdown-rs derives those reference
/// identifiers from visible label source, so changing only the label would
/// silently turn the link back into plain text.
fn synchronize_implicit_reference_identifiers(
    source: &str,
    associations: &[ReferenceAssociation],
    blockers: &HashSet<String>,
    mask_rejected_dollars: bool,
    rewrite_math_delimiters: bool,
    prepared: &mut [u8],
) {
    let normalize = normalize_reference_identifier;
    let mut grouped = HashMap::<&str, Vec<&ReferenceAssociation>>::new();
    for association in associations {
        grouped
            .entry(&association.identifier)
            .or_default()
            .push(association);
    }

    let mut rewrites = Vec::with_capacity(grouped.len());
    for (identifier, group) in grouped {
        let implicit_sources = group
            .iter()
            .filter(|association| association.role == ReferenceAssociationRole::Implicit)
            .map(|association| association.source.clone())
            .collect::<Vec<_>>();
        let mut desired = None::<String>;
        let mut safe = !blockers.contains(identifier);

        for association in group
            .iter()
            .copied()
            .filter(|association| association.role == ReferenceAssociationRole::Implicit)
        {
            let Some(original_label) = source.get(association.source.clone()) else {
                safe = false;
                break;
            };
            let Ok(prepared_label) = std::str::from_utf8(&prepared[association.source.clone()])
            else {
                safe = false;
                break;
            };
            if normalize(original_label) != identifier {
                safe = false;
                break;
            }

            let prepared_identifier = normalize(prepared_label);
            if prepared_identifier == identifier {
                continue;
            }
            if desired
                .as_ref()
                .is_some_and(|existing| existing != &prepared_identifier)
            {
                safe = false;
                break;
            }
            desired = Some(prepared_identifier);
        }

        if !safe {
            rewrites.push(ReferenceIdentifierRewrite {
                original_identifier: identifier.to_string(),
                prepared_identifier: identifier.to_string(),
                implicit_sources,
                replacements: Vec::new(),
                restore_implicit: true,
            });
            continue;
        }
        let Some(desired) = desired else {
            rewrites.push(ReferenceIdentifierRewrite {
                original_identifier: identifier.to_string(),
                prepared_identifier: identifier.to_string(),
                implicit_sources,
                replacements: Vec::new(),
                restore_implicit: false,
            });
            continue;
        };
        let mut replacements = Vec::<(Range<usize>, String)>::new();
        let mut definition_seen = false;

        for association in group.iter().copied() {
            let Some(original) = source.get(association.source.clone()) else {
                safe = false;
                break;
            };
            let candidate = match association.role {
                ReferenceAssociationRole::Implicit => {
                    match std::str::from_utf8(&prepared[association.source.clone()]) {
                        Ok(candidate) => candidate.to_string(),
                        Err(_) => {
                            safe = false;
                            break;
                        }
                    }
                }
                ReferenceAssociationRole::Definition | ReferenceAssociationRole::Opaque => {
                    definition_seen |= association.role == ReferenceAssociationRole::Definition;
                    mirror_reference_identifier(
                        original,
                        mask_rejected_dollars,
                        rewrite_math_delimiters,
                    )
                }
            };

            if candidate.len() != association.source.len() || normalize(&candidate) != desired {
                safe = false;
                break;
            }
            if matches!(
                association.role,
                ReferenceAssociationRole::Definition | ReferenceAssociationRole::Opaque
            ) {
                replacements.push((association.source.clone(), candidate));
            }
        }

        if !safe || !definition_seen {
            rewrites.push(ReferenceIdentifierRewrite {
                original_identifier: identifier.to_string(),
                prepared_identifier: identifier.to_string(),
                implicit_sources,
                replacements: Vec::new(),
                restore_implicit: true,
            });
            continue;
        }
        rewrites.push(ReferenceIdentifierRewrite {
            original_identifier: identifier.to_string(),
            prepared_identifier: desired,
            implicit_sources,
            replacements,
            restore_implicit: false,
        });
    }

    // A rewrite is safe only if the complete prepared identifier mapping stays
    // one-to-one. If multiple original definitions converge, restore every
    // participant. Restoring one group can expose its original identifier as a
    // new collision target, so propagate those conflicts through the mapping.
    let mut target_groups = HashMap::<&str, Vec<usize>>::new();
    for (index, rewrite) in rewrites.iter().enumerate() {
        target_groups
            .entry(&rewrite.prepared_identifier)
            .or_default()
            .push(index);
    }
    let mut conflicts = HashSet::new();
    let mut pending = VecDeque::new();
    for indices in target_groups.values().filter(|indices| indices.len() > 1) {
        for &index in indices {
            if conflicts.insert(index) {
                pending.push_back(index);
            }
        }
    }
    while let Some(index) = pending.pop_front() {
        let original = rewrites[index].original_identifier.as_str();
        if let Some(indices) = target_groups.get(original) {
            for &dependent in indices {
                if conflicts.insert(dependent) {
                    pending.push_back(dependent);
                }
            }
        }
    }

    for (index, rewrite) in rewrites.into_iter().enumerate() {
        if rewrite.restore_implicit || conflicts.contains(&index) {
            restore_implicit_reference_labels(source, &rewrite.implicit_sources, prepared);
        }
        if conflicts.contains(&index) {
            continue;
        }
        for (range, replacement) in rewrite.replacements {
            prepared[range].copy_from_slice(replacement.as_bytes());
        }
    }
}

/// CommonMark association normalization used by markdown-rs: collapse Markdown
/// whitespace, trim it, then perform the same two-step Unicode case folding.
fn normalize_reference_identifier(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut in_whitespace = true;
    let mut index = 0;
    let mut start = 0;

    while index < bytes.len() {
        if matches!(bytes[index], b'\t' | b'\n' | b'\r' | b' ') {
            if !in_whitespace {
                collapsed.push_str(&value[start..index]);
                in_whitespace = true;
            }
        } else if in_whitespace {
            if start != 0 {
                collapsed.push(' ');
            }
            start = index;
            in_whitespace = false;
        }
        index += 1;
    }
    if !in_whitespace {
        collapsed.push_str(&value[start..]);
    }

    collapsed.to_lowercase().to_uppercase().to_lowercase()
}

fn restore_implicit_reference_labels(source: &str, ranges: &[Range<usize>], prepared: &mut [u8]) {
    for range in ranges {
        if let Some(original) = source.get(range.clone()) {
            prepared[range.clone()].copy_from_slice(original.as_bytes());
        }
    }
}

fn mirror_reference_identifier(
    source: &str,
    mask_rejected_dollars: bool,
    rewrite_math_delimiters: bool,
) -> String {
    let ranges = preparable_text_ranges(source);
    let scan_view = delimiter_safe_scan_view(source, &ranges);
    let scan = scan_math_in_parse_view(&scan_view);
    let mut prepared = source.as_bytes().to_vec();
    if rewrite_math_delimiters {
        for token in scan
            .tokens
            .iter()
            .filter(|token| math_token_is_visible(source, &ranges, token))
        {
            prepare_visible_math_token(source, token, &mut prepared);
        }
    }
    mask_literal_backticks(source, &ranges, &mut prepared);
    if mask_rejected_dollars {
        for offset in scan.escapable_dollars {
            if range_is_covered(&ranges.preparable, &(offset..offset + 1)) {
                prepared[offset] = PREPARED_DOLLAR_MASK;
            }
        }
    }
    String::from_utf8(prepared).unwrap_or_else(|_| source.to_string())
}

fn prepare_visible_math_token(source: &str, token: &MathToken, prepared: &mut [u8]) {
    let (opening, closing) = token.delimiter_ranges();
    match token.delimiter {
        MathDelimiter::Dollar => {
            prepared[opening].fill(PREPARED_INLINE_MATH_DELIMITER);
            prepared[closing].fill(PREPARED_INLINE_MATH_DELIMITER);
            for offset in token.body.clone() {
                if source.as_bytes()[offset] == PREPARED_INLINE_MATH_DELIMITER {
                    prepared[offset] = PREPARED_INLINE_MATH_BODY_MASK;
                }
            }
        }
        MathDelimiter::Parenthesized | MathDelimiter::DisplayBracket => {
            prepared[opening].copy_from_slice(b"$$");
            prepared[closing].copy_from_slice(b"$$");
        }
        MathDelimiter::DisplayDollar => {}
    }
}

fn mask_literal_backticks(source: &str, ranges: &MarkdownSourceRanges, prepared: &mut [u8]) {
    for (offset, prepared_byte) in prepared.iter_mut().enumerate().take(source.len()) {
        let byte = offset..offset + 1;
        if source.as_bytes()[offset] == PREPARED_INLINE_MATH_DELIMITER
            && delimiter_is_mutable(&byte, ranges)
        {
            *prepared_byte = PREPARED_INLINE_MATH_BODY_MASK;
        }
    }
}

/// Last-resort parse view for adversarial identifier/currency interactions.
/// Keep authoritative reference syntax and raw currency bytes, while disabling
/// visible math tokens that could pair across them. Masked dollar formulas are
/// restored by the inline text fallback; slash-delimited formulas remain on
/// gpui-component's native Markdown path.
fn conservative_reference_fallback(
    source: &str,
    ranges: &MarkdownSourceRanges,
    visible_tokens: &[&MathToken],
    preserve_reference_labels: bool,
) -> Vec<u8> {
    let mut prepared = source.as_bytes().to_vec();
    for token in visible_tokens {
        let token_range = token.start..token.end;
        if preserve_reference_labels
            && ranges
                .reference_associations
                .iter()
                .any(|association| ranges_overlap(&association.source, &token_range))
        {
            continue;
        }
        if matches!(
            token.delimiter,
            MathDelimiter::Dollar | MathDelimiter::DisplayDollar
        ) {
            let (opening, closing) = token.delimiter_ranges();
            prepared[opening].fill(PREPARED_DOLLAR_MASK);
            prepared[closing].fill(PREPARED_DOLLAR_MASK);
        }
    }

    // Raw-HTML Text siblings still need their dollars hidden even when every
    // normal formula has been disabled.
    for range in &ranges.restorable {
        for offset in range.clone() {
            if source.as_bytes()[offset] == b'$'
                && !range_is_covered(&ranges.preparable, &(offset..offset + 1))
            {
                prepared[offset] = PREPARED_DOLLAR_MASK;
            }
        }
    }
    prepared
}

fn stabilize_math_parse_view(
    source: &str,
    ranges: &MarkdownSourceRanges,
    visible_tokens: &[&MathToken],
    pending_display: Option<&Range<usize>>,
    mask_rejected_dollars: bool,
    rewrite_math_delimiters: bool,
    prepared: &mut [u8],
) -> bool {
    let accepted = visible_tokens
        .iter()
        .map(|token| token.delimiter_ranges())
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();

    loop {
        if !seen.insert(prepared.to_vec()) {
            return false;
        }
        let Some(suppressed_unclaimed_math) =
            suppress_unclaimed_math_nodes(ranges, &accepted, pending_display, prepared)
        else {
            return false;
        };
        synchronize_implicit_reference_identifiers(
            source,
            &ranges.reference_associations,
            &ranges.unsafe_reference_identifiers,
            mask_rejected_dollars || suppressed_unclaimed_math,
            rewrite_math_delimiters,
            prepared,
        );
        let Some(parsed_math) = parsed_math_nodes(prepared) else {
            return false;
        };
        let all_nodes_are_claimed = parsed_math.pairs.iter().all(|pair| accepted.contains(pair))
            && parsed_math
                .pending_display
                .iter()
                .all(|range| Some(range) == pending_display);
        if all_nodes_are_claimed {
            if native_semantics_match(source, prepared, ranges, visible_tokens, pending_display) {
                return true;
            }
            if !mask_next_native_hazard_dollar(
                source,
                ranges,
                visible_tokens,
                pending_display,
                prepared,
            ) {
                return false;
            }
        }
    }
}

fn mask_next_native_hazard_dollar(
    source: &str,
    ranges: &MarkdownSourceRanges,
    visible_tokens: &[&MathToken],
    pending_display: Option<&Range<usize>>,
    prepared: &mut [u8],
) -> bool {
    (0..source.len()).rev().any(|offset| {
        let delimiter = offset..offset + 1;
        if source.as_bytes()[offset] != b'$'
            || prepared[offset] != b'$'
            || is_escaped(source, offset)
            || !delimiter_is_mutable(&delimiter, ranges)
            || visible_tokens
                .iter()
                .any(|token| token.start <= offset && offset < token.end)
            || pending_display.is_some_and(|pending| pending.contains(&offset))
        {
            return false;
        }
        prepared[offset] = PREPARED_DOLLAR_MASK;
        true
    })
}

/// Mask a delimiter from every math node that was not accepted by Nostra's
/// native-Text-leaf policy. markdown-rs is intentionally more permissive than
/// that policy and can otherwise pair dollars across links, images, marks, or
/// a currency closing context before the inline plugin gets control.
fn suppress_unclaimed_math_nodes(
    ranges: &MarkdownSourceRanges,
    accepted: &[(Range<usize>, Range<usize>)],
    pending_display: Option<&Range<usize>>,
    prepared: &mut [u8],
) -> Option<bool> {
    let parsed_math = parsed_math_nodes(prepared)?;
    let mut changed = false;

    for (opening, closing) in parsed_math
        .pairs
        .into_iter()
        .filter(|pair| !accepted.contains(pair))
    {
        changed |= mask_unclaimed_delimiter_pair(&opening, &closing, ranges, prepared)?;
    }

    for pending in parsed_math
        .pending_display
        .into_iter()
        .filter(|range| Some(range) != pending_display)
    {
        let opening = pending.start..pending.start.checked_add(2)?;
        if prepared.get(opening.clone()) != Some(b"$$") || !delimiter_is_mutable(&opening, ranges) {
            return None;
        }
        prepared[opening].fill(PREPARED_DOLLAR_MASK);
        changed = true;
    }

    Some(changed)
}

fn mask_unclaimed_delimiter_pair(
    opening: &Range<usize>,
    closing: &Range<usize>,
    ranges: &MarkdownSourceRanges,
    prepared: &mut [u8],
) -> Option<bool> {
    let opening_association = delimiter_is_in_reference_association(opening, ranges);
    let closing_association = delimiter_is_in_reference_association(closing, ranges);
    let opening_mutable = delimiter_is_mutable(opening, ranges);
    let closing_mutable = delimiter_is_mutable(closing, ranges);

    let delimiters = if opening_association && closing_association {
        vec![opening.clone(), closing.clone()]
    } else if closing_mutable && !closing_association {
        vec![closing.clone()]
    } else if opening_mutable && !opening_association {
        vec![opening.clone()]
    } else if closing_mutable {
        vec![closing.clone()]
    } else if opening_mutable {
        vec![opening.clone()]
    } else {
        return None;
    };

    let mut changed = false;
    for delimiter in delimiters {
        if prepared[delimiter.clone()]
            .iter()
            .any(|byte| *byte != PREPARED_DOLLAR_MASK)
        {
            prepared[delimiter].fill(PREPARED_DOLLAR_MASK);
            changed = true;
        }
    }
    Some(changed)
}

fn delimiter_is_in_reference_association(
    delimiter: &Range<usize>,
    ranges: &MarkdownSourceRanges,
) -> bool {
    ranges
        .reference_associations
        .iter()
        .any(|association| range_is_covered(std::slice::from_ref(&association.source), delimiter))
}

fn delimiter_is_mutable(delimiter: &Range<usize>, ranges: &MarkdownSourceRanges) -> bool {
    range_is_covered(&ranges.preparable, delimiter)
        || range_is_covered(&ranges.restorable, delimiter)
        || range_is_covered(&ranges.authoritative_image_alt, delimiter)
        || delimiter_is_in_reference_association(delimiter, ranges)
}

#[derive(Default)]
struct ParsedMathNodes {
    pairs: Vec<(Range<usize>, Range<usize>)>,
    pending_display: Vec<Range<usize>>,
}

fn parsed_math_nodes(prepared: &[u8]) -> Option<ParsedMathNodes> {
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

fn math_node_delimiter_pair(
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

fn reference_semantics_match(
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

fn reference_is_owned_by_formula(
    resolution: &ReferenceResolution,
    active_formulas: &[Range<usize>],
) -> bool {
    range_is_covered(active_formulas, &resolution.source)
}

fn native_semantics_match(
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

fn native_mark_is_owned_by_formula(
    structure: &NativeStructure,
    active_formulas: &[Range<usize>],
) -> bool {
    matches!(
        structure.kind,
        NativeStructureKind::Strong | NativeStructureKind::Emphasis | NativeStructureKind::Delete
    ) && range_is_covered(active_formulas, &structure.source)
}

fn normalize_native_topology_for_displays(
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

fn is_single_line_boundary(source: &str) -> bool {
    let source = source
        .strip_suffix("\r\n")
        .or_else(|| source.strip_suffix('\n'));
    source.is_some_and(|source| source.bytes().all(|byte| matches!(byte, b' ' | b'\t')))
}

fn is_nearby_blank_boundary(source: &str) -> bool {
    source.bytes().all(|byte| byte.is_ascii_whitespace())
        && source.bytes().filter(|byte| *byte == b'\n').count() <= 2
}

fn active_protected_display_ranges(
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

fn native_baseline_root(
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

fn active_formula_ranges(
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

fn prepared_token_is_active(prepared: &[u8], token: &MathToken) -> bool {
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
fn delimiter_safe_scan_view(source: &str, ranges: &MarkdownSourceRanges) -> String {
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

fn is_standalone_display_delimiter(
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

fn is_markdown_container_prefix(mut prefix: &str) -> bool {
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

fn preparable_text_ranges(source: &str) -> MarkdownSourceRanges {
    preparable_text_ranges_from_view(source, source)
}

fn preparable_text_ranges_from_view(source: &str, parse_view: &str) -> MarkdownSourceRanges {
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

fn prepared_reference_resolution_topology(source: &str) -> Option<Vec<ReferenceResolution>> {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options.math_text_single_dollar = false;
    let root = markdown::to_mdast(source, &options).ok()?;
    reference_resolution_topology_from_root(&root)
}

fn reference_resolution_topology_from_root(
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

fn prepared_native_structure_topology(
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

fn native_structure_topology_from_root(root: &markdown_ast::Node) -> Option<Vec<NativeStructure>> {
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

fn record_reference_association(
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

fn reference_label_ranges(node: &markdown_ast::Node, source: &str) -> Vec<Range<usize>> {
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

fn record_link_reference(
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

fn record_image_reference(
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

fn record_authoritative_image_alt(
    node: &markdown_ast::Node,
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    if let Some(alt) = reference_label_ranges(node, source).into_iter().next() {
        ranges.authoritative_image_alt.push(alt);
    }
    mark_opaque(node, ranges);
}

fn collect_preparable_text(
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

fn collect_inline_root(
    children: &[markdown_ast::Node],
    source: &str,
    ranges: &mut MarkdownSourceRanges,
) {
    let mut open_html_elements = Vec::new();
    collect_inline_children(children, source, ranges, &mut open_html_elements);
}

fn collect_inline_children(
    children: &[markdown_ast::Node],
    source: &str,
    ranges: &mut MarkdownSourceRanges,
    open_html_elements: &mut Vec<String>,
) {
    for child in children {
        collect_inline_node(child, source, ranges, open_html_elements);
    }
}

fn collect_inline_node(
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

fn collect_text_range(
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

fn is_explicit_link(link: &markdown_ast::Link, source: &str) -> bool {
    let Some(position) = link.position.as_ref() else {
        return false;
    };
    source
        .get(position.start.offset..position.end.offset)
        .is_some_and(|source| source.trim_start().starts_with('['))
}

fn mark_opaque(node: &markdown_ast::Node, ranges: &mut MarkdownSourceRanges) {
    if let Some(position) = node.position() {
        ranges
            .opaque
            .push(position.start.offset..position.end.offset);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HtmlTag {
    Open(String),
    Close(String),
    Neutral,
}

fn update_html_context(source: &str, open_elements: &mut Vec<String>) {
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

fn classify_html_tag(source: &str) -> HtmlTag {
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

fn html_tag_end(source: &str) -> Option<usize> {
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

fn is_void_html_element(name: &str) -> bool {
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

fn range_is_covered(ranges: &[Range<usize>], candidate: &Range<usize>) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= candidate.start && range.end >= candidate.end)
}

fn range_is_covered_by_union(
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

fn inline_token_crosses_only_owned_marks(
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

fn delimiter_is_visible(
    source: &str,
    ranges: &MarkdownSourceRanges,
    delimiter: &Range<usize>,
) -> bool {
    range_is_covered(&ranges.preparable, delimiter)
        || is_standalone_display_delimiter(source, delimiter, ranges)
}

fn math_token_is_visible(source: &str, ranges: &MarkdownSourceRanges, token: &MathToken) -> bool {
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

fn pending_display_is_visible(
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

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

#[derive(Clone, Copy)]
struct CodeFence {
    marker: u8,
    length: usize,
}

/// Update CommonMark fenced-code state and report whether `line` is itself a
/// fence boundary. Math recognition happens before mdast conversion, so it
/// must independently respect this block construct.
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
            if marker == b'`' && rest[length..].contains(&b'`') {
                return false;
            }
            *state = Some(CodeFence { marker, length });
            true
        }
        Some(open) if open.marker == marker && length >= open.length => {
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
    if line.as_bytes().get(spaces) == Some(&b'\t') {
        usize::MAX
    } else {
        spaces
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

impl MathToken {
    fn delimiter_ranges(&self) -> (Range<usize>, Range<usize>) {
        (self.start..self.body.start, self.body.end..self.end)
    }
}

#[derive(Default)]
struct MathScan {
    tokens: Vec<MathToken>,
    escapable_dollars: Vec<usize>,
    pending_display: Option<Range<usize>>,
}

#[cfg(test)]
fn scan_math(source: &str) -> MathScan {
    scan_math_with_context(source, true)
}

fn scan_math_in_parse_view(source: &str) -> MathScan {
    scan_math_with_context(source, false)
}

fn scan_math_with_context(source: &str, exclude_indented_code: bool) -> MathScan {
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
            if fence_boundary
                || code_fence.is_some()
                || (exclude_indented_code && line_indentation(line) > 3)
            {
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
        {
            if let Some(closing_start) =
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

                // Empty and dot-only examples are deliberately literal.
                // Consume the pair as one unit so its close cannot become a
                // later opener.
                for offset in ix..end {
                    if source.as_bytes()[offset] == b'$' && !is_escaped(source, offset) {
                        scan.escapable_dollars.push(offset);
                    }
                }
                ix = end;
                line_start = source[..ix].rfind('\n').map_or(0, |offset| offset + 1);
                continue;
            }

            if delimiter == MathDelimiter::DisplayDollar {
                // An explicit unclosed display opener owns the remaining
                // stream tail. Do not scan formula-body dollars as sibling
                // Markdown candidates; the next cold prefix parse will either
                // keep this pending range or recognize its eventual close.
                scan.escapable_dollars
                    .extend(ix..ix + delimiter.opening_len());
                scan.pending_display = Some(ix..source.len());
                break;
            }
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

    fn parse_inline_math<'a>(
        source: &'a str,
        prepared: &str,
    ) -> (markdown_ast::Node, Vec<&'a str>) {
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
                "strong" => {
                    count_nodes(&root, |node| matches!(node, markdown_ast::Node::Strong(_)))
                }
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
        let prepared =
            try_prepare_math_source(&closed).expect("closed display with real definition");
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
            let root = markdown::to_mdast(&source, &markdown::ParseOptions::gfm())
                .expect("definition AST");
            let identifier = root
                .children()
                .expect("document children")
                .iter()
                .find_map(|node| match node {
                    markdown_ast::Node::Definition(definition) => {
                        Some(definition.identifier.as_str())
                    }
                    _ => None,
                })
                .expect("reference definition");
            assert_eq!(normalize_reference_identifier(label), identifier);
        }
    }

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
}
