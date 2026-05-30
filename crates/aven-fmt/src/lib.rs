use aven_core::Diagnostic;
use aven_parser::{Token, TokenKind, parse_module};

const INDENT_WIDTH: usize = 2;

pub fn format_source(source: &str) -> Result<String, Vec<Diagnostic>> {
    let parse = parse_module(source);
    if parse.diagnostics.iter().any(Diagnostic::is_error) {
        return Err(parse.diagnostics);
    }

    let line_count = source.lines().count();
    let line_starts = line_starts(source);
    let mut line_indents = layout_line_indents(line_count, &line_starts, &parse.layout_tokens);
    fill_trivia_line_indents(source, &mut line_indents);

    let mut output = String::with_capacity(source.len() + 1);

    for (line_index, line) in source.lines().enumerate() {
        let line = line.trim_end();

        if line.trim().is_empty() {
            output.push('\n');
            continue;
        }

        let indent = line_indents.get(line_index).copied().flatten().unwrap_or(0);
        output.push_str(&" ".repeat(indent * INDENT_WIDTH));
        output.push_str(line.trim_start());
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
}
