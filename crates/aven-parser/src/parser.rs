use aven_core::{Diagnostic, Label, Span};

use crate::{Token, TokenKind, layout_source};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Binding(Binding),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
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
    Name(String),
    ComptimeName(String),
    Group(Box<Expr>),
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    Record(Vec<RecordEntry>),
    Set(Vec<Expr>),
    Call { callee: Box<Expr>, args: Vec<Expr> },
    Lambda { params: Vec<Param>, body: Box<Expr> },
    Block(Vec<Item>),
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
    Rename {
        from: String,
        from_span: Span,
        to: String,
        to_span: Span,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Number(String),
    String(String),
    Regex(String),
    Path(String),
    Label(String),
}

#[derive(Debug, Clone)]
pub struct ParseOutput {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse_module(source: &str) -> ParseOutput {
    let mut layout = layout_source(source);
    let mut diagnostics = Vec::new();
    diagnostics.append(&mut layout.diagnostics);
    diagnostics.extend(scan_delimiters(&layout.tokens));

    let mut parser = Parser {
        tokens: &layout.tokens,
        cursor: 0,
        diagnostics,
    };
    let module = parser.parse_module();

    ParseOutput {
        module,
        diagnostics: parser.diagnostics,
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    diagnostics: Vec<Diagnostic>,
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
                    .with_code("parse.missing-binding-name")
                    .with_label(Label::primary(Span::point(span.start), "expected a name"))
                    .with_note("add a name before `=`, for example `name = expr`"),
            );
            self.recover_to_next_line();
            return None;
        }

        let name_token = &self.tokens[self.cursor];
        if equals != self.cursor + 1 {
            let span = Span::new(name_token.span.start, self.tokens[equals - 1].span.end);
            self.report_invalid_binding_name(span);
            self.recover_to_next_line();
            return None;
        }

        let (name, name_span) = match &name_token.kind {
            TokenKind::Identifier(name) | TokenKind::ComptimeIdentifier(name) => {
                (name.clone(), name_token.span)
            }
            _ => {
                self.report_invalid_binding_name(name_token.span);
                self.recover_to_next_line();
                return None;
            }
        };

        self.cursor = equals + 1;
        let value = self.parse_binding_value(self.tokens[equals].span.end);
        let span = name_span.merge(value.span);
        self.consume_newline();

        Some(Binding {
            name,
            name_span,
            value,
            span,
        })
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
        if self.is_lambda_start() {
            return self.parse_lambda();
        }

