use std::collections::HashMap;

use aven_core::{Diagnostic, Span};
use aven_parser::{
    Expr, ExprKind, Item, MatchArm, Module, ParseOutput, RecordEntry, Token, TokenKind,
    is_identifier, parse_module, walk_expr_children,
};

const INDENT_WIDTH: usize = 2;
/// Soft line-width budget used only for deciding whether an authored inline
/// match stays on one line or breaks to the standard indented arm block.
const MAX_LINE_WIDTH: usize = 100;

pub fn format_source(source: &str) -> Result<String, Vec<Diagnostic>> {
    let parse = parse_module(source);
    format_parsed_source(source, &parse)
}

pub fn format_parsed_source(source: &str, parse: &ParseOutput) -> Result<String, Vec<Diagnostic>> {
    if parse.diagnostics.iter().any(Diagnostic::is_error) {
        return Err(parse.diagnostics.clone());
    }

    let field_name_spans = collect_field_name_spans(&parse.module);
    let line_count = source.lines().count();
    let line_starts = line_starts(source);
    let mut line_indents = layout_line_indents(line_count, &line_starts, &parse.layout_tokens);
    fill_trivia_line_indents(source, &mut line_indents);
    let line_tokens = content_tokens_by_line(line_count, &line_starts, &parse.raw_tokens);
    let inline_matches = collect_inline_matches(&parse.module, &line_starts, &parse.raw_tokens);

    let mut output = String::with_capacity(source.len() + 1);

    for (line_index, tokens) in line_tokens.iter().enumerate() {
        if tokens.is_empty() {
            output.push('\n');
            continue;
        }

        let indent = line_indents.get(line_index).copied().flatten().unwrap_or(0);
        let indent_text = " ".repeat(indent * INDENT_WIDTH);

        let mut flat = String::new();
        flat.push_str(&indent_text);
        emit_line(&mut flat, source, tokens, &field_name_spans);

        let breakable = inline_matches
            .iter()
            .find(|match_layout| match_layout.line == line_index);

        if flat.chars().count() > MAX_LINE_WIDTH
            && let Some(match_layout) = breakable.filter(|layout| layout.can_break_to_layout)
        {
            emit_broken_inline_match(
                &mut output,
                source,
                tokens,
                &field_name_spans,
                match_layout,
                indent,
            );
        } else {
            output.push_str(&flat);
            output.push('\n');
        }
    }

    Ok(output)
}

/// An authored inline match: `?>` and every arm start on the same source line.
/// Derived from spans (no AST flag). Block matches are never collected here, so
/// the formatter never collapses them to inline.
struct InlineMatchLayout {
    line: usize,
    match_span: Span,
    subject_span: Span,
    arm_spans: Vec<Span>,
    can_break_to_layout: bool,
}

fn collect_inline_matches(
    module: &Module,
    line_starts: &[usize],
    tokens: &[Token],
) -> Vec<InlineMatchLayout> {
    let mut matches = Vec::new();
    for item in &module.items {
        collect_item_inline_matches(item, line_starts, tokens, &mut matches);
    }
    matches
}

fn collect_item_inline_matches(
    item: &Item,
    line_starts: &[usize],
    tokens: &[Token],
    matches: &mut Vec<InlineMatchLayout>,
) {
    match item {
        Item::Binding(binding) => {
            if let Some(annotation) = &binding.annotation {
                collect_expr_inline_matches(annotation, line_starts, tokens, matches);
            }
            collect_expr_inline_matches(&binding.value, line_starts, tokens, matches);
        }
        Item::PatternBinding(binding) => {
            collect_expr_inline_matches(&binding.pattern, line_starts, tokens, matches);
            collect_expr_inline_matches(&binding.value, line_starts, tokens, matches);
        }
        Item::SpreadBinding(binding) => {
            collect_expr_inline_matches(&binding.value, line_starts, tokens, matches);
        }
        Item::Signature(signature) => {
            collect_expr_inline_matches(&signature.annotation, line_starts, tokens, matches);
        }
        Item::Expr(expr) => collect_expr_inline_matches(expr, line_starts, tokens, matches),
    }
}

