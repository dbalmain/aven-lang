use std::collections::HashMap;

use aven_core::Span;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensLegend,
};

use super::{ParsedDocument, span_to_range};

const TOKEN_COMMENT: u32 = 0;
const TOKEN_STRING: u32 = 1;
const TOKEN_NUMBER: u32 = 2;
const TOKEN_REGEXP: u32 = 3;
const TOKEN_OPERATOR: u32 = 4;
const TOKEN_VARIABLE: u32 = 5;
const TOKEN_TYPE: u32 = 6;
const TOKEN_FUNCTION: u32 = 7;
const TOKEN_PARAMETER: u32 = 8;
const TOKEN_PROPERTY: u32 = 9;

const MODIFIER_DEFINITION: u32 = 1 << 0;
const MODIFIER_DOCUMENTATION: u32 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticStyle {
    token_type: u32,
    modifiers: u32,
}

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::REGEXP,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::TYPE,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::PROPERTY,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::DOCUMENTATION,
        ],
    }
}

pub(crate) fn tokens(document: &ParsedDocument) -> SemanticTokens {
    let styles = semantic_token_styles(document);
    let mut builder = SemanticTokenBuilder::default();

    for token in &document.parse_output().raw_tokens {
        let style = styles
            .get(&token.span)
            .copied()
            .or_else(|| semantic_style_for_token(&token.kind));

        if let Some(style) = style {
            builder.push(document, token.span, style);
        }
    }

    SemanticTokens {
        result_id: None,
        data: builder.tokens,
    }
}

fn semantic_token_styles(document: &ParsedDocument) -> HashMap<Span, SemanticStyle> {
    let mut collector = StyleCollector::new(document);
    collector.collect_items(&document.parse_output().module.items);
    collector.styles
}

/// AST-derived style overrides that take precedence over a token's bare lexical
/// classification: top-level definitions (resolved through the merged
/// declaration list so a signature and its binding agree), nested binders, and
/// record field labels.
struct StyleCollector {
    top_level: HashMap<Span, SemanticStyle>,
    styles: HashMap<Span, SemanticStyle>,
}

impl StyleCollector {
    fn new(document: &ParsedDocument) -> Self {
        let declarations = document.declarations();
        let mut top_level = HashMap::new();

        // Top-level names use the merged-declaration style so both halves of a
        // `signature + binding` pair classify the same, where nested binders
        // (no declaration) fall back to their bare item style.
        for item in &document.parse_output().module.items {
            let name_span = match item {
                aven_parser::Item::Binding(binding) => binding.name_span,
                aven_parser::Item::Signature(signature) => signature.name_span,
                aven_parser::Item::Expr(_) => continue,
            };

            if let Some(declaration) = declarations
                .iter()
                .find(|declaration| declaration.span.contains(name_span))
            {
                top_level.insert(name_span, declaration_semantic_style(declaration));
            }
        }

        Self {
            top_level,
            styles: HashMap::new(),
        }
    }

    fn collect_items(&mut self, items: &[aven_parser::Item]) {
        for item in items {
            match item {
                aven_parser::Item::Binding(binding) => {
                    let style = self
                        .top_level
                        .get(&binding.name_span)
                        .copied()
                        .unwrap_or_else(|| binding_semantic_style(binding));
                    self.styles.insert(binding.name_span, style);
                    if let Some(annotation) = &binding.annotation {
                        self.collect_expr(annotation);
                    }
                    self.collect_expr(&binding.value);
                }
                aven_parser::Item::Signature(signature) => {
                    let style = self
                        .top_level
                        .get(&signature.name_span)
                        .copied()
                        .unwrap_or_else(|| signature_semantic_style(signature));
                    self.styles.insert(signature.name_span, style);
                    self.collect_expr(&signature.annotation);
                }
                aven_parser::Item::Expr(expr) => self.collect_expr(expr),
            }
        }
    }

