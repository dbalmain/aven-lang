use aven_core::{Diagnostic, Label, Span};

use crate::{LexOutput, Token, TokenKind, lex_source};

#[derive(Debug, Clone)]
pub struct LayoutOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn layout_source(source: &str) -> LayoutOutput {
    let (_, output) = lex_then_layout(source);
    output
}

pub(crate) fn lex_then_layout(source: &str) -> (Vec<Token>, LayoutOutput) {
    let LexOutput {
        tokens,
        mut diagnostics,
    } = lex_source(source);
    let mut output = layout_tokens(&tokens);
    diagnostics.append(&mut output.diagnostics);

    (
        tokens,
        LayoutOutput {
            tokens: output.tokens,
            diagnostics,
        },
    )
}

pub fn layout_tokens(tokens: &[Token]) -> LayoutOutput {
    let mut builder = LayoutBuilder {
        output: Vec::new(),
        diagnostics: Vec::new(),
        indent_stack: vec![0],
        at_line_start: true,
        line_has_code: false,
        pending_indent: 0,
        pending_indent_span: None,
        last_offset: 0,
    };

    builder.layout(tokens);

    LayoutOutput {
        tokens: builder.output,
        diagnostics: builder.diagnostics,
    }
}

struct LayoutBuilder {
    output: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    indent_stack: Vec<usize>,
    at_line_start: bool,
    line_has_code: bool,
    pending_indent: usize,
    pending_indent_span: Option<Span>,
    last_offset: usize,
}

impl LayoutBuilder {
    fn layout(&mut self, tokens: &[Token]) {
        for token in tokens {
            self.last_offset = token.span.end;

            match &token.kind {
                TokenKind::RawIndent { spaces } if self.at_line_start => {
                    self.pending_indent = *spaces;
                    self.pending_indent_span = Some(token.span);
                }
                TokenKind::RawIndent { .. } => {}
                TokenKind::RawNewline => self.end_line(token.span),
                TokenKind::Comment(_) => {}
                TokenKind::DocComment(_) => self.push_code_token(token),
                _ => self.push_code_token(token),
            }
        }

        if self.line_has_code {
            self.push(TokenKind::Newline, Span::point(self.last_offset));
        }

        self.close_open_blocks(Span::point(self.last_offset));
    }

    fn push_code_token(&mut self, token: &Token) {
        if self.at_line_start {
            self.apply_pending_indent(token.span);
        }

        self.push(token.kind.clone(), token.span);
        self.line_has_code = true;
        self.at_line_start = false;
    }

    fn apply_pending_indent(&mut self, token_span: Span) {
        let target = self.pending_indent;
        let current = self.current_indent();
        let span = self
            .pending_indent_span
            .unwrap_or_else(|| Span::point(token_span.start));

        match target.cmp(&current) {
            std::cmp::Ordering::Greater => {
                self.indent_stack.push(target);
                self.push(TokenKind::Indent, span);
            }
            std::cmp::Ordering::Less => self.dedent_to(target, span),
            std::cmp::Ordering::Equal => {}
        }

        self.at_line_start = false;
        self.pending_indent = 0;
        self.pending_indent_span = None;
    }

    fn dedent_to(&mut self, target: usize, span: Span) {
        let expected = self.indent_stack.clone();

        while self.indent_stack.len() > 1 && self.current_indent() > target {
            self.indent_stack.pop();
            self.push(TokenKind::Dedent, Span::point(span.start));
        }

        if self.current_indent() == target {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error("inconsistent indentation")
                .with_code("layout.inconsistent-indentation")
                .with_label(Label::primary(
                    span,
                    format!("this line is indented to {}", spaces_text(target)),
                ))
                .with_note(format!(
                    "indentation must match an open block: {}",
                    expected_indents(&expected),
                )),
        );

        if target > self.current_indent() {
            self.indent_stack.push(target);
            self.push(TokenKind::Indent, span);
        }
    }

    fn end_line(&mut self, span: Span) {
        if self.line_has_code {
            self.push(TokenKind::Newline, span);
        }

        self.at_line_start = true;
        self.line_has_code = false;
        self.pending_indent = 0;
        self.pending_indent_span = None;
    }

    fn close_open_blocks(&mut self, span: Span) {
        while self.indent_stack.len() > 1 {
            self.indent_stack.pop();
            self.push(TokenKind::Dedent, span);
        }
    }

    fn current_indent(&self) -> usize {
        self.indent_stack.last().copied().unwrap_or(0)
    }

    fn push(&mut self, kind: TokenKind, span: Span) {
        self.output.push(Token { kind, span });
    }
}

fn spaces_text(spaces: usize) -> String {
    match spaces {
        1 => "1 space".to_owned(),
        spaces => format!("{spaces} spaces"),
    }
}

fn expected_indents(indents: &[usize]) -> String {
    indents
        .iter()
        .map(|indent| spaces_text(*indent))
        .collect::<Vec<_>>()
        .join(", ")
}
