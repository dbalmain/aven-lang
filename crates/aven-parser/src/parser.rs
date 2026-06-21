use aven_core::{Diagnostic, FileId, Label, SourceFile, Span, codes};

use crate::{Keyword, Token, TokenKind, lex_then_layout};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Binding(Binding),
    Signature(Signature),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub name_span: Span,
    /// Optional `: type` ascription, parsed as an ordinary expression.
    pub annotation: Option<Expr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub name_span: Span,
    /// The annotation term following `:`, parsed as an ordinary expression.
    pub annotation: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub name_span: Span,
    pub comptime: bool,
    /// Optional `: type` ascription, parsed as an ordinary expression.
    pub annotation: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Missing,
    Literal(Literal),
    Undefined,
    Null,
    Name(String),
    ComptimeName(String),
    Tag(String),
    Group(Box<Expr>),
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    Record(Vec<RecordEntry>),
    Set(Vec<RecordEntry>),
    /// `Array[a]` / `users[2]`: postfix square-bracket application or indexing.
    /// Whether this is a type application or an element index is decided in a
    /// later semantic phase.
    Index {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `?T`: prefix optional marker.
    Optional(Box<Expr>),
    /// `T?`: postfix nullable marker. Match uses the distinct `?>` operator,
    /// so bare postfix `?` is parsed uniformly.
    Nullable(Box<Expr>),
    /// `T!`: postfix nullable-strip marker in type position.
    NonNull(Box<Expr>),
    /// `a -> b` / `(A, B) -> C`: a function/arrow form, right-associative.
    /// A parenthesized tuple on the left flattens into `params`.
    Arrow {
        params: Vec<Expr>,
        result: Box<Expr>,
    },
    FieldAccess {
        receiver: Box<Expr>,
        field: String,
        field_span: Span,
        null_safe: bool,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Binary {
        left: Box<Expr>,
        operator: String,
        operator_span: Span,
        right: Box<Expr>,
    },
    Unary {
        operator: String,
        operator_span: Span,
        value: Box<Expr>,
    },
    Propagate {
        value: Box<Expr>,
        operator_span: Span,
        mode: PropagationMode,
    },
    Match {
        subject: Box<Expr>,
        operator_span: Span,
        arms: Vec<MatchArm>,
    },
    Lambda {
        params: Vec<Param>,
        /// Optional `: type` return annotation, parsed as an ordinary expression.
        return_annotation: Option<Box<Expr>>,
        body: Box<Expr>,
    },
    Block(Vec<Item>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropagationMode {
    ReturnError,
    Panic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchArm {
    /// Parsed as an ordinary expression; pattern meaning is assigned later.
    pub pattern: Expr,
    pub guards: Vec<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordEntry {
    Field {
        name: String,
        name_span: Span,
        value: Expr,
        overwrite: bool,
        span: Span,
    },
    FieldComputed {
        key: Expr,
        value: Expr,
        span: Span,
    },
    Shorthand {
        name: String,
        name_span: Span,
        span: Span,
    },
    Spread {
        value: Expr,
        overwrite: bool,
        span: Span,
    },
    Delete {
        name: String,
        name_span: Span,
        span: Span,
    },
    DeleteComputed {
        key: Expr,
        span: Span,
    },
    Rename {
        from: String,
        from_span: Span,
        to: String,
        to_span: Span,
        span: Span,
    },
    Iteration {
        source: Expr,
        binder: String,
        binder_span: Span,
        guard: Option<Expr>,
        body: Vec<RecordEntry>,
        span: Span,
    },
    /// `..`: the open-row marker inside a record (type) shape.
    Open {
        span: Span,
    },
    /// A bare member of a `@{...}` set/variant shape: `@Red`, `@Ok(1)`,
    /// `@ParseError(Text)`, `@NotFound`.
    Element(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Literal {
    Bool(bool),
    Number(String),
    String(String),
    Regex(String),
    Path(String),
    Label(String),
}

#[derive(Debug, Clone)]
pub struct ParseOutput {
    pub file_id: FileId,
    /// Raw lexer tokens, including comments and raw newline/indent trivia.
    ///
    /// The formatter can use this stream to preserve source trivia without
    /// requiring the AST to carry every comment and blank line.
    pub raw_tokens: Vec<Token>,
    /// Parser-facing tokens after layout has converted raw indentation into
    /// `Indent`/`Dedent`/`Newline` markers.
    pub layout_tokens: Vec<Token>,
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse_module(source: &str) -> ParseOutput {
    parse_module_with_file_id(FileId(0), source)
}

pub fn parse_source(file: &SourceFile) -> ParseOutput {
    parse_module_with_file_id(file.id, file.source())
}

fn parse_module_with_file_id(file_id: FileId, source: &str) -> ParseOutput {
    let (raw_tokens, mut layout) = lex_then_layout(source);
    let layout_tokens = layout.tokens;
    let mut diagnostics = std::mem::take(&mut layout.diagnostics);
    diagnostics.extend(scan_delimiters(&layout_tokens));

    let (module, diagnostics) = {
        let mut parser = Parser {
            tokens: &layout_tokens,
            cursor: 0,
            diagnostics,
        };
        let module = parser.parse_module();
        (module, parser.diagnostics)
    };

    ParseOutput {
        file_id,
        raw_tokens,
        layout_tokens,
        module,
        diagnostics,
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    diagnostics: Vec<Diagnostic>,
}

/// Whether a brace-entry loop is parsing a record `{...}` or a set/variant
/// `@{...}`. The only behavioural difference is how bare terms are treated:
/// a bare label is a `Shorthand` in a record, while a bare term is an `Element`
/// in a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryMode {
    Record,
    Set,
}

#[derive(Debug, Clone)]
struct InfixOperator {
    text: String,
    span: Span,
    left_binding_power: u8,
    right_binding_power: u8,
}

impl Parser<'_> {
    fn parse_module(&mut self) -> Module {
        let mut items = Vec::new();

        while !self.at_end() {
            self.skip_newlines_and_doc_comments();

            if self.at_end() {
                break;
            }

            if self.current_is(TokenKind::Dedent) {
                self.advance();
                continue;
            }

            if self.current_is(TokenKind::Indent) {
                self.report_unexpected_indentation();
                self.advance();
                continue;
            }

            if let Some(item) = self.parse_item() {
                items.push(item);
            } else {
                self.recover_to_next_line();
            }
        }

        Module { items }
    }

    fn parse_block(&mut self, indent_span: Span) -> Expr {
        let start = indent_span.start;
        self.advance();
        let mut items = Vec::new();

        while !self.at_end() && !self.current_is(TokenKind::Dedent) {
            self.skip_newlines_and_doc_comments();

            if self.at_end() || self.current_is(TokenKind::Dedent) {
                break;
            }

            if self.current_is(TokenKind::Indent) {
                self.report_unexpected_indentation();
                self.advance();
                continue;
            }

            if let Some(item) = self.parse_item() {
                items.push(item);
            } else {
                self.recover_to_next_line();
            }
        }

        let end = if self.current_is(TokenKind::Dedent) {
            let span = self.current_span();
            self.advance();
            span.end
        } else {
            self.previous_end()
        };

        Expr {
            kind: ExprKind::Block(items),
            span: Span::new(start, end),
        }
    }

    fn parse_item(&mut self) -> Option<Item> {
        let equals = self.find_binding_equals();

        if let Some(equals) = equals {
            return self.parse_binding(equals).map(Item::Binding);
        }

        if self.is_signature_start() {
            return self.parse_signature().map(Item::Signature);
        }

        let expr = self.parse_expression();
        self.report_unsupported_remainder();
        self.consume_newline();

        Some(Item::Expr(expr))
    }

    fn parse_binding(&mut self, equals: usize) -> Option<Binding> {
        if equals == self.cursor {
            let span = self.tokens[equals].span;
            self.diagnostics.push(
                Diagnostic::error("binding is missing a name")
                    .with_code(codes::parse::MISSING_BINDING_NAME)
                    .with_label(Label::primary(Span::point(span.start), "expected a name"))
                    .with_note("add a name before `=`, for example `name = expr`"),
            );
            self.recover_to_next_line();
            return None;
        }

        let name_token = &self.tokens[self.cursor];
        let (name, name_span) = match &name_token.kind {
            TokenKind::Identifier(name) | TokenKind::ComptimeIdentifier(name) => {
                (name.clone(), name_token.span)
            }
            _ => {
                let span = Span::new(name_token.span.start, self.tokens[equals - 1].span.end);
                self.report_invalid_binding_name(span);
                self.recover_to_next_line();
                return None;
            }
        };

        self.advance();

        // `name : type = value`: the optional annotation occupies everything
        // between the name and the depth-0 `=`.
        let annotation = if self.current_is_operator(":") {
            self.advance();
            Some(self.parse_annotation_term())
        } else {
            None
        };

        if self.cursor != equals {
            // The name (and optional annotation) did not consume up to `=`;
            // whatever is left is not a valid binding head.
            let span = Span::new(name_span.start, self.tokens[equals - 1].span.end);
            self.report_invalid_binding_name(span);
            self.recover_to_next_line();
            return None;
        }

        self.cursor = equals + 1;
        let value = self.parse_binding_value(self.tokens[equals].span.end);
        let span = name_span.merge(value.span);
        self.consume_newline();

        Some(Binding {
            name,
            name_span,
            annotation,
            value,
            span,
        })
    }

    fn parse_signature(&mut self) -> Option<Signature> {
        let name_token = self.tokens[self.cursor].clone();
        let (name, name_span) = match &name_token.kind {
            TokenKind::Identifier(name) | TokenKind::ComptimeIdentifier(name) => {
                (name.clone(), name_token.span)
            }
            _ => return None,
        };

        self.advance();
        // `is_signature_start` guarantees the next token is `:`.
        self.advance();
        let annotation = self.parse_annotation_term();
        let span = name_span.merge(annotation.span);
        self.report_unsupported_remainder();
        self.consume_newline();

        Some(Signature {
            name,
            name_span,
            annotation,
            span,
        })
    }

    /// Parse the term following a `:` ascription. Types are ordinary
    /// expressions under the fold, so this is just the normal expression entry
    /// point with a dedicated "expected type" diagnostic when the term is
    /// missing (the next token closes the binding with `=`/`=>` or a boundary).
    fn parse_annotation_term(&mut self) -> Expr {
        if self.current_is_operator("=") || self.current_is_operator("=>") {
            return self.report_expected_type(self.current_span().start);
        }

        if self.at_item_boundary() {
            return self.report_expected_type(self.previous_end());
        }

        self.parse_arrow_with_lambda(false)
    }

    fn parse_binding_value(&mut self, missing_offset: usize) -> Expr {
        if self.current_is(TokenKind::Newline) {
            self.advance();

            if self.current_is(TokenKind::Indent) {
                return self.parse_block(self.current_span());
            }

            return self.report_missing_binding_value(missing_offset);
        }

        if self.at_item_boundary() {
            return self.report_missing_binding_value(missing_offset);
        }

        let expr = self.parse_expression();
        self.report_unsupported_remainder();
        expr
    }

    fn parse_expression(&mut self) -> Expr {
        let expr = self.parse_arrow();

        if self.current_is_operator("?>") {
            return self.finish_match(expr);
        }

        expr
    }

    /// `a -> b`, right-associative. A parenthesized tuple on the left flattens
    /// its elements into `params`, so `(A, B) -> C` has two params while
    /// `A -> C` has one. Wraps the binary-expression layer.
    fn parse_arrow(&mut self) -> Expr {
        self.parse_arrow_with_lambda(true)
    }

    fn parse_arrow_with_lambda(&mut self, allow_lambda: bool) -> Expr {
        let left = self.parse_binary_expression(0, allow_lambda);

        if !self.current_is_operator("->") {
            return left;
        }

        self.advance();
        // Right-recurse so `a -> b -> c` parses as `a -> (b -> c)`.
        let result = self.parse_arrow_with_lambda(allow_lambda);
        let span = left.span.merge(result.span);

        let params = match left.kind {
            ExprKind::Tuple(items) => items,
            _ => vec![left],
        };

        Expr {
            kind: ExprKind::Arrow {
                params,
                result: Box::new(result),
            },
            span,
        }
    }

    fn parse_binary_expression(&mut self, min_binding_power: u8, allow_lambda: bool) -> Expr {
        if allow_lambda && self.is_lambda_start() {
            return self.parse_lambda();
        }

        let mut left = self.parse_unary();

        while let Some(operator) = self.current_infix_operator() {
            if operator.left_binding_power < min_binding_power {
                break;
            }

            self.advance();
            let right = self.parse_binary_expression(operator.right_binding_power, allow_lambda);
            let span = left.span.merge(right.span);

            left = Expr {
                kind: ExprKind::Binary {
                    left: Box::new(left),
                    operator: operator.text,
                    operator_span: operator.span,
                    right: Box::new(right),
                },
                span,
            };
        }

        left
    }

    fn parse_unary(&mut self) -> Expr {
        if self.current_is_operator("?") {
            let operator_span = self.current_span();
            self.advance();
            let value = self.parse_unary();
            let span = operator_span.merge(value.span);

            return Expr {
                kind: ExprKind::Optional(Box::new(value)),
                span,
            };
        }

        if self.current_is_operator("-") || self.current_is_operator("!") {
            let Some(operator) = self.current().cloned() else {
                return self.parse_postfix();
            };
            let TokenKind::Operator(operator_text) = operator.kind else {
                return self.parse_postfix();
            };
            self.advance();
            let value = self.parse_unary();
            let span = operator.span.merge(value.span);

            return Expr {
                kind: ExprKind::Unary {
                    operator: operator_text,
                    operator_span: operator.span,
                    value: Box::new(value),
                },
                span,
            };
        }

        self.parse_postfix()
    }

    fn parse_lambda(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();
        let params = self.parse_lambda_params();
        self.consume_close_paren();

        // Optional `: type` return annotation between the params and `=>`.
        let return_annotation = if self.current_is_operator(":") {
            self.advance();
            Some(Box::new(self.parse_annotation_term()))
        } else {
            None
        };

        self.consume_operator("=>");

        let body = if self.current_is(TokenKind::Newline) {
            self.advance();
            if self.current_is(TokenKind::Indent) {
                self.parse_block(self.current_span())
            } else {
                self.report_missing_lambda_body(self.previous_end())
            }
        } else if self.at_item_boundary() {
            self.report_missing_lambda_body(self.previous_end())
        } else {
            self.parse_expression()
        };

        let span = Span::new(start, body.span.end);
        Expr {
            kind: ExprKind::Lambda {
                params,
                return_annotation,
                body: Box::new(body),
            },
            span,
        }
    }

    fn parse_lambda_params(&mut self) -> Vec<Param> {
        let mut params = Vec::new();

        if self.current_is(TokenKind::CloseParen) {
            return params;
        }

        loop {
            match self.current() {
                Some(Token {
                    kind: TokenKind::Identifier(name) | TokenKind::ComptimeIdentifier(name),
                    span,
                }) => {
                    let name = name.clone();
                    let name_span = *span;
                    self.advance();

                    let annotation = if self.current_is_operator(":") {
                        self.advance();
                        Some(self.parse_annotation_term())
                    } else {
                        None
                    };

                    let span = annotation
                        .as_ref()
                        .map_or(name_span, |term| name_span.merge(term.span));

                    params.push(Param {
                        name,
                        name_span,
                        comptime: false,
                        annotation,
                        span,
                    });
                }
                Some(Token {
                    kind: TokenKind::ComptimeParamMarker(name),
                    span,
                }) => {
                    let name = name.clone();
                    let marker_span = *span;
                    let name_span = Span::new(marker_span.start + 1, marker_span.end);
                    self.advance();

                    let annotation = if self.current_is_operator(":") {
                        self.advance();
                        Some(self.parse_annotation_term())
                    } else {
                        None
                    };

                    let span = annotation
                        .as_ref()
                        .map_or(marker_span, |term| marker_span.merge(term.span));

                    params.push(Param {
                        name,
                        name_span,
                        comptime: true,
                        annotation,
                        span,
                    });
                }
                Some(token) => {
                    self.diagnostics.push(
                        Diagnostic::error("expected lambda parameter")
                            .with_code(codes::parse::EXPECTED_PARAMETER)
                            .with_label(Label::primary(token.span, "expected a parameter name"))
                            .with_note("use an identifier like `x`, or `_` to ignore an argument"),
                    );
                    self.advance();
                }
                None => break,
            }

            if self.consume_comma() {
                continue;
            }

            break;
        }

        params
    }

    fn parse_postfix(&mut self) -> Expr {
        let mut expr = self.parse_atom();

        loop {
            if self.current_is(TokenKind::OpenParen) {
                expr = self.finish_call(expr);
                continue;
            }

            if self.current_is(TokenKind::OpenBracket) {
                expr = self.finish_index(expr);
                continue;
            }

            if self.current_is_empty_set_postfix(expr.span.end) {
                expr = self.finish_set_type_postfix(expr);
                continue;
            }

            if self.current_is_operator(".") || self.current_is_operator("?.") {
                expr = self.finish_field_access(expr);
                continue;
            }

            if self.current_is_operator("?^") || self.current_is_operator("?!") {
                expr = self.finish_propagation(expr);
                continue;
            }

            // `T?`: postfix nullable. Bare `?` is unconditionally nullable now;
            // the match operator is the distinct `?>` token, handled in
            // `parse_expression`.
            if self.current_is_operator("?") {
                let operator_span = self.current_span();
                self.advance();
                expr = Expr {
                    span: expr.span.merge(operator_span),
                    kind: ExprKind::Nullable(Box::new(expr)),
                };
                continue;
            }

            if self.current_is_operator("!") {
                let operator_span = self.current_span();
                self.advance();
                expr = Expr {
                    span: expr.span.merge(operator_span),
                    kind: ExprKind::NonNull(Box::new(expr)),
                };
                continue;
            }

            break;
        }

        expr
    }

    fn finish_index(&mut self, callee: Expr) -> Expr {
        let start = callee.span.start;
        let (bracket_span, args) = self.parse_bracketed_expressions();

        if args.is_empty() {
            return collection_type_application("Array", bracket_span, callee, bracket_span.end);
        }

        Expr {
            kind: ExprKind::Index {
                callee: Box::new(callee),
                args,
            },
            span: Span::new(start, bracket_span.end),
        }
    }

    fn parse_bracketed_expressions(&mut self) -> (Span, Vec<Expr>) {
        let open_span = self.current_span();
        self.advance();
        let args = self.parse_expression_list(TokenKind::CloseBracket);
        let end = self.consume_close(TokenKind::CloseBracket);

        (Span::new(open_span.start, end), args)
    }

    fn parse_bracketed_key(&mut self) -> Option<(Expr, Span)> {
        let (span, mut args) = self.parse_bracketed_expressions();
        if args.len() != 1 {
            self.report_expected_record_label(span);
            return None;
        }

        Some((args.remove(0), span))
    }

    fn finish_set_type_postfix(&mut self, element: Expr) -> Expr {
        let marker_span = self.current_span();
        self.advance();
        self.advance();
        let end = self.current_span().end;
        self.advance();

        collection_type_application("Set", marker_span.merge(Span::point(end)), element, end)
    }

    fn finish_call(&mut self, callee: Expr) -> Expr {
        let start = callee.span.start;
        self.advance();
        let mut args = Vec::new();

        if self.current_is(TokenKind::CloseParen) {
            let end = self.current_span().end;
            self.advance();
            return Expr {
                kind: ExprKind::Call {
                    callee: Box::new(callee),
                    args,
                },
                span: Span::new(start, end),
            };
        }

        loop {
            if self.at_item_boundary() {
                break;
            }

            if self.current_is_any_close_delimiter() {
                break;
            }

            args.push(self.parse_expression());

            if self.consume_comma() {
                continue;
            }

            break;
        }

        let end = if self.current_is(TokenKind::CloseParen) {
            let span = self.current_span();
            self.advance();
            span.end
        } else {
            self.consume_close_delimiter_if_present();
            self.previous_end()
        };

        Expr {
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
            span: Span::new(start, end),
        }
    }

    fn finish_propagation(&mut self, value: Expr) -> Expr {
        let operator_span = self.current_span();
        let mode = if self.current_is_operator("?!") {
            PropagationMode::Panic
        } else {
            PropagationMode::ReturnError
        };
        self.advance();

        Expr {
            span: value.span.merge(operator_span),
            kind: ExprKind::Propagate {
                value: Box::new(value),
                operator_span,
                mode,
            },
        }
    }

    fn finish_match(&mut self, subject: Expr) -> Expr {
        // Consume the `?>` match operator.
        let operator_span = self.current_span();
        self.advance();

        let arms = if self.current_is(TokenKind::Newline) {
            self.advance();

            if self.current_is(TokenKind::Indent) {
                self.parse_match_arm_block()
            } else {
                // `?>` followed by a newline without an indented block (or by a
                // boundary/EOF): the arms are simply missing.
                self.report_missing_match_arms(operator_span.end);
                Vec::new()
            }
        } else if self.at_item_boundary() {
            self.report_missing_match_arms(operator_span.end);
            Vec::new()
        } else {
            // Tokens follow `?>` on the same line, e.g. `result ?> @Ok(x) => x`.
            self.report_inline_match_arms(self.current_span());
            self.recover_to_next_line();
            Vec::new()
        };

        let end = arms
            .last()
            .map(|arm| arm.span.end)
            .unwrap_or(operator_span.end);

        Expr {
            span: Span::new(subject.span.start, end),
            kind: ExprKind::Match {
                subject: Box::new(subject),
                operator_span,
                arms,
            },
        }
    }

    fn parse_match_arm_block(&mut self) -> Vec<MatchArm> {
        self.advance();
        let mut arms = Vec::new();

        while !self.at_end() && !self.current_is(TokenKind::Dedent) {
            self.skip_newlines_and_doc_comments();

            if self.at_end() || self.current_is(TokenKind::Dedent) {
                break;
            }

            if self.current_is(TokenKind::Indent) {
                self.report_unexpected_indentation();
                self.advance();
                continue;
            }

            arms.push(self.parse_match_arm());
            self.consume_newline();
        }

        if self.current_is(TokenKind::Dedent) {
            self.advance();
        }

        arms
    }

    fn parse_match_arm(&mut self) -> MatchArm {
        let pattern = self.parse_match_pattern_term();
        let mut guards = Vec::new();

        while self.consume_comma() {
            if self.current_is_operator("=>") || self.at_item_boundary() {
                guards.push(missing_expr(Span::point(self.previous_end())));
                break;
            }

            guards.push(self.parse_expression());
        }

        if !self.consume_operator("=>") {
            self.report_expected_match_arrow(self.current_span());
            self.recover_to_next_line();
            let body = missing_expr(Span::point(pattern.span.end));
            return MatchArm {
                span: pattern.span.merge(body.span),
                pattern,
                guards,
                body,
            };
        }

        let body = if self.current_is(TokenKind::Newline) {
            self.advance();
            if self.current_is(TokenKind::Indent) {
                self.parse_block(self.current_span())
            } else {
                self.report_missing_match_body(self.previous_end())
            }
        } else if self.at_item_boundary() {
            self.report_missing_match_body(self.previous_end())
        } else {
            self.parse_expression()
        };

        MatchArm {
            span: pattern.span.merge(body.span),
            pattern,
            guards,
            body,
        }
    }

    fn parse_match_pattern_term(&mut self) -> Expr {
        if self.current_is_operator("=>")
            || self.current_is(TokenKind::Comma)
            || self.at_item_boundary()
        {
            return self.report_expected_pattern(self.current_span());
        }

        self.parse_arrow_with_lambda(false)
    }

    fn finish_field_access(&mut self, receiver: Expr) -> Expr {
        let operator_span = self.current_span();
        let null_safe = self.current_is_operator("?.");
        self.advance();

        let Some((field, field_span)) = self.parse_label_name() else {
            self.report_expected_field_name(self.current_span());
            return Expr {
                span: receiver.span.merge(operator_span),
                kind: ExprKind::FieldAccess {
                    receiver: Box::new(receiver),
                    field: String::new(),
                    field_span: Span::point(operator_span.end),
                    null_safe,
                },
            };
        };

        Expr {
            span: receiver.span.merge(field_span),
            kind: ExprKind::FieldAccess {
                receiver: Box::new(receiver),
                field,
                field_span,
                null_safe,
            },
        }
    }

    fn parse_atom(&mut self) -> Expr {
        let Some(token) = self.current().cloned() else {
            return missing_expr(Span::point(self.previous_end()));
        };

        match token.kind {
            TokenKind::Keyword(keyword) => {
                self.advance();
                match keyword {
                    Keyword::True => literal_expr(Literal::Bool(true), token.span),
                    Keyword::False => literal_expr(Literal::Bool(false), token.span),
                    Keyword::Null => Expr {
                        kind: ExprKind::Null,
                        span: token.span,
                    },
                    Keyword::Undefined => Expr {
                        kind: ExprKind::Undefined,
                        span: token.span,
                    },
                }
            }
            TokenKind::Identifier(name) => {
                self.advance();
                Expr {
                    kind: ExprKind::Name(name),
                    span: token.span,
                }
            }
            TokenKind::ComptimeIdentifier(name) => {
                self.advance();
                Expr {
                    kind: ExprKind::ComptimeName(name),
                    span: token.span,
                }
            }
            TokenKind::ComptimeParamMarker(_) => {
                self.report_unexpected_comptime_marker(token.span);
                self.advance();
                missing_expr(token.span)
            }
            TokenKind::Tag(name) => {
                self.advance();
                Expr {
                    kind: ExprKind::Tag(name),
                    span: token.span,
                }
            }
            TokenKind::Number(number) => {
                self.advance();
                literal_expr(Literal::Number(number), token.span)
            }
            TokenKind::StringLiteral(text) => {
                self.advance();
                literal_expr(Literal::String(text), token.span)
            }
            TokenKind::RegexLiteral(regex) => {
                self.advance();
                literal_expr(Literal::Regex(regex), token.span)
            }
            TokenKind::PathLiteral(path) => {
                self.advance();
                literal_expr(Literal::Path(path), token.span)
            }
            TokenKind::LabelPath(label) => {
                self.advance();
                literal_expr(Literal::Label(label), token.span)
            }
            TokenKind::Operator(operator)
                if operator == "@" && self.next_is(TokenKind::OpenBrace) =>
            {
                self.parse_set()
            }
            TokenKind::OpenParen => self.parse_group_or_tuple(),
            TokenKind::OpenBracket => self.parse_array(),
            TokenKind::OpenBrace => self.parse_record(),
            TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                self.advance();
                missing_expr(token.span)
            }
            _ => {
                self.diagnostics.push(
                    Diagnostic::error("expected expression")
                        .with_code(codes::parse::EXPECTED_EXPRESSION)
                        .with_label(Label::primary(token.span, "expected an expression here"))
                        .with_note("expressions are literals, identifiers, function calls, lambdas, or collection literals"),
                );
                self.advance();
                missing_expr(token.span)
            }
        }
    }

    fn parse_group_or_tuple(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();
        self.skip_collection_trivia();

        if self.current_is(TokenKind::CloseParen) {
            let end = self.current_span().end;
            self.advance();
            return Expr {
                kind: ExprKind::Tuple(Vec::new()),
                span: Span::new(start, end),
            };
        }

        let first = self.parse_expression();
        self.skip_collection_trivia();

        if !self.current_is(TokenKind::Comma) {
            let end = if self.current_is(TokenKind::CloseParen) {
                self.consume_close(TokenKind::CloseParen)
            } else if self.close_exists_before_item_boundary(TokenKind::CloseParen) {
                self.previous_end()
            } else {
                self.recover_to_next_line();
                self.previous_end()
            };
            return Expr {
                kind: ExprKind::Group(Box::new(first)),
                span: Span::new(start, end),
            };
        }

        let first_comma_span = self.current_span();
        self.advance();

        let mut items = vec![first];
        loop {
            self.skip_collection_trivia();

            if self.current_is(TokenKind::CloseParen) || self.at_end() {
                break;
            }

            if self.current_is_any_close_delimiter() {
                break;
            }

            items.push(self.parse_expression());
            self.skip_collection_trivia();

            if self.current_is(TokenKind::Comma) {
                self.advance();
                continue;
            }

            break;
        }

        if items.len() == 1 {
            self.report_single_item_tuple(first_comma_span);
        }

        let end = self.consume_close(TokenKind::CloseParen);

        Expr {
            kind: ExprKind::Tuple(items),
            span: Span::new(start, end),
        }
    }

    fn parse_array(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();
        let items = self.parse_expression_list(TokenKind::CloseBracket);
        let end = self.consume_close(TokenKind::CloseBracket);

        Expr {
            kind: ExprKind::Array(items),
            span: Span::new(start, end),
        }
    }

    fn parse_set(&mut self) -> Expr {
        let start = self.current_span().start;
        // Consume the `@{` set/variant-set literal opener.
        self.advance();
        self.advance();
        let entries = self.parse_entry_list(EntryMode::Set);
        let end = self.consume_close(TokenKind::CloseBrace);

        Expr {
            kind: ExprKind::Set(entries),
            span: Span::new(start, end),
        }
    }

    fn parse_expression_list(&mut self, close: TokenKind) -> Vec<Expr> {
        self.parse_delimited(close, false, |parser| Some(parser.parse_expression()))
    }

    fn parse_record(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();
        let entries = self.parse_entry_list(EntryMode::Record);
        let end = self.consume_close(TokenKind::CloseBrace);

        Expr {
            kind: ExprKind::Record(entries),
            span: Span::new(start, end),
        }
    }

    /// Shared entry loop for both `{...}` records and `@{...}` sets/variants.
    /// The only difference is how a bare term is interpreted, which is handled
    /// inside `parse_record_entry` via the `EntryMode`.
    fn parse_entry_list(&mut self, mode: EntryMode) -> Vec<RecordEntry> {
        self.parse_delimited(TokenKind::CloseBrace, true, |parser| {
            parser.parse_record_entry(mode)
        })
    }

    fn parse_delimited<T>(
        &mut self,
        close: TokenKind,
        allow_semicolon: bool,
        mut parse_item: impl FnMut(&mut Self) -> Option<T>,
    ) -> Vec<T> {
        let mut items = Vec::new();
        self.skip_collection_trivia();

        while !self.at_end() && !self.current_is(close.clone()) {
            if self.current_is_any_close_delimiter() {
                break;
            }

            if let Some(item) = parse_item(self) {
                items.push(item);
            }

            if self.current_is(close.clone()) {
                break;
            }

            let had_separator = self.consume_collection_separator(allow_semicolon);
            if self.current_is(close.clone()) {
                break;
            }

            if !had_separator {
                break;
            }
        }

        items
    }

    fn parse_record_entry(&mut self, mode: EntryMode) -> Option<RecordEntry> {
        if matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::ComptimeParamMarker(_))
        ) {
            self.report_unexpected_comptime_marker(self.current_span());
            self.recover_record_entry();
            return None;
        }

        if mode == EntryMode::Record
            && matches!(
                self.current().map(|token| &token.kind),
                Some(TokenKind::LabelPath(_))
            )
        {
            self.report_expected_record_label(self.current_span());
            self.recover_record_entry();
            return None;
        }

        if self.current_is_operator("..") || self.current_is_operator(":..") {
            let operator_span = self.current_span();
            let overwrite = self.current_is_operator(":..");
            self.advance();

            if !overwrite
                && (self.current_is(TokenKind::CloseBrace)
                    || self.current_is(TokenKind::Newline)
                    || self.current_is(TokenKind::Indent)
                    || self.current_is(TokenKind::Dedent)
                    || self.current_is(TokenKind::Comma)
                    || self.current_is(TokenKind::Semicolon))
            {
                return Some(RecordEntry::Open {
                    span: operator_span,
                });
            }

            let value = self.parse_expression();
            let span = operator_span.merge(value.span);
            return Some(RecordEntry::Spread {
                value,
                overwrite,
                span,
            });
        }

        if self.current_is_operator("-") {
            let operator_span = self.current_span();
            self.advance();
            if self.current_is(TokenKind::OpenBracket) {
                let Some((key, bracket_span)) = self.parse_bracketed_key() else {
                    self.recover_record_entry();
                    return None;
                };
                return Some(RecordEntry::DeleteComputed {
                    key,
                    span: operator_span.merge(bracket_span),
                });
            }

            let Some((name, name_span)) = self.parse_transform_label_name(mode) else {
                self.report_expected_record_label(self.current_span());
                self.recover_record_entry();
                return None;
            };
            return Some(RecordEntry::Delete {
                name,
                name_span,
                span: operator_span.merge(name_span),
            });
        }

        if self.current_is(TokenKind::OpenBracket) {
            let Some((key, bracket_span)) = self.parse_bracketed_key() else {
                self.recover_record_entry();
                return None;
            };

            if !self.current_is_operator(":") {
                self.report_expected_record_entry(self.current_span());
                self.recover_record_entry();
                return None;
            }
            self.advance();

            let value = self.parse_expression();
            let span = bracket_span.merge(value.span);
            return Some(RecordEntry::FieldComputed { key, value, span });
        }

        if mode == EntryMode::Record && self.iteration_arrow_follows() {
            let source = self.parse_binary_expression(0, true);
            self.consume_operator("->");
            let Some((binder, binder_span)) = self.parse_iteration_binder() else {
                self.report_expected_record_label(self.current_span());
                self.recover_record_entry();
                return None;
            };

            let guard = if self.current_is(TokenKind::Comma) {
                self.advance();
                Some(self.parse_binary_expression(0, true))
            } else {
                None
            };

            let semicolon_span = self.current_span();
            if !self.current_is(TokenKind::Semicolon) {
                self.report_expected_record_entry(self.current_span());
                self.recover_record_entry();
                return None;
            }
            self.advance();

            let body = self.parse_entry_list(mode);
            let end = body
                .last()
                .map(record_entry_span)
                .unwrap_or(semicolon_span)
                .end;
            return Some(RecordEntry::Iteration {
                span: Span::new(source.span.start, end),
                source,
                binder,
                binder_span,
                guard,
                body,
            });
        }

        if mode == EntryMode::Set
            && self.current_is_transform_label_start()
            && self.next_is_operator("->")
        {
            let Some((from, from_span)) = self.parse_transform_label_name(mode) else {
                self.report_expected_record_label(self.current_span());
                self.recover_record_entry();
                return None;
            };
            self.advance();
            let Some((to, to_span)) = self.parse_transform_label_name(mode) else {
                self.report_expected_record_label(self.current_span());
                self.recover_record_entry();
                return None;
            };
            return Some(RecordEntry::Rename {
                from,
                from_span,
                to,
                to_span,
                span: from_span.merge(to_span),
            });
        }

        // In a set/variant shape, a bare term is an element (`@Red`, `@Ok(1)`,
        // `@ParseError(Text)`). The element parser covers calls and other terms,
        // so labels do not get the record-only treatment below.
        if mode == EntryMode::Set {
            let term = self.parse_expression();
            if matches!(term.kind, ExprKind::Missing) {
                self.report_expected_record_entry(term.span);
                self.recover_record_entry();
                return None;
            }
            return Some(RecordEntry::Element(term));
        }

        // A `(k, v)` tuple in a record/comprehension body is an add-entry item.
        if self.current_is(TokenKind::OpenParen) {
            let term = self.parse_expression();
            if matches!(term.kind, ExprKind::Missing) {
                self.report_expected_record_entry(term.span);
                self.recover_record_entry();
                return None;
            }
            return Some(RecordEntry::Element(term));
        }

        let Some((name, name_span)) = self.parse_label_name() else {
            self.report_expected_record_entry(self.current_span());
            self.recover_record_entry();
            return None;
        };

        // A rename `name -> to` must be detected before treating `->` as a
        // function-type arrow inside a field value.
        if self.current_is_operator("->") {
            self.advance();
            let Some((to, to_span)) = self.parse_label_name() else {
                self.report_expected_record_label(self.current_span());
                self.recover_record_entry();
                return None;
            };
            return Some(RecordEntry::Rename {
                from: name,
                from_span: name_span,
                to,
                to_span,
                span: name_span.merge(to_span),
            });
        }

        let separator = if self.current_is_operator(":") {
            Some((false, false))
        } else if self.current_is_operator("::") {
            Some((true, false))
        } else if self.current_is_operator("=") {
            Some((false, true))
        } else if self.current_is_operator(":=") {
            Some((true, true))
        } else {
            None
        };

        if let Some((overwrite, legacy)) = separator {
            if legacy {
                self.report_legacy_record_field_separator(self.current_span(), &name, overwrite);
            }
            self.advance();
            let value = self.parse_expression();
            let span = name_span.merge(value.span);
            return Some(RecordEntry::Field {
                name,
                name_span,
                value,
                overwrite,
                span,
            });
        }

        Some(RecordEntry::Shorthand {
            name,
            name_span,
            span: name_span,
        })
    }

    fn parse_iteration_binder(&mut self) -> Option<(String, Span)> {
        let token = self.current()?.clone();
        let TokenKind::Identifier(name) = token.kind else {
            return None;
        };
        self.advance();
        Some((name, token.span))
    }

    fn iteration_arrow_follows(&self) -> bool {
        let mut depth = 0usize;
        let mut index = self.cursor;

        while let Some(token) = self.tokens.get(index) {
            match &token.kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => {
                    depth += 1;
                }
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    if depth == 0 {
                        return false;
                    }
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Operator(operator) if operator == "->" && depth == 0 => {
                    let binder = self.skip_collection_trivia_from(index + 1);
                    let Some(Token {
                        kind: TokenKind::Identifier(_),
                        ..
                    }) = self.tokens.get(binder)
                    else {
                        return false;
                    };
                    let after_binder = self.skip_collection_trivia_from(binder + 1);
                    return match self.tokens.get(after_binder) {
                        Some(Token {
                            kind: TokenKind::Semicolon,
                            ..
                        }) => true,
                        Some(Token {
                            kind: TokenKind::Comma,
                            ..
                        }) => self.guard_semicolon_follows(after_binder + 1),
                        _ => false,
                    };
                }
                TokenKind::Newline
                | TokenKind::Indent
                | TokenKind::Dedent
                | TokenKind::Comma
                | TokenKind::Semicolon
                    if depth == 0 =>
                {
                    return false;
                }
                _ => {}
            }

            index += 1;
        }

        false
    }

    fn guard_semicolon_follows(&self, start: usize) -> bool {
        let mut depth = 0usize;
        let mut index = self.skip_collection_trivia_from(start);

        while let Some(token) = self.tokens.get(index) {
            match &token.kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => {
                    depth += 1;
                }
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    if depth == 0 {
                        return false;
                    }
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Semicolon if depth == 0 => return true,
                TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Comma
                    if depth == 0 =>
                {
                    return false;
                }
                _ => {}
            }

            index += 1;
        }

        false
    }

    fn skip_collection_trivia_from(&self, mut index: usize) -> usize {
        while self.tokens.get(index).is_some_and(|token| {
            matches!(
                token.kind,
                TokenKind::Newline
                    | TokenKind::Indent
                    | TokenKind::Dedent
                    | TokenKind::DocComment(_)
            )
        }) {
            index += 1;
        }

        index
    }

    fn parse_label_name(&mut self) -> Option<(String, Span)> {
        let token = self.current()?.clone();
        match token.kind {
            TokenKind::Identifier(name) | TokenKind::ComptimeIdentifier(name) => {
                self.advance();
                Some((name, token.span))
            }
            _ => None,
        }
    }

    fn parse_transform_label_name(&mut self, mode: EntryMode) -> Option<(String, Span)> {
        let token = self.current()?.clone();
        if mode == EntryMode::Set
            && let TokenKind::Tag(name) = token.kind
        {
            self.advance();
            return Some((name, token.span));
        }

        self.parse_label_name()
    }

    fn current_is_transform_label_start(&self) -> bool {
        matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::Identifier(_) | TokenKind::ComptimeIdentifier(_) | TokenKind::Tag(_))
        )
    }

    fn recover_record_entry(&mut self) {
        while !self.at_end()
            && !self.current_is(TokenKind::CloseBrace)
            && !self.current_is(TokenKind::Newline)
            && !self.current_is(TokenKind::Indent)
            && !self.current_is(TokenKind::Dedent)
            && !self.current_is(TokenKind::Comma)
            && !self.current_is(TokenKind::Semicolon)
        {
            self.advance();
        }
    }

    fn find_binding_equals(&self) -> Option<usize> {
        let mut depth = 0usize;

        for index in self.cursor..self.tokens.len() {
            let token = &self.tokens[index];

            match &token.kind {
                TokenKind::Newline | TokenKind::Dedent => return None,
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => {
                    depth += 1;
                }
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Operator(operator) if operator == "=" && depth == 0 => {
                    return Some(index);
                }
                _ => {}
            }
        }

        None
    }

    fn is_signature_start(&self) -> bool {
        // A signature is `name : term` with no depth-0 `=` (callers check the
        // no-`=` part via `find_binding_equals` first).
        matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::Identifier(_) | TokenKind::ComptimeIdentifier(_))
        ) && self
            .tokens
            .get(self.cursor + 1)
            .is_some_and(|token| token.is_operator(":"))
    }

    fn is_lambda_start(&self) -> bool {
        if !self.current_is(TokenKind::OpenParen) {
            return false;
        }

        let mut depth = 0usize;

        for index in self.cursor..self.tokens.len() {
            match &self.tokens[index].kind {
                TokenKind::OpenParen => depth += 1,
                TokenKind::CloseParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self.lambda_arrow_follows(index + 1);
                    }
                }
                TokenKind::Newline | TokenKind::Dedent if depth == 0 => return false,
                _ => {}
            }
        }

        false
    }

    /// After the lambda parameter list's closing `)`, the head is a lambda when
    /// either `=>` follows directly, or a `: returnType` annotation followed by
    /// a depth-0 `=>` (before any newline) follows.
    fn lambda_arrow_follows(&self, start: usize) -> bool {
        let Some(token) = self.tokens.get(start) else {
            return false;
        };

        if token.is_operator("=>") {
            return true;
        }

        if !token.is_operator(":") {
            return false;
        }

        let mut depth = 0usize;
        for token in &self.tokens[start + 1..] {
            match &token.kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Operator(operator) if operator == "=>" && depth == 0 => return true,
                TokenKind::Newline | TokenKind::Dedent if depth == 0 => return false,
                _ => {}
            }
        }

        false
    }

    fn report_invalid_binding_name(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("invalid binding name")
                .with_code(codes::parse::INVALID_BINDING_NAME)
                .with_label(Label::primary(
                    span,
                    "binding names must be a single identifier",
                ))
                .with_note("a binding name is a single identifier such as `name` or `myValue`"),
        );
    }

    fn report_missing_binding_value(&mut self, offset: usize) -> Expr {
        let span = Span::point(offset);
        self.diagnostics.push(
            Diagnostic::error("binding is missing a value")
                .with_code(codes::parse::MISSING_BINDING_VALUE)
                .with_label(Label::primary(span, "expected an expression after `=`"))
                .with_note(
                    "add an expression after `=`, or use an indented block on the next line",
                ),
        );
        missing_expr(span)
    }

    fn report_missing_lambda_body(&mut self, offset: usize) -> Expr {
        let span = Span::point(offset);
        self.diagnostics.push(
            Diagnostic::error("lambda is missing a body")
                .with_code(codes::parse::MISSING_LAMBDA_BODY)
                .with_label(Label::primary(
                    span,
                    "expected an expression or indented block after `=>`",
                ))
                .with_note("a lambda body is an expression on the same line, or an indented block: `(params) =>\n  body`"),
        );
        missing_expr(span)
    }

    fn report_missing_match_arms(&mut self, offset: usize) {
        let span = Span::point(offset);
        self.diagnostics.push(
            Diagnostic::error("match expression is missing arms")
                .with_code(codes::parse::MISSING_MATCH_ARMS)
                .with_label(Label::primary(
                    span,
                    "expected an indented block of match arms after `?>`",
                ))
                .with_note("write one arm per line, for example `@Ok(value) => value`"),
        );
    }

    fn report_inline_match_arms(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("match arms must start on the next line, indented")
                .with_code(codes::parse::INLINE_MATCH_ARMS)
                .with_label(Label::primary(span, "move these arms to an indented block"))
                .with_note("write one arm per line, indented under `?>`, for example:\n  result ?>\n    @Ok(value) => value"),
        );
    }

    fn report_expected_match_arrow(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("expected match arm arrow")
                .with_code(codes::parse::EXPECTED_MATCH_ARROW)
                .with_label(Label::primary(span, "expected `=>` after this pattern"))
                .with_note("match arms use `pattern => expression`"),
        );
    }

    fn report_missing_match_body(&mut self, offset: usize) -> Expr {
        let span = Span::point(offset);
        self.diagnostics.push(
            Diagnostic::error("match arm is missing a body")
                .with_code(codes::parse::MISSING_MATCH_BODY)
                .with_label(Label::primary(
                    span,
                    "expected an expression or indented block after `=>`",
                ))
                .with_note("add the expression that should run when this pattern matches"),
        );
        missing_expr(span)
    }

    fn report_expected_pattern(&mut self, span: Span) -> Expr {
        self.diagnostics.push(
            Diagnostic::error("expected pattern")
                .with_code(codes::parse::EXPECTED_PATTERN)
                .with_label(Label::primary(span, "expected a pattern here"))
                .with_note("patterns can be `_`, names, literals, tuples, records, or constructors like `@Ok(value)`"),
        );
        missing_expr(span)
    }

    fn report_single_item_tuple(&mut self, comma_span: Span) {
        self.diagnostics.push(
            Diagnostic::error("anonymous 1-tuples are not supported")
                .with_code(codes::parse::SINGLE_ITEM_TUPLE)
                .with_label(Label::primary(
                    comma_span,
                    "this comma creates an anonymous 1-tuple",
                ))
                .with_note(
                    "remove the comma for grouping, or use a tagged tuple like `@Ok(value)`",
                ),
        );
    }

    fn report_expected_type(&mut self, offset: usize) -> Expr {
        let span = Span::point(offset);
        self.diagnostics.push(
            Diagnostic::error("expected type")
                .with_code(codes::parse::EXPECTED_TYPE)
                .with_label(Label::primary(span, "expected a type here"))
                .with_note("types include names like `Text`, variables like `a`, functions like `a -> b`, records, variants, and applications like `Array[a]`"),
        );
        missing_expr(span)
    }

    fn report_expected_record_entry(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("expected record entry")
                .with_code(codes::parse::EXPECTED_RECORD_ENTRY)
                .with_label(Label::primary(span, "expected a record field or transform"))
                .with_note("record entries are fields, shorthands, spreads, deletes, or renames"),
        );
    }

    fn report_legacy_record_field_separator(&mut self, span: Span, name: &str, overwrite: bool) {
        let (message, replacement, example) = if overwrite {
            (
                "record replacements use `::`, not `:=`",
                "replace this marker with `::`",
                format!("{name} :: value"),
            )
        } else {
            (
                "record fields use `:`, not `=`",
                "replace this separator with `:`",
                format!("{name}: value"),
            )
        };

        self.diagnostics.push(
            Diagnostic::error(message)
                .with_code(codes::parse::EXPECTED_RECORD_ENTRY)
                .with_label(Label::primary(span, replacement))
                .with_note(format!("write `{example}`")),
        );
    }

    fn report_expected_record_label(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("expected record label")
                .with_code(codes::parse::EXPECTED_RECORD_LABEL)
                .with_label(Label::primary(span, "expected a field name here"))
                .with_note("use a bare field name such as `password` or `fullName`"),
        );
    }

    fn report_expected_field_name(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("expected field name")
                .with_code(codes::parse::EXPECTED_FIELD_NAME)
                .with_label(Label::primary(
                    span,
                    "expected a field name after this access",
                ))
                .with_note("field access uses `.name` or `?.name`"),
        );
    }

    fn report_unexpected_separator(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("unexpected separator")
                .with_code(codes::parse::UNEXPECTED_SEPARATOR)
                .with_label(Label::primary(span, "extra separator"))
                .with_note("remove the extra `,` or `;`"),
        );
    }

    fn report_unexpected_comptime_marker(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("unexpected comptime parameter marker")
                .with_code(codes::parse::UNEXPECTED_COMPTIME_MARKER)
                .with_label(Label::primary(span, "`@` marker used outside a parameter declaration"))
                .with_note("comptime markers belong on lambda parameter declarations, for example `(@key: Keys) => key`"),
        );
    }

    fn report_unexpected_indentation(&mut self) {
        let span = self.current_span();
        self.diagnostics.push(
            Diagnostic::error("unexpected indentation")
                .with_code(codes::parse::UNEXPECTED_INDENTATION)
                .with_label(Label::primary(span, "top-level items cannot be indented"))
                .with_note("remove the indentation or place the item inside a block"),
        );
    }

    fn report_unsupported_remainder(&mut self) {
        let Some(token) = self.current() else {
            return;
        };

        if self.at_item_boundary() {
            return;
        }

        if token.is_close_delimiter() {
            self.consume_close_delimiter_if_present();
            return;
        }

        self.diagnostics.push(
            Diagnostic::error("unsupported expression syntax")
                .with_code(codes::parse::UNSUPPORTED_SYNTAX)
                .with_label(Label::primary(
                    token.span,
                    "this syntax is not supported by the core parser yet",
                ))
                .with_note("custom operators are not supported yet; supported operators are `|>`, `??`, `||`, `&&`, `==`, `!=`, `<`, `<=`, `>`, `>=`, `+`, `-`, `*`, `/`, `%`, and `^`"),
        );
        self.recover_to_next_line();
    }

    fn consume_close_paren(&mut self) {
        if self.current_is(TokenKind::CloseParen) {
            self.advance();
            return;
        }

        self.consume_close_delimiter_if_present();
    }

    fn consume_close_delimiter_if_present(&mut self) -> bool {
        if self.current().is_some_and(Token::is_close_delimiter) {
            self.advance();
            return true;
        }

        false
    }

    fn consume_operator(&mut self, expected: &str) -> bool {
        if self
            .current()
            .is_some_and(|token| token.is_operator(expected))
        {
            self.advance();
            return true;
        }

        false
    }

    fn consume_comma(&mut self) -> bool {
        if self.current_is(TokenKind::Comma) {
            self.advance();
            return true;
        }

        false
    }

    fn current_is_operator(&self, expected: &str) -> bool {
        self.current()
            .is_some_and(|token| token.is_operator(expected))
    }

    fn next_is_operator(&self, expected: &str) -> bool {
        self.tokens
            .get(self.cursor + 1)
            .is_some_and(|token| token.is_operator(expected))
    }

    fn current_infix_operator(&self) -> Option<InfixOperator> {
        let Some(Token {
            kind: TokenKind::Operator(operator),
            span,
        }) = self.current()
        else {
            return None;
        };

        let (left_binding_power, right_binding_power) = infix_binding_power(operator)?;

        Some(InfixOperator {
            text: operator.clone(),
            span: *span,
            left_binding_power,
            right_binding_power,
        })
    }

    fn consume_collection_separator(&mut self, allow_semicolon: bool) -> bool {
        let mut consumed = self.skip_collection_trivia();
        let mut seen_separator = false;

        loop {
            if self.current_is(TokenKind::Comma)
                || (allow_semicolon && self.current_is(TokenKind::Semicolon))
            {
                let span = self.current_span();
                if seen_separator {
                    self.report_unexpected_separator(span);
                }

                seen_separator = true;
                consumed = true;
                self.advance();
                consumed |= self.skip_collection_trivia();
                continue;
            }

            break;
        }

        consumed
    }

    fn skip_collection_trivia(&mut self) -> bool {
        let mut consumed = false;

        while self.current_is(TokenKind::Newline)
            || self.current_is(TokenKind::Indent)
            || self.current_is(TokenKind::Dedent)
            || matches!(
                self.current().map(|token| &token.kind),
                Some(TokenKind::DocComment(_))
            )
        {
            consumed = true;
            self.advance();
        }

        consumed
    }

    fn consume_newline(&mut self) -> bool {
        if self.current_is(TokenKind::Newline) {
            self.advance();
            return true;
        }

        false
    }

    fn skip_newlines_and_doc_comments(&mut self) {
        while self.current_is(TokenKind::Newline)
            || matches!(
                self.current().map(|token| &token.kind),
                Some(TokenKind::DocComment(_))
            )
        {
            self.advance();
        }
    }

    fn recover_to_next_line(&mut self) {
        while !self.at_end()
            && !self.current_is(TokenKind::Newline)
            && !self.current_is(TokenKind::Dedent)
        {
            self.advance();
        }
        self.consume_newline();
    }

    fn at_item_boundary(&self) -> bool {
        // A block-bodied lambda's closing `Dedent` may already be consumed as
        // collection trivia, leaving the cursor on the next top-level item with
        // the Dedent as the *previous* token. Treat that as a boundary too so a
        // following binding (`first = () =>\n  value\nsecond = 2`) is not
        // swallowed as a continuation of the lambda body.
        self.at_end()
            || self.current_is(TokenKind::Newline)
            || self.current_is(TokenKind::Dedent)
            || self.previous_is(TokenKind::Dedent)
    }

    fn previous_is(&self, kind: TokenKind) -> bool {
        self.cursor
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .is_some_and(|token| token.kind == kind)
    }

    fn current(&self) -> Option<&Token> {
        self.tokens.get(self.cursor)
    }

    fn current_span(&self) -> Span {
        self.current()
            .map(|token| token.span)
            .unwrap_or_else(|| Span::point(self.previous_end()))
    }

    fn current_is(&self, kind: TokenKind) -> bool {
        self.current().is_some_and(|token| token.kind == kind)
    }

    fn next_is(&self, kind: TokenKind) -> bool {
        self.tokens
            .get(self.cursor + 1)
            .is_some_and(|token| token.kind == kind)
    }

    fn current_is_empty_set_postfix(&self, previous_end: usize) -> bool {
        let Some(at) = self.current() else {
            return false;
        };
        let Some(open) = self.tokens.get(self.cursor + 1) else {
            return false;
        };
        let Some(close) = self.tokens.get(self.cursor + 2) else {
            return false;
        };

        at.is_operator("@")
            && at.span.start == previous_end
            && open.kind == TokenKind::OpenBrace
            && open.span.start == at.span.end
            && close.kind == TokenKind::CloseBrace
            && close.span.start == open.span.end
    }

    fn close_exists_before_item_boundary(&self, close: TokenKind) -> bool {
        for token in &self.tokens[self.cursor..] {
            if token.kind == close {
                return true;
            }

            if matches!(token.kind, TokenKind::Newline | TokenKind::Dedent) {
                return false;
            }
        }

        false
    }

    fn current_is_any_close_delimiter(&self) -> bool {
        self.current().is_some_and(Token::is_close_delimiter)
    }

    fn consume_close(&mut self, close: TokenKind) -> usize {
        if self.current_is(close) {
            let span = self.current_span();
            self.advance();
            return span.end;
        }

        self.consume_close_delimiter_if_present();
        self.previous_end()
    }

    fn advance(&mut self) {
        if !self.at_end() {
            self.cursor += 1;
        }
    }

    fn at_end(&self) -> bool {
        self.cursor >= self.tokens.len()
    }

    fn previous_end(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }

        self.tokens
            .get(self.cursor.saturating_sub(1))
            .map_or(0, |token| token.span.end)
    }
}

