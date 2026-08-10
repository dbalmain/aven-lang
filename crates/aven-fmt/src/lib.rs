use std::collections::HashMap;

use aven_core::{Diagnostic, Span};
use aven_parser::{
    Expr, ExprKind, Item, MatchArm, Module, ModuleRole, OperatorFixityTable, ParseOutput,
    RecordEntry, Token, TokenKind, is_identifier, parse_module, parse_module_with_fixities,
    walk_expr_children, walk_module_exprs,
};

const INDENT_WIDTH: usize = 2;
/// Soft line-width budget used to decide when a call must use the canonical
/// one-argument-per-line shape. Kept separate so it can become a formatter
/// configuration field without coupling it to other width policies.
const CALL_MAX_LINE_WIDTH: usize = 80;
/// Soft line-width budget used only for deciding whether an authored inline
/// match stays on one line or breaks to the standard indented arm block.
const INLINE_MATCH_MAX_LINE_WIDTH: usize = 100;

pub fn format_source(source: &str) -> Result<String, Vec<Diagnostic>> {
    let parse = parse_module(source);
    format_parsed_source(source, &parse)
}

pub fn format_source_with_fixities(
    source: &str,
    operator_fixities: &OperatorFixityTable,
) -> Result<String, Vec<Diagnostic>> {
    let parse = parse_module_with_fixities(source, operator_fixities, ModuleRole::Entry);
    format_parsed_source_with_reparse(source, &parse, |source| {
        parse_module_with_fixities(source, operator_fixities, ModuleRole::Entry)
    })
}

pub fn format_parsed_source(source: &str, parse: &ParseOutput) -> Result<String, Vec<Diagnostic>> {
    format_parsed_source_with_reparse(source, parse, parse_module)
}

fn format_parsed_source_with_reparse(
    source: &str,
    parse: &ParseOutput,
    reparse: impl Fn(&str) -> ParseOutput,
) -> Result<String, Vec<Diagnostic>> {
    if parse.diagnostics.iter().any(Diagnostic::is_error) {
        return Err(parse.diagnostics.clone());
    }

    let mut formatted = format_lines(source, parse);
    let mut reparsed = reparse(&formatted);
    if reparsed.diagnostics.iter().any(Diagnostic::is_error) {
        return Ok(source.to_owned());
    }

    loop {
        let wrapped = normalize_call_layouts(&formatted, &reparsed);
        if wrapped == formatted {
            return Ok(formatted);
        }

        let wrapped_parse = reparse(&wrapped);
        if wrapped_parse.diagnostics.iter().any(Diagnostic::is_error) {
            return Ok(formatted);
        }

        formatted = wrapped;
        reparsed = wrapped_parse;
    }
}

fn format_lines(source: &str, parse: &ParseOutput) -> String {
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

        if flat.chars().count() > INLINE_MATCH_MAX_LINE_WIDTH
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

    output
}

#[derive(Debug)]
struct CallLayout {
    span: Span,
    callee_span: Span,
    argument_spans: Vec<Span>,
    separator_spans: Vec<Span>,
    trailing_comma_span: Option<Span>,
    open_span: Span,
    close_span: Span,
    chain_operator_span: Option<Span>,
    chain_receiver_span: Option<Span>,
}

#[derive(Debug)]
struct WhitespaceEdit {
    span: Span,
    replacement: String,
}

