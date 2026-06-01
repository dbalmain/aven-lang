use aven_core::{Diagnostic, Label, Span};

#[derive(Debug, Clone)]
pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub(crate) fn is_operator(&self, expected: &str) -> bool {
        matches!(&self.kind, TokenKind::Operator(operator) if operator == expected)
    }

    pub(crate) fn is_close_delimiter(&self) -> bool {
        matches!(
            self.kind,
            TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Identifier(String),
    ComptimeIdentifier(String),
    Number(String),
    StringLiteral(String),
    RegexLiteral(String),
    PathLiteral(String),
    LabelPath(String),
    Operator(String),
    OpenParen,
    CloseParen,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    Comma,
    Semicolon,
    RawNewline,
    RawIndent { spaces: usize },
    Newline,
    Indent,
    Dedent,
    Comment(String),
    DocComment(String),
}

impl TokenKind {
    pub fn describe(&self) -> String {
        match self {
            Self::Identifier(name) => format!("identifier `{name}`"),
            Self::ComptimeIdentifier(name) => format!("comptime_identifier `{name}`"),
            Self::Number(number) => format!("number `{number}`"),
            Self::StringLiteral(text) => format!("string `{text}`"),
            Self::RegexLiteral(regex) => format!("regex `{regex}`"),
            Self::PathLiteral(path) => format!("path `{path}`"),
            Self::LabelPath(path) => format!("label `{path}`"),
            Self::Operator(operator) => format!("operator `{operator}`"),
            Self::OpenParen => "delimiter `(`".to_owned(),
            Self::CloseParen => "delimiter `)`".to_owned(),
            Self::OpenBrace => "delimiter `{`".to_owned(),
            Self::CloseBrace => "delimiter `}`".to_owned(),
            Self::OpenBracket => "delimiter `[`".to_owned(),
            Self::CloseBracket => "delimiter `]`".to_owned(),
            Self::Comma => "comma".to_owned(),
            Self::Semicolon => "semicolon".to_owned(),
            Self::RawNewline => "newline".to_owned(),
            Self::RawIndent { spaces } => format!("indent `{spaces}`"),
            Self::Newline => "layout newline".to_owned(),
            Self::Indent => "layout indent".to_owned(),
            Self::Dedent => "layout dedent".to_owned(),
            Self::Comment(text) => format!("comment `{text}`"),
            Self::DocComment(text) => format!("doc_comment `{text}`"),
        }
    }
}

pub fn lex_source(source: &str) -> LexOutput {
    let mut lexer = Lexer {
        source,
        offset: 0,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
        at_line_start: true,
    };

    lexer.lex();

    LexOutput {
        tokens: lexer.tokens,
        diagnostics: lexer.diagnostics,
    }
}

pub fn is_identifier(name: &str) -> bool {
    let output = lex_source(name);

    if !output.diagnostics.is_empty() {
        return false;
    }

    matches!(
        output.tokens.as_slice(),
        [Token {
            kind: TokenKind::Identifier(_) | TokenKind::ComptimeIdentifier(_),
            span,
        }] if span.start == 0 && span.end == name.len()
    )
}

struct Lexer<'a> {
    source: &'a str,
    offset: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    at_line_start: bool,
}