fn collect_expr_inline_matches(
    expr: &Expr,
    line_starts: &[usize],
    tokens: &[Token],
    matches: &mut Vec<InlineMatchLayout>,
) {
    if let ExprKind::Match {
        subject,
        operator_span,
        arms,
    } = &expr.kind
    {
        if let Some(layout) = inline_match_layout(
            expr.span,
            subject,
            *operator_span,
            arms,
            line_starts,
            tokens,
        ) {
            matches.push(layout);
        }
        collect_expr_inline_matches(subject, line_starts, tokens, matches);
        for arm in arms {
            collect_expr_inline_matches(&arm.pattern, line_starts, tokens, matches);
            for guard in &arm.guards {
                collect_expr_inline_matches(guard, line_starts, tokens, matches);
            }
            collect_expr_inline_matches(&arm.body, line_starts, tokens, matches);
        }
        return;
    }

    walk_expr_children(expr, &mut |child| {
        collect_expr_inline_matches(child, line_starts, tokens, matches);
    });
}

fn inline_match_layout(
    match_span: Span,
    subject: &Expr,
    operator_span: Span,
    arms: &[MatchArm],
    line_starts: &[usize],
    tokens: &[Token],
) -> Option<InlineMatchLayout> {
    if arms.is_empty() {
        return None;
    }

    let line = line_for_offset(line_starts, operator_span.start);
    let same_line = |span: Span| {
        line_for_offset(line_starts, span.start) == line
            && line_for_offset(line_starts, span.end.saturating_sub(1)) == line
    };

    if !same_line(operator_span) || !arms.iter().all(|arm| same_line(arm.span)) {
        return None;
    }

    Some(InlineMatchLayout {
        line,
        match_span,
        subject_span: subject.span,
        arm_spans: arms.iter().map(|arm| arm.span).collect(),
        can_break_to_layout: !is_inside_delimiter(match_span, tokens),
    })
}

/// Layout match arms end at a physical line boundary. Inside a delimiter that
/// boundary cannot safely terminate the expression, so retain the authored
/// inline arms even when they exceed the soft width budget.
fn is_inside_delimiter(span: Span, tokens: &[Token]) -> bool {
    let mut depth = 0usize;

    for token in tokens {
        if token.span.start >= span.start {
            break;
        }

        match token.kind {
            TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
            TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    depth > 0
}

fn emit_broken_inline_match(
    output: &mut String,
    source: &str,
    tokens: &[&Token],
    field_name_spans: &HashMap<Span, &str>,
    match_layout: &InlineMatchLayout,
    indent: usize,
) {
    let prefix: Vec<&Token> = tokens
        .iter()
        .copied()
        .filter(|token| token.span.end <= match_layout.match_span.start)
        .collect();
    let subject: Vec<&Token> = tokens_in_span(tokens, match_layout.subject_span);
    let arms: Vec<Vec<&Token>> = match_layout
        .arm_spans
        .iter()
        .map(|span| tokens_in_span(tokens, *span))
        .collect();
    let suffix: Vec<&Token> = tokens
        .iter()
        .copied()
        .filter(|token| token.span.start >= match_layout.match_span.end)
        .collect();

    let base_indent = " ".repeat(indent * INDENT_WIDTH);
    let arm_indent = " ".repeat((indent + 1) * INDENT_WIDTH);

    output.push_str(&base_indent);
    if !prefix.is_empty() {
        emit_line(output, source, &prefix, field_name_spans);
        if !subject.is_empty() {
            output.push(' ');
        }
    }
    emit_line(output, source, &subject, field_name_spans);
    if !subject.is_empty() {
        output.push(' ');
    }
    output.push_str("?>");

    for (index, arm_tokens) in arms.iter().enumerate() {
        output.push('\n');
        output.push_str(&arm_indent);
        if index + 1 == arms.len() && !suffix.is_empty() {
            let mut last_line = arm_tokens.clone();
            last_line.extend_from_slice(&suffix);
            emit_line(output, source, &last_line, field_name_spans);
        } else {
            emit_line(output, source, arm_tokens, field_name_spans);
        }
    }

    output.push('\n');
}

fn tokens_in_span<'a>(tokens: &[&'a Token], span: Span) -> Vec<&'a Token> {
    tokens
        .iter()
        .copied()
        .filter(|token| token.span.start >= span.start && token.span.end <= span.end)
        .collect()
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

