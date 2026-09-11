use aven_parser::{
    ExprKind, InterpolationSegment, Item, Literal, decode_string_literal, parse_module,
};

fn expression(source: &str) -> aven_parser::Expr {
    let parsed = parse_module(source);
    assert!(
        parsed.diagnostics.is_empty(),
        "{source:?}: {:?}",
        parsed.diagnostics
    );
    let Item::Expr(expr) = parsed.module.items.into_iter().next().expect("one item") else {
        panic!("expression")
    };
    expr
}

fn literal(source: &str) -> String {
    let ExprKind::Literal(Literal::String(text)) = expression(source).kind else {
        panic!("text literal")
    };
    decode_string_literal(&text)
}

#[test]
fn raw_delimiters_preserve_escapes_and_interpolation_markers() {
    assert_eq!(literal(r##"r"\n${name}\""##), "\\n${name}\\");
    assert_eq!(
        literal(r###"r##"a "# b " c\u{41}${x}"##"###),
        "a \"# b \" c\\u{41}${x}"
    );
    assert_eq!(
        literal("r#\"\"\"\n  a \"\"\" b\\n${x}\n  \"\"\"#"),
        "a \"\"\" b\\n${x}"
    );
}

#[test]
fn triple_strings_dedent_and_normalize_before_decoding_escapes() {
    assert_eq!(
        literal("\"\"\"\n    first\n      second\n    \"\"\""),
        "first\n  second"
    );
    assert_eq!(
        literal("\"\"\"\r\n  a\\r\r\n\r\n  b\r\n  \"\"\""),
        "a\r\n\nb"
    );
    assert_eq!(literal("r\"\"\"\r  a\\r\r  b\r  \"\"\""), "a\\r\nb");
    assert_eq!(literal("\"\"\"\n  a\n\n  \"\"\""), "a\n");
    assert_eq!(literal("\"\"\"\n  a\n \n    \n  b\n  \"\"\""), "a\n\n  \nb");
    assert_eq!(literal("\"\"\"\n\"\"\""), "");
    assert_eq!(literal("\"\"\"\n  \tdata\n  \"\"\""), "\tdata");
}

#[test]
fn interpolation_fragments_dedent_without_changing_expression_spans() {
    let source = "\"\"\"\r\n  hello ${name}\r\n    ${r#\"\\n\"#}!\r\n  \"\"\"";
    let ExprKind::Interpolation(segments) = expression(source).kind else {
        panic!("interpolation")
    };
    assert_eq!(segments[0], InterpolationSegment::Text("hello ".into()));
    assert_eq!(segments[2], InterpolationSegment::Text("\n  ".into()));
    assert_eq!(segments[4], InterpolationSegment::Text("!".into()));
    let InterpolationSegment::Expr(name) = &segments[1] else {
        panic!("name")
    };
    assert_eq!(&source[name.span.start..name.span.end], "name");
    let InterpolationSegment::Expr(raw) = &segments[3] else {
        panic!("raw")
    };
    assert_eq!(&source[raw.span.start..raw.span.end], r##"r#"\n"#"##);
}

/// Blanks after the opening delimiter are invisible in an editor and belong to
/// no line of the value, so they are skipped. Java, Kotlin and Swift all accept
/// them, and an editor that trims-on-save must not change a program's meaning.
#[test]
fn blanks_after_the_opening_delimiter_are_not_content() {
    assert_eq!(literal("\"\"\"  \n  a\n  \"\"\""), "a");
    assert_eq!(literal("\"\"\"\t\r\n  a\r\n  \"\"\""), "a");
    assert_eq!(literal("r\"\"\" \n  a\n  \"\"\""), "a");
    // Anything else on the opening line still has no margin to dedent against.
    assert!(
        parse_module("\"\"\" x\n  a\n  \"\"\"")
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("lex.invalid-multiline-string"))
    );
}

#[test]
fn rejects_malformed_multiline_layout_and_unmatched_delimiters() {
    for source in [
        "\"\"\"text\n\"\"\"",
        "\"\"\"\n a\n  \"\"\"",
        "\"\"\"\n  text\"\"\"",
        "\"\"\"\n\tx\n\t\"\"\"",
    ] {
        let parsed = parse_module(source);
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("lex.invalid-multiline-string")),
            "{source:?}: {:?}",
            parsed.diagnostics
        );
    }
    for source in [
        "r#\"unterminated\"",
        "r##\"wrong\"#",
        "r\"\"\"\nmissing",
        "\"\"\"\n${name}\n",
    ] {
        assert!(
            parse_module(source)
                .diagnostics
                .iter()
                .any(aven_core::Diagnostic::is_error),
            "{source:?}"
        );
    }
    // The public decoder is intentionally lenient even without a successful lex.
    for malformed in ["", "r", "r#", "\"", "\"\"\"", "r#\"é", "\"\"\"é\"\"\""] {
        let _ = decode_string_literal(malformed);
    }
}

/// Temporary dump of the four historical string-literal findings.
#[test]
fn dump_string_literal_findings() {
    fn dump(label: &str, source: &str) {
        let parsed = parse_module(source);
        eprintln!("=== {label} ===\nsource:\n{source}\ndiags ({})", parsed.diagnostics.len());
        for diagnostic in &parsed.diagnostics {
            eprintln!(
                "  {} {}: {}",
                diagnostic.severity,
                diagnostic.code.as_deref().unwrap_or("?"),
                diagnostic.message
            );
            for label in &diagnostic.labels {
                eprintln!("    label {}..{}: {}", label.span.start, label.span.end, label.message);
            }
            for note in &diagnostic.notes {
                eprintln!("    note: {note}");
            }
        }
    }

    dump(
        "1 wrapped interpolation",
        "q = \"\"\"\n  ${\"a\"\n    + \"b\"}\n  \"\"\"\n",
    );
    dump(
        "1 wrapped interpolation parenthesized",
        "q = \"\"\"\n  ${\n    \"a\" + \"b\"\n  }\n  \"\"\"\n",
    );
    dump(
        "1 wrapped interpolation call",
        "q = \"\"\"\n  ${join(\n    \"a\",\n    \"b\"\n  )}\n  \"\"\"\n",
    );
    dump(
        "2 hole under-indented",
        "q = \"\"\"\n  head ${\n x}\n  \"\"\"\n",
    );
    dump(
        "2 hole at margin",
        "q = \"\"\"\n  head ${\n  x}\n  \"\"\"\n",
    );
    dump(
        "2 hole deeper than margin",
        "q = \"\"\"\n  head ${\n    x}\n  \"\"\"\n",
    );
    dump("3 opener with content no closer", "q = \"\"\"text\n");
    dump("3 opener with content and closer same line", "q = \"\"\"text\"\"\"\n");
    dump("3 opener with content then proper closer", "q = \"\"\"text\n  a\n  \"\"\"\n");
    dump("3 opener blanks then content on same line", "q = \"\"\"  x\n  a\n  \"\"\"\n");
}