fn literal_expr(literal: Literal, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Literal(literal),
        span,
    }
}

fn collection_type_application(
    collection: &str,
    collection_span: Span,
    element: Expr,
    end: usize,
) -> Expr {
    let start = element.span.start;
    Expr {
        kind: ExprKind::Index {
            callee: Box::new(Expr {
                kind: ExprKind::ComptimeName(collection.to_owned()),
                span: collection_span,
            }),
            args: vec![element],
        },
        span: Span::new(start, end),
    }
}

fn missing_expr(span: Span) -> Expr {
    Expr {
        kind: ExprKind::Missing,
        span,
    }
}

fn record_entry_span(entry: &RecordEntry) -> Span {
    match entry {
        RecordEntry::Field { span, .. }
        | RecordEntry::FieldComputed { span, .. }
        | RecordEntry::Shorthand { span, .. }
        | RecordEntry::Spread { span, .. }
        | RecordEntry::Delete { span, .. }
        | RecordEntry::DeleteComputed { span, .. }
        | RecordEntry::Rename { span, .. }
        | RecordEntry::Iteration { span, .. }
        | RecordEntry::Open { span } => *span,
        RecordEntry::Element(expr) => expr.span,
    }
}

fn infix_binding_power(operator: &str) -> Option<(u8, u8)> {
    let precedence = match operator {
        "|>" => 1,
        "??" => 2,
        "||" => 3,
        "&&" => 4,
        "==" | "!=" | "<" | "<=" | ">" | ">=" => 5,
        "+" | "-" => 6,
        "*" | "/" | "%" => 7,
        "^" => return Some((8, 8)),
        _ => return None,
    };

    Some((precedence, precedence + 1))
}