fn normalize_call_layouts(source: &str, parse: &ParseOutput) -> String {
    let line_starts = line_starts(source);
    let mut interpolation_spans = Vec::new();
    let mut binary_spans = Vec::new();
    walk_module_exprs(&parse.module, &mut |expr| {
        if matches!(expr.kind, ExprKind::Interpolation(_)) {
            interpolation_spans.push(expr.span);
        }
        if matches!(expr.kind, ExprKind::Binary { .. }) {
            binary_spans.push(expr.span);
        }
    });
    let mut calls = Vec::new();
    walk_module_exprs(&parse.module, &mut |expr| {
        if let ExprKind::Call { callee, args } = &expr.kind
            && let Some(call) = call_layout(expr.span, callee, args, &parse.raw_tokens)
            && call_can_reflow(
                source,
                &line_starts,
                &call,
                &interpolation_spans,
                &binary_spans,
            )
        {
            calls.push(call);
        }
    });

    let candidates = calls
        .iter()
        .filter_map(|call| call_layout_edits(source, &line_starts, call).map(|edits| (call, edits)))
        .collect::<Vec<_>>();
    let mut edits = Vec::new();

    for (call, call_edits) in &candidates {
        let contained_by_pending_call = candidates.iter().any(|(other, _)| {
            other.span != call.span
                && other.span.start <= call.span.start
                && call.span.end <= other.span.end
        });
        if !contained_by_pending_call {
            edits.extend(call_edits.iter().map(|edit| WhitespaceEdit {
                span: edit.span,
                replacement: edit.replacement.clone(),
            }));
        }
    }

    apply_whitespace_edits(source, edits)
}

/// Reflow only calls whose surrounding layout cannot contribute meaning.
/// Long unsafe calls deliberately remain long: the width is a soft budget.
fn call_can_reflow(
    source: &str,
    line_starts: &[usize],
    call: &CallLayout,
    interpolation_spans: &[Span],
    binary_spans: &[Span],
) -> bool {
    let arguments_are_single_line = call.argument_spans.iter().all(|span| {
        line_for_offset(line_starts, span.start)
            == line_for_offset(line_starts, span.end.saturating_sub(1))
    });
    let touches_interpolation = interpolation_spans.iter().any(|interpolation| {
        interpolation.start < call.span.end && call.span.start < interpolation.end
    });
    let is_binary_operand = binary_spans
        .iter()
        .any(|binary| binary.start <= call.span.start && call.span.end <= binary.end);
    let ends_line = source
        .get(call.close_span.end..)
        .and_then(|suffix| suffix.lines().next())
        .is_none_or(|suffix| suffix.trim().is_empty());

    arguments_are_single_line && !touches_interpolation && !is_binary_operand && ends_line
}

fn call_layout(span: Span, callee: &Expr, args: &[Expr], tokens: &[Token]) -> Option<CallLayout> {
    let open_span = tokens
        .iter()
        .find(|token| {
            token.kind == TokenKind::OpenParen
                && token.span.start >= callee.span.end
                && token.span.end <= span.end
        })?
        .span;
    let close_span = tokens
        .iter()
        .rev()
        .find(|token| {
            token.kind == TokenKind::CloseParen
                && token.span.start >= open_span.end
                && token.span.end <= span.end
        })?
        .span;

    let (chain_operator_span, chain_receiver_span) = match &callee.kind {
        ExprKind::FieldAccess {
            receiver,
            field_span,
            ..
        } => {
            let operator = tokens.iter().find(|token| {
                is_tight_access_operator(token)
                    && token.span.start >= receiver.span.end
                    && token.span.end <= field_span.start
            });
            (operator.map(|token| token.span), Some(receiver.span))
        }
        _ => (None, None),
    };
    let argument_spans = args.iter().map(|arg| arg.span).collect::<Vec<_>>();
    let separator_spans = argument_spans
        .windows(2)
        .map(|pair| {
            tokens
                .iter()
                .find(|token| {
                    token.kind == TokenKind::Comma
                        && token.span.start >= pair[0].end
                        && token.span.end <= pair[1].start
                })
                .map(|token| token.span)
        })
        .collect::<Option<Vec<_>>>()?;
    let trailing_comma_span = argument_spans.last().and_then(|last| {
        tokens
            .iter()
            .find(|token| {
                token.kind == TokenKind::Comma
                    && token.span.start >= last.end
                    && token.span.end <= close_span.start
            })
            .map(|token| token.span)
    });

    Some(CallLayout {
        span,
        callee_span: callee.span,
        argument_spans,
        separator_spans,
        trailing_comma_span,
        open_span,
        close_span,
        chain_operator_span,
        chain_receiver_span,
    })
}

