use aven_core::{Diagnostic, Label, Span, codes};

use crate::operators::is_custom_operator_byte;

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
    Keyword(Keyword),
    Identifier(String),
    ComptimeIdentifier(String),
    ComptimeParamMarker(String),
    Number(String),
    StringLiteral(String),
    InterpolationStart(String),
    InterpolationMiddle(String),
    InterpolationEnd(String),
    RegexLiteral(String),
    Tag(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    True,
    False,
    Null,
    Undefined,
}

impl Keyword {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::True => "true",
            Self::False => "false",
            Self::Null => "null",
            Self::Undefined => "undefined",
        }
    }
}

impl TokenKind {
    pub fn describe(&self) -> String {
        match self {
            Self::Keyword(keyword) => format!("keyword `{}`", keyword.as_str()),
            Self::Identifier(name) => format!("identifier `{name}`"),
            Self::ComptimeIdentifier(name) => format!("comptime_identifier `{name}`"),
            Self::ComptimeParamMarker(name) => format!("comptime_param `@{name}`"),
            Self::Number(number) => format!("number `{number}`"),
            Self::StringLiteral(text) => format!("string `{text}`"),
            Self::InterpolationStart(text) => format!("interpolation_start `{text}`"),
            Self::InterpolationMiddle(text) => format!("interpolation_middle `{text}`"),
            Self::InterpolationEnd(text) => format!("interpolation_end `{text}`"),
            Self::RegexLiteral(regex) => format!("regex `{regex}`"),
            Self::Tag(name) => format!("tag `@{name}`"),
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
        interp_contexts: Vec::new(),
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
    interp_contexts: Vec<InterpolationContext>,
}

#[derive(Debug, Clone, Copy)]
struct InterpolationContext {
    brace_depth: usize,
    start: usize,
}

impl Lexer<'_> {
    fn lex(&mut self) {
        self.scan_leading_bom();

        while self.offset < self.source.len() {
            if self.at_line_start && self.interp_contexts.is_empty() {
                self.scan_indent();
                if self.offset >= self.source.len() {
                    break;
                }
            }

            if self.interpolation_newline_or_eof() {
                continue;
            }

            match self.current_byte() {
                Some(b' ' | b'\t') => self.offset += 1,
                Some(b'\n' | b'\r') => self.scan_newline(),
                Some(b'#') => self.scan_comment(),
                Some(b'a'..=b'z' | b'A'..=b'Z' | b'_') => self.scan_identifier(),
                Some(b'0'..=b'9') => self.scan_number(),
                Some(b'"') => self.scan_string(),
                Some(b'@') => self.scan_label_or_operator(),
                Some(b'/') if self.regex_allowed_here() => self.scan_regex_or_operator(),
                Some(b'(') => self.push_single(TokenKind::OpenParen),
                Some(b')') => self.push_single(TokenKind::CloseParen),
                Some(b'{') => self.scan_open_brace(),
                Some(b'}') => self.scan_close_brace(),
                Some(b'[') => self.push_single(TokenKind::OpenBracket),
                Some(b']') => self.push_single(TokenKind::CloseBracket),
                Some(b',') => self.push_single(TokenKind::Comma),
                Some(b';') => self.push_single(TokenKind::Semicolon),
                Some(byte) if is_operator_start_byte(byte) => self.scan_operator(),
                Some(_) => self.scan_unexpected_character(),
                None => break,
            }
        }

        if let Some(context) = self.interp_contexts.last().copied() {
            self.push_unterminated_interpolation(context.start, true);
            self.interp_contexts.clear();
        }
    }

    fn scan_leading_bom(&mut self) {
        if !self.source.starts_with('\u{feff}') {
            return;
        }

        let end = '\u{feff}'.len_utf8();
        self.diagnostics.push(
            Diagnostic::error("leading byte order mark is not supported")
                .with_code(codes::lex::LEADING_BOM)
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
                            .with_code(codes::lex::TAB_INDENTATION)
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

        let text = &self.source[start..self.offset];
        let kind = if let Some(keyword) = keyword_from_text(text) {
            TokenKind::Keyword(keyword)
        } else if is_comptime_identifier_name(text) {
            TokenKind::ComptimeIdentifier(text.to_owned())
        } else {
            TokenKind::Identifier(text.to_owned())
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

        while self.offset < self.source.len() {
            let Some(ch) = self.current_char() else {
                break;
            };

            match ch {
                '\\' => self.scan_string_escape(),
                '$' if self.peek_byte(1) == Some(b'{') => {
                    let interpolation_start = self.offset;
                    self.push(
                        TokenKind::InterpolationStart(self.source[start..self.offset].to_owned()),
                        Span::new(start, self.offset),
                    );
                    self.offset += 2;
                    self.push_interpolation_context(interpolation_start);
                    return;
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

    fn scan_string_continuation(&mut self) {
        let start = self.offset;

        while self.offset < self.source.len() {
            let Some(ch) = self.current_char() else {
                break;
            };

            match ch {
                '\\' => self.scan_string_escape(),
                '$' if self.peek_byte(1) == Some(b'{') => {
                    let interpolation_start = self.offset;
                    self.push(
                        TokenKind::InterpolationMiddle(self.source[start..self.offset].to_owned()),
                        Span::new(start, self.offset),
                    );
                    self.offset += 2;
                    self.push_interpolation_context(interpolation_start);
                    return;
                }
                '"' => {
                    self.offset += 1;
                    self.push(
                        TokenKind::InterpolationEnd(self.source[start..self.offset].to_owned()),
                        Span::new(start, self.offset),
                    );
                    return;
                }
                '\n' | '\r' => {
                    self.push_unterminated_interpolation_fragment(start);
                    return;
                }
                _ => self.offset += ch.len_utf8(),
            }
        }

        self.push_unterminated_interpolation_fragment(start);
    }

    fn scan_string_escape(&mut self) {
        let escape_start = self.offset;
        self.offset += 1;

        let Some(ch) = self.current_char() else {
            return;
        };

        match ch {
            'n' | 'r' | 't' | '"' | '\\' => {
                self.offset += ch.len_utf8();
            }
            'u' => self.scan_unicode_escape(escape_start),
            _ => {
                self.offset += ch.len_utf8();
                self.push_unknown_escape_diagnostic(
                    Span::new(escape_start, self.offset),
                    "this escape is not supported",
                );
            }
        }
    }

    fn scan_unicode_escape(&mut self, escape_start: usize) {
        self.offset += 1;

        if self.current_byte() != Some(b'{') {
            self.push_unknown_escape_diagnostic(
                Span::new(escape_start, self.offset),
                "unicode escapes must use `\\u{H}`",
            );
            return;
        }

        self.offset += 1;
        let hex_start = self.offset;
        let mut hex = String::new();
        let mut valid_hex = true;

        while let Some(ch) = self.current_char() {
            if matches!(ch, '"' | '\n' | '\r') || (ch == '$' && self.peek_byte(1) == Some(b'{')) {
                self.push_unknown_escape_diagnostic(
                    Span::new(escape_start, self.offset),
                    "unterminated unicode escape",
                );
                return;
            }

            if ch == '}' {
                self.offset += 1;
                if hex_start == self.offset.saturating_sub(1)
                    || !valid_hex
                    || u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .is_none()
                {
                    self.push_unknown_escape_diagnostic(
                        Span::new(escape_start, self.offset),
                        "malformed unicode escape",
                    );
                }
                return;
            }

            if ch.is_ascii_hexdigit() {
                hex.push(ch);
            } else {
                valid_hex = false;
            }
            self.offset += ch.len_utf8();
        }

        self.push_unknown_escape_diagnostic(
            Span::new(escape_start, self.offset),
            "unterminated unicode escape",
        );
    }

    fn push_unknown_escape_diagnostic(&mut self, span: Span, label: &'static str) {
        self.diagnostics.push(
            Diagnostic::error("unknown string escape")
                .with_code(codes::lex::UNKNOWN_ESCAPE)
                .with_label(Label::primary(span, label))
                .with_note(
                    "supported escapes are `\\\\`, `\\\"`, `\\n`, `\\r`, `\\t`, and `\\u{H}`",
                ),
        );
    }

    fn push_unterminated_string(&mut self, start: usize) {
        self.diagnostics.push(
            Diagnostic::error("unterminated string literal")
                .with_code(codes::lex::UNTERMINATED_STRING)
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

    fn push_unterminated_interpolation_fragment(&mut self, fragment_start: usize) {
        self.push_unterminated_interpolation(fragment_start, false);
        self.push(
            TokenKind::InterpolationEnd(self.source[fragment_start..self.offset].to_owned()),
            Span::new(fragment_start, self.offset),
        );
    }

    fn push_unterminated_interpolation(&mut self, start: usize, synthesize_end: bool) {
        self.diagnostics.push(
            Diagnostic::error("unterminated string interpolation")
                .with_code(codes::lex::UNTERMINATED_INTERPOLATION)
                .with_label(Label::primary(
                    Span::new(start, self.offset),
                    "interpolated string starts here",
                ))
                .with_note("close the interpolation with `}` and the string with a `\"`."),
        );

        if synthesize_end {
            self.push(
                TokenKind::InterpolationEnd(String::new()),
                Span::point(self.offset),
            );
        }
    }

    fn interpolation_newline_or_eof(&mut self) -> bool {
        if self.interp_contexts.is_empty() || !self.at_newline_or_eof() {
            return false;
        }

        let start = self
            .interp_contexts
            .last()
            .map(|context| context.start)
            .unwrap_or(self.offset);
        self.push_unterminated_interpolation(start, true);
        self.interp_contexts.clear();
        true
    }

    fn push_interpolation_context(&mut self, start: usize) {
        self.interp_contexts.push(InterpolationContext {
            brace_depth: 0,
            start,
        });
    }

    fn scan_open_brace(&mut self) {
        if let Some(context) = self.interp_contexts.last_mut() {
            context.brace_depth += 1;
        }

        self.push_single(TokenKind::OpenBrace);
    }

    fn scan_close_brace(&mut self) {
        let Some(context) = self.interp_contexts.last_mut() else {
            self.push_single(TokenKind::CloseBrace);
            return;
        };

        if context.brace_depth > 0 {
            context.brace_depth -= 1;
            self.push_single(TokenKind::CloseBrace);
            return;
        }

        self.interp_contexts.pop();
        self.offset += 1;
        self.scan_string_continuation();
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

        if self.source.as_bytes()[start + 1].is_ascii_uppercase() {
            self.push(
                TokenKind::Tag(self.source[start + 1..self.offset].to_owned()),
                Span::new(start, self.offset),
            );
            return;
        }

        self.push(
            TokenKind::ComptimeParamMarker(self.source[start + 1..self.offset].to_owned()),
            Span::new(start, self.offset),
        );
    }

    fn scan_label_segment(&mut self) {
        while self.current_byte().is_some_and(is_identifier_continue_byte) {
            self.offset += 1;
        }
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
                .with_code(codes::lex::UNTERMINATED_REGEX)
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
            .is_none_or(|token| match &token.kind {
                // Field access never starts a regex: after `.` / `?.` the next
                // token is a member name (identifier or operator member), and
                // bare `/` must stay the division operator (`./lib` paths).
                TokenKind::Operator(operator) if operator == "." || operator == "?." => false,
                TokenKind::Operator(_)
                | TokenKind::OpenParen
                | TokenKind::OpenBrace
                | TokenKind::OpenBracket => true,
                _ => false,
            })
    }

    fn scan_operator(&mut self) {
        match self.current_byte() {
            Some(b'?') => self.scan_reserved_operator("?", &["?.", "??", "?^", "?!", "?>", "?"]),
            Some(b'=') => self.scan_reserved_operator("=", &["=>", "==", "="]),
            Some(b':') => self.scan_reserved_operator(":", &[":..", "::", ":=", ":"]),
            Some(b'.') => self.scan_reserved_operator(".", &["..", "."]),
            Some(b'|') => self.scan_reserved_operator("|", &["|>", "||", "|"]),
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

        // Continuations are keyed by the matched token, not only the reserved
        // prefix: bare `.` must not absorb operator members (`Int.+`), while
        // multi-character reserved forms like `..` still reject `..<`.
        if self
            .current_byte()
            .is_some_and(|byte| reserved_operator_continues(operator, byte))
        {
            while self
                .current_byte()
                .is_some_and(|byte| reserved_operator_continues(operator, byte))
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
                .with_code(codes::lex::RESERVED_OPERATOR)
                .with_label(Label::primary(
                    Span::new(start, self.offset),
                    "this operator namespace is reserved by the language",
                ))
                .with_note("custom operators cannot start with `=`, `:`, `.`, `?`, `@`, or `|`"),
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
                .with_code(codes::lex::UNEXPECTED_CHARACTER)
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

fn keyword_from_text(text: &str) -> Option<Keyword> {
    match text {
        "true" => Some(Keyword::True),
        "false" => Some(Keyword::False),
        "null" => Some(Keyword::Null),
        "undefined" => Some(Keyword::Undefined),
        _ => None,
    }
}

pub fn is_comptime_identifier_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_uppercase())
}

fn is_operator_start_byte(byte: u8) -> bool {
    is_custom_operator_byte(byte) || matches!(byte, b':' | b'.' | b'?' | b'|')
}

fn is_custom_operator_continue_byte(byte: u8) -> bool {
    is_custom_operator_byte(byte)
}

fn is_operator_run_byte(byte: u8) -> bool {
    is_custom_operator_continue_byte(byte) || matches!(byte, b':' | b'?' | b'.' | b'@' | b'|')
}

fn reserved_operator_continues(matched: &str, byte: u8) -> bool {
    match matched {
        "?" => byte != b':' && is_operator_run_byte(byte),
        "@" => is_operator_run_byte(byte),
        "=" | "==" | "=>" => byte == b'=',
        ":" | "::" | ":=" | ":.." => matches!(byte, b':' | b'.' | b'?' | b'='),
        // Bare `.` is field access: do not absorb following operator members
        // (`+`, `-`, `*`, …). Only multi-dot / `?` runs stay reserved
        // continuations (`...`, `.?`).
        "." => matches!(byte, b'.' | b'?'),
        // `..` and longer reserved `.` forms still reject custom continuations
        // (`..<`, `...`).
        ".." => is_custom_operator_continue_byte(byte) || matches!(byte, b'.' | b'?'),
        "|" | "||" | "|>" => is_operator_run_byte(byte),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use aven_core::Span;

    use super::{Keyword, TokenKind, is_identifier, lex_source};

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
        assert!(!is_identifier("true"));
        assert!(!is_identifier("false"));
        assert!(!is_identifier("null"));
        assert!(!is_identifier("undefined"));
    }

    #[test]
    fn lexes_value_keywords_as_reserved_tokens() {
        let output = lex_source("true false null undefined");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::Keyword(Keyword::True),
                TokenKind::Keyword(Keyword::False),
                TokenKind::Keyword(Keyword::Null),
                TokenKind::Keyword(Keyword::Undefined),
            ]
        );
    }

    #[test]
    fn lexes_string_interpolation_fragments_and_body_tokens() {
        let output = lex_source("\"a${b}c\"");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::InterpolationStart("\"a".to_owned()),
                TokenKind::Identifier("b".to_owned()),
                TokenKind::InterpolationEnd("c\"".to_owned()),
            ]
        );
    }

    #[test]
    fn lexes_brace_balanced_interpolation_bodies() {
        let output = lex_source("\"${ {x: 1} }\"");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::InterpolationStart("\"".to_owned()),
                TokenKind::OpenBrace,
                TokenKind::Identifier("x".to_owned()),
                TokenKind::Operator(":".to_owned()),
                TokenKind::Number("1".to_owned()),
                TokenKind::CloseBrace,
                TokenKind::InterpolationEnd("\"".to_owned()),
            ]
        );
    }

    #[test]
    fn former_path_literal_now_lexes_as_constituent_tokens() {
        let output = lex_source("./lib/Text");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        // `PathLiteral` is gone: a bare `./lib/Text` is no longer a single
        // token. Bare `.` no longer absorbs following operator characters, so
        // this splits into field-access `.`, division `/`, then the rest.
        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::Operator(".".to_owned()),
                TokenKind::Operator("/".to_owned()),
                TokenKind::Identifier("lib".to_owned()),
                TokenKind::Operator("/".to_owned()),
                TokenKind::ComptimeIdentifier("Text".to_owned()),
            ]
        );
    }

    #[test]
    fn lexes_unbound_operator_member_access_as_dot_then_operator() {
        let output = lex_source("Int.+(1, 2)");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::ComptimeIdentifier("Int".to_owned()),
                TokenKind::Operator(".".to_owned()),
                TokenKind::Operator("+".to_owned()),
                TokenKind::OpenParen,
                TokenKind::Number("1".to_owned()),
                TokenKind::Comma,
                TokenKind::Number("2".to_owned()),
                TokenKind::CloseParen,
            ]
        );
    }

    #[test]
    fn lexes_custom_operator_runs_without_absorbing_pipe_tokens() {
        let output = lex_source("left.**(right) *| tail |> next || fallback && guard ?? default");
        let operators: Vec<_> = output
            .tokens
            .into_iter()
            .filter_map(|token| match token.kind {
                TokenKind::Operator(operator) => Some(operator),
                _ => None,
            })
            .collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(operators, [".", "**", "*", "|", "|>", "||", "&&", "??"]);
    }

    #[test]
    fn spaced_dot_plus_still_splits_without_reserved_diagnostic() {
        // Was previously `lex.reserved-operator` (`.` absorbed `+`). Now it is
        // `.` then `+`; the parser/checker must reject misuse in expression
        // position rather than inventing a binary meaning for `a .+ b`.
        let output = lex_source("a .+ b");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("a".to_owned()),
                TokenKind::Operator(".".to_owned()),
                TokenKind::Operator("+".to_owned()),
                TokenKind::Identifier("b".to_owned()),
            ]
        );
    }

    #[test]
    fn quoted_former_path_literal_still_lexes_as_a_string() {
        let output = lex_source("\"./lib/Text\"");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![TokenKind::StringLiteral("\"./lib/Text\"".to_owned())]
        );
    }

    #[test]
    fn escaped_interpolation_marker_recovers_as_a_plain_string() {
        let output = lex_source(r#""\${x}""#);
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(
            output.diagnostics[0].code.as_deref(),
            Some("lex.unknown-escape")
        );
        assert_eq!(
            tokens,
            vec![TokenKind::StringLiteral(r#""\${x}""#.to_owned())]
        );
    }

    #[test]
    fn lexes_multiple_interpolations() {
        let output = lex_source("\"${a}${b}\"");
        let tokens: Vec<_> = output.tokens.into_iter().map(|token| token.kind).collect();

        assert!(output.diagnostics.is_empty());
        assert_eq!(
            tokens,
            vec![
                TokenKind::InterpolationStart("\"".to_owned()),
                TokenKind::Identifier("a".to_owned()),
                TokenKind::InterpolationMiddle(String::new()),
                TokenKind::Identifier("b".to_owned()),
                TokenKind::InterpolationEnd("\"".to_owned()),
            ]
        );
    }

    #[test]
    fn reports_unterminated_interpolation_on_newline_and_eof() {
        for source in ["\"a${b\n", "\"a${b"] {
            let output = lex_source(source);
            let codes: Vec<_> = output
                .diagnostics
                .iter()
                .filter_map(|diagnostic| diagnostic.code.as_deref())
                .collect();

            assert_eq!(codes, vec!["lex.unterminated-interpolation"]);
        }
    }

    #[test]
    fn reports_unknown_and_malformed_string_escapes() {
        for (source, span) in [
            (r#""\q""#, Span::new(1, 3)),
            (r#""\u""#, Span::new(1, 3)),
            (r#""\u{zz}""#, Span::new(1, 7)),
            (r#""\u{41""#, Span::new(1, 6)),
        ] {
            let output = lex_source(source);

            assert_eq!(output.diagnostics.len(), 1);
            assert_eq!(
                output.diagnostics[0].code.as_deref(),
                Some("lex.unknown-escape")
            );
            assert_eq!(output.diagnostics[0].labels[0].span, span);
        }
    }
}
