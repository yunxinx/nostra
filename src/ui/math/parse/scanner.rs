//! Delimiter scanning with code-fence and inline-code awareness.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct CodeFence {
    marker: u8,
    length: usize,
}

/// Update CommonMark fenced-code state and report whether `line` is itself a
/// fence boundary. Math recognition happens before mdast conversion, so it
/// must independently respect this block construct.
pub(super) fn update_code_fence(line: &str, state: &mut Option<CodeFence>) -> bool {
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

pub(super) fn line_indentation(line: &str) -> usize {
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
pub(super) enum MathDelimiter {
    Dollar,
    Parenthesized,
    DisplayDollar,
    DisplayBracket,
}

impl MathDelimiter {
    pub(super) fn opening_len(self) -> usize {
        match self {
            Self::Dollar => 1,
            Self::Parenthesized | Self::DisplayDollar | Self::DisplayBracket => 2,
        }
    }

    pub(super) fn closing_len(self) -> usize {
        self.opening_len()
    }

    pub(super) fn is_display(self) -> bool {
        matches!(self, Self::DisplayDollar | Self::DisplayBracket)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MathToken {
    pub(super) delimiter: MathDelimiter,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) body: Range<usize>,
    pub(super) block_range: Option<Range<usize>>,
}

impl MathToken {
    pub(super) fn delimiter_ranges(&self) -> (Range<usize>, Range<usize>) {
        (self.start..self.body.start, self.body.end..self.end)
    }
}

#[derive(Default)]
pub(super) struct MathScan {
    pub(super) tokens: Vec<MathToken>,
    pub(super) escapable_dollars: Vec<usize>,
    pub(super) pending_display: Option<Range<usize>>,
}

#[cfg(test)]
pub(super) fn scan_math(source: &str) -> MathScan {
    scan_math_with_context(source, true)
}

pub(super) fn scan_math_in_parse_view(source: &str) -> MathScan {
    scan_math_with_context(source, false)
}

pub(super) fn scan_math_with_context(source: &str, exclude_indented_code: bool) -> MathScan {
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
                    let after_close = source[end..].chars().next();
                    if delimiter == MathDelimiter::Dollar
                        && body_is_currency_like(&source[body.clone()], after_close)
                    {
                        // Reject the pair without consuming the closer so a
                        // later `$x$` after `$5; equation $x$` can still match.
                        scan.escapable_dollars.push(ix);
                        ix += delimiter.opening_len();
                        continue;
                    }
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

pub(super) fn math_delimiter_at(source: &str, ix: usize) -> Option<MathDelimiter> {
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

pub(super) fn find_math_close(
    source: &str,
    start: usize,
    delimiter: MathDelimiter,
) -> Option<usize> {
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
                    let followed_by_digit = source[ix + 1..]
                        .chars()
                        .next()
                        .is_some_and(|next| next.is_ascii_digit());
                    if !followed_by_digit {
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

pub(super) fn exact_dollar_run(source: &str, ix: usize, length: usize) -> bool {
    count_run(source, ix, b'$') == Some(length)
}

pub(super) fn standalone_block_range(
    source: &str,
    start: usize,
    end: usize,
) -> Option<Range<usize>> {
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

pub(super) fn is_ellipsis_placeholder(body: &str) -> bool {
    !body.is_empty()
        && body
            .chars()
            .all(|character| matches!(character, '.' | '…' | '⋯'))
}

pub(super) fn is_valid_dollar_opener(source: &str, ix: usize) -> bool {
    if is_escaped(source, ix)
        || source.as_bytes().get(ix.wrapping_sub(1)) == Some(&b'$')
        || source.as_bytes().get(ix + 1) == Some(&b'$')
    {
        return false;
    }

    // Inner whitespace is allowed (`$ x $`). EOF after `$` cannot open a pair.
    !source[ix + 1..].is_empty()
}

/// Currency, range, and placeholder bodies must stay prose.
///
/// `after_close` is the first character after the candidate closing `$`.
pub(super) fn body_is_currency_like(body: &str, after_close: Option<char>) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_ellipsis_placeholder(trimmed) {
        return true;
    }
    if after_close.is_some_and(|character| character.is_ascii_digit()) && is_range_amount(trimmed) {
        return true;
    }
    is_amount_prose(trimmed)
}

fn is_range_amount(trimmed: &str) -> bool {
    let Some(last) = trimmed
        .chars()
        .last()
        .filter(|character| matches!(character, '~' | '～' | '-'))
    else {
        return false;
    };
    let number = &trimmed[..trimmed.len() - last.len_utf8()];
    is_plain_amount(number)
}

fn is_amount_prose(trimmed: &str) -> bool {
    let Some(amount_len) = leading_amount_len(trimmed) else {
        return false;
    };
    let rest = &trimmed[amount_len..];
    rest.is_empty()
        || rest.chars().next().is_some_and(|character| {
            character.is_whitespace() || is_currency_punctuation(character)
        })
}

fn is_currency_punctuation(character: char) -> bool {
    matches!(
        character,
        ',' | ';' | ':' | '!' | '?' | '.' | '"' | '\'' | ')' | ']' | '/'
    )
}

fn is_plain_amount(value: &str) -> bool {
    leading_amount_len(value).is_some_and(|len| len == value.len())
}

fn leading_amount_len(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    while index + 4 <= bytes.len()
        && bytes[index] == b','
        && bytes[index + 1..index + 4].iter().all(u8::is_ascii_digit)
    {
        index += 4;
    }
    if index < bytes.len() && bytes[index] == b'.' {
        let decimal_start = index + 1;
        if decimal_start < bytes.len() && bytes[decimal_start].is_ascii_digit() {
            index = decimal_start;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
        }
    }
    Some(index)
}

pub(super) fn count_run(source: &str, ix: usize, needle: u8) -> Option<usize> {
    if source.as_bytes().get(ix) != Some(&needle) {
        return None;
    }
    let mut end = ix + 1;
    while source.as_bytes().get(end) == Some(&needle) {
        end += 1;
    }
    Some(end - ix)
}

pub(super) fn is_escaped(source: &str, ix: usize) -> bool {
    let mut backslashes = 0;
    let mut cursor = ix;
    while cursor > 0 && source.as_bytes()[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}
