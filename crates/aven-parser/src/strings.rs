use std::char;

/// The common delimiter shape for ordinary and raw text literals.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StringDelimiter {
    pub raw: bool,
    pub multiline: bool,
    pub hashes: usize,
    pub opening_len: usize,
}

impl StringDelimiter {
    pub fn at(text: &str) -> Option<Self> {
        let raw = text.starts_with('r');
        let prefix = usize::from(raw);
        let hashes = if raw {
            text[prefix..].bytes().take_while(|byte| *byte == b'#').count()
        } else { 0 };
        let quoted = text.get(prefix + hashes..)?;
        if !quoted.starts_with('"') { return None; }
        let multiline = quoted.starts_with("\"\"\"");
        Some(Self { raw, multiline, hashes, opening_len: prefix + hashes + if multiline { 3 } else { 1 } })
    }

    pub fn closing(self) -> String {
        format!("{}{}", if self.multiline { "\"\"\"" } else { "\"" }, "#".repeat(self.hashes))
    }
}

/// Byte ranges retained from a multiline body. Working in source coordinates
/// keeps interpolation and diagnostic spans independent of newline normalization.
pub(crate) fn multiline_content_ranges(
    text: &str,
    opening_len: usize,
    closing_start: usize,
) -> Result<Vec<std::ops::Range<usize>>, std::ops::Range<usize>> {
    let newline_len = |tail: &str| if tail.starts_with("\r\n") { 2 } else { 1 };
    if !text[opening_len..].starts_with(['\r', '\n']) {
        return Err(opening_len..opening_len);
    }
    let body_start = opening_len + newline_len(&text[opening_len..]);
    let closing_line = text[..closing_start].rfind(['\r', '\n']).map_or(0, |index| index + 1);
    if !text[closing_line..closing_start].bytes().all(|byte| byte == b' ') {
        return Err(closing_line..closing_start);
    }
    let margin = closing_start - closing_line;
    let mut ranges = Vec::new();
    let mut line = body_start;
    while line < closing_line {
        let end = text[line..closing_line].find(['\r', '\n']).map_or(closing_line, |index| line + index);
        let next = if end < closing_line { end + newline_len(&text[end..]) } else { end };
        let content = &text[line..end];
        let spaces = content.bytes().take_while(|byte| *byte == b' ').count();
        let blank = content.bytes().all(|byte| matches!(byte, b' ' | b'\t'));
        if spaces < margin && !blank { return Err(line..end); }
        let stripped = if blank && spaces < margin { end } else { line + margin };
        ranges.push(stripped..end);
        // The newline immediately before the closer is a delimiter boundary.
        if next < closing_line { ranges.push(end..next); }
        line = next;
    }
    Ok(ranges)
}

pub(crate) fn retained_fragment(text: &str, fragment: std::ops::Range<usize>, ranges: &[std::ops::Range<usize>]) -> String {
    let mut result = String::new();
    for range in ranges {
        let start = range.start.max(fragment.start);
        let end = range.end.min(fragment.end);
        if start < end { result.push_str(&text[start..end]); }
    }
    result.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn decode_string_literal(text: &str) -> String {
    let Some(delimiter) = StringDelimiter::at(text) else { return decode_string_fragment(text); };
    let closing = delimiter.closing();
    let Some(inner_end) = text.strip_suffix(&closing).map(str::len).filter(|end| *end >= delimiter.opening_len) else {
        return decode_string_fragment(text);
    };
    let inner = if delimiter.multiline {
        multiline_content_ranges(text, delimiter.opening_len, inner_end)
            .map(|ranges| retained_fragment(text, 0..text.len(), &ranges))
            .unwrap_or_else(|_| text[delimiter.opening_len..inner_end].to_owned())
    } else {
        text[delimiter.opening_len..inner_end].to_owned()
    };
    if delimiter.raw { inner } else { decode_string_fragment(&inner) }
}

pub(crate) fn decode_string_fragment(text: &str) -> String {
    let mut decoded = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }

        let Some(escaped) = chars.next() else {
            decoded.push('\\');
            break;
        };

        match escaped {
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '"' => decoded.push('"'),
            '\\' => decoded.push('\\'),
            '$' => decoded.push('$'),
            'u' => decode_unicode_escape(&mut chars, &mut decoded),
            other => decoded.push(other),
        }
    }

    decoded
}

fn decode_unicode_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    decoded: &mut String,
) {
    if chars.peek() != Some(&'{') {
        decoded.push('u');
        return;
    }

    let mut raw = String::new();
    let mut hex = String::new();

    if let Some(open) = chars.next() {
        raw.push(open);
    }
    for ch in chars.by_ref() {
        raw.push(ch);
        if ch == '}' {
            if let Ok(value) = u32::from_str_radix(&hex, 16)
                && let Some(scalar) = char::from_u32(value)
            {
                decoded.push(scalar);
                return;
            }

            decoded.push('u');
            decoded.push_str(&raw);
            return;
        }

        hex.push(ch);
    }

    decoded.push('u');
    decoded.push_str(&raw);
}

#[cfg(test)]
mod tests {
    use super::decode_string_literal;

    #[test]
    fn decodes_supported_escapes() {
        assert_eq!(decode_string_literal(r#""\\\"\n\r\t""#), "\\\"\n\r\t");
        assert_eq!(decode_string_literal(r#""\$""#), "$");
        assert_eq!(decode_string_literal(r#""a\${b}""#), "a${b}");
    }

    #[test]
    fn decodes_unicode_scalar_escape() {
        assert_eq!(decode_string_literal(r#""\u{41}""#), "A");
        assert_eq!(decode_string_literal(r#""\u{1f600}""#), "\u{1f600}");
    }

    #[test]
    fn preserves_malformed_escapes_leniently() {
        assert_eq!(decode_string_literal(r#""\q""#), "q");
        assert_eq!(decode_string_literal(r#""\u""#), "u");
        assert_eq!(decode_string_literal(r#""\u{zz}""#), "u{zz}");
    }
}