fn emit_line(
    output: &mut String,
    source: &str,
    tokens: &[&Token],
    field_name_spans: &HashMap<Span, &str>,
) {
    for (index, token) in tokens.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|index| tokens.get(index).copied());
        let previous_previous = index
            .checked_sub(2)
            .and_then(|index| tokens.get(index).copied());
        let next = tokens.get(index + 1).copied();

        if let Some(previous) = previous
            && !is_operator_member_open_paren(previous, token, field_name_spans)
            && needs_space(previous_previous, previous, token, next)
        {
            output.push(' ');
        }

        output.push_str(&token_text(source, token, field_name_spans));
    }
}

fn is_operator_member_open_paren(
    previous: &Token,
    current: &Token,
    field_name_spans: &HashMap<Span, &str>,
) -> bool {
    current.kind == TokenKind::OpenParen
        && field_name_spans
            .get(&previous.span)
            .is_some_and(|name| !is_identifier(name))
}

fn token_text(source: &str, token: &Token, field_name_spans: &HashMap<Span, &str>) -> String {
    if let Some(&name) = field_name_spans.get(&token.span)
        && is_identifier(name)
    {
        return name.to_owned();
    }

    match &token.kind {
        TokenKind::InterpolationStart(text) => return format!("{text}${{"),
        TokenKind::InterpolationMiddle(text) => return format!("}}{text}${{"),
        TokenKind::InterpolationEnd(text) => return format!("}}{text}"),
        _ => {}
    }

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
        || is_interpolation_continuation(current)
        || is_interpolation_prefix(previous)
        || is_close_paren_or_bracket(current)
        || is_tight_set_postfix_marker(previous, current, next)
        || is_tight_postfix_operator(current, Some(previous))
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
        || is_tight_prefix_operator(previous, previous_previous, Some(current))
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
        if is_spread_operator(previous) || is_open_paren_or_bracket(previous) {
            return false;
        }
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

fn is_open_paren_or_bracket(token: &Token) -> bool {
    matches!(token.kind, TokenKind::OpenParen | TokenKind::OpenBracket)
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
        "." | "?." | "?" | "!" | "?^" | "?!" | "@" | ".." | ":.."
    ))
}

fn is_tight_access_operator(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if operator == "." || operator == "?.")
}

fn is_spread_operator(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if matches!(operator.as_str(), ".." | ":.."))
}

/// The annotation/field colon binds tight to the label on its left (`name: T`,
/// `x: Int`) and keeps a single space after it. The `::` replace marker is a
/// separate binary operator and stays spaced on both sides.
fn is_colon(token: &Token) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if operator == ":")
}

fn is_tight_postfix_operator(token: &Token, previous: Option<&Token>) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if matches!(operator.as_str(), "?^" | "?!"))
        || matches!(&token.kind, TokenKind::Operator(operator) if operator == "?" && previous.is_some_and(can_end_postfix_operand))
        || matches!(&token.kind, TokenKind::Operator(operator) if operator == "!" && previous.is_some_and(can_end_postfix_operand))
}

fn is_tight_prefix_operator(token: &Token, previous: Option<&Token>, next: Option<&Token>) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if matches!(operator.as_str(), ".." | ":.."))
        || matches!(&token.kind, TokenKind::Operator(operator) if operator == "!" && previous.is_none_or(|previous| !can_end_postfix_operand(previous)))
        || matches!(&token.kind, TokenKind::Operator(operator) if operator == "?" && previous.is_none_or(|previous| !can_end_postfix_operand(previous)))
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
        TokenKind::Keyword(_)
            | TokenKind::Identifier(_)
            | TokenKind::ComptimeIdentifier(_)
            | TokenKind::InterpolationEnd(_)
            | TokenKind::CloseParen
            | TokenKind::CloseBracket
            | TokenKind::CloseBrace
    )
}

fn is_interpolation_prefix(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::InterpolationStart(_) | TokenKind::InterpolationMiddle(_)
    )
}

