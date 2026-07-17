use crate::parser::{Expr, ExprKind, InterpolationSegment, Item, RecordEntry};

pub fn walk_expr_children<'a>(expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    match &expr.kind {
        ExprKind::Group(inner)
        | ExprKind::Optional(inner)
        | ExprKind::Nullable(inner)
        | ExprKind::NonNull(inner)
        | ExprKind::Unary { value: inner, .. }
        | ExprKind::Propagate { value: inner, .. } => visit(inner),
        ExprKind::Tuple(items) => walk_exprs(items, visit),
        ExprKind::Interpolation(segments) => {
            for segment in segments {
                if let InterpolationSegment::Expr(expr) = segment {
                    visit(expr);
                }
            }
        }
        ExprKind::Record(entries) | ExprKind::Set(entries) | ExprKind::Array(entries) => {
            walk_record_entry_exprs(entries, visit);
        }
        ExprKind::Index { callee, args } | ExprKind::Call { callee, args } => {
            visit(callee);
            walk_exprs(args, visit);
        }
        ExprKind::Arrow { params, result } => {
            walk_exprs(params, visit);
            visit(result);
        }
        ExprKind::FieldAccess { receiver, .. } => visit(receiver),
        ExprKind::Binary { left, right, .. } => {
            visit(left);
            visit(right);
        }
        ExprKind::Match { subject, arms, .. } => {
            visit(subject);
            for arm in arms {
                visit(&arm.pattern);
                walk_exprs(&arm.guards, visit);
                visit(&arm.body);
            }
        }
        ExprKind::Lambda {
            params,
            return_annotation,
            requirements,
            body,
        } => {
            for param in params {
                if let Some(annotation) = &param.annotation {
                    visit(annotation);
                }
                if let Some(default) = &param.default {
                    visit(default);
                }
            }
            if let Some(annotation) = return_annotation {
                visit(annotation);
            }
            for requirement in requirements {
                visit(&requirement.bound);
            }
            visit(body);
        }
        ExprKind::Block(items) => {
            for item in items {
                match item {
                    Item::Binding(binding) => {
                        if let Some(annotation) = &binding.annotation {
                            visit(annotation);
                        }
                        visit(&binding.value);
                    }
                    Item::PatternBinding(binding) => {
                        visit(&binding.pattern);
                        visit(&binding.value);
                    }
                    Item::SpreadBinding(binding) => visit(&binding.value),
                    Item::Signature(signature) => visit(&signature.annotation),
                    Item::Expr(expr) => visit(expr),
                }
            }
        }
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Undefined
        | ExprKind::Null
        | ExprKind::Name(_)
        | ExprKind::ComptimeName(_)
        | ExprKind::Tag(_) => {}
    }
}

pub fn find_map_expr_children<'a, T>(
    expr: &'a Expr,
    mut find: impl FnMut(&'a Expr) -> Option<T>,
) -> Option<T> {
    let mut found = None;
    walk_expr_children(expr, &mut |child| {
        if found.is_none() {
            found = find(child);
        }
    });
    found
}

fn walk_exprs<'a>(items: &'a [Expr], visit: &mut impl FnMut(&'a Expr)) {
    for item in items {
        visit(item);
    }
}

fn walk_record_entry_exprs<'a>(entries: &'a [RecordEntry], visit: &mut impl FnMut(&'a Expr)) {
    for entry in entries {
        match entry {
            RecordEntry::Field { value, .. }
            | RecordEntry::Method { value, .. }
            | RecordEntry::Spread { value, .. }
            | RecordEntry::DeleteComputed { key: value, .. }
            | RecordEntry::Element(value) => visit(value),
            RecordEntry::FieldComputed { key, value, .. } => {
                visit(key);
                visit(value);
            }
            RecordEntry::FieldDefault {
                annotation,
                default,
                ..
            } => {
                visit(annotation);
                visit(default);
            }
            RecordEntry::Iteration {
                source,
                guard,
                body,
                ..
            } => {
                visit(source);
                if let Some(guard) = guard {
                    visit(guard);
                }
                walk_record_entry_exprs(body, visit);
            }
            RecordEntry::Shorthand { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Open { .. } => {}
        }
    }
}
