use aven_core::Span;

use crate::items::{MergedItem, merged_items};
use crate::parser::{Binding, Expr, ExprKind, Item, MatchArm, Module, RecordEntry};
use crate::walk::find_map_expr_children;
use crate::{Token, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingSite<'a> {
    pub name: &'a str,
    pub span: Span,
}

pub fn resolve_local_definition(module: &Module, name: &str, reference: Span) -> Option<Span> {
    let scope = scope_at_module(module, reference);

    scope
        .binder_at
        .filter(|binding| binding.name == name)
        .map(|binding| binding.span)
        .or_else(|| find_visible_binding(&scope.visible, name))
}

pub fn resolve_local_references(
    module: &Module,
    tokens: &[Token],
    name: &str,
    reference: Span,
) -> Option<Vec<Span>> {
    let definition = resolve_local_definition(module, name, reference)?;

    if is_top_level_definition(module, name, definition) {
        return None;
    }

    let references = tokens
        .iter()
        .filter_map(|token| match &token.kind {
            TokenKind::Identifier(token_name) | TokenKind::ComptimeIdentifier(token_name)
                if token_name == name
                    && resolve_local_definition(module, name, token.span) == Some(definition) =>
            {
                Some(token.span)
            }
            _ => None,
        })
        .collect();

    Some(references)
}

pub fn visible_local_bindings(module: &Module, at: Span) -> Vec<BindingSite<'_>> {
    scope_at_module(module, at).visible
}

pub fn annotation_for_definition(module: &Module, definition: Span) -> Option<&Expr> {
    annotation_for_definition_in_items(&module.items, definition)
}

/// The parameters and body of a lambda expression, looking through grouping
/// parens (`((x) => x)` resolves like `(x) => x`).
pub fn lambda_parts(expr: &Expr) -> Option<(&[crate::Param], &Expr)> {
    let mut expr = expr;
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    match &expr.kind {
        ExprKind::Lambda { params, body, .. } => Some((params, body)),
        _ => None,
    }
}

pub fn render_annotation(source: &str, annotation: &Expr) -> String {
    source
        .get(annotation.span.start..annotation.span.end)
        .unwrap_or("")
        .trim()
        .to_owned()
}

fn is_top_level_definition(module: &Module, name: &str, definition: Span) -> bool {
    module.items.iter().any(|item| match item {
        Item::Binding(binding) => binding.name == name && binding.name_span == definition,
        Item::PatternBinding(binding) => pattern_bindings(&binding.pattern)
            .into_iter()
            .any(|site| site.name == name && site.span == definition),
        Item::SpreadBinding(_) => false,
        Item::Signature(signature) => signature.name == name && signature.name_span == definition,
        Item::Expr(_) => false,
    })
}

fn annotation_for_definition_in_items(items: &[Item], definition: Span) -> Option<&Expr> {
    for item in merged_items(items) {
        match item {
            MergedItem::Binding { signature, binding } => {
                if let Some(signature) = signature
                    && signature.name_span == definition
                {
                    return Some(&signature.annotation);
                }

                if binding.name_span == definition {
                    return binding
                        .annotation
                        .as_ref()
                        .or_else(|| signature.map(|signature| &signature.annotation));
                }

                if let Some(found) = annotation_for_definition_in_binding(binding, definition) {
                    return Some(found);
                }
            }
            MergedItem::PatternBinding(binding) => {
                if let Some(found) = annotation_for_definition_in_expr(&binding.value, definition) {
                    return Some(found);
                }
            }
            MergedItem::SpreadBinding(binding) => {
                if let Some(found) = annotation_for_definition_in_expr(&binding.value, definition) {
                    return Some(found);
                }
            }
            MergedItem::Signature(signature) => {
                if signature.name_span == definition {
                    return Some(&signature.annotation);
                }
            }
            MergedItem::Expr(expr) => {
                if let Some(found) = annotation_for_definition_in_expr(expr, definition) {
                    return Some(found);
                }
            }
        }
    }

    None
}

fn annotation_for_definition_in_binding(binding: &Binding, definition: Span) -> Option<&Expr> {
    if let Some(annotation) = &binding.annotation
        && annotation.span.contains(definition)
    {
        return None;
    }

    annotation_for_definition_in_expr(&binding.value, definition)
}

