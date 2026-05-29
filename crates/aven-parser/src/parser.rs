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
    Call { callee: Box<Expr>, args: Vec<Expr> },
    Lambda { params: Vec<Param>, body: Box<Expr> },
    Block(Vec<Item>),
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
            TokenKind::OpenParen => self.parse_group(),
            TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                self.advance();
                missing_expr(token.span)
            }
            _ => {
                self.diagnostics.push(
                    Diagnostic::error("expected expression")
                        .with_code("parse.expected-expression")
                        .with_label(Label::primary(token.span, "expected an expression here"))
                        .with_note("expressions are literals, identifiers, function calls, or parenthesized groups"),
                );
                self.advance();
                missing_expr(token.span)
            }
        }
    }

    fn parse_group(&mut self) -> Expr {
        let start = self.current_span().start;
        self.advance();

        let expr = if self.current_is(TokenKind::CloseParen) {
            missing_expr(Span::point(self.current_span().start))
        } else {
            self.parse_expression()
        };

        self.skip_until_close_or_line(TokenKind::CloseParen);
        let end = self.previous_end();

        Expr {
            kind: ExprKind::Group(Box::new(expr)),
            span: Span::new(start, end),
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

    fn skip_until_close_or_line(&mut self, close: TokenKind) {
        while !self.at_end() && !self.at_item_boundary() {
            if self.current_is(close.clone()) {
                self.advance();
                return;
            }

            if self.current().is_some_and(Token::is_close_delimiter) {
                self.advance();
                return;
            }

            self.advance();
        }
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

    fn current_is_any_close_delimiter(&self) -> bool {
        self.current().is_some_and(Token::is_close_delimiter)
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
    use super::{ExprKind, Item, Literal, parse_module};

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
}