    fn collect_expr(&mut self, expr: &aven_parser::Expr) {
        match &expr.kind {
            aven_parser::ExprKind::Lambda {
                params,
                return_annotation,
                body,
            } => {
                for param in params {
                    self.styles.insert(
                        param.name_span,
                        SemanticStyle {
                            token_type: TOKEN_PARAMETER,
                            modifiers: MODIFIER_DEFINITION,
                        },
                    );
                    if let Some(annotation) = &param.annotation {
                        self.collect_expr(annotation);
                    }
                }

                if let Some(annotation) = return_annotation {
                    self.collect_expr(annotation);
                }
                self.collect_expr(body);
            }
            aven_parser::ExprKind::Block(items) => self.collect_items(items),
            aven_parser::ExprKind::Record(entries) | aven_parser::ExprKind::Set(entries) => {
                self.collect_record_entries(entries);
                aven_parser::walk_expr_children(expr, &mut |child| self.collect_expr(child));
            }
            _ => aven_parser::walk_expr_children(expr, &mut |child| self.collect_expr(child)),
        }
    }

    fn collect_record_entries(&mut self, entries: &[aven_parser::RecordEntry]) {
        let property = SemanticStyle {
            token_type: TOKEN_PROPERTY,
            modifiers: 0,
        };

        for entry in entries {
            match entry {
                aven_parser::RecordEntry::Field { name_span, .. }
                | aven_parser::RecordEntry::Shorthand { name_span, .. }
                | aven_parser::RecordEntry::Delete { name_span, .. } => {
                    self.styles.insert(*name_span, property);
                }
                aven_parser::RecordEntry::Rename {
                    from_span, to_span, ..
                } => {
                    self.styles.insert(*from_span, property);
                    self.styles.insert(*to_span, property);
                }
                aven_parser::RecordEntry::Spread { .. }
                | aven_parser::RecordEntry::Open { .. }
                | aven_parser::RecordEntry::Element(_) => {}
            }
        }
    }
}

fn definition_style(is_comptime: bool, is_callable: bool) -> SemanticStyle {
    let token_type = if is_comptime {
        TOKEN_TYPE
    } else if is_callable {
        TOKEN_FUNCTION
    } else {
        TOKEN_VARIABLE
    };

    SemanticStyle {
        token_type,
        modifiers: MODIFIER_DEFINITION,
    }
}

fn declaration_semantic_style(declaration: &aven_parser::Declaration) -> SemanticStyle {
    let is_callable = match declaration.kind {
        aven_parser::DeclarationKind::Function => true,
        aven_parser::DeclarationKind::Signature => {
            matches!(
                declaration.shape,
                aven_parser::DeclarationShape::Callable(_)
            )
        }
        aven_parser::DeclarationKind::Binding => false,
    };

    definition_style(
        declaration.phase == aven_parser::DeclarationPhase::Comptime,
        is_callable,
    )
}

fn binding_semantic_style(binding: &aven_parser::Binding) -> SemanticStyle {
    definition_style(
        aven_parser::is_comptime_identifier_name(&binding.name),
        matches!(binding.value.kind, aven_parser::ExprKind::Lambda { .. }),
    )
}

fn signature_semantic_style(signature: &aven_parser::Signature) -> SemanticStyle {
    definition_style(
        aven_parser::is_comptime_identifier_name(&signature.name),
        matches!(
            signature.annotation.kind,
            aven_parser::ExprKind::Arrow { .. }
        ),
    )
}