fn annotation_for_definition_in_expr(expr: &Expr, definition: Span) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::Lambda { params, body, .. } => {
            if let Some(param) = params.iter().find(|param| param.name_span == definition) {
                return param.annotation.as_ref();
            }

            annotation_for_definition_in_expr(body, definition)
        }
        ExprKind::Block(items) => annotation_for_definition_in_items(items, definition),
        _ => find_map_expr_children(expr, |child| {
            annotation_for_definition_in_expr(child, definition)
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeAt<'a> {
    visible: Vec<BindingSite<'a>>,
    binder_at: Option<BindingSite<'a>>,
}

impl<'a> ScopeAt<'a> {
    fn from_visible(visible: Vec<BindingSite<'a>>) -> Self {
        Self {
            visible,
            binder_at: None,
        }
    }
}

fn scope_at_module(module: &Module, at: Span) -> ScopeAt<'_> {
    let visible = Vec::new();

    for item in &module.items {
        if let Some(found) = scope_at_item(item, at, &visible) {
            return found;
        }
    }

    ScopeAt::from_visible(visible)
}

fn scope_at_item<'a>(item: &'a Item, at: Span, outer: &[BindingSite<'a>]) -> Option<ScopeAt<'a>> {
    match item {
        Item::Binding(binding) => {
            if !binding.span.contains(at) {
                return None;
            }

            let binder_at = binding_site_at(binding.name.as_str(), binding.name_span, at);

            if let Some(annotation) = &binding.annotation
                && let Some(found) = scope_at_expr(annotation, at, outer)
            {
                return Some(found);
            }

            scope_at_expr(&binding.value, at, outer).or_else(|| {
                Some(ScopeAt {
                    visible: outer.to_vec(),
                    binder_at,
                })
            })
        }
        Item::PatternBinding(binding) => {
            if !binding.span.contains(at) {
                return None;
            }

            scope_at_expr(&binding.value, at, outer).or_else(|| {
                let binders = pattern_bindings(&binding.pattern);
                Some(ScopeAt {
                    visible: outer.to_vec(),
                    binder_at: binding_at_reference(&binders, at),
                })
            })
        }
        Item::SpreadBinding(binding) => {
            if !binding.span.contains(at) {
                return None;
            }

            scope_at_expr(&binding.value, at, outer)
                .or_else(|| Some(ScopeAt::from_visible(outer.to_vec())))
        }
        Item::Signature(signature) => {
            if !signature.span.contains(at) {
                return None;
            }

            let binder_at = binding_site_at(signature.name.as_str(), signature.name_span, at);
            scope_at_expr(&signature.annotation, at, outer).or_else(|| {
                Some(ScopeAt {
                    visible: outer.to_vec(),
                    binder_at,
                })
            })
        }
        Item::Expr(expr) => scope_at_expr(expr, at, outer),
    }
}

fn scope_at_expr<'a>(expr: &'a Expr, at: Span, outer: &[BindingSite<'a>]) -> Option<ScopeAt<'a>> {
    if !expr.span.contains(at) {
        return None;
    }

    match &expr.kind {
        ExprKind::Lambda {
            params,
            return_annotation,
            body,
            ..
        } => scope_at_lambda(params, return_annotation.as_deref(), body, at, outer),
        ExprKind::Match { subject, arms, .. } => {
            scope_at_expr(subject, at, outer).or_else(|| scope_at_match_arms(arms, at, outer))
        }
        ExprKind::Block(items) => Some(scope_at_block(items, at, outer)),
        ExprKind::Record(entries) | ExprKind::Set(entries) | ExprKind::Array(entries) => {
            scope_at_record_entries(entries, at, outer)
                .or_else(|| Some(ScopeAt::from_visible(outer.to_vec())))
        }
        _ => find_map_expr_children(expr, |child| scope_at_expr(child, at, outer))
            .or_else(|| Some(ScopeAt::from_visible(outer.to_vec()))),
    }
}

fn scope_at_lambda<'a>(
    params: &'a [crate::parser::Param],
    return_annotation: Option<&'a Expr>,
    body: &'a Expr,
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<ScopeAt<'a>> {
    if let Some(param) = params.iter().find(|param| param.name_span.contains(at)) {
        let binder = BindingSite {
            name: param.name.as_str(),
            span: param.name_span,
        };
        let mut visible = outer.to_vec();
        visible.push(binder);
        return Some(ScopeAt {
            visible,
            binder_at: Some(binder),
        });
    }

    if let Some(annotation) = return_annotation
        && let Some(found) = scope_at_expr(annotation, at, outer)
    {
        return Some(found);
    }

    if body.span.contains(at) {
        let mut visible = outer.to_vec();
        visible.extend(params.iter().map(|param| BindingSite {
            name: param.name.as_str(),
            span: param.name_span,
        }));

        return scope_at_expr(body, at, &visible).or(Some(ScopeAt::from_visible(visible)));
    }

    Some(ScopeAt::from_visible(outer.to_vec()))
}

fn scope_at_block<'a>(items: &'a [Item], at: Span, outer: &[BindingSite<'a>]) -> ScopeAt<'a> {
    let mut visible = outer.to_vec();

    for item in items {
        let span = item_span(item);

        if span.contains(at) {
            return scope_at_item(item, at, &visible)
                .unwrap_or_else(|| ScopeAt::from_visible(visible));
        }

        if span.end <= at.start {
            match item {
                Item::Binding(binding) => visible.push(BindingSite {
                    name: binding.name.as_str(),
                    span: binding.name_span,
                }),
                Item::PatternBinding(binding) => {
                    visible.extend(pattern_bindings(&binding.pattern));
                }
                Item::SpreadBinding(_) | Item::Signature(_) | Item::Expr(_) => {}
            }
        }
    }

    ScopeAt::from_visible(visible)
}

fn scope_at_match_arms<'a>(
    arms: &'a [MatchArm],
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<ScopeAt<'a>> {
    arms.iter().find_map(|arm| {
        if !arm.span.contains(at) {
            return None;
        }

        let binders = pattern_bindings(&arm.pattern);

        if arm.pattern.span.contains(at) {
            let mut visible = outer.to_vec();
            visible.extend(
                binders
                    .iter()
                    .copied()
                    .filter(|binding| binding.span.contains(at)),
            );
            return Some(ScopeAt {
                visible,
                binder_at: binding_at_reference(&binders, at),
            });
        }

        if exprs_contain(&arm.guards, at) {
            let mut visible = outer.to_vec();
            visible.extend(binders);
            return scope_at_exprs(&arm.guards, at, &visible)
                .or(Some(ScopeAt::from_visible(visible)));
        }

        if arm.body.span.contains(at) {
            let mut visible = outer.to_vec();
            visible.extend(binders);
            return scope_at_expr(&arm.body, at, &visible).or(Some(ScopeAt::from_visible(visible)));
        }

        Some(ScopeAt::from_visible(outer.to_vec()))
    })
}

fn scope_at_exprs<'a>(
    items: &'a [Expr],
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<ScopeAt<'a>> {
    items.iter().find_map(|item| scope_at_expr(item, at, outer))
}

