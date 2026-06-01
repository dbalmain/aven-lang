use aven_core::Span;

use crate::parser::{Expr, ExprKind, Item, Module, RecordEntry};

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
        ExprKind::Group(inner)
        | ExprKind::Nullable(inner)
        | ExprKind::Unary { value: inner, .. }
        | ExprKind::Propagate { value: inner, .. } => resolve_in_expr(inner, name, reference),
        ExprKind::Tuple(items) | ExprKind::Array(items) => resolve_in_exprs(items, name, reference),
        ExprKind::Record(entries) | ExprKind::Set(entries) => {
            resolve_in_record_entries(entries, name, reference)
        }
        ExprKind::Index { callee, args } | ExprKind::Call { callee, args } => {
            resolve_in_expr(callee, name, reference)
                .or_else(|| resolve_in_exprs(args, name, reference))
        }
        ExprKind::Arrow { params, result } => resolve_in_exprs(params, name, reference)
            .or_else(|| resolve_in_expr(result, name, reference)),
        ExprKind::FieldAccess { receiver, .. } => resolve_in_expr(receiver, name, reference),
        ExprKind::Binary { left, right, .. } => resolve_in_expr(left, name, reference)
            .or_else(|| resolve_in_expr(right, name, reference)),
        ExprKind::Match { subject, arms, .. } => {
            resolve_in_expr(subject, name, reference).or_else(|| {
                arms.iter().find_map(|arm| {
                    resolve_in_expr(&arm.pattern, name, reference)
                        .or_else(|| resolve_in_exprs(&arm.guards, name, reference))
                        .or_else(|| resolve_in_expr(&arm.body, name, reference))
                })
            })
        }
        ExprKind::Block(items) => items
            .iter()
            .find_map(|item| resolve_in_item(item, name, reference)),
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Name(_)
        | ExprKind::ComptimeName(_) => None,
    }
}

fn resolve_in_exprs(items: &[Expr], name: &str, reference: Span) -> Option<Span> {
    items
        .iter()
        .find_map(|item| resolve_in_expr(item, name, reference))
}

fn resolve_in_record_entries(entries: &[RecordEntry], name: &str, reference: Span) -> Option<Span> {
    entries.iter().find_map(|entry| match entry {
        RecordEntry::Field { value, .. }
        | RecordEntry::Spread { value, .. }
        | RecordEntry::Element(value) => resolve_in_expr(value, name, reference),
        RecordEntry::Shorthand { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::Rename { .. }
        | RecordEntry::Open { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_module;

    #[test]
    fn resolves_lambda_parameters_before_top_level_bindings() {
        let output = parse_module("x = 1\nf = (x) => x\n");
        let span = resolve_local_definition(&output.module, "x", Span::new(17, 18));

        assert_eq!(span, Some(Span::new(11, 12)));
    }

    #[test]
    fn resolves_the_nearest_lambda_parameter() {
        let output = parse_module("x = 1\nf = (x) => (x) => x\n");
        let span = resolve_local_definition(&output.module, "x", Span::new(24, 25));

        assert_eq!(span, Some(Span::new(18, 19)));
    }

    #[test]
    fn ignores_top_level_bindings() {
        let output = parse_module("x = 1\nvalue = x\n");
        let span = resolve_local_definition(&output.module, "x", Span::new(14, 15));

        assert_eq!(span, None);
    }
}
