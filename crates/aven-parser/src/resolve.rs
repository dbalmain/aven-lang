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
    module
        .items
        .iter()
        .find_map(|item| resolve_in_item(item, name, reference))
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
    let visible = Vec::new();

    for item in &module.items {
        if let Some(found) = visible_bindings_in_item(item, at, &visible) {
            return found;
        }
    }

    visible
}

pub fn annotation_for_definition(module: &Module, definition: Span) -> Option<&Expr> {
    annotation_for_definition_in_items(&module.items, definition)
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

fn resolve_in_item(item: &Item, name: &str, reference: Span) -> Option<Span> {
    match item {
        Item::Binding(binding) => {
            if !binding.span.contains(reference) {
                return None;
            }

            if binding.name == name && binding.name_span.contains(reference) {
                return Some(binding.name_span);
            }

            binding
                .annotation
                .as_ref()
                .and_then(|annotation| resolve_in_expr(annotation, name, reference))
                .or_else(|| resolve_in_expr(&binding.value, name, reference))
        }
        Item::Signature(signature) => {
            if !signature.span.contains(reference) {
                return None;
            }

            if signature.name == name && signature.name_span.contains(reference) {
                return Some(signature.name_span);
            }

            resolve_in_expr(&signature.annotation, name, reference)
        }
        Item::Expr(expr) => resolve_in_expr(expr, name, reference),
    }
}

fn resolve_in_expr(expr: &Expr, name: &str, reference: Span) -> Option<Span> {
    if !expr.span.contains(reference) {
        return None;
    }

    match &expr.kind {
        ExprKind::Lambda {
            params,
            return_annotation,
            body,
        } => {
            if let Some(param) = params
                .iter()
                .find(|param| param.name == name && param.name_span.contains(reference))
            {
                return Some(param.name_span);
            }

            if let Some(annotation) = return_annotation
                && let Some(found) = resolve_in_expr(annotation, name, reference)
            {
                return Some(found);
            }

            if let Some(found) = resolve_in_expr(body, name, reference) {
                return Some(found);
            }

            if body.span.contains(reference) {
                return params
                    .iter()
                    .find(|param| param.name == name)
                    .map(|param| param.name_span);
            }

            None
        }
        ExprKind::Match { subject, arms, .. } => resolve_in_match(subject, arms, name, reference),
        ExprKind::Block(items) => resolve_in_block(items, name, reference),
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Name(_)
        | ExprKind::ComptimeName(_)
        | ExprKind::Tag(_) => None,
        _ => find_map_expr_children(expr, |child| resolve_in_expr(child, name, reference)),
    }
}

fn resolve_in_block(items: &[Item], name: &str, reference: Span) -> Option<Span> {
    let mut visible = Vec::new();

    for item in items {
        let span = item_span(item);

        if span.contains(reference) {
            return resolve_in_item(item, name, reference)
                .or_else(|| find_visible_binding(&visible, name));
        }

        if span.end <= reference.start
            && let Item::Binding(binding) = item
        {
            visible.push(BindingSite {
                name: &binding.name,
                span: binding.name_span,
            });
        }
    }

    None
}

fn item_span(item: &Item) -> Span {
    match item {
        Item::Binding(binding) => binding.span,
        Item::Signature(signature) => signature.span,
        Item::Expr(expr) => expr.span,
    }
}

fn resolve_in_match(
    subject: &Expr,
    arms: &[MatchArm],
    name: &str,
    reference: Span,
) -> Option<Span> {
    if let Some(found) = resolve_in_expr(subject, name, reference) {
        return Some(found);
    }

    arms.iter().find_map(|arm| {
        if !arm.span.contains(reference) {
            return None;
        }

        let binders = pattern_bindings(&arm.pattern);

        if arm.pattern.span.contains(reference) {
            return find_binding_at_reference(&binders, name, reference);
        }

        if let Some(found) = resolve_in_exprs(&arm.guards, name, reference) {
            return Some(found);
        }

        if exprs_contain(&arm.guards, reference) {
            return find_visible_binding(&binders, name);
        }

        if let Some(found) = resolve_in_expr(&arm.body, name, reference) {
            return Some(found);
        }

        if arm.body.span.contains(reference) {
            return find_visible_binding(&binders, name);
        }

        None
    })
}

fn resolve_in_exprs(items: &[Expr], name: &str, reference: Span) -> Option<Span> {
    items
        .iter()
        .find_map(|item| resolve_in_expr(item, name, reference))
}

fn exprs_contain(items: &[Expr], reference: Span) -> bool {
    items.iter().any(|item| item.span.contains(reference))
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
        ExprKind::Record(entries) | ExprKind::Set(entries) => {
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
            RecordEntry::Field { value, .. } | RecordEntry::Element(value) => {
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
            RecordEntry::Delete { .. } | RecordEntry::Open { .. } => {}
        }
    }
}

fn find_binding_at_reference(
    bindings: &[BindingSite<'_>],
    name: &str,
    reference: Span,
) -> Option<Span> {
    bindings
        .iter()
        .rev()
        .find(|binding| binding.name == name && binding.span.contains(reference))
        .map(|binding| binding.span)
}

fn find_visible_binding(bindings: &[BindingSite<'_>], name: &str) -> Option<Span> {
    bindings
        .iter()
        .rev()
        .find(|binding| binding.name == name)
        .map(|binding| binding.span)
}

fn visible_bindings_in_item<'a>(
    item: &'a Item,
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<Vec<BindingSite<'a>>> {
    match item {
        Item::Binding(binding) => {
            if !binding.span.contains(at) {
                return None;
            }

            if let Some(annotation) = &binding.annotation
                && let Some(found) = visible_bindings_in_expr(annotation, at, outer)
            {
                return Some(found);
            }

            visible_bindings_in_expr(&binding.value, at, outer).or_else(|| Some(outer.to_vec()))
        }
        Item::Signature(signature) => {
            if !signature.span.contains(at) {
                return None;
            }

            visible_bindings_in_expr(&signature.annotation, at, outer)
                .or_else(|| Some(outer.to_vec()))
        }
        Item::Expr(expr) => visible_bindings_in_expr(expr, at, outer),
    }
}

fn visible_bindings_in_expr<'a>(
    expr: &'a Expr,
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<Vec<BindingSite<'a>>> {
    if !expr.span.contains(at) {
        return None;
    }

    match &expr.kind {
        ExprKind::Lambda {
            params,
            return_annotation,
            body,
        } => {
            if let Some(param) = params.iter().find(|param| param.name_span.contains(at)) {
                let mut visible = outer.to_vec();
                visible.push(BindingSite {
                    name: &param.name,
                    span: param.name_span,
                });
                return Some(visible);
            }

            if let Some(annotation) = return_annotation
                && let Some(found) = visible_bindings_in_expr(annotation, at, outer)
            {
                return Some(found);
            }

            if body.span.contains(at) {
                let mut visible = outer.to_vec();
                visible.extend(params.iter().map(|param| BindingSite {
                    name: param.name.as_str(),
                    span: param.name_span,
                }));

                return visible_bindings_in_expr(body, at, &visible).or(Some(visible));
            }

            Some(outer.to_vec())
        }
        ExprKind::Match { subject, arms, .. } => visible_bindings_in_expr(subject, at, outer)
            .or_else(|| visible_bindings_in_match_arms(arms, at, outer)),
        ExprKind::Block(items) => visible_bindings_in_block(items, at, outer),
        _ => find_map_expr_children(expr, |child| visible_bindings_in_expr(child, at, outer))
            .or_else(|| Some(outer.to_vec())),
    }
}

fn visible_bindings_in_block<'a>(
    items: &'a [Item],
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<Vec<BindingSite<'a>>> {
    let mut visible = outer.to_vec();

    for item in items {
        let span = item_span(item);

        if span.contains(at) {
            return visible_bindings_in_item(item, at, &visible).or(Some(visible));
        }

        if span.end <= at.start
            && let Item::Binding(binding) = item
        {
            visible.push(BindingSite {
                name: &binding.name,
                span: binding.name_span,
            });
        }
    }

    Some(visible)
}

fn visible_bindings_in_match_arms<'a>(
    arms: &'a [MatchArm],
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<Vec<BindingSite<'a>>> {
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
            return Some(visible);
        }

        if exprs_contain(&arm.guards, at) {
            let mut visible = outer.to_vec();
            visible.extend(binders);
            return resolve_visible_bindings_in_exprs(&arm.guards, at, &visible).or(Some(visible));
        }

        if arm.body.span.contains(at) {
            let mut visible = outer.to_vec();
            visible.extend(binders);
            return visible_bindings_in_expr(&arm.body, at, &visible).or(Some(visible));
        }

        Some(outer.to_vec())
    })
}

fn resolve_visible_bindings_in_exprs<'a>(
    items: &'a [Expr],
    at: Span,
    outer: &[BindingSite<'a>],
) -> Option<Vec<BindingSite<'a>>> {
    items
        .iter()
        .find_map(|item| visible_bindings_in_expr(item, at, outer))
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