fn scope_at_record_entries<'a>(
    entries: &'a [RecordEntry],
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<ScopeAt<'a>> {
    entries.iter().find_map(|entry| {
        if !record_entry_span(entry).contains(at) {
            return None;
        }

        match entry {
            RecordEntry::Field { value, .. }
            | RecordEntry::Spread { value, .. }
            | RecordEntry::DeleteComputed { key: value, .. }
            | RecordEntry::Element(value) => scope_at_expr(value, at, outer)
                .or_else(|| Some(ScopeAt::from_visible(outer.to_vec()))),
            RecordEntry::FieldComputed { key, value, .. } => {
                if key.span.contains(at) {
                    return scope_at_expr(key, at, outer)
                        .or_else(|| Some(ScopeAt::from_visible(outer.to_vec())));
                }
                if value.span.contains(at) {
                    return scope_at_expr(value, at, outer)
                        .or_else(|| Some(ScopeAt::from_visible(outer.to_vec())));
                }
                Some(ScopeAt::from_visible(outer.to_vec()))
            }
            RecordEntry::Iteration {
                source,
                binder,
                binder_span,
                guard,
                body,
                ..
            } => {
                if source.span.contains(at) {
                    return scope_at_expr(source, at, outer)
                        .or_else(|| Some(ScopeAt::from_visible(outer.to_vec())));
                }

                let binder_site = BindingSite {
                    name: binder.as_str(),
                    span: *binder_span,
                };
                let mut visible = outer.to_vec();
                visible.push(binder_site);

                if binder_span.contains(at) {
                    return Some(ScopeAt {
                        visible,
                        binder_at: Some(binder_site),
                    });
                }

                if let Some(guard) = guard
                    && guard.span.contains(at)
                {
                    return scope_at_expr(guard, at, &visible)
                        .or_else(|| Some(ScopeAt::from_visible(visible)));
                }

                if body
                    .iter()
                    .any(|entry| record_entry_span(entry).contains(at))
                {
                    return scope_at_record_entries(body, at, &visible)
                        .or_else(|| Some(ScopeAt::from_visible(visible)));
                }

                Some(ScopeAt::from_visible(outer.to_vec()))
            }
            RecordEntry::Shorthand { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Open { .. } => Some(ScopeAt::from_visible(outer.to_vec())),
        }
    })
}