fn call_layout_edits(
    source: &str,
    line_starts: &[usize],
    call: &CallLayout,
) -> Option<Vec<WhitespaceEdit>> {
    let open_line = line_for_offset(line_starts, call.open_span.start);
    let close_line = line_for_offset(line_starts, call.close_span.start);
    let arguments_are_wrapped = open_line != close_line
        || call.argument_spans.iter().any(|span| {
            line_for_offset(line_starts, span.start) != open_line
                || line_for_offset(line_starts, span.end.saturating_sub(1)) != open_line
        });
    let call_overruns = open_line == close_line
        && column_for_offset(source, line_starts, call.close_span.end) > CALL_MAX_LINE_WIDTH;

    let chain_was_wrapped = call
        .chain_operator_span
        .zip(call.chain_receiver_span)
        .is_some_and(|(operator, receiver)| {
            line_for_offset(line_starts, operator.start)
                > line_for_offset(line_starts, receiver.end.saturating_sub(1))
        });
    let wrap_chain = call.chain_operator_span.is_some() && (chain_was_wrapped || call_overruns);
    let wrap_arguments = arguments_are_wrapped || call_overruns;
    if !wrap_chain && !wrap_arguments {
        return None;
    }

    let base_indent = leading_indent(source, line_starts, call.callee_span.start);
    let call_indent = base_indent + usize::from(wrap_chain) * INDENT_WIDTH;
    let argument_indent = call_indent + INDENT_WIDTH;
    let mut edits = Vec::new();

    if wrap_chain {
        let operator = call.chain_operator_span?;
        let receiver = call.chain_receiver_span?;
        push_whitespace_edit(
            source,
            &mut edits,
            Span::new(receiver.end, operator.start),
            format!("\n{}", " ".repeat(call_indent)),
        )?;
    }

    if wrap_arguments {
        let argument_prefix = format!("\n{}", " ".repeat(argument_indent));
        let close_prefix = format!("\n{}", " ".repeat(call_indent));
        if let Some(first) = call.argument_spans.first() {
            push_line_prefix_edit(
                source,
                &mut edits,
                Span::new(call.open_span.end, first.start),
                argument_prefix.clone(),
            )?;

            for (pair, comma) in call.argument_spans.windows(2).zip(&call.separator_spans) {
                push_whitespace_edit(
                    source,
                    &mut edits,
                    Span::new(pair[0].end, comma.start),
                    String::new(),
                )?;
                push_line_prefix_edit(
                    source,
                    &mut edits,
                    Span::new(comma.end, pair[1].start),
                    argument_prefix.clone(),
                )?;
            }

            let close_start = call
                .trailing_comma_span
                .map_or(call.argument_spans.last()?.end, |span| span.end);
            push_line_prefix_edit(
                source,
                &mut edits,
                Span::new(close_start, call.close_span.start),
                close_prefix,
            )?;
        } else {
            push_line_prefix_edit(
                source,
                &mut edits,
                Span::new(call.open_span.end, call.close_span.start),
                close_prefix,
            )?;
        }
    }

    edits.retain(|edit| source.get(edit.span.start..edit.span.end) != Some(&edit.replacement));
    (!edits.is_empty()).then_some(edits)
}

fn push_whitespace_edit(
    source: &str,
    edits: &mut Vec<WhitespaceEdit>,
    span: Span,
    replacement: String,
) -> Option<()> {
    source
        .get(span.start..span.end)?
        .chars()
        .all(char::is_whitespace)
        .then_some(())?;
    edits.push(WhitespaceEdit { span, replacement });
    Some(())
}

/// Replace only the trailing whitespace before a token. This keeps comments
/// between a delimiter/separator and an argument in place while still putting
/// the argument itself on its canonical line.
fn push_line_prefix_edit(
    source: &str,
    edits: &mut Vec<WhitespaceEdit>,
    span: Span,
    replacement: String,
) -> Option<()> {
    let text = source.get(span.start..span.end)?;
    let trailing_start = text
        .char_indices()
        .rev()
        .find_map(|(offset, character)| {
            (!character.is_whitespace()).then_some(offset + character.len_utf8())
        })
        .unwrap_or(0);
    edits.push(WhitespaceEdit {
        span: Span::new(span.start + trailing_start, span.end),
        replacement,
    });
    Some(())
}

