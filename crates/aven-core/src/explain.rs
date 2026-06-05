#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticExplanation {
    pub code: &'static str,
    pub text: &'static str,
}

pub fn explain(code: &str) -> Option<DiagnosticExplanation> {
    EXPLANATIONS
        .iter()
        .copied()
        .find(|explanation| explanation.code == code)
}

const EXPLANATIONS: &[DiagnosticExplanation] = &[
    DiagnosticExplanation {
        code: "layout.inconsistent-indentation",
        text: "A line dedented to a column that does not match an open layout block. Align it with an existing block level or change the surrounding indentation.",
    },
    DiagnosticExplanation {
        code: "lex.leading-bom",
        text: "The file starts with a UTF-8 byte-order mark. Aven source files should be plain UTF-8 without a leading BOM.",
    },
    DiagnosticExplanation {
        code: "lex.reserved-operator",
        text: "This symbolic operator starts with a reserved character sequence. Operators beginning with =, :, ., ?, or @ are reserved for language syntax.",
    },
    DiagnosticExplanation {
        code: "lex.tab-indentation",
        text: "Tabs are not accepted in indentation for v0. Use spaces so layout depth is stable across editors.",
    },
    DiagnosticExplanation {
        code: "lex.unexpected-character",
        text: "The lexer found a character that is not part of Aven source syntax. Remove it or rewrite it using a supported literal, identifier, or operator form.",
    },
    DiagnosticExplanation {
        code: "lex.unterminated-regex",
        text: "A regex literal was opened but not closed. Add the closing /, or use Regex.compile(pattern) when the pattern is dynamic.",
    },
    DiagnosticExplanation {
        code: "lex.unterminated-string",
        text: "A string literal was opened but not closed. Add the closing quote, or use a raw string form once multi-line string support exists.",
    },
    DiagnosticExplanation {
        code: "name.accidental-shadowing",
        text: "A local binding reuses a name that is already visible in an enclosing local scope. Rename it, or use the explicit shadowing syntax once implemented.",
    },
    DiagnosticExplanation {
        code: "name.duplicate-declaration",
        text: "Two top-level declarations have the same name and are not a clearly typed overload set. Rename one declaration or add complete annotations for overloads.",
    },
    DiagnosticExplanation {
        code: "name.duplicate-local",
        text: "Two local binders introduce the same name in one scope. Keep only one binding or rename one of them.",
    },
    DiagnosticExplanation {
        code: "name.unused-binding",
        text: "A local binding, parameter, or pattern binder is never used. Remove it, use it, or prefix the name with _ to mark the unused binding as intentional.",
    },
    DiagnosticExplanation {
        code: "name.uppercase-runtime-binding",
        text: "Uppercase names are reserved for compile-time identifiers. Runtime parameters must use lowercase names.",
    },
    DiagnosticExplanation {
        code: "parse.expected-expression",
        text: "The parser expected an expression term such as a literal, identifier, call, collection, lambda, or parenthesized group.",
    },
    DiagnosticExplanation {
        code: "parse.expected-field-name",
        text: "A field access or nil-safe field access must be followed by a field name.",
    },
    DiagnosticExplanation {
        code: "parse.expected-match-arrow",
        text: "A match arm pattern must be followed by => before its body expression.",
    },
    DiagnosticExplanation {
        code: "parse.expected-parameter",
        text: "A lambda parameter must be an identifier, or _ when the argument is intentionally ignored.",
    },
    DiagnosticExplanation {
        code: "parse.expected-pattern",
        text: "The parser expected a pattern term in a match arm. Use a literal, name, constructor call, tuple, record pattern, or _ wildcard.",
    },
    DiagnosticExplanation {
        code: "parse.expected-record-entry",
        text: "The parser expected a record entry such as a field, shorthand, spread, overwrite spread, delete, rename, or comprehension entry.",
    },
    DiagnosticExplanation {
        code: "parse.expected-record-label",
        text: "A record entry needs a valid field label. Use an identifier-style label for now; quoted labels are reserved for a later parser slice.",
    },
    DiagnosticExplanation {
        code: "parse.expected-type",
        text: "The parser expected a type annotation term after :. Type syntax uses the same expression grammar as value syntax.",
    },
    DiagnosticExplanation {
        code: "parse.inline-match-arms",
        text: "Match arms must be written as an indented block after ?>. Move the arms onto following indented lines.",
    },
    DiagnosticExplanation {
        code: "parse.invalid-binding-name",
        text: "A binding name must be a single identifier. Use a lowercase runtime identifier or uppercase compile-time identifier before =.",
    },
    DiagnosticExplanation {
        code: "parse.mismatched-delimiter",
        text: "A closing delimiter does not match the most recent open delimiter. Change it to the expected delimiter or fix the nested grouping.",
    },
    DiagnosticExplanation {
        code: "parse.missing-binding-name",
        text: "A binding is missing the name before =. Add a name such as value = expression.",
    },
    DiagnosticExplanation {
        code: "parse.missing-binding-value",
        text: "A binding is missing its value expression. Add an expression after =, or put an indented block on the following lines.",
    },
    DiagnosticExplanation {
        code: "parse.missing-lambda-body",
        text: "A lambda was introduced with => but no body followed. Add an expression body on the same line or an indented block.",
    },
    DiagnosticExplanation {
        code: "parse.missing-match-arms",
        text: "A ?> match expression must be followed by an indented block of pattern => body arms.",
    },
    DiagnosticExplanation {
        code: "parse.missing-match-body",
        text: "A match arm has a pattern and => but no body expression. Add the expression returned by that arm.",
    },
    DiagnosticExplanation {
        code: "parse.single-item-tuple",
        text: "Anonymous one-item tuples are not allowed. Remove the comma for grouping, or use a tagged one-item tuple such as Ok(value).",
    },
    DiagnosticExplanation {
        code: "parse.unclosed-delimiter",
        text: "A delimiter such as (, [, {, or @{ was opened but not closed. Add the matching closing delimiter.",
    },
    DiagnosticExplanation {
        code: "parse.unexpected-delimiter",
        text: "A closing delimiter appeared where no matching opener was active. Remove it or add the missing opener.",
    },
    DiagnosticExplanation {
        code: "parse.unexpected-indentation",
        text: "An indented line appeared where the parser was not expecting a layout block. Remove the indentation or introduce a block-form expression.",
    },
    DiagnosticExplanation {
        code: "parse.unexpected-separator",
        text: "A collection contains an extra comma or semicolon separator. Remove the duplicate separator.",
    },
    DiagnosticExplanation {
        code: "parse.unsupported-syntax",
        text: "The syntax is intentionally not supported by the current parser slice. Rewrite using currently supported operators or wait for the planned syntax milestone.",
    },
    DiagnosticExplanation {
        code: "type.lowercase-variant-tag",
        text: "Variant type members must use uppercase tags. Rename the tag to an uppercase marker such as Ok or Error.",
    },
    DiagnosticExplanation {
        code: "type.type-only-record-entry",
        text: "This record entry form is only meaningful in type position. Use it inside an annotation or replace it with a value-level record entry.",
    },
    DiagnosticExplanation {
        code: "type.unknown-name",
        text: "A type annotation references an uppercase name that is not a known builtin or in-scope compile-time declaration. Define it or correct the spelling.",
    },
];

#[cfg(test)]
mod tests {
    use super::{EXPLANATIONS, explain};

    #[test]
    fn looks_up_known_diagnostic_codes() {
        let explanation = explain("parse.unclosed-delimiter").expect("expected explanation");

        assert_eq!(explanation.code, "parse.unclosed-delimiter");
        assert!(explanation.text.contains("opened but not closed"));
    }

    #[test]
    fn returns_none_for_unknown_codes() {
        assert!(explain("parse.not-real").is_none());
    }

    #[test]
    fn explanation_codes_are_sorted_and_unique() {
        for pair in EXPLANATIONS.windows(2) {
            assert!(pair[0].code < pair[1].code);
        }
    }
}