fn item_span(item: &Item) -> Span {
    match item {
        Item::Binding(binding) => binding.span,
        Item::PatternBinding(binding) => binding.span,
        Item::SpreadBinding(binding) => binding.span,
        Item::Signature(signature) => signature.span,
        Item::Expr(expr) => expr.span,
    }
}

fn binding_site_at<'a>(name: &'a str, span: Span, at: Span) -> Option<BindingSite<'a>> {
    span.contains(at).then_some(BindingSite { name, span })
}

fn binding_at_reference<'a>(
    bindings: &[BindingSite<'a>],
    reference: Span,
) -> Option<BindingSite<'a>> {
    bindings
        .iter()
        .rev()
        .find(|binding| binding.span.contains(reference))
        .copied()
}

fn record_entry_span(entry: &RecordEntry) -> Span {
    match entry {
        RecordEntry::Field { span, .. }
        | RecordEntry::FieldComputed { span, .. }
        | RecordEntry::Shorthand { span, .. }
        | RecordEntry::Spread { span, .. }
        | RecordEntry::Delete { span, .. }
        | RecordEntry::DeleteComputed { span, .. }
        | RecordEntry::Rename { span, .. }
        | RecordEntry::Iteration { span, .. }
        | RecordEntry::Open { span } => *span,
        RecordEntry::Element(expr) => expr.span,
    }
}

fn exprs_contain(items: &[Expr], reference: Span) -> bool {
    items.iter().any(|item| item.span.contains(reference))
}

/// The specifier of a static `import("...")` call, when `expr` is one. This is
/// the single definition of "a static import" shared by the checker, the
/// module-graph driver, and tooling — extend it here if the recognized shape
/// ever widens (e.g. comptime-known non-literal specifiers).
pub fn static_import_specifier(expr: &Expr) -> Option<String> {
    let ExprKind::Call { callee, args } = &expr.kind else {
        return None;
    };
    if !matches!(&callee.kind, ExprKind::Name(name) if name == "import") {
        return None;
    }
    let ExprKind::Literal(crate::Literal::String(raw)) = &args.first()?.kind else {
        return None;
    };
    Some(crate::decode_string_literal(raw))
}

pub fn pattern_bindings(pattern: &Expr) -> Vec<BindingSite<'_>> {
    let mut bindings = Vec::new();
    collect_pattern_bindings(pattern, &mut bindings);
    bindings
}