impl Lexer<'_> {
    fn lex(&mut self) {
        self.scan_leading_bom();

        while self.offset < self.source.len() {
            if self.at_line_start {
                self.scan_indent();
                if self.offset >= self.source.len() {
                    break;
                }
            }

            match self.current_byte() {
                Some(b' ' | b'\t') => self.offset += 1,
                Some(b'\n' | b'\r') => self.scan_newline(),
                Some(b'#') => self.scan_comment(),
                Some(b'a'..=b'z' | b'A'..=b'Z' | b'_') => self.scan_identifier(),
                Some(b'0'..=b'9') => self.scan_number(),
                Some(b'"') => self.scan_string(),
                Some(b'@') => self.scan_label_or_operator(),
                Some(b'.') if self.starts_with("./") || self.starts_with("../") => self.scan_path(),
                Some(b'~') if self.starts_with("~/") => self.scan_path(),
                Some(b'$') if self.starts_with("$/") => self.scan_path(),
                Some(b'/') if self.starts_with("//") => self.scan_path(),
                Some(b'/') if self.regex_allowed_here() => self.scan_regex_or_operator(),
                Some(b'(') => self.push_single(TokenKind::OpenParen),
                Some(b')') => self.push_single(TokenKind::CloseParen),
                Some(b'{') => self.push_single(TokenKind::OpenBrace),
                Some(b'}') => self.push_single(TokenKind::CloseBrace),
                Some(b'[') => self.push_single(TokenKind::OpenBracket),
                Some(b']') => self.push_single(TokenKind::CloseBracket),
                Some(b',') => self.push_single(TokenKind::Comma),
                Some(b';') => self.push_single(TokenKind::Semicolon),
                Some(byte) if is_operator_start_byte(byte) => self.scan_operator(),
                Some(_) => self.scan_unexpected_character(),
                None => break,
            }
        }
    }

    fn scan_leading_bom(&mut self) {
        if !self.source.starts_with('\u{feff}') {
            return;
        }

        let end = '\u{feff}'.len_utf8();
        self.diagnostics.push(
            Diagnostic::error("leading byte order mark is not supported")
                .with_code("lex.leading-bom")
                .with_label(Label::primary(
                    Span::new(0, end),
                    "remove this byte order mark",
                )),
        );
        self.offset = end;
    }

    fn scan_indent(&mut self) {
        let start = self.offset;
        let mut spaces = 0;

        while let Some(byte) = self.current_byte() {
            match byte {
                b' ' => {
                    spaces += 1;
                    self.offset += 1;
                }
                b'\t' => {
                    let tab_start = self.offset;
                    self.offset += 1;
                    self.diagnostics.push(
                        Diagnostic::error("tabs are not allowed in indentation")
                            .with_code("lex.tab-indentation")
                            .with_label(Label::primary(
                                Span::new(tab_start, self.offset),
                                "use spaces for indentation",
                            )),
                    );
                }
                _ => break,
            }
        }

        if spaces > 0 && !self.at_newline_or_eof() {
            self.push(
                TokenKind::RawIndent { spaces },
                Span::new(start, self.offset),
            );
        }

        self.at_line_start = false;
    }

    fn scan_newline(&mut self) {
        let start = self.offset;

        if self.starts_with("\r\n") {
            self.offset += 2;
        } else {
            self.offset += 1;
        }

        self.push(TokenKind::RawNewline, Span::new(start, self.offset));
        self.at_line_start = true;
    }

    fn scan_comment(&mut self) {
        let start = self.offset;
        let is_doc = self.starts_with("##");

        self.offset += if is_doc { 2 } else { 1 };
        let text_start = self.offset;

        while !self.at_newline_or_eof() {
            self.offset += self.current_char_len();
        }

        let text = self.source[text_start..self.offset].to_owned();
        let span = Span::new(start, self.offset);

        if is_doc {
            self.push(TokenKind::DocComment(text), span);
        } else {
            self.push(TokenKind::Comment(text), span);
        }
    }

    fn scan_identifier(&mut self) {
        let start = self.offset;
        self.offset += 1;

        while matches!(
            self.current_byte(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.offset += 1;
        }

        let text = self.source[start..self.offset].to_owned();
        let kind = if is_comptime_identifier_name(&text) {
            TokenKind::ComptimeIdentifier(text)
        } else {
            TokenKind::Identifier(text)
        };
        self.push(kind, Span::new(start, self.offset));
    }

    fn scan_number(&mut self) {
        // TODO(milestone-4a): validate numeric literal shape after lexing or
        // in the parser so forms like `100_`, `1__000`, and `1_.0` get
        // grammar-specific diagnostics instead of becoming ordinary numbers.
        let start = self.offset;
        self.scan_digits_and_underscores();

        if self.current_byte() == Some(b'.')
            && self.peek_byte(1).is_some_and(|b| b.is_ascii_digit())
        {
            self.offset += 1;
            self.scan_digits_and_underscores();
        }

        if matches!(self.current_byte(), Some(b'e' | b'E')) {
            let exponent_start = self.offset;
            self.offset += 1;

            if matches!(self.current_byte(), Some(b'+' | b'-')) {
                self.offset += 1;
            }

            if self.current_byte().is_some_and(|b| b.is_ascii_digit()) {
                self.scan_digits_and_underscores();
            } else {
                self.offset = exponent_start;
            }
        }

        self.push(
            TokenKind::Number(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_digits_and_underscores(&mut self) {
        while matches!(self.current_byte(), Some(b'0'..=b'9' | b'_')) {
            self.offset += 1;
        }
    }

    fn scan_string(&mut self) {
        let start = self.offset;
        self.offset += 1;
        let mut escaped = false;

        while self.offset < self.source.len() {
            let Some(ch) = self.current_char() else {
                break;
            };

            if escaped {
                escaped = false;
                self.offset += ch.len_utf8();
                continue;
            }

            match ch {
                '\\' => {
                    escaped = true;
                    self.offset += 1;
                }
                '"' => {
                    self.offset += 1;
                    self.push(
                        TokenKind::StringLiteral(self.source[start..self.offset].to_owned()),
                        Span::new(start, self.offset),
                    );
                    return;
                }
                '\n' | '\r' => {
                    self.push_unterminated_string(start);
                    return;
                }
                _ => self.offset += ch.len_utf8(),
            }
        }

        self.push_unterminated_string(start);
    }

    fn push_unterminated_string(&mut self, start: usize) {
        self.diagnostics.push(
            Diagnostic::error("unterminated string literal")
                .with_code("lex.unterminated-string")
                .with_label(Label::primary(
                    Span::new(start, self.offset),
                    "string starts here",
                ))
                .with_note(
                    "close the string with a `\"`, or use a raw string for multi-line content.",
                ),
        );
        self.push(
            TokenKind::StringLiteral(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_label_or_operator(&mut self) {
        let start = self.offset;

        if self.peek_byte(1) == Some(b'{') {
            self.push_single(TokenKind::Operator("@".to_owned()));
            return;
        }

        if self
            .peek_byte(1)
            .is_some_and(|byte| reserved_operator_continues("@", byte))
        {
            self.scan_reserved_operator_run("@");
            return;
        }

        if !self.peek_byte(1).is_some_and(is_identifier_start_byte) {
            self.push_single(TokenKind::Operator("@".to_owned()));
            return;
        }

        self.offset += 1;
        self.scan_label_segment();

        while self.current_byte() == Some(b'/')
            && self.peek_byte(1).is_some_and(is_identifier_start_byte)
        {
            self.offset += 1;
            self.scan_label_segment();
        }

        self.push(
            TokenKind::LabelPath(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_label_segment(&mut self) {
        while self.current_byte().is_some_and(is_identifier_continue_byte) {
            self.offset += 1;
        }
    }

    fn scan_path(&mut self) {
        let start = self.offset;

        while let Some(byte) = self.current_byte() {
            if is_path_end_byte(byte) {
                break;
            }
            self.offset += self.current_char_len();
        }

        self.push(
            TokenKind::PathLiteral(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_regex_or_operator(&mut self) {
        let Some(end) = self.find_regex_end() else {
            self.scan_unterminated_regex_or_operator();
            return;
        };

        let start = self.offset;
        self.offset = end;

        while self
            .current_byte()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        {
            self.offset += 1;
        }

        self.push(
            TokenKind::RegexLiteral(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_unterminated_regex_or_operator(&mut self) {
        let start = self.offset;

        if self
            .peek_byte(1)
            .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b')' | b']' | b'}'))
        {
            self.scan_operator();
            return;
        }

        while !self.at_newline_or_eof() {
            self.offset += self.current_char_len();
        }

        self.diagnostics.push(
            Diagnostic::error("unterminated regex literal")
                .with_code("lex.unterminated-regex")
                .with_label(Label::primary(
                    Span::new(start, self.offset),
                    "regex starts here",
                ))
                .with_note("close the regex with `/`, or use `Regex.compile(pattern)` for a dynamic pattern."),
        );
        self.push(
            TokenKind::RegexLiteral(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn find_regex_end(&self) -> Option<usize> {
        let mut offset = self.offset + 1;
        let mut escaped = false;
        let mut in_class = false;

        while offset < self.source.len() {
            let ch = self.source[offset..].chars().next()?;

            if escaped {
                escaped = false;
                offset += ch.len_utf8();
                continue;
            }

            match ch {
                '\\' => {
                    escaped = true;
                    offset += 1;
                }
                '[' => {
                    in_class = true;
                    offset += 1;
                }
                ']' => {
                    in_class = false;
                    offset += 1;
                }
                '/' if !in_class => return Some(offset + 1),
                '\n' | '\r' => return None,
                _ => offset += ch.len_utf8(),
            }
        }

        None
    }

    fn regex_allowed_here(&self) -> bool {
        self.tokens
            .iter()
            .rev()
            .find(|token| !token.kind.is_trivia())
            .is_none_or(|token| {
                matches!(
                    token.kind,
                    TokenKind::Operator(_)
                        | TokenKind::OpenParen
                        | TokenKind::OpenBrace
                        | TokenKind::OpenBracket
                )
            })
    }

    fn scan_operator(&mut self) {
        match self.current_byte() {
            Some(b'?') => self.scan_reserved_operator("?", &["?.", "??", "?^", "?!", "?>", "?"]),
            Some(b'=') => self.scan_reserved_operator("=", &["=>", "==", "="]),
            Some(b':') => self.scan_reserved_operator(":", &[":..", ":=", ":"]),
            Some(b'.') => self.scan_reserved_operator(".", &["..", "."]),
            _ => self.scan_custom_operator(),
        }
    }

    fn scan_reserved_operator(&mut self, prefix: &str, allowed: &[&str]) {
        let start = self.offset;

        let rest = &self.source[start..];
        let operator = allowed
            .iter()
            .find(|operator| rest.starts_with(**operator))
            .copied()
            .unwrap_or(prefix);
        self.offset += operator.len();

        if self
            .current_byte()
            .is_some_and(|byte| reserved_operator_continues(prefix, byte))
        {
            while self
                .current_byte()
                .is_some_and(|byte| reserved_operator_continues(prefix, byte))
            {
                self.offset += self.current_char_len();
            }
            self.push_reserved_operator_diagnostic(start, prefix);
        }

        self.push(
            TokenKind::Operator(operator.to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_reserved_operator_run(&mut self, prefix: &str) {
        let start = self.offset;
        self.scan_operator_run();
        self.push_reserved_operator_diagnostic(start, prefix);
        self.push(
            TokenKind::Operator(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn push_reserved_operator_diagnostic(&mut self, start: usize, prefix: &str) {
        self.diagnostics.push(
            Diagnostic::error(format!("reserved `{prefix}` operator"))
                .with_code("lex.reserved-operator")
                .with_label(Label::primary(
                    Span::new(start, self.offset),
                    "this operator namespace is reserved by the language",
                ))
                .with_note("custom operators cannot start with `=`, `:`, `.`, `?`, or `@`"),
        );
    }

    fn scan_custom_operator(&mut self) {
        let start = self.offset;
        self.offset += self.current_char_len();

        while self
            .current_byte()
            .is_some_and(is_custom_operator_continue_byte)
        {
            self.offset += self.current_char_len();
        }

        self.push(
            TokenKind::Operator(self.source[start..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_operator_run(&mut self) {
        self.offset += self.current_char_len();

        while self.current_byte().is_some_and(is_operator_run_byte) {
            self.offset += self.current_char_len();
        }
    }

    fn scan_unexpected_character(&mut self) {
        let start = self.offset;
        let ch = self.current_char().unwrap_or('\0');
        let len = self.current_char_len();
        self.offset += len;

        self.diagnostics.push(
            Diagnostic::error(format!("unexpected character `{ch}`"))
                .with_code("lex.unexpected-character")
                .with_label(Label::primary(
                    Span::new(start, self.offset),
                    "this character is not valid syntax",
                ))
                .with_note("see the lexical grammar section of the language spec for accepted source characters"),
        );
    }

    fn push_single(&mut self, kind: TokenKind) {
        let start = self.offset;
        self.offset += 1;
        self.push(kind, Span::new(start, self.offset));
    }

    fn push(&mut self, kind: TokenKind, span: Span) {
        self.tokens.push(Token { kind, span });
    }

    fn current_byte(&self) -> Option<u8> {
        self.source.as_bytes().get(self.offset).copied()
    }

    fn peek_byte(&self, distance: usize) -> Option<u8> {
        self.source.as_bytes().get(self.offset + distance).copied()
    }

    fn current_char(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }

    fn current_char_len(&self) -> usize {
        self.current_char().map_or(0, char::len_utf8)
    }

    fn starts_with(&self, text: &str) -> bool {
        self.source[self.offset..].starts_with(text)
    }

    fn at_newline_or_eof(&self) -> bool {
        matches!(self.current_byte(), None | Some(b'\n' | b'\r'))
    }
}

impl TokenKind {
    fn is_trivia(&self) -> bool {
        matches!(
            self,
            Self::RawNewline
                | Self::RawIndent { .. }
                | Self::Newline
                | Self::Indent
                | Self::Dedent
                | Self::Comment(_)
                | Self::DocComment(_)
        )
    }
}

fn is_identifier_start_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_identifier_continue_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

pub fn is_comptime_identifier_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_uppercase())
}

fn is_operator_start_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'=' | b':'
            | b'.'
            | b'?'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'^'
            | b'|'
            | b'&'
            | b'<'
            | b'>'
            | b'!'
            | b'~'
            | b'$'
    )
}

fn is_custom_operator_continue_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'+' | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'^'
            | b'|'
            | b'&'
            | b'<'
            | b'>'
            | b'!'
            | b'~'
            | b'$'
            | b'='
    )
}

fn is_operator_run_byte(byte: u8) -> bool {
    is_custom_operator_continue_byte(byte) || matches!(byte, b':' | b'?' | b'.' | b'@')
}

fn reserved_operator_continues(prefix: &str, byte: u8) -> bool {
    match prefix {
        "?" | "@" => is_operator_run_byte(byte),
        "=" => byte == b'=',
        ":" => matches!(byte, b':' | b'.' | b'?' | b'='),
        "." => is_custom_operator_continue_byte(byte) || matches!(byte, b'.' | b'?'),
        _ => false,
    }
}

fn is_path_end_byte(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'#' | b',' | b';'
        )
}

#[cfg(test)]
mod tests {
    use super::{TokenKind, is_identifier, lex_source};

    #[test]
    fn normalizes_newline_shapes_to_newline_tokens() {
        let output = lex_source("a\r\nb\rc\n");
        let spans: Vec<_> = output
            .tokens
            .iter()
            .filter(|token| token.kind == TokenKind::RawNewline)
            .map(|token| token.span)
            .collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].start, 1);
        assert_eq!(spans[0].end, 3);
        assert_eq!(spans[1].start, 4);
        assert_eq!(spans[1].end, 5);
        assert_eq!(spans[2].start, 6);
        assert_eq!(spans[2].end, 7);
    }

    #[test]
    fn reports_bom_and_tabs_in_indentation() {
        let output = lex_source("\u{feff}\tname = 1");
        let codes: Vec<_> = output
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect();

        assert_eq!(codes, vec!["lex.leading-bom", "lex.tab-indentation"]);
    }

    #[test]
    fn validates_identifiers_with_the_lexer() {
        assert!(is_identifier("name"));
        assert!(is_identifier("Name"));
        assert!(is_identifier("_scratch"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("1name"));
        assert!(!is_identifier("name+"));
        assert!(!is_identifier("two words"));
    }
}