fn semantic_style_for_token(kind: &aven_parser::TokenKind) -> Option<SemanticStyle> {
    let token_type = match kind {
        aven_parser::TokenKind::Identifier(_) | aven_parser::TokenKind::ComptimeParamMarker(_) => {
            TOKEN_VARIABLE
        }
        aven_parser::TokenKind::ComptimeIdentifier(_) => TOKEN_TYPE,
        aven_parser::TokenKind::Number(_) => TOKEN_NUMBER,
        aven_parser::TokenKind::StringLiteral(_) | aven_parser::TokenKind::PathLiteral(_) => {
            TOKEN_STRING
        }
        aven_parser::TokenKind::RegexLiteral(_) => TOKEN_REGEXP,
        aven_parser::TokenKind::LabelPath(_) | aven_parser::TokenKind::Tag(_) => TOKEN_PROPERTY,
        aven_parser::TokenKind::Operator(_) => TOKEN_OPERATOR,
        aven_parser::TokenKind::Comment(_) => TOKEN_COMMENT,
        aven_parser::TokenKind::DocComment(_) => TOKEN_COMMENT,
        aven_parser::TokenKind::OpenParen
        | aven_parser::TokenKind::CloseParen
        | aven_parser::TokenKind::OpenBrace
        | aven_parser::TokenKind::CloseBrace
        | aven_parser::TokenKind::OpenBracket
        | aven_parser::TokenKind::CloseBracket
        | aven_parser::TokenKind::Comma
        | aven_parser::TokenKind::Semicolon
        | aven_parser::TokenKind::RawNewline
        | aven_parser::TokenKind::RawIndent { .. }
        | aven_parser::TokenKind::Newline
        | aven_parser::TokenKind::Indent
        | aven_parser::TokenKind::Dedent => return None,
    };

    let modifiers = match kind {
        aven_parser::TokenKind::DocComment(_) => MODIFIER_DOCUMENTATION,
        _ => 0,
    };

    Some(SemanticStyle {
        token_type,
        modifiers,
    })
}

#[derive(Debug, Default)]
struct SemanticTokenBuilder {
    tokens: Vec<SemanticToken>,
    previous_line: u32,
    previous_start: u32,
}

impl SemanticTokenBuilder {
    fn push(&mut self, document: &ParsedDocument, span: Span, style: SemanticStyle) {
        if span.is_empty() {
            return;
        }

        let range = span_to_range(document, span);

        // The full encoding addresses each token by (line, start, length) on a
        // single line, so multi-line spans (e.g. future multi-line strings or
        // block comments) and empty ranges are skipped rather than mis-encoded;
        // they go unhighlighted until they are split into per-line spans.
        if range.start.line != range.end.line || range.start.character >= range.end.character {
            return;
        }

        let delta_line = range.start.line - self.previous_line;
        let delta_start = if delta_line == 0 {
            range.start.character - self.previous_start
        } else {
            range.start.character
        };

        self.tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length: range.end.character - range.start.character,
            token_type: style.token_type,
            token_modifiers_bitset: style.modifiers,
        });

        self.previous_line = range.start.line;
        self.previous_start = range.start.character;
    }
}

#[cfg(test)]
mod tests {
    use aven_core::{FileId, SourceFile};
    use tower_lsp::lsp_types::{SemanticTokenModifier, SemanticTokenType};

    use super::*;

