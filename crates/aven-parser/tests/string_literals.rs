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
    // Anything else on the opening line still has no margin to dedent against,
    // and that one defect is one diagnostic.
    let opener = parse_module("\"\"\" x\n  a\n  \"\"\"");
    assert_eq!(
        opener
            .diagnostics
            .iter()
            .map(|d| d.code.as_deref())
            .collect::<Vec<_>>(),
        [Some("lex.invalid-multiline-string")],
        "{:?}",
        opener.diagnostics
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

fn diagnostic_codes(source: &str) -> Vec<Option<String>> {
    parse_module(source)
        .diagnostics
        .iter()
        .map(|d| d.code.clone())
        .collect()
}

fn interpolation_expr(source: &str) -> Vec<InterpolationSegment> {
    let ExprKind::Interpolation(segments) = expression(source).kind else {
        panic!("interpolation")
    };
    segments
}

#[test]
fn wrapped_interpolation_in_a_multiline_string_parses() {
    let source = "\"\"\"\n  ${\n    \"a\" + \"b\"\n  }\n  \"\"\"";
    let segments = interpolation_expr(source);
    assert_eq!(segments[0], InterpolationSegment::Text(String::new()));
    let InterpolationSegment::Expr(value) = &segments[1] else {
        panic!("expr")
    };
    assert_eq!(&source[value.span.start..value.span.end], "\"a\" + \"b\"");
    assert_eq!(segments[2], InterpolationSegment::Text(String::new()));

    let call = "\"\"\"\n  ${join(\n    \"a\",\n    \"b\"\n  )}\n  \"\"\"";
    let segments = interpolation_expr(call);
    let InterpolationSegment::Expr(value) = &segments[1] else {
        panic!("call")
    };
    assert_eq!(
        &call[value.span.start..value.span.end],
        "join(\n    \"a\",\n    \"b\"\n  )"
    );

    let nested = "\"\"\"\n  outer ${\"\"\"\n    inner ${x}\n    \"\"\"} tail\n  \"\"\"";
    assert!(
        parse_module(nested).diagnostics.is_empty(),
        "{:?}",
        parse_module(nested).diagnostics
    );

    let chain = "\"\"\"\n  ${value\n    .foo()}\n  \"\"\"";
    let segments = interpolation_expr(chain);
    let InterpolationSegment::Expr(value) = &segments[1] else {
        panic!("chain")
    };
    assert_eq!(
        &chain[value.span.start..value.span.end],
        "value\n    .foo()"
    );
}

#[test]
fn infix_split_across_lines_is_not_a_continued_interpolation() {
    // Matching `("a"\n  + "b")`: a newline does not continue `+`.
    let source = "\"\"\"\n  ${\"a\"\n    + \"b\"}\n  \"\"\"";
    let codes = diagnostic_codes(source);
    assert!(
        codes
            .iter()
            .any(|code| code.as_deref() == Some("parse.interpolation-continuation")),
        "{codes:?}"
    );
    assert!(
        !codes
            .iter()
            .any(|code| code.as_deref() == Some("lex.unterminated-interpolation")),
        "lexer must not reject a multiline hole as unterminated: {codes:?}"
    );
}

#[test]
fn margin_validation_includes_lines_inside_interpolation_holes() {
    let under = "\"\"\"\n  head ${\n x\n  }\n  \"\"\"";
    assert_eq!(
        diagnostic_codes(under),
        [Some("lex.invalid-multiline-string".into())],
        "{under:?}"
    );

    let at_margin = "\"\"\"\n  head ${\n  x\n  }\n  \"\"\"";
    assert!(
        parse_module(at_margin).diagnostics.is_empty(),
        "{:?}",
        parse_module(at_margin).diagnostics
    );

    let deeper = "\"\"\"\n  head ${\n    x\n  }\n  \"\"\"";
    assert!(
        parse_module(deeper).diagnostics.is_empty(),
        "{:?}",
        parse_module(deeper).diagnostics
    );
}

#[test]
fn malformed_triple_opener_is_one_diagnostic() {
    for source in [
        "\"\"\"text",
        "\"\"\"text\"\"\"",
        "\"\"\"text\n  a\n  \"\"\"",
        "\"\"\"  x\n  a\n  \"\"\"",
    ] {
        let codes = diagnostic_codes(source);
        assert_eq!(
            codes,
            [Some("lex.invalid-multiline-string".into())],
            "{source:?}: {codes:?}"
        );
        assert!(
            parse_module(source).diagnostics[0]
                .message
                .contains("must start on the next line"),
            "{source:?}: {:?}",
            parse_module(source).diagnostics
        );
    }
}