fn collect_pattern_bindings<'a>(pattern: &'a Expr, bindings: &mut Vec<BindingSite<'a>>) {
    // Patterns are parsed as ordinary expressions, so this walk stays total
    // over `ExprKind`; later semantic validation decides which shapes are legal.
    match &pattern.kind {
        ExprKind::Name(name) if name != "_" => bindings.push(BindingSite {
            name,
            span: pattern.span,
        }),
        ExprKind::Binary { operator, .. } if operator == "|" => {
            collect_or_pattern_bindings(pattern, bindings);
        }
        ExprKind::Record(entries) | ExprKind::Set(entries) | ExprKind::Array(entries) => {
            collect_pattern_bindings_from_record_entries(entries, bindings);
        }
        ExprKind::Index { callee, args } | ExprKind::Call { callee, args } => {
            if !matches!(callee.kind, ExprKind::Tag(_)) {
                collect_pattern_bindings(callee, bindings);
            }
            collect_pattern_bindings_from_exprs(args, bindings);
        }
        ExprKind::Lambda { .. } => {}
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Undefined
        | ExprKind::Null
        | ExprKind::Name(_)
        | ExprKind::ComptimeName(_)
        | ExprKind::Tag(_) => {}
        _ => {
            crate::walk::walk_expr_children(pattern, &mut |child| {
                collect_pattern_bindings(child, bindings);
            });
        }
    }
}

fn collect_or_pattern_bindings<'a>(pattern: &'a Expr, bindings: &mut Vec<BindingSite<'a>>) {
    let mut alternatives = Vec::new();
    collect_or_pattern_alternatives(pattern, &mut alternatives);

    let mut names = Vec::new();
    for alternative in alternatives {
        let mut alternative_bindings = Vec::new();
        collect_pattern_bindings(alternative, &mut alternative_bindings);

        let mut names_in_alternative = Vec::new();
        for binding in alternative_bindings {
            if names_in_alternative.contains(&binding.name) || !names.contains(&binding.name) {
                bindings.push(binding);
            }
            if !names_in_alternative.contains(&binding.name) {
                names_in_alternative.push(binding.name);
            }
            if !names.contains(&binding.name) {
                names.push(binding.name);
            }
        }
    }
}

fn collect_or_pattern_alternatives<'a>(pattern: &'a Expr, alternatives: &mut Vec<&'a Expr>) {
    match &pattern.kind {
        ExprKind::Group(inner) => collect_or_pattern_alternatives(inner, alternatives),
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } if operator == "|" => {
            collect_or_pattern_alternatives(left, alternatives);
            collect_or_pattern_alternatives(right, alternatives);
        }
        _ => alternatives.push(pattern),
    }
}

fn collect_pattern_bindings_from_exprs<'a>(items: &'a [Expr], bindings: &mut Vec<BindingSite<'a>>) {
    for item in items {
        collect_pattern_bindings(item, bindings);
    }
}

fn collect_pattern_bindings_from_record_entries<'a>(
    entries: &'a [RecordEntry],
    bindings: &mut Vec<BindingSite<'a>>,
) {
    for entry in entries {
        match entry {
            RecordEntry::Field { value, .. }
            | RecordEntry::FieldComputed { value, .. }
            | RecordEntry::Element(value) => {
                collect_pattern_bindings(value, bindings);
            }
            RecordEntry::Shorthand {
                name, name_span, ..
            } => bindings.push(BindingSite {
                name,
                span: *name_span,
            }),
            RecordEntry::Spread { value, .. } => {
                if let ExprKind::Name(name) = &value.kind
                    && name != "_"
                {
                    bindings.push(BindingSite {
                        name,
                        span: value.span,
                    });
                }
            }
            RecordEntry::Rename { to, to_span, .. } => bindings.push(BindingSite {
                name: to,
                span: *to_span,
            }),
            RecordEntry::Iteration { body, .. } => {
                collect_pattern_bindings_from_record_entries(body, bindings);
            }
            RecordEntry::Delete { .. }
            | RecordEntry::DeleteComputed { .. }
            | RecordEntry::Open { .. } => {}
        }
    }
}