    #[test]
    fn semantic_tokens_classify_lexical_and_definition_tokens() {
        let document = parsed_document(
            "## Doc\n\
             User = { name: Text }\n\
             value = (item) => item + 1\n\
             path = ./data.json\n\
             regex = /a+/\n\
             tag = @Ok\n"
                .to_owned(),
        );
        let tokens = decoded_semantic_tokens(&document);

        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 0,
                start: 0,
                length: 6,
                token_type: "comment",
                modifiers: MODIFIER_DOCUMENTATION,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 1,
                start: 0,
                length: 4,
                token_type: "type",
                modifiers: MODIFIER_DEFINITION,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 1,
                start: 9,
                length: 4,
                token_type: "property",
                modifiers: 0,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 1,
                start: 15,
                length: 4,
                token_type: "type",
                modifiers: 0,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 2,
                start: 0,
                length: 5,
                token_type: "function",
                modifiers: MODIFIER_DEFINITION,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 2,
                start: 9,
                length: 4,
                token_type: "parameter",
                modifiers: MODIFIER_DEFINITION,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 2,
                start: 23,
                length: 1,
                token_type: "operator",
                modifiers: 0,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 2,
                start: 25,
                length: 1,
                token_type: "number",
                modifiers: 0,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 3,
                start: 7,
                length: 11,
                token_type: "string",
                modifiers: 0,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 4,
                start: 8,
                length: 4,
                token_type: "regexp",
                modifiers: 0,
            },
        );
        assert_semantic_token(
            &tokens,
            DecodedSemanticToken {
                line: 5,
                start: 6,
                length: 3,
                token_type: "property",
                modifiers: 0,
            },
        );
    }

    #[test]
    fn legend_order_matches_token_constants() {
        let legend = legend();

        let expected_types = [
            (TOKEN_COMMENT, SemanticTokenType::COMMENT),
            (TOKEN_STRING, SemanticTokenType::STRING),
            (TOKEN_NUMBER, SemanticTokenType::NUMBER),
            (TOKEN_REGEXP, SemanticTokenType::REGEXP),
            (TOKEN_OPERATOR, SemanticTokenType::OPERATOR),
            (TOKEN_VARIABLE, SemanticTokenType::VARIABLE),
            (TOKEN_TYPE, SemanticTokenType::TYPE),
            (TOKEN_FUNCTION, SemanticTokenType::FUNCTION),
            (TOKEN_PARAMETER, SemanticTokenType::PARAMETER),
            (TOKEN_PROPERTY, SemanticTokenType::PROPERTY),
        ];
        assert_eq!(legend.token_types.len(), expected_types.len());
        for (index, expected) in expected_types {
            assert_eq!(legend.token_types[index as usize], expected);
        }

        let expected_modifiers = [
            (MODIFIER_DEFINITION, SemanticTokenModifier::DEFINITION),
            (MODIFIER_DOCUMENTATION, SemanticTokenModifier::DOCUMENTATION),
        ];
        assert_eq!(legend.token_modifiers.len(), expected_modifiers.len());
        for (bit, expected) in expected_modifiers {
            assert_eq!(
                legend.token_modifiers[bit.trailing_zeros() as usize],
                expected
            );
        }
    }

    fn parsed_document(source: impl Into<String>) -> ParsedDocument {
        let file = SourceFile::new(FileId(0), "semantic-token-test.av".to_owned(), None, source);
        aven_compiler::DocumentSnapshot::parse(aven_compiler::Revision::default(), file)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct DecodedSemanticToken {
        line: u32,
        start: u32,
        length: u32,
        token_type: &'static str,
        modifiers: u32,
    }

    fn decoded_semantic_tokens(document: &ParsedDocument) -> Vec<DecodedSemanticToken> {
        let mut line = 0;
        let mut start = 0;

        tokens(document)
            .data
            .into_iter()
            .map(|token| {
                line += token.delta_line;
                if token.delta_line == 0 {
                    start += token.delta_start;
                } else {
                    start = token.delta_start;
                }

                DecodedSemanticToken {
                    line,
                    start,
                    length: token.length,
                    token_type: semantic_token_type_name(token.token_type),
                    modifiers: token.token_modifiers_bitset,
                }
            })
            .collect()
    }

    fn semantic_token_type_name(index: u32) -> &'static str {
        match index {
            TOKEN_COMMENT => "comment",
            TOKEN_STRING => "string",
            TOKEN_NUMBER => "number",
            TOKEN_REGEXP => "regexp",
            TOKEN_OPERATOR => "operator",
            TOKEN_VARIABLE => "variable",
            TOKEN_TYPE => "type",
            TOKEN_FUNCTION => "function",
            TOKEN_PARAMETER => "parameter",
            TOKEN_PROPERTY => "property",
            _ => panic!("unknown semantic token type index {index}"),
        }
    }

    fn assert_semantic_token(tokens: &[DecodedSemanticToken], expected: DecodedSemanticToken) {
        assert!(
            tokens.contains(&expected),
            "expected semantic token {expected:?} in {tokens:?}"
        );
    }
}