fn apply_whitespace_edits(source: &str, mut edits: Vec<WhitespaceEdit>) -> String {
    edits.sort_by_key(|edit| edit.span.start);
    let mut output = source.to_owned();
    for edit in edits.into_iter().rev() {
        output.replace_range(edit.span.start..edit.span.end, &edit.replacement);
    }
    output
}

fn leading_indent(source: &str, line_starts: &[usize], offset: usize) -> usize {
    let line_start = line_starts[line_for_offset(line_starts, offset)];
    source
        .get(line_start..offset)
        .unwrap_or_default()
        .bytes()
        .take_while(|byte| *byte == b' ')
        .count()
}

fn column_for_offset(source: &str, line_starts: &[usize], offset: usize) -> usize {
    let line_start = line_starts[line_for_offset(line_starts, offset)];
    source
        .get(line_start..offset)
        .unwrap_or_default()
        .chars()
        .count()
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
        Item::MethodAttachment(attachment) => {
            collect_expr_inline_matches(&attachment.owner, line_starts, tokens, matches);
            for member in &attachment.members {
                collect_record_entry_inline_matches(member, line_starts, tokens, matches);
            }
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

fn collect_record_entry_inline_matches(
    entry: &RecordEntry,
    line_starts: &[usize],
    tokens: &[Token],
    matches: &mut Vec<InlineMatchLayout>,
) {
    let mut collect = |expr: &Expr| {
        collect_expr_inline_matches(expr, line_starts, tokens, matches);
    };
    match entry {
        RecordEntry::Field { value, .. }
        | RecordEntry::Method { value, .. }
        | RecordEntry::Spread { value, .. }
        | RecordEntry::DeleteComputed { key: value, .. }
        | RecordEntry::Element(value) => collect(value),
        RecordEntry::FieldComputed { key, value, .. } => {
            collect(key);
            collect(value);
        }
        RecordEntry::FieldDefault {
            annotation,
            default,
            ..
        } => {
            collect(annotation);
            collect(default);
        }
        RecordEntry::Iteration {
            source,
            guard,
            body,
            ..
        } => {
            collect(source);
            if let Some(guard) = guard {
                collect(guard);
            }
            for member in body {
                collect_record_entry_inline_matches(member, line_starts, tokens, matches);
            }
        }
        RecordEntry::Shorthand { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::Rename { .. }
        | RecordEntry::Open { .. } => {}
    }
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
        || (is_tight_access_operator(current) && !is_bare_receiver_after(previous))
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

    if is_tight_access_operator(previous) && is_binary_operator(current) {
        return true;
    }

    if is_tight_access_operator(previous)
        || is_tight_prefix_operator(previous, previous_previous, Some(current))
        || is_at_set_marker(previous, Some(current))
    {
        return false;
    }

    if is_binary_operator(current)
        || is_binary_operator(previous)
        || is_infix_range_operator(current, Some(previous))
        || is_infix_range_operator(previous, previous_previous)
    {
        return true;
    }

    true
}

fn is_bare_receiver_after(previous: &Token) -> bool {
    is_binary_operator(previous) || is_separator(previous)
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

fn is_infix_range_operator(token: &Token, previous: Option<&Token>) -> bool {
    matches!(&token.kind, TokenKind::Operator(operator) if operator == "..")
        && previous.is_some_and(can_end_postfix_operand)
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
    matches!(&token.kind, TokenKind::Operator(operator) if operator == ":..")
        || matches!(&token.kind, TokenKind::Operator(operator) if operator == ".." && previous.is_none_or(|previous| !can_end_postfix_operand(previous)))
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
            | TokenKind::Number(_)
            | TokenKind::StringLiteral(_)
            | TokenKind::RegexLiteral(_)
            | TokenKind::Tag(_)
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
        Item::MethodAttachment(attachment) => {
            collect_expr_field_names(&attachment.owner, spans);
            for member in &attachment.members {
                collect_record_entry_field_names(member, spans);
            }
        }
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
            quoted,
            value,
            ..
        } => {
            // Preserve source quoting for string-literal field names so fmt
            // never rewrites `{"Yacht": v}` into `{Yacht: v}` (which would
            // change type-export semantics). Unquoted names still map through
            // so spacing/normalisation of bare identifiers is unchanged.
            if !quoted {
                spans.insert(*name_span, name.as_str());
            }
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

    #[test]
    fn keeps_null_safe_index_tight() {
        // `?[` mirrors `?.`: no space between `?` and `[`, and no space after the
        // receiver before `?`.
        let formatted = "cell = grid[r]?[c]\nch = text?[i]\n";
        assert_eq!(
            format_source("cell = grid[r] ? [c]\nch = text ? [i]\n"),
            Ok(formatted.to_owned())
        );
        assert_eq!(format_source(formatted), Ok(formatted.to_owned()));
    }

    #[test]
    fn formats_method_attachment_blocks_and_bare_receiver() {
        let formatted = concat!(
            "Array(Array(a)) {\n",
            "  flattenOne(): Array(a) =>\n",
            "    .[0]\n",
            "\n",
            "  self(): Array(Array(a)) => .\n",
            "}\n",
        );

        assert_eq!(
            format_source(
                "Array(Array(a)){\n    flattenOne():Array(a)=>\n        .[0]\n\n    self():Array(Array(a))=>.\n}\n"
            ),
            Ok(formatted.to_owned())
        );
        assert_eq!(format_source(formatted), Ok(formatted.to_owned()));
    }

    #[test]
    fn formats_slot_record_initializer_idempotently() {
        let formatted = concat!(
            "Queue = { limit: Int, display(): Text }\n",
            "queue: Queue = { limit: 2, display(): Text => \"queue of ${.limit}\" }\n",
        );

        assert_eq!(
            format_source(
                "Queue = { limit: Int, display(): Text }\nqueue:Queue={limit:2,display():Text=>\"queue of ${.limit}\"}\n"
            ),
            Ok(formatted.to_owned())
        );
        assert_eq!(format_source(formatted), Ok(formatted.to_owned()));
    }

    #[test]
    fn formats_annotationless_method_entry_without_inventing_return_type() {
        let formatted = "Csv = { csv(): Text }\nannotated: Csv = { csv() => \"a\" }\n";

        assert_eq!(
            format_source("Csv={csv():Text}\nannotated:Csv={csv()=>\"a\"}\n"),
            Ok(formatted.to_owned())
        );
        // Must not invent a `: Text` (or any) return annotation on reformat.
        assert_eq!(format_source(formatted), Ok(formatted.to_owned()));
        assert!(
            formatted.contains("csv() =>"),
            "formatter must leave the slot body annotationless: {formatted}"
        );
        assert!(
            !formatted.contains("csv(): Text =>"),
            "formatter must not invent a return annotation: {formatted}"
        );
    }

    #[test]
    fn preserves_dollar_escape_in_string_literals() {
        // `\$` is source-preserved (not rewritten to `\u{24}` or bare `$`).
        let source = r#"value = "\$" "#;
        let formatted = format_source(source).expect("format");
        assert!(
            formatted.contains(r#"\$"#),
            "formatter must preserve `\\$`: {formatted}"
        );
        assert_eq!(format_source(&formatted), Ok(formatted.clone()));

        let literal_interp = r#"value = "a\${b}" "#;
        let formatted = format_source(literal_interp).expect("format");
        assert!(
            formatted.contains(r#"\${b}"#),
            "formatter must preserve escaped interpolation: {formatted}"
        );
        assert_eq!(format_source(&formatted), Ok(formatted));
    }
}