fn find_visible_binding(bindings: &[BindingSite<'_>], name: &str) -> Option<Span> {
    bindings
        .iter()
        .rev()
        .find(|binding| binding.name == name)
        .map(|binding| binding.span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Item, parse_module};

    #[test]
    fn resolves_lambda_parameters_before_top_level_bindings() {
        let source = "x = 1\nf = (x) => x\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "x", nth_span(source, "x", 2));

        assert_eq!(span, Some(nth_span(source, "x", 1)));
    }

    #[test]
    fn resolves_the_nearest_lambda_parameter() {
        let source = "x = 1\nf = (x) => (x) => x\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "x", nth_span(source, "x", 3));

        assert_eq!(span, Some(nth_span(source, "x", 2)));
    }

    #[test]
    fn ignores_top_level_bindings() {
        let source = "x = 1\nvalue = x\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "x", nth_span(source, "x", 1));

        assert_eq!(span, None);
    }

    #[test]
    fn resolves_previous_block_bindings() {
        let source = "f = () =>\n  x = 1\n  y = x\n  y\n";
        let output = parse_module(source);

        let x_span = resolve_local_definition(&output.module, "x", nth_span(source, "x", 1));
        let y_span = resolve_local_definition(&output.module, "y", nth_span(source, "y", 1));

        assert_eq!(x_span, Some(nth_span(source, "x", 0)));
        assert_eq!(y_span, Some(nth_span(source, "y", 0)));
    }

    #[test]
    fn does_not_resolve_block_binding_inside_its_own_value() {
        let source = "f = () =>\n  x = x\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "x", nth_span(source, "x", 1));

        assert_eq!(span, None);
    }

    #[test]
    fn resolves_block_bindings_inside_nested_lambdas() {
        let source = "f = () =>\n  x = 1\n  g = () => x\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "x", nth_span(source, "x", 1));

        assert_eq!(span, Some(nth_span(source, "x", 0)));
    }

    #[test]
    fn resolves_constructor_pattern_binders_in_match_bodies() {
        let source = "f = (result) =>\n  result ?>\n    @Ok(value) => value\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "value", nth_span(source, "value", 1));

        assert_eq!(span, Some(nth_span(source, "value", 0)));
    }

    #[test]
    fn resolves_pattern_binders_in_match_guards() {
        let source = "f = (result) =>\n  result ?>\n    @Ok(value), value > 0 => value\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "value", nth_span(source, "value", 1));

        assert_eq!(span, Some(nth_span(source, "value", 0)));
    }

    #[test]
    fn resolves_record_pattern_binders() {
        let source = "f = (user) =>\n  user ?>\n    { name, age -> years } => name + years\n";
        let output = parse_module(source);

        let name_span =
            resolve_local_definition(&output.module, "name", nth_span(source, "name", 1));
        let years_span =
            resolve_local_definition(&output.module, "years", nth_span(source, "years", 1));

        assert_eq!(name_span, Some(nth_span(source, "name", 0)));
        assert_eq!(years_span, Some(nth_span(source, "years", 0)));
    }

    #[test]
    fn resolves_record_iteration_binder_in_body() {
        let source = "f = (keys, o) => { keys -> k; (k, o[k]) }\n";
        let output = parse_module(source);

        let tuple_key = resolve_local_definition(&output.module, "k", nth_span(source, "k", 3));
        let index_key = resolve_local_definition(&output.module, "k", nth_span(source, "k", 4));

        assert_eq!(tuple_key, Some(nth_span(source, "k", 2)));
        assert_eq!(index_key, Some(nth_span(source, "k", 2)));
    }

    #[test]
    fn resolves_rest_pattern_binders() {
        let source = "f = (user) =>\n  user ?>\n    { ..rest } => rest\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "rest", nth_span(source, "rest", 1));

        assert_eq!(span, Some(nth_span(source, "rest", 0)));
    }

    #[test]
    fn local_references_include_definition_and_uses() {
        let source = "x = 1\nf = (x) => (x) => x\n";
        let output = parse_module(source);
        let spans = resolve_local_references(
            &output.module,
            &output.raw_tokens,
            "x",
            nth_span(source, "x", 3),
        );

        assert_eq!(
            spans,
            Some(vec![nth_span(source, "x", 2), nth_span(source, "x", 3)])
        );
    }

    #[test]
    fn local_references_skip_top_level_declarations() {
        let source = "x = 1\nvalue = x\n";
        let output = parse_module(source);
        let spans = resolve_local_references(
            &output.module,
            &output.raw_tokens,
            "x",
            nth_span(source, "x", 0),
        );

        assert_eq!(spans, None);
    }

    #[test]
    fn local_definition_matches_visible_stack_for_reference_sites() {
        let cases = [
            ("x = 1\nf = (x) => x\n", "x", 2),
            ("x = 1\nvalue = x\n", "x", 1),
            ("f = () =>\n  x = 1\n  y = x\n", "x", 1),
            (
                "f = (result) =>\n  result ?>\n    @Ok(value), value > 0 => value\n",
                "value",
                1,
            ),
            (
                "f = (result) =>\n  result ?>\n    @Ok(value), value > 0 => value\n",
                "value",
                2,
            ),
        ];

        for (source, name, occurrence) in cases {
            let output = parse_module(source);
            let reference = nth_span(source, name, occurrence);
            let expected = visible_local_bindings(&output.module, reference)
                .iter()
                .rev()
                .find(|binding| binding.name == name)
                .map(|binding| binding.span);

            assert_eq!(
                resolve_local_definition(&output.module, name, reference),
                expected
            );
        }
    }

    #[test]
    fn pattern_bindings_extracts_reusable_binder_sites() {
        let source = "pattern = { name, age -> years, ..rest }\n";
        let output = parse_module(source);
        let Item::Binding(binding) = &output.module.items[0] else {
            panic!("expected binding");
        };

        let bindings: Vec<_> = pattern_bindings(&binding.value)
            .into_iter()
            .map(|binding| (binding.name, binding.span))
            .collect();

        assert_eq!(
            bindings,
            vec![
                ("name", nth_span(source, "name", 0)),
                ("years", nth_span(source, "years", 0)),
                ("rest", nth_span(source, "rest", 0)),
            ]
        );
    }

    #[test]
    fn visible_local_bindings_include_lambda_parameters() {
        let source = "f = (x) => x\n";
        let output = parse_module(source);
        let bindings = visible_binding_pairs(&output.module, nth_span(source, "x", 1));

        assert_eq!(bindings, vec![("x", nth_span(source, "x", 0))]);
    }

    #[test]
    fn visible_local_bindings_include_previous_block_bindings() {
        let source = "f = () =>\n  x = 1\n  y = x\n";
        let output = parse_module(source);
        let bindings = visible_binding_pairs(&output.module, nth_span(source, "x", 1));

        assert_eq!(bindings, vec![("x", nth_span(source, "x", 0))]);
    }

    #[test]
    fn visible_local_bindings_exclude_later_block_bindings() {
        let source = "f = () =>\n  x = y\n  y = 1\n";
        let output = parse_module(source);
        let bindings = visible_binding_pairs(&output.module, nth_span(source, "y", 0));

        assert!(bindings.is_empty());
    }

    #[test]
    fn visible_local_bindings_include_match_pattern_binders_in_body() {
        let source = "f = (result) =>\n  result ?>\n    @Ok(value) => value\n";
        let output = parse_module(source);
        let bindings = visible_binding_pairs(&output.module, nth_span(source, "value", 1));

        assert_eq!(
            bindings,
            vec![
                ("result", nth_span(source, "result", 0)),
                ("value", nth_span(source, "value", 0)),
            ]
        );
    }

    #[test]
    fn finds_signature_annotation_for_binding_definition() {
        let source = "double : (Int) -> Int\ndouble = (value) => value\n";
        let output = parse_module(source);
        let Item::Binding(binding) = &output.module.items[1] else {
            panic!("expected binding");
        };
        let Some(annotation) = annotation_for_definition(&output.module, binding.name_span) else {
            panic!("expected annotation");
        };

        assert_eq!(render_annotation(source, annotation), "(Int) -> Int");
    }

    #[test]
    fn finds_lambda_parameter_annotations() {
        let source = "id = (value : Text) => value\n";
        let output = parse_module(source);
        let span = Span::new(6, 11);
        let Some(annotation) = annotation_for_definition(&output.module, span) else {
            panic!("expected annotation");
        };

        assert_eq!(render_annotation(source, annotation), "Text");
    }

    fn nth_span(source: &str, needle: &str, occurrence: usize) -> Span {
        let Some((start, _)) = source.match_indices(needle).nth(occurrence) else {
            panic!("expected occurrence {occurrence} of {needle:?}");
        };

        Span::new(start, start + needle.len())
    }

    fn visible_binding_pairs(module: &Module, at: Span) -> Vec<(&str, Span)> {
        visible_local_bindings(module, at)
            .into_iter()
            .map(|binding| (binding.name, binding.span))
            .collect()
    }
}
