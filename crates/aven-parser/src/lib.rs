use aven_core::{Diagnostic, Label, Span};

mod layout;
mod lexer;

pub use layout::{LayoutOutput, layout_source, layout_tokens};
pub use lexer::{LexOutput, Token, TokenKind, lex_source};

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
pub struct Expr {
    // TODO: replace this source slice with a real expression AST in the parser milestone.
    pub text: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ParseOutput {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse_module(source: &str) -> ParseOutput {
    let mut parser = Parser {
        source,
        offset: 0,
        items: Vec::new(),
        diagnostics: Vec::new(),
        delimiter_stack: Vec::new(),
    };

    parser.parse();

    ParseOutput {
        module: Module {
            items: parser.items,
        },
        diagnostics: parser.diagnostics,
    }
}

struct Parser<'a> {
    source: &'a str,
    offset: usize,
    items: Vec<Item>,
    diagnostics: Vec<Diagnostic>,
    delimiter_stack: Vec<(char, Span)>,
}

impl Parser<'_> {
    fn parse(&mut self) {
        for line in self.source.split_inclusive('\n') {
            let line_start = self.offset;
            self.offset += line.len();
            self.parse_line(line_start, line.trim_end_matches('\n'));
        }

        for (delimiter, span) in self.delimiter_stack.drain(..) {
            self.diagnostics.push(
                Diagnostic::error(format!("unclosed `{delimiter}`"))
                    .with_code("parse.unclosed-delimiter")
                    .with_label(Label::primary(span, "opened here")),
            );
        }
    }

    fn parse_line(&mut self, line_start: usize, line: &str) {
        let content_end = strip_comment(line);
        let code = &line[..content_end];
        self.scan_delimiters(line_start, code);

        let trimmed = code.trim();
        if trimmed.is_empty() {
            return;
        }

        let leading = code.len() - code.trim_start().len();
        if leading > 0 {
            self.diagnostics.push(
                Diagnostic::error("unexpected indentation")
                    .with_code("parse.unexpected-indentation")
                    .with_label(Label::primary(
                        Span::new(line_start, line_start + leading),
                        "indented blocks are not supported by the starter parser yet",
                    ))
                    .with_note(
                        "indented blocks will be supported in milestone 3 (layout and blocks)",
                    ),
            );
            return;
        }

        match find_top_level_equals(code) {
            Some(eq_index) => self.parse_binding(line_start, code, eq_index),
            None => {
                let span = Span::new(line_start, line_start + code.trim_end().len());
                self.items.push(Item::Expr(Expr {
                    text: trimmed.to_owned(),
                    span,
                }));
            }
        }
    }

    fn parse_binding(&mut self, line_start: usize, code: &str, eq_index: usize) {
        let name_part = code[..eq_index].trim();
        let value_part = code[eq_index + 1..].trim();

        if name_part.is_empty() {
            self.diagnostics.push(
                Diagnostic::error("binding is missing a name")
                    .with_code("parse.missing-binding-name")
                    .with_label(Label::primary(
                        Span::point(line_start + eq_index),
                        "expected a name",
                    )),
            );
            return;
        }

        let name_offset = code[..eq_index].find(name_part).unwrap_or(0);
        let name_span = Span::new(
            line_start + name_offset,
            line_start + name_offset + name_part.len(),
        );

        if !is_identifier(name_part) {
            self.diagnostics.push(
                Diagnostic::error("invalid binding name")
                    .with_code("parse.invalid-binding-name")
                    .with_label(Label::primary(
                        name_span,
                        "binding names must start with a letter or `_`",
                    )),
            );
            return;
        }

        if value_part.is_empty() {
            self.diagnostics.push(
                Diagnostic::error("binding is missing a value")
                    .with_code("parse.missing-binding-value")
                    .with_label(Label::primary(
                        Span::point(line_start + eq_index + 1),
                        "expected an expression after `=`",
                    )),
            );
            return;
        }

        let value_offset = eq_index + 1 + code[eq_index + 1..].find(value_part).unwrap_or(0);
        let value_span = Span::new(
            line_start + value_offset,
            line_start + value_offset + value_part.len(),
        );
        let span = name_span.merge(value_span);

        self.items.push(Item::Binding(Binding {
            name: name_part.to_owned(),
            name_span,
            value: Expr {
                text: value_part.to_owned(),
                span: value_span,
            },
            span,
        }));
    }

    // TODO(milestone-4a): this starter parser duplicates lexer scanning for
    // delimiters and strings. Remove it once parsing consumes the lexer/layout
    // token stream.
    fn scan_delimiters(&mut self, line_start: usize, code: &str) {
        let mut chars = code.char_indices().peekable();

        while let Some((index, ch)) = chars.next() {
            match ch {
                '"' => self.scan_string(line_start + index, &mut chars),
                '(' | '[' | '{' => {
                    self.delimiter_stack
                        .push((ch, Span::new(line_start + index, line_start + index + 1)));
                }
                ')' | ']' | '}' => self.close_delimiter(line_start + index, ch),
                _ => {}
            }
        }
    }

    fn scan_string(
        &mut self,
        start: usize,
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    ) {
        let mut escaped = false;
        for (_, ch) in chars.by_ref() {
            if escaped {
                escaped = false;
                continue;
            }

            match ch {
                '\\' => escaped = true,
                '"' => return,
                _ => {}
            }
        }

        self.diagnostics.push(
            Diagnostic::error("unterminated string literal")
                .with_code("parse.unterminated-string")
                .with_label(Label::primary(
                    Span::new(start, start + 1),
                    "string starts here",
                )),
        );
    }

    fn close_delimiter(&mut self, offset: usize, close: char) {
        let expected_open = match close {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            _ => return,
        };

        match self.delimiter_stack.pop() {
            Some((open, _)) if open == expected_open => {}
            Some((open, span)) => {
                self.diagnostics.push(
                    Diagnostic::error(format!("mismatched delimiter `{close}`"))
                        .with_code("parse.mismatched-delimiter")
                        .with_label(Label::primary(span, format!("opened with `{open}`")))
                        .with_label(Label::primary(
                            Span::new(offset, offset + 1),
                            format!("closed with `{close}`"),
                        )),
                );
            }
            None => {
                self.diagnostics.push(
                    Diagnostic::error(format!("unexpected `{close}`"))
                        .with_code("parse.unexpected-delimiter")
                        .with_label(Label::primary(
                            Span::new(offset, offset + 1),
                            "unexpected here",
                        )),
                );
            }
        }
    }
}

// TODO(milestone-4a): comment stripping and top-level `=` detection are
// transitional starter-parser helpers. Remove them once parsing consumes the
// lexer/layout token stream.
fn strip_comment(line: &str) -> usize {
    let mut escaped = false;
    let mut in_string = false;

    for (index, ch) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '#' => return index,
            _ => {}
        }
    }

    line.len()
}

fn find_top_level_equals(code: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in code.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '=' if depth == 0 && is_binding_equals(code, index) => return Some(index),
            _ => {}
        }
    }

    None
}

fn is_binding_equals(code: &str, index: usize) -> bool {
    let previous = code[..index].chars().next_back();
    let next = code[index + 1..].chars().next();

    !matches!(previous, Some('=' | ':' | '!' | '<' | '>')) && !matches!(next, Some('=' | '>'))
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }

    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}