fn scan_delimiters(tokens: &[Token]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut stack = Vec::new();

    for token in tokens {
        match token.kind {
            TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => {
                stack.push((token.kind.clone(), token.span));
            }
            TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                close_delimiter(token, &mut stack, &mut diagnostics);
            }
            _ => {}
        }
    }

    for (delimiter, span) in stack {
        diagnostics.push(
            Diagnostic::error(format!("unclosed `{}`", delimiter_text(&delimiter)))
                .with_code(codes::parse::UNCLOSED_DELIMITER)
                .with_label(Label::primary(span, "opened here")),
        );
    }

    diagnostics
}

fn close_delimiter(
    close: &Token,
    stack: &mut Vec<(TokenKind, Span)>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some((open, span)) = stack.pop() else {
        diagnostics.push(
            Diagnostic::error(format!("unexpected `{}`", delimiter_text(&close.kind)))
                .with_code(codes::parse::UNEXPECTED_DELIMITER)
                .with_label(Label::primary(close.span, "unexpected here")),
        );
        return;
    };

    if matching_close(&open) == close.kind {
        return;
    }

    diagnostics.push(
        Diagnostic::error(format!(
            "mismatched delimiter `{}`",
            delimiter_text(&close.kind)
        ))
        .with_code(codes::parse::MISMATCHED_DELIMITER)
        .with_label(Label::primary(
            span,
            format!("opened with `{}`", delimiter_text(&open)),
        ))
        .with_label(Label::primary(
            close.span,
            format!("closed with `{}`", delimiter_text(&close.kind)),
        )),
    );
}

