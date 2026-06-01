use aven_core::Span;

use crate::parser::{Expr, ExprKind, Item, MatchArm, Module, RecordEntry};
use crate::walk::find_map_expr_children;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BindingSite<'a> {
    pub(crate) name: &'a str,
    pub(crate) span: Span,
}

pub fn resolve_local_definition(module: &Module, name: &str, reference: Span) -> Option<Span> {
    module
        .items
        .iter()
        .find_map(|item| resolve_in_item(item, name, reference))
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
        | ExprKind::ComptimeName(_) => None,
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

pub(crate) fn pattern_bindings(pattern: &Expr) -> Vec<BindingSite<'_>> {
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
            // `Ok(value)` treats uppercase `Ok` as a constructor tag, not a
            // binder. A lowercase callee is still collected so semantic
            // diagnostics can reject or interpret it later.
            if !matches!(callee.kind, ExprKind::ComptimeName(_)) {
                collect_pattern_bindings(callee, bindings);
            }
            collect_pattern_bindings_from_exprs(args, bindings);
        }
        ExprKind::Lambda { .. } => {}
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Name(_)
        | ExprKind::ComptimeName(_) => {}
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
        let source = "f = (result) =>\n  result ?>\n    Ok(value) => value\n";
        let output = parse_module(source);
        let span = resolve_local_definition(&output.module, "value", nth_span(source, "value", 1));

        assert_eq!(span, Some(nth_span(source, "value", 0)));
    }

    #[test]
    fn resolves_pattern_binders_in_match_guards() {
        let source = "f = (result) =>\n  result ?>\n    Ok(value), value > 0 => value\n";
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

    fn nth_span(source: &str, needle: &str, occurrence: usize) -> Span {
        let Some((start, _)) = source.match_indices(needle).nth(occurrence) else {
            panic!("expected occurrence {occurrence} of {needle:?}");
        };

        Span::new(start, start + needle.len())
    }
}
