use aven_core::Diagnostic;
use aven_parser::{ParseOutput, Token, TokenKind, parse_module};

const INDENT_WIDTH: usize = 2;

pub fn format_source(source: &str) -> Result<String, Vec<Diagnostic>> {
    let parse = parse_module(source);
    format_parsed_source(source, &parse)
}

pub fn format_parsed_source(source: &str, parse: &ParseOutput) -> Result<String, Vec<Diagnostic>> {
    if parse.diagnostics.iter().any(Diagnostic::is_error) {
        return Err(parse.diagnostics.clone());
    }

    let line_count = source.lines().count();
    let line_starts = line_starts(source);
    let mut line_indents = layout_line_indents(line_count, &line_starts, &parse.layout_tokens);
    fill_trivia_line_indents(source, &mut line_indents);
    let line_tokens = content_tokens_by_line(line_count, &line_starts, &parse.raw_tokens);

    let mut output = String::with_capacity(source.len() + 1);

    for (line_index, tokens) in line_tokens.iter().enumerate() {
        if tokens.is_empty() {
            output.push('\n');
            continue;
        }

        let indent = line_indents.get(line_index).copied().flatten().unwrap_or(0);
        output.push_str(&" ".repeat(indent * INDENT_WIDTH));
        emit_line(&mut output, source, tokens);
        output.push('\n');
    }

    Ok(output)
}

fn layout_line_indents(
    line_count: usize,
    line_starts: &[usize],
    tokens: &[Token],
) -> Vec<Option<usize>> {
    let mut line_indents = vec![None; line_count];
    let mut depth = 0usize;

    for token in tokens {
        match token.kind {
            TokenKind::Indent => depth += 1,
            TokenKind::Dedent => depth = depth.saturating_sub(1),
            TokenKind::Newline => {}
            _ => {
                let line = line_for_offset(line_starts, token.span.start);
                if line < line_indents.len() && line_indents[line].is_none() {
                    line_indents[line] = Some(depth);
                }
            }
        }
    }

    line_indents
}

fn fill_trivia_line_indents(source: &str, line_indents: &mut [Option<usize>]) {
    let lines = source.lines().collect::<Vec<_>>();

    for index in 0..line_indents.len() {
        if line_indents[index].is_some() || lines[index].trim().is_empty() {
            continue;
        }

        line_indents[index] = nearest_indent(index, line_indents);
    }
}

fn nearest_indent(index: usize, line_indents: &[Option<usize>]) -> Option<usize> {
    let next = line_indents[index + 1..]
        .iter()
        .copied()
        .find(Option::is_some)
        .flatten();
    let previous = line_indents[..index]
        .iter()
        .rev()
        .copied()
        .find(Option::is_some)
        .flatten();

    next.or(previous)
}

fn content_tokens_by_line<'a>(
    line_count: usize,
    line_starts: &[usize],
    tokens: &'a [Token],
) -> Vec<Vec<&'a Token>> {
    let mut lines = vec![Vec::new(); line_count];

    for token in tokens {
        if matches!(
            token.kind,
            TokenKind::RawIndent { .. } | TokenKind::RawNewline
        ) {
            continue;
        }

        let line = line_for_offset(line_starts, token.span.start);
        if line < lines.len() {
            lines[line].push(token);
        }
    }

    lines
}

fn emit_line(output: &mut String, source: &str, tokens: &[&Token]) {
    for (index, token) in tokens.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|index| tokens.get(index).copied());
        let previous_previous = index
            .checked_sub(2)
            .and_then(|index| tokens.get(index).copied());
        let next = tokens.get(index + 1).copied();

        if let Some(previous) = previous
            && needs_space(previous_previous, previous, token, next)
        {
            output.push(' ');
        }

        output.push_str(&token_text(source, token));
    }
}

fn token_text(source: &str, token: &Token) -> String {
    let text = source
        .get(token.span.start..token.span.end)
        .unwrap_or_default();

    if matches!(token.kind, TokenKind::Comment(_) | TokenKind::DocComment(_)) {
        text.trim_end().to_owned()
    } else {
        text.to_owned()
    }
}

fn needs_space(
    previous_previous: Option<&Token>,
    previous: &Token,
    current: &Token,
    next: Option<&Token>,
) -> bool {
    if is_comment(current) {
        return true;
    }

    if is_comment(previous)
        || is_close_paren_or_bracket(current)
        || is_tight_set_postfix_marker(previous, current, next)
        || is_tight_postfix_operator(current)
        || is_tight_access_operator(current)
        || is_colon(current)
    {
        return false;
    }

    if is_close_brace(current) {
        return !is_open_brace(previous);
    }

    if is_prefix_minus(previous, previous_previous) {
        return false;
    }

    if is_open_delimiter(current) {
        return needs_space_before_open_delimiter(previous, current);
    }

    if is_open_delimiter(previous) {
        return needs_space_after_open_delimiter(previous, current);
    }

    if is_separator(current) {
        return false;
    }

    if is_separator(previous) {
        return true;
    }

    if is_prefix_minus(previous, previous_previous) {
        return false;
    }

    if is_tight_access_operator(previous)
        || is_tight_postfix_operator(previous)
        || is_tight_prefix_operator(previous, Some(current))
        || is_at_set_marker(previous, Some(current))
    {
        return false;
    }

    if is_binary_operator(current) || is_binary_operator(previous) {
        return true;
    }

    true
}

fn needs_space_before_open_delimiter(previous: &Token, current: &Token) -> bool {
    if is_separator(previous) {
        return true;
    }

    if is_open_brace(current) {
        return !is_at_set_marker(previous, Some(current));
    }

    if is_binary_operator(previous) {
        return true;
    }

    false
}

