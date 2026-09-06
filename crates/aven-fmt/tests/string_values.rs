//! The formatter may move a multiline literal's margin; it may never change
//! what the literal decodes to.
//!
//! A golden fixture proves one layout. This walks the decoded *values* on both
//! sides of a format, which is the property the margin rule exists to protect:
//! reindenting content and closer together is only correct if the text that
//! comes out is byte-identical.

use aven_core::Diagnostic;
use aven_parser::{TokenKind, decode_string_literal, lex_source};

/// Every string payload in a source, in order, already decoded.
///
/// Interpolation fragments arrive as their own tokens carrying the same
/// spelling a whole literal would, so decoding each one covers the segments
/// between `${...}` holes as well as plain literals.
fn decoded_payloads(source: &str) -> Vec<String> {
    let lexed = lex_source(source);
    assert!(
        !lexed.diagnostics.iter().any(Diagnostic::is_error),
        "{source:?}: {:?}",
        lexed.diagnostics
    );
    lexed
        .tokens
        .iter()
        .filter_map(|token| match &token.kind {
            TokenKind::StringLiteral(text)
            | TokenKind::InterpolationStart(text)
            | TokenKind::InterpolationMiddle(text)
            | TokenKind::InterpolationEnd(text) => Some(decode_string_literal(text)),
            _ => None,
        })
        .collect()
}

fn assert_values_survive_formatting(source: &str) {
    let before = decoded_payloads(source);
    assert!(!before.is_empty(), "{source:?} has no string payload");
    let formatted = aven_fmt::format_source(source)
        .unwrap_or_else(|diagnostics| panic!("{source:?}: {diagnostics:?}"));
    assert_eq!(
        decoded_payloads(&formatted),
        before,
        "formatting changed a literal's value\n--- before ---\n{source}\n--- after ---\n{formatted}"
    );
    let twice = aven_fmt::format_source(&formatted)
        .unwrap_or_else(|diagnostics| panic!("{formatted:?}: {diagnostics:?}"));
    assert_eq!(
        twice, formatted,
        "formatting is not idempotent for {source:?}"
    );
}

#[test]
fn reindenting_a_multiline_literal_preserves_its_value() {
    for source in [
        // Content deeper than the margin keeps its relative indentation.
        "q = \"\"\"\n      select *\n        from t\n      \"\"\"\n",
        // A blank line, and a whitespace-only line longer than the margin.
        "q = \"\"\"\n  a\n\n     \n  b\n  \"\"\"\n",
        // A trailing blank content line is the way to request a final newline.
        "q = \"\"\"\n  a\n\n  \"\"\"\n",
        // Tabs after the margin are payload, not indentation.
        "q = \"\"\"\n  \tcol\n  \"\"\"\n",
        // Escapes still decode in an ordinary triple-quoted literal.
        "q = \"\"\"\n  a\\tb\\u{41}\n  \"\"\"\n",
        // A raw triple-quoted literal decodes none of that.
        "q = r\"\"\"\n  a\\tb${x}\n  \"\"\"\n",
        // Hashes let the payload carry the closing spelling itself.
        "q = r#\"\"\"\n  a \"\"\" b\n  \"\"\"#\n",
        // Interpolation, including a nested raw literal inside the hole.
        "q = \"\"\"\n  head ${name}\n    ${r#\"\\n\"#} tail\n  \"\"\"\n",
        // The literal is nested inside indented syntax, so the margin moves.
        "f = (v) =>\n  v ?>\n    0 =>\n        \"\"\"\n          zero\n          \"\"\"\n    _ => \"other\"\n",
        // Two literals at different depths in one expression.
        "pair = (\n  \"\"\"\n    left\n    \"\"\",\n  \"\"\"\n      right\n      \"\"\"\n)\n",
    ] {
        assert_values_survive_formatting(source);
    }
}

/// A source with no LF at all still formats, and the literal it carries decodes
/// to the same normalized text a LF source would give.
#[test]
fn carriage_return_only_sources_format_without_changing_values() {
    let cr = "a = 1\rq = \"\"\"\r  one\r  two\r  \"\"\"\rb = 2\r";
    let lf = cr.replace('\r', "\n");
    assert_eq!(decoded_payloads(cr), decoded_payloads(&lf));
    let formatted = aven_fmt::format_source(cr)
        .unwrap_or_else(|diagnostics| panic!("CR-only source: {diagnostics:?}"));
    assert_eq!(decoded_payloads(&formatted), decoded_payloads(&lf));
    // The bindings around the literal must survive too, not just its payload.
    assert!(formatted.contains("a = 1"), "{formatted:?}");
    assert!(formatted.contains("b = 2"), "{formatted:?}");
}