fn is_interpolation_continuation(token: &Token) -> bool {
    matches!(
        token.kind,
        TokenKind::InterpolationMiddle(_) | TokenKind::InterpolationEnd(_)
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

fn collect_field_name_spans(module: &Module) -> HashMap<Span, &str> {
    let mut spans = HashMap::new();
    for item in &module.items {
        collect_item_field_names(item, &mut spans);
    }
    spans
}

fn collect_item_field_names<'a>(item: &'a Item, spans: &mut HashMap<Span, &'a str>) {
    match item {
        Item::Binding(binding) => {
            if let Some(annotation) = &binding.annotation {
                collect_expr_field_names(annotation, spans);
            }
            collect_expr_field_names(&binding.value, spans);
        }
        Item::PatternBinding(binding) => {
            collect_expr_field_names(&binding.pattern, spans);
            collect_expr_field_names(&binding.value, spans);
        }
        Item::SpreadBinding(binding) => collect_expr_field_names(&binding.value, spans),
        Item::Signature(signature) => collect_expr_field_names(&signature.annotation, spans),
        Item::Expr(expr) => collect_expr_field_names(expr, spans),
    }
}

fn collect_expr_field_names<'a>(expr: &'a Expr, spans: &mut HashMap<Span, &'a str>) {
    match &expr.kind {
        ExprKind::FieldAccess {
            receiver,
            field,
            field_span,
            ..
        } => {
            spans.insert(*field_span, field.as_str());
            collect_expr_field_names(receiver, spans);
        }
        ExprKind::Record(entries) | ExprKind::Set(entries) => {
            for entry in entries {
                collect_record_entry_field_names(entry, spans);
            }
        }
        _ => walk_expr_children(expr, &mut |child| collect_expr_field_names(child, spans)),
    }
}

fn collect_record_entry_field_names<'a>(
    entry: &'a RecordEntry,
    spans: &mut HashMap<Span, &'a str>,
) {
    match entry {
        RecordEntry::Field {
            name,
            name_span,
            value,
            ..
        } => {
            spans.insert(*name_span, name.as_str());
            collect_expr_field_names(value, spans);
        }
        RecordEntry::FieldComputed { key, value, .. } => {
            collect_expr_field_names(key, spans);
            collect_expr_field_names(value, spans);
        }
        RecordEntry::Method {
            name,
            name_span,
            value,
            ..
        } => {
            spans.insert(*name_span, name.as_str());
            collect_expr_field_names(value, spans);
        }
        RecordEntry::FieldDefault {
            name,
            name_span,
            annotation,
            default,
            ..
        } => {
            spans.insert(*name_span, name.as_str());
            collect_expr_field_names(annotation, spans);
            collect_expr_field_names(default, spans);
        }
        RecordEntry::Spread { value, .. }
        | RecordEntry::DeleteComputed { key: value, .. }
        | RecordEntry::Element(value) => {
            collect_expr_field_names(value, spans);
        }
        RecordEntry::Iteration {
            source,
            guard,
            body,
            ..
        } => {
            collect_expr_field_names(source, spans);
            if let Some(guard) = guard {
                collect_expr_field_names(guard, spans);
            }
            for body_entry in body {
                collect_record_entry_field_names(body_entry, spans);
            }
        }
        RecordEntry::Shorthand { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::Rename { .. }
        | RecordEntry::Open { .. } => {}
    }
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
    fn formats_lambda_parameter_defaults_stably() {
        let formatted =
            "log = (msg: Text, fields: Record = {}) => msg\ngreet = (name = \"world\") => name\n";

        // Spacing around `=` and `:` is normalised...
        assert_eq!(
            format_source("log=(msg:Text,fields:Record={})=>msg\ngreet=(name=\"world\")=>name\n"),
            Ok(formatted.to_owned())
        );
        // ...and re-formatting an already-formatted lambda is idempotent.
        assert_eq!(format_source(formatted), Ok(formatted.to_owned()));
    }

    #[test]
    fn keeps_braces_tight_inside_parens_and_brackets() {
        let formatted = "a = signup({ name: \"Dave\" })\nb = h([{ x: 1 }, { x: 2 }])\nc = g(@{ \"s\" })\nd = { x: { y: 1 } }\n";

        assert_eq!(
            format_source(
                "a = signup( { name: \"Dave\" })\nb = h([ { x: 1 }, { x: 2 }])\nc = g(@{ \"s\" })\nd = { x: { y: 1 } }\n"
            ),
            Ok(formatted.to_owned())
        );
        assert_eq!(format_source(formatted), Ok(formatted.to_owned()));
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