        self.parse_call()
    }

    fn parse_lambda(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();
        let params = self.parse_lambda_params();
        self.consume_close_paren();
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
                    params.push(Param {
                        name: name.clone(),
                        span: *span,
                    });
                    self.advance();
                }
                Some(token) => {
                    self.diagnostics.push(
                        Diagnostic::error("expected lambda parameter")
                            .with_code("parse.expected-parameter")
                            .with_label(Label::primary(token.span, "expected a parameter name"))
                            .with_note("use an identifier like `x`, or `_` to ignore an argument"),
                    );
                    self.advance();
                }
                None => break,
            }

            if self.consume_operator(",") {
                continue;
            }

            break;
        }

        params
    }

    fn parse_call(&mut self) -> Expr {
        let mut expr = self.parse_atom();

        while self.current_is(TokenKind::OpenParen) {
            expr = self.finish_call(expr);
        }

        expr
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

            if self.consume_operator(",") {
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

    fn parse_atom(&mut self) -> Expr {
        let Some(token) = self.current().cloned() else {
            return missing_expr(Span::point(self.previous_end()));
        };

        match token.kind {
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
                        .with_code("parse.expected-expression")
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

        if !self.current_is_operator(",") {
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

            if self.current_is_operator(",") {
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
        let items = self.parse_expression_list(TokenKind::CloseBrace);
        let end = self.consume_close(TokenKind::CloseBrace);

        Expr {
            kind: ExprKind::Set(items),
            span: Span::new(start, end),
        }
    }

    fn parse_expression_list(&mut self, close: TokenKind) -> Vec<Expr> {
        let mut items = Vec::new();
        self.skip_collection_trivia();

        while !self.at_end() && !self.current_is(close.clone()) {
            if self.current_is_any_close_delimiter() {
                break;
            }

            items.push(self.parse_expression());

            if self.current_is(close.clone()) {
                break;
            }

            let had_separator = self.consume_collection_separator(false);
            if self.current_is(close.clone()) {
                break;
            }

            if !had_separator {
                break;
            }
        }

        items
    }

    fn parse_record(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();
        let mut entries = Vec::new();
        self.skip_collection_trivia();

        while !self.at_end() && !self.current_is(TokenKind::CloseBrace) {
            if self.current_is_any_close_delimiter() {
                break;
            }

            if let Some(entry) = self.parse_record_entry() {
                entries.push(entry);
            }

            if self.current_is(TokenKind::CloseBrace) {
                break;
            }

            let had_separator = self.consume_collection_separator(true);
            if self.current_is(TokenKind::CloseBrace) {
                break;
            }

            if !had_separator {
                break;
            }
        }

        let end = self.consume_close(TokenKind::CloseBrace);

        Expr {
            kind: ExprKind::Record(entries),
            span: Span::new(start, end),
        }
    }

    fn parse_record_entry(&mut self) -> Option<RecordEntry> {
        if matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::LabelPath(_))
        ) {
            self.report_expected_record_label(self.current_span());
            self.recover_record_entry();
            return None;
        }

        if self.current_is_operator("..") || self.current_is_operator(":..") {
            let operator_span = self.current_span();
            let overwrite = self.current_is_operator(":..");
            self.advance();
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
            let Some((name, name_span)) = self.parse_label_name() else {
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

        let Some((name, name_span)) = self.parse_label_name() else {
            self.report_expected_record_entry(self.current_span());
            self.recover_record_entry();
            return None;
        };

        if self.current_is_operator("=") || self.current_is_operator(":=") {
            let overwrite = self.current_is_operator(":=");
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

        Some(RecordEntry::Shorthand {
            name,
            name_span,
            span: name_span,
        })
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

    fn recover_record_entry(&mut self) {
        while !self.at_end()
            && !self.current_is(TokenKind::CloseBrace)
            && !self.current_is(TokenKind::Newline)
            && !self.current_is(TokenKind::Indent)
            && !self.current_is(TokenKind::Dedent)
            && !self.current_is_operator(",")
            && !self.current_is_operator(";")
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
                        return self
                            .tokens
                            .get(index + 1)
                            .is_some_and(|token| token.is_operator("=>"));
                    }
                }
                TokenKind::Newline | TokenKind::Dedent if depth == 0 => return false,
                _ => {}
            }
        }

        false
    }

    fn report_invalid_binding_name(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("invalid binding name")
                .with_code("parse.invalid-binding-name")
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
                .with_code("parse.missing-binding-value")
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
                .with_code("parse.missing-lambda-body")
                .with_label(Label::primary(
                    span,
                    "expected an expression or indented block after `=>`",
                ))
                .with_note("a lambda body is an expression on the same line, or an indented block: `(params) =>\n  body`"),
        );
        missing_expr(span)
    }

    fn report_single_item_tuple(&mut self, comma_span: Span) {
        self.diagnostics.push(
            Diagnostic::error("anonymous 1-tuples are not supported")
                .with_code("parse.single-item-tuple")
                .with_label(Label::primary(
                    comma_span,
                    "this comma creates an anonymous 1-tuple",
                ))
                .with_note("remove the comma for grouping, or use a tagged tuple like `Ok(value)`"),
        );
    }

    fn report_expected_record_entry(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("expected record entry")
                .with_code("parse.expected-record-entry")
                .with_label(Label::primary(span, "expected a record field or transform"))
                .with_note("record entries are fields, shorthands, spreads, deletes, or renames"),
        );
    }

    fn report_expected_record_label(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("expected record label")
                .with_code("parse.expected-record-label")
                .with_label(Label::primary(span, "expected a field name here"))
                .with_note("use a bare field name such as `password` or `fullName`"),
        );
    }

    fn report_unexpected_separator(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("unexpected separator")
                .with_code("parse.unexpected-separator")
                .with_label(Label::primary(span, "extra separator"))
                .with_note("remove the extra `,` or `;`"),
        );
    }

    fn report_unexpected_indentation(&mut self) {
        let span = self.current_span();
        self.diagnostics.push(
            Diagnostic::error("unexpected indentation")
                .with_code("parse.unexpected-indentation")
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
                .with_code("parse.unsupported-syntax")
                .with_label(Label::primary(
                    token.span,
                    "this syntax is not supported by the core parser yet",
                ))
                .with_note("operator expressions will be parsed in Milestone 4c"),
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

    fn current_is_operator(&self, expected: &str) -> bool {
        self.current()
            .is_some_and(|token| token.is_operator(expected))
    }

    fn consume_collection_separator(&mut self, allow_semicolon: bool) -> bool {
        let mut consumed = self.skip_collection_trivia();
        let mut seen_separator = false;

        loop {
            if let Some(separator_count) = self.current_separator_count(allow_semicolon) {
                let span = self.current_span();
                let first_extra_index = usize::from(!seen_separator);

                for index in first_extra_index..separator_count {
                    self.report_unexpected_separator(Span::new(
                        span.start + index,
                        span.start + index + 1,
                    ));
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

    fn current_separator_count(&self, allow_semicolon: bool) -> Option<usize> {
        let Some(Token {
            kind: TokenKind::Operator(operator),
            ..
        }) = self.current()
        else {
            return None;
        };

        if operator.is_empty()
            || !operator
                .bytes()
                .all(|byte| byte == b',' || (allow_semicolon && byte == b';'))
        {
            return None;
        }

        Some(operator.len())
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
        self.at_end() || self.current_is(TokenKind::Newline) || self.current_is(TokenKind::Dedent)
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

fn missing_expr(span: Span) -> Expr {
    Expr {
        kind: ExprKind::Missing,
        span,
    }
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
                .with_code("parse.unclosed-delimiter")
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
                .with_code("parse.unexpected-delimiter")
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
        .with_code("parse.mismatched-delimiter")
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
    use super::{ExprKind, Item, Literal, ParseOutput, RecordEntry, parse_module};

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
    fn parses_lambda_bindings_into_ast_nodes() {
        let output = parse_module("identity = (value) => value\n");

        assert!(output.diagnostics.is_empty());
        let Some(Item::Binding(binding)) = output.module.items.first() else {
            panic!("expected binding item");
        };
        let ExprKind::Lambda { params, body } = &binding.value.kind else {
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
    fn parses_structural_literals_into_ast_nodes() {
        let output =
            parse_module("items = [1, \"two\"]\npair = (1, \"two\")\ncolors = @{ Red, Ok(1) }\n");

        assert!(output.diagnostics.is_empty());
        assert!(matches!(binding_value(&output, 0), ExprKind::Array(items) if items.len() == 2));
        assert!(matches!(binding_value(&output, 1), ExprKind::Tuple(items) if items.len() == 2));
        assert!(matches!(binding_value(&output, 2), ExprKind::Set(items) if items.len() == 2));
    }

    #[test]
    fn parses_record_transform_entries_into_ast_nodes() {
        let output = parse_module(
            "cleaned = { ..user, :..defaults, -password, name -> fullName, active = true }\n",
        );

        assert!(output.diagnostics.is_empty());
        let ExprKind::Record(entries) = binding_value(&output, 0) else {
            panic!("expected record expression");
        };

        assert_eq!(entries.len(), 5);
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
            RecordEntry::Rename { from, to, .. } if from == "name" && to == "fullName"
        ));
        assert!(matches!(
            &entries[4],
            RecordEntry::Field {
                name,
                overwrite: false,
                ..
            } if name == "active"
        ));
    }

    fn binding_value(output: &ParseOutput, index: usize) -> &ExprKind {
        let Some(Item::Binding(binding)) = output.module.items.get(index) else {
            panic!("expected binding item");
        };
        &binding.value.kind
    }
}
