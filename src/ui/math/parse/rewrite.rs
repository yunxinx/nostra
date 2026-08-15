//! Reference synchronization and adaptive parse-view rewriting.

use super::*;

/// Keep definition lookup stable when a shortcut/collapsed link label needs a
/// length-preserving parse-only rewrite. markdown-rs derives those reference
/// identifiers from visible label source, so changing only the label would
/// silently turn the link back into plain text.
pub(super) fn synchronize_implicit_reference_identifiers(
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
pub(super) fn normalize_reference_identifier(value: &str) -> String {
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

pub(super) fn restore_implicit_reference_labels(
    source: &str,
    ranges: &[Range<usize>],
    prepared: &mut [u8],
) {
    for range in ranges {
        if let Some(original) = source.get(range.clone()) {
            prepared[range.clone()].copy_from_slice(original.as_bytes());
        }
    }
}

pub(super) fn mirror_reference_identifier(
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

pub(super) fn prepare_visible_math_token(source: &str, token: &MathToken, prepared: &mut [u8]) {
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

pub(super) fn mask_literal_backticks(
    source: &str,
    ranges: &MarkdownSourceRanges,
    prepared: &mut [u8],
) {
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
pub(super) fn conservative_reference_fallback(
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

pub(super) fn stabilize_math_parse_view(
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

pub(super) fn mask_next_native_hazard_dollar(
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
pub(super) fn suppress_unclaimed_math_nodes(
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

pub(super) fn mask_unclaimed_delimiter_pair(
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

pub(super) fn delimiter_is_in_reference_association(
    delimiter: &Range<usize>,
    ranges: &MarkdownSourceRanges,
) -> bool {
    ranges
        .reference_associations
        .iter()
        .any(|association| range_is_covered(std::slice::from_ref(&association.source), delimiter))
}

pub(super) fn delimiter_is_mutable(
    delimiter: &Range<usize>,
    ranges: &MarkdownSourceRanges,
) -> bool {
    range_is_covered(&ranges.preparable, delimiter)
        || range_is_covered(&ranges.restorable, delimiter)
        || range_is_covered(&ranges.authoritative_image_alt, delimiter)
        || delimiter_is_in_reference_association(delimiter, ranges)
}
