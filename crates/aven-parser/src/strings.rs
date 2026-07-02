use std::char;

pub fn decode_string_literal(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|stripped| stripped.strip_suffix('"'))
        .unwrap_or(text);

    decode_string_fragment(inner)
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