fn needs_space_after_open_delimiter(previous: &Token, current: &Token) -> bool {
    is_open_brace(previous) && !is_close_delimiter(current)
}

fn is_comment(token: &Token) -> bool {
    matches!(token.kind, TokenKind::Comment(_) | TokenKind::DocComment(_))
}

fn is_open_delimiter(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace
    )
}

fn is_open_brace(token: &Token) -> bool {
    matches!(token.kind, TokenKind::OpenBrace)
}

fn is_close_delimiter(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace
    )
}

fn is_close_paren_or_bracket(token: &Token) -> bool {
    matches!(token.kind, TokenKind::CloseParen | TokenKind::CloseBracket)
}

fn is_close_brace(token: &Token) -> bool {
    matches!(token.kind, TokenKind::CloseBrace)
}

fn is_separator(token: &Token) -> bool {
    matches!(token.kind, TokenKind::Comma | TokenKind::Semicolon)
}

fn is_binary_operator(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if !matches!(
        operator.as_str(),
        "." | "?." | "?" | "?^" | "?!" | "@" | ".." | ":.."
    ))
}

fn is_tight_access_operator(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if operator == "." || operator == "?.")
}

/// The annotation/field colon binds tight to the label on its left (`name: T`,
/// `x: Int`) and keeps a single space after it. The `::` replace marker is a
/// separate binary operator and stays spaced on both sides.
fn is_colon(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if operator == ":")
}

fn is_tight_postfix_operator(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if matches!(operator.as_str(), "?" | "?^" | "?!"))
}

fn is_tight_prefix_operator(token: &Token, next: Option<&Token>) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if matches!(operator.as_str(), ".." | ":.."))
        || is_at_set_marker(token, next)
}

fn is_at_set_marker(token: &Token, next: Option<&Token>) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if operator == "@")
        && next.is_some_and(is_open_brace)
}

fn is_tight_set_postfix_marker(previous: &Token, current: &Token, next: Option<&Token>) -> bool {
    is_at_set_marker(current, next) && can_end_postfix_operand(previous)
}

fn can_end_postfix_operand(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::Identifier(_)
            | TokenKind::ComptimeIdentifier(_)
            | TokenKind::CloseParen
            | TokenKind::CloseBracket
            | TokenKind::CloseBrace
    )
}

fn is_prefix_minus(token: &Token, previous_previous: Option<&Token>) -> bool {
    if !matches!(&token.kind, TokenKind::Operator(operator) if operator == "-") {
        return false;
    }

    previous_previous.is_none_or(|previous| {
        is_open_delimiter(previous) || is_separator(previous) || is_binary_operator(previous)
    })
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];

    for (offset, ch) in source.char_indices() {
        if ch == '\n' {
            starts.push(offset + 1);
        }
    }

    starts
}

fn line_for_offset(line_starts: &[usize], offset: usize) -> usize {
    line_starts
        .partition_point(|line_start| *line_start <= offset)
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_whitespace_and_adds_final_newline() {
        assert_eq!(format_source("x = 1   \n\n"), Ok("x = 1\n\n".to_owned()));
        assert_eq!(format_source("x = 1"), Ok("x = 1\n".to_owned()));
    }

    #[test]
    fn normalizes_layout_indentation_to_two_spaces() {
        assert_eq!(
            format_source("x =\n    y =\n        z = 2\t\n"),
            Ok("x =\n  y =\n    z = 2\n".to_owned())
        );
    }

    #[test]
    fn preserves_existing_two_space_indentation() {
        assert_eq!(
            format_source("x =\n  y =\n    z = 2\t\n"),
            Ok("x =\n  y =\n    z = 2\n".to_owned())
        );
    }

    #[test]
    fn formats_from_existing_parse_output() {
        let source = "x =\n    y = 1   \n";
        let parse = aven_parser::parse_module(source);

        assert_eq!(
            format_parsed_source(source, &parse),
            Ok("x =\n  y = 1\n".to_owned())
        );
    }

    #[test]
    fn preserves_comments_and_blank_lines() {
        let input =
            "# module comment   \nvalue =\n    # block comment   \n    item = 1   \n\nnext = 2\n";

        assert_eq!(
            format_source(input),
            Ok("# module comment\nvalue =\n  # block comment\n  item = 1\n\nnext = 2\n".to_owned())
        );
    }

    #[test]
    fn formatting_is_idempotent() {
        let formatted = match format_source(
            "# module comment   \nvalue =\n    # block comment   \n    item = 1   \n\nnext = 2",
        ) {
            Ok(formatted) => formatted,
            Err(diagnostics) => panic!("expected formatting to succeed, got {diagnostics:?}"),
        };

        assert_eq!(format_source(&formatted), Ok(formatted));
    }

    #[test]
    fn refuses_to_format_sources_with_parse_errors() {
        let result = format_source("value = )\n");

        assert!(matches!(
            result,
            Err(diagnostics) if diagnostics.iter().any(Diagnostic::is_error)
        ));
    }

    #[test]
    fn normalizes_simple_expression_spacing() {
        assert_eq!(
            format_source(
                "sum=add(1,2)+user . age\njson=users ?. active |>toJson ( )\nshape=@{@Red,@Ok(1)}\nrecord={name:\"Ada\",age:36}\nnegative=-1\noffset=1 + -2\ncleaned={..user,-password}\n"
            ),
            Ok(
                "sum = add(1, 2) + user.age\njson = users?.active |> toJson()\nshape = @{ @Red, @Ok(1) }\nrecord = { name: \"Ada\", age: 36 }\nnegative = -1\noffset = 1 + -2\ncleaned = { ..user, -password }\n"
                    .to_owned()
            )
        );
    }
}