fn matching_close(open: &TokenKind) -> TokenKind {
    match open {
        TokenKind::OpenParen => TokenKind::CloseParen,
        TokenKind::OpenBracket => TokenKind::CloseBracket,
        TokenKind::OpenBrace => TokenKind::CloseBrace,
        _ => unreachable!("matching_close called on non-open-delimiter: {open:?}"),
    }
}

fn delimiter_text(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::OpenParen => "(",
        TokenKind::CloseParen => ")",
        TokenKind::OpenBracket => "[",
        TokenKind::CloseBracket => "]",
        TokenKind::OpenBrace => "{",
        TokenKind::CloseBrace => "}",
        _ => unreachable!("delimiter_text called on non-delimiter token: {kind:?}"),
    }
}

#[cfg(test)]
mod tests {
    use aven_core::{FileId, SourceFile};

    use super::{
        ExprKind, Item, Literal, ParseOutput, PropagationMode, RecordEntry, parse_module,
        parse_source,
    };
    use crate::TokenKind;

    #[test]
    fn parses_call_expressions_into_ast_nodes() {
        let output = parse_module("print(message)\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Expr(expr)) = output.module.items.first() else {
            panic!("expected expression item");
        };
        let ExprKind::Call { callee, args } = &expr.kind else {
            panic!("expected call expression");
        };

        assert!(matches!(callee.kind, ExprKind::Name(_)));
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn parse_output_preserves_raw_and_layout_token_streams() {
        let output = parse_module("# plain\n## doc\nvalue = 1\n");

        assert!(output.diagnostics.is_empty());
        assert!(
            output
                .raw_tokens
                .iter()
                .any(|token| matches!(token.kind, TokenKind::Comment(_)))
        );
        assert!(
            output
                .raw_tokens
                .iter()
                .any(|token| matches!(token.kind, TokenKind::DocComment(_)))
        );
        assert!(
            !output
                .layout_tokens
                .iter()
                .any(|token| matches!(token.kind, TokenKind::Comment(_)))
        );
        assert!(
            output
                .layout_tokens
                .iter()
                .any(|token| matches!(token.kind, TokenKind::DocComment(_)))
        );
    }

    #[test]
    fn parse_source_preserves_file_id() {
        let file = SourceFile::new(FileId(42), "test.av", None, "value = 1\n");
        let output = parse_source(&file);

        assert_eq!(output.file_id, FileId(42));
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn parses_lambda_bindings_into_ast_nodes() {
        let output = parse_module("identity = (value) => value\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };
        let ExprKind::Lambda { params, body, .. } = &binding.value.kind else {
            panic!("expected lambda expression");
        };

        assert_eq!(params.len(), 1);
        assert!(matches!(body.kind, ExprKind::Name(_)));
    }

    #[test]
    fn parses_string_literals_into_ast_nodes() {
        let output = parse_module("name = \"Aven\"\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };

        assert!(matches!(
            &binding.value.kind,
            ExprKind::Literal(Literal::String(text)) if text == "\"Aven\""
        ));
    }

    #[test]
    fn parses_negative_number_literals_into_ast_nodes() {
        let output = parse_module("value = -1\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };

        let ExprKind::Unary {
            operator, value, ..
        } = &binding.value.kind
        else {
            panic!("expected unary expression");
        };

        assert_eq!(operator, "-");
        assert!(matches!(
            &value.kind,
            ExprKind::Literal(Literal::Number(number)) if number == "1"
        ));
    }

    #[test]
    fn parses_unary_minus_after_binary_operators() {
        let output = parse_module("result = 1 + -2\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Binary {
            operator, right, ..
        } = binding_value(&output, 0)
        else {
            panic!("expected binary expression");
        };

        assert_eq!(operator, "+");
        let ExprKind::Unary {
            operator, value, ..
        } = &right.kind
        else {
            panic!("expected unary expression");
        };

        assert_eq!(operator, "-");
        assert!(matches!(
            &value.kind,
            ExprKind::Literal(Literal::Number(number)) if number == "2"
        ));
    }

    #[test]
    fn parses_unary_minus_before_names_and_groups() {
        let output = parse_module("left = -x\nright = -(a + b)\n");

        assert!(output.diagnostics.is_empty());
        assert!(matches!(
            binding_value(&output, 0),
            ExprKind::Unary { value, .. } if matches!(&value.kind, ExprKind::Name(name) if name == "x")
        ));
        assert!(matches!(
            binding_value(&output, 1),
            ExprKind::Unary { value, .. } if matches!(&value.kind, ExprKind::Group(_))
        ));
    }

    #[test]
    fn parses_structural_literals_into_ast_nodes() {
        let output =
            parse_module("items = [1, \"two\"]\npair = (1, \"two\")\ncolors = @{ @Red, @Ok(1) }\n");

        assert!(output.diagnostics.is_empty());
        assert!(matches!(binding_value(&output, 0), ExprKind::Array(items) if items.len() == 2));
        assert!(matches!(binding_value(&output, 1), ExprKind::Tuple(items) if items.len() == 2));
        let ExprKind::Set(entries) = binding_value(&output, 2) else {
            panic!("expected set expression");
        };
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0],
            RecordEntry::Element(expr) if matches!(&expr.kind, ExprKind::Tag(name) if name == "Red")
        ));
        assert!(matches!(
            &entries[1],
            RecordEntry::Element(expr) if matches!(&expr.kind, ExprKind::Call { .. })
        ));
    }

    #[test]
    fn parses_record_transform_entries_into_ast_nodes() {
        let output = parse_module(
            "cleaned = { ..user, :..defaults, -password, -[key], [key]: value, [other]: maybe, name -> fullName, active: true, age :: 37 }\n",
        );

        assert!(output.diagnostics.is_empty());
        let ExprKind::Record(entries) = binding_value(&output, 0) else {
            panic!("expected record expression");
        };

        assert_eq!(entries.len(), 9);
        assert!(matches!(
            &entries[0],
            RecordEntry::Spread {
                overwrite: false,
                ..
            }
        ));
        assert!(matches!(
            &entries[1],
            RecordEntry::Spread {
                overwrite: true,
                ..
            }
        ));
        assert!(matches!(
            &entries[2],
            RecordEntry::Delete { name, .. } if name == "password"
        ));
        assert!(matches!(
            &entries[3],
            RecordEntry::DeleteComputed { key, .. }
                if matches!(&key.kind, ExprKind::Name(name) if name == "key")
        ));
        assert!(matches!(
            &entries[4],
            RecordEntry::FieldComputed { key, .. } if matches!(&key.kind, ExprKind::Name(name) if name == "key")
        ));
        assert!(matches!(
            &entries[5],
            RecordEntry::FieldComputed { key, .. } if matches!(&key.kind, ExprKind::Name(name) if name == "other")
        ));
        assert!(matches!(
            &entries[6],
            RecordEntry::Rename { from, to, .. } if from == "name" && to == "fullName"
        ));
        assert!(matches!(
            &entries[7],
            RecordEntry::Field {
                name,
                overwrite: false,
                ..
            } if name == "active"
        ));
        assert!(matches!(
            &entries[8],
            RecordEntry::Field {
                name,
                overwrite: true,
                ..
            } if name == "age"
        ));
    }

    #[test]
    fn reports_legacy_record_separators_and_recovers_fields() {
        let output = parse_module("user = { name = \"Ada\", age := 36 }\n");
        let codes: Vec<_> = output
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect();

        assert_eq!(
            codes,
            vec!["parse.expected-record-entry", "parse.expected-record-entry"]
        );
        let ExprKind::Record(entries) = binding_value(&output, 0) else {
            panic!("expected record expression");
        };
        assert!(matches!(
            &entries[0],
            RecordEntry::Field {
                name,
                overwrite: false,
                ..
            } if name == "name"
        ));
        assert!(matches!(
            &entries[1],
            RecordEntry::Field {
                name,
                overwrite: true,
                ..
            } if name == "age"
        ));
    }

    #[test]
    fn parses_binary_precedence_into_ast_nodes() {
        let output = parse_module("value = 1 + 2 * 3\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Binary {
            operator, right, ..
        } = binding_value(&output, 0)
        else {
            panic!("expected binary expression");
        };

        assert_eq!(operator, "+");
        assert!(matches!(
            &right.kind,
            ExprKind::Binary {
                operator,
                ..
            } if operator == "*"
        ));
    }

    #[test]
    fn parses_field_access_and_pipelines_into_ast_nodes() {
        let output = parse_module("value = users?.active |> toJson()\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Binary {
            operator,
            left,
            right,
            ..
        } = binding_value(&output, 0)
        else {
            panic!("expected binary pipeline expression");
        };

        assert_eq!(operator, "|>");
        assert!(matches!(
            &left.kind,
            ExprKind::FieldAccess {
                field,
                null_safe: true,
                ..
            } if field == "active"
        ));
        assert!(matches!(&right.kind, ExprKind::Call { .. }));
    }

    #[test]
    fn parses_result_propagation_into_ast_nodes() {
        let output = parse_module("value = read(path)?^\nquick = read(path)?!\n");

        assert!(output.diagnostics.is_empty());
        assert!(matches!(
            binding_value(&output, 0),
            ExprKind::Propagate {
                mode: PropagationMode::ReturnError,
                ..
            }
        ));
        assert!(matches!(
            binding_value(&output, 1),
            ExprKind::Propagate {
                mode: PropagationMode::Panic,
                ..
            }
        ));
    }

    #[test]
    fn parses_match_operator_into_ast_nodes() {
        let output =
            parse_module("value = result ?>\n  @Ok(x) => x\n  @Err(error) => fallback(error)\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Match { subject, arms, .. } = binding_value(&output, 0) else {
            panic!("expected match expression");
        };

        assert!(matches!(&subject.kind, ExprKind::Name(name) if name == "result"));
        assert_eq!(arms.len(), 2);
        assert!(matches!(
            &arms[0].pattern.kind,
            ExprKind::Call { callee, args }
                if matches!(&callee.kind, ExprKind::Tag(name) if name == "Ok")
                    && args.len() == 1
        ));
        assert!(matches!(
            &arms[1].pattern.kind,
            ExprKind::Call { callee, args }
                if matches!(&callee.kind, ExprKind::Tag(name) if name == "Err")
                    && args.len() == 1
        ));
    }

    #[test]
    fn parses_record_patterns_and_match_guards() {
        let output = parse_module(
            "value = user ?>\n  { age }, age >= 18 => \"adult\"\n  { givenName -> firstName, status: @Active, ..rest } => firstName\n  { .. } => \"ignored\"\n",
        );

        assert!(output.diagnostics.is_empty());
        let ExprKind::Match { arms, .. } = binding_value(&output, 0) else {
            panic!("expected match expression");
        };

        assert_eq!(arms.len(), 3);
        assert_eq!(arms[0].guards.len(), 1);
        assert!(
            matches!(&arms[0].guards[0].kind, ExprKind::Binary { operator, .. } if operator == ">=")
        );
        assert!(matches!(
            &arms[0].pattern.kind,
            ExprKind::Record(entries)
                if matches!(
                    &entries[..],
                    [RecordEntry::Shorthand { name, .. }] if name == "age"
                )
        ));
        assert!(matches!(
            &arms[1].pattern.kind,
            ExprKind::Record(entries)
                if entries.len() == 3
                    && matches!(
                        &entries[0],
                        RecordEntry::Rename { from, to, .. }
                            if from == "givenName" && to == "firstName"
                    )
                    && matches!(
                        &entries[1],
                        RecordEntry::Field { name, value, .. }
                            if name == "status"
                                && matches!(&value.kind, ExprKind::Tag(name) if name == "Active")
                    )
                    && matches!(
                        &entries[2],
                        RecordEntry::Spread { value, overwrite: false, .. }
                            if matches!(&value.kind, ExprKind::Name(name) if name == "rest")
                    )
        ));
        assert!(matches!(
            &arms[2].pattern.kind,
            ExprKind::Record(entries)
                if matches!(&entries[..], [RecordEntry::Open { .. }])
        ));
    }

    #[test]
    fn parses_parenthesized_patterns_as_grouping() {
        let output = parse_module("value = result ?>\n  (@Ok(x)) => x\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Match { arms, .. } = binding_value(&output, 0) else {
            panic!("expected match expression");
        };

        assert_eq!(arms.len(), 1);
        assert!(matches!(
            &arms[0].pattern.kind,
            ExprKind::Group(inner)
                if matches!(
                    &inner.kind,
                    ExprKind::Call { callee, args }
                        if matches!(&callee.kind, ExprKind::Tag(name) if name == "Ok")
                            && args.len() == 1
                )
        ));
    }

    #[test]
    fn parses_function_type_into_arrow_with_flattened_params() {
        let output = parse_module("mapper : (Array[a], a -> b) -> Array[b]\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Signature(signature)) = output.module.items.first() else {
            panic!("expected signature item");
        };
        let ExprKind::Arrow { params, result } = &signature.annotation.kind else {
            panic!("expected arrow type");
        };

        // `(Array[a], a -> b)` flattens to two params.
        assert_eq!(params.len(), 2);
        assert!(matches!(&params[0].kind, ExprKind::Index { .. }));
        assert!(matches!(&params[1].kind, ExprKind::Arrow { .. }));
        assert!(matches!(&result.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn parses_single_param_arrow_with_one_param() {
        let output = parse_module("f : a -> b\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Signature(signature)) = output.module.items.first() else {
            panic!("expected signature item");
        };
        let ExprKind::Arrow { params, .. } = &signature.annotation.kind else {
            panic!("expected arrow type");
        };
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn parses_index_application_into_ast_nodes() {
        let output = parse_module("xs : Array[a]\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Signature(signature)) = output.module.items.first() else {
            panic!("expected signature item");
        };
        let ExprKind::Index { callee, args } = &signature.annotation.kind else {
            panic!("expected index expression");
        };
        assert!(matches!(&callee.kind, ExprKind::ComptimeName(name) if name == "Array"));
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn parses_postfix_nullable_and_stops_at_equals() {
        let output = parse_module("value : Text? = name\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };
        let annotation = binding.annotation.as_ref().expect("expected an annotation");
        let ExprKind::Nullable(inner) = &annotation.kind else {
            panic!("expected nullable annotation, got {:?}", annotation.kind);
        };
        assert!(matches!(&inner.kind, ExprKind::ComptimeName(name) if name == "Text"));
        assert!(matches!(&binding.value.kind, ExprKind::Name(name) if name == "name"));
    }

    #[test]
    fn parses_prefix_optional_and_composed_nullability() {
        let output = parse_module("optional : ?Text = name\nboth : ?Text? = name\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(optional)) = output.module.items.first() else {
            panic!("expected optional binding");
        };
        let optional_annotation = optional.annotation.as_ref().expect("expected annotation");
        let ExprKind::Optional(inner) = &optional_annotation.kind else {
            panic!(
                "expected optional annotation, got {:?}",
                optional_annotation.kind
            );
        };
        assert!(matches!(&inner.kind, ExprKind::ComptimeName(name) if name == "Text"));

        let Some(Item::Binding(both)) = output.module.items.get(1) else {
            panic!("expected composed binding");
        };
        let both_annotation = both.annotation.as_ref().expect("expected annotation");
        let ExprKind::Optional(inner) = &both_annotation.kind else {
            panic!(
                "expected optional outer annotation, got {:?}",
                both_annotation.kind
            );
        };
        assert!(
            matches!(&inner.kind, ExprKind::Nullable(nullable_inner) if matches!(&nullable_inner.kind, ExprKind::ComptimeName(name) if name == "Text"))
        );
    }

    #[test]
    fn match_operator_followed_by_indented_block_parses_match() {
        let output = parse_module("value = result ?>\n  @Ok(x) => x\n");

        assert!(output.diagnostics.is_empty());
        assert!(matches!(binding_value(&output, 0), ExprKind::Match { .. }));
    }

    #[test]
    fn bare_question_postfix_is_nullable_with_no_diagnostics() {
        let output = parse_module("value = result ?\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Nullable(inner) = binding_value(&output, 0) else {
            panic!("expected nullable expression");
        };
        assert!(matches!(&inner.kind, ExprKind::Name(name) if name == "result"));
    }

    #[test]
    fn match_operator_without_arm_block_reports_missing_match_arms() {
        let output = parse_module("value = result ?>\n");

        let codes: Vec<_> = output
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect();
        assert_eq!(codes, vec!["parse.missing-match-arms"]);
    }

    #[test]
    fn parses_lambda_param_and_return_annotations() {
        let output =
            parse_module("load = (path : Path) : Result[Config, ConfigError] =>\n  read(path)?^\n");

        assert!(output.diagnostics.is_empty());
        let ExprKind::Lambda {
            params,
            return_annotation,
            ..
        } = binding_value(&output, 0)
        else {
            panic!("expected lambda expression");
        };
        assert_eq!(params.len(), 1);
        assert!(matches!(
            params[0].annotation.as_ref().map(|a| &a.kind),
            Some(ExprKind::ComptimeName(name)) if name == "Path"
        ));
        let return_annotation = return_annotation
            .as_ref()
            .expect("expected return annotation");
        assert!(matches!(&return_annotation.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn parses_record_type_with_open_optional_type_and_delete_entries() {
        let output = parse_module(
            "user : { name: Text, email: Text?, phone: ?Text, -password, .. } = current\n",
        );

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };
        let annotation = binding.annotation.as_ref().expect("expected annotation");
        let ExprKind::Record(entries) = &annotation.kind else {
            panic!("expected record type annotation");
        };
        assert_eq!(entries.len(), 5);
        assert!(matches!(
            &entries[0],
            RecordEntry::Field { name, .. } if name == "name"
        ));
        assert!(matches!(
            &entries[1],
            RecordEntry::Field { name, value, .. }
                if name == "email" && matches!(&value.kind, ExprKind::Nullable(_))
        ));
        assert!(matches!(
            &entries[2],
            RecordEntry::Field { name, value, .. }
                if name == "phone" && matches!(&value.kind, ExprKind::Optional(_))
        ));
        assert!(matches!(
            &entries[3],
            RecordEntry::Delete { name, .. } if name == "password"
        ));
        assert!(matches!(&entries[4], RecordEntry::Open { .. }));
    }

    #[test]
    fn parses_variant_type_elements_spreads_and_deletes() {
        let output = parse_module(
            "error : @{@ParseError(Text), @NotFound, -@Internal, @NotFound -> @Missing, ..FileError} = @ParseError(\"bad\")\n",
        );

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };
        let annotation = binding.annotation.as_ref().expect("expected annotation");
        let ExprKind::Set(entries) = &annotation.kind else {
            panic!("expected variant set annotation");
        };
        assert_eq!(entries.len(), 5);
        assert!(matches!(
            &entries[0],
            RecordEntry::Element(expr) if matches!(&expr.kind, ExprKind::Call { .. })
        ));
        assert!(matches!(
            &entries[1],
            RecordEntry::Element(expr) if matches!(&expr.kind, ExprKind::Tag(name) if name == "NotFound")
        ));
        assert!(matches!(
            &entries[4],
            RecordEntry::Spread {
                overwrite: false,
                ..
            }
        ));
        assert!(matches!(
            &entries[2],
            RecordEntry::Delete { name, .. } if name == "Internal"
        ));
        assert!(matches!(
            &entries[3],
            RecordEntry::Rename { from, to, .. } if from == "NotFound" && to == "Missing"
        ));
    }

    #[test]
    fn parses_top_level_signature_item() {
        let output = parse_module("load : (Path) -> Result[Config, ConfigError]\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Signature(signature)) = output.module.items.first() else {
            panic!("expected signature item");
        };
        assert_eq!(signature.name, "load");
        assert!(matches!(&signature.annotation.kind, ExprKind::Arrow { .. }));
    }

    fn binding_value(output: &ParseOutput, index: usize) -> &ExprKind {
        let Some(Item::Binding(binding)) = output.module.items.get(index) else {
            panic!("expected binding item");
        };
        &binding.value.kind
    }
}
