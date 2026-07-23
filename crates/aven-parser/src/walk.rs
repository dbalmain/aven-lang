use aven_core::Span;

use crate::parser::{Expr, ExprKind, InterpolationSegment, Item, RecordEntry};
use crate::resolve::pattern_bindings;

/// How a binder name is introduced in the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinderRole {
    /// `name = value` (top-level, block, or nested).
    Binding,
    /// Pattern binding site (`{ x } = ...`) or match-arm pattern binder.
    Pattern,
    /// Lambda parameter.
    Parameter,
    /// Record comprehension iteration binder (`for x in ...`).
    Iteration,
}

/// A name span that introduces a binder, shared by LSP semantic tokens, inlays,
/// and any other consumer that must not miss a binder form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinderSite<'a> {
    pub name: &'a str,
    pub span: Span,
    pub role: BinderRole,
    /// True when this binding's value is written as a lambda. Only meaningful
    /// for [`BinderRole::Binding`]; other roles leave this `false`.
    pub is_callable: bool,
}

/// Visit every binder-introducing name span under `items` (and nested exprs).
///
/// Covers ordinary bindings, pattern bindings, lambda parameters, match-arm
/// pattern binders, and record-comprehension iteration binders. Signature
/// names are intentionally omitted — they are declaration labels, not value
/// binders (inlay and local-rename treat them that way).
pub fn walk_binder_sites_in_items<'a>(items: &'a [Item], visit: &mut impl FnMut(BinderSite<'a>)) {
    for item in items {
        walk_binder_sites_in_item(item, visit);
    }
}

/// Visit every binder-introducing name span under `expr`.
pub fn walk_binder_sites_in_expr<'a>(expr: &'a Expr, visit: &mut impl FnMut(BinderSite<'a>)) {
    match &expr.kind {
        ExprKind::Lambda { params, .. } => {
            for param in params {
                visit(BinderSite {
                    name: param.name.as_str(),
                    span: param.name_span,
                    role: BinderRole::Parameter,
                    is_callable: false,
                });
            }
            // Child exprs (annotations, defaults, requirements, body) via the
            // shared structural walker so Lambda fields are not listed twice.
            walk_expr_children(expr, &mut |child| walk_binder_sites_in_expr(child, visit));
        }
        ExprKind::Block(items) => walk_binder_sites_in_items(items, visit),
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                for site in pattern_bindings(&arm.pattern) {
                    visit(BinderSite {
                        name: site.name,
                        span: site.span,
                        role: BinderRole::Pattern,
                        is_callable: false,
                    });
                }
            }
            // subject, arm patterns/guards/bodies — patterns introduce no extra
            // binder sites beyond what pattern_bindings already yielded.
            walk_expr_children(expr, &mut |child| walk_binder_sites_in_expr(child, visit));
        }
        ExprKind::Record(entries) | ExprKind::Set(entries) | ExprKind::Array(entries) => {
            walk_binder_sites_in_record_entries(entries, visit);
        }
        ExprKind::PrimitiveFamily { base, members } => {
            walk_binder_sites_in_expr(base, visit);
            walk_binder_sites_in_record_entries(members, visit);
        }
        _ => walk_expr_children(expr, &mut |child| walk_binder_sites_in_expr(child, visit)),
    }
}

fn walk_binder_sites_in_item<'a>(item: &'a Item, visit: &mut impl FnMut(BinderSite<'a>)) {
    match item {
        Item::Binding(binding) => {
            visit(BinderSite {
                name: binding.name.as_str(),
                span: binding.name_span,
                role: BinderRole::Binding,
                is_callable: matches!(binding.value.kind, ExprKind::Lambda { .. }),
            });
            if let Some(annotation) = &binding.annotation {
                walk_binder_sites_in_expr(annotation, visit);
            }
            walk_binder_sites_in_expr(&binding.value, visit);
        }
        Item::PatternBinding(binding) => {
            for site in pattern_bindings(&binding.pattern) {
                visit(BinderSite {
                    name: site.name,
                    span: site.span,
                    role: BinderRole::Pattern,
                    is_callable: false,
                });
            }
            walk_binder_sites_in_expr(&binding.value, visit);
        }
        Item::SpreadBinding(binding) => {
            walk_binder_sites_in_expr(&binding.value, visit);
        }
        Item::MethodAttachment(attachment) => {
            walk_binder_sites_in_expr(&attachment.owner, visit);
            walk_binder_sites_in_record_entries(&attachment.members, visit);
        }
        Item::Signature(signature) => {
            // Declaration label only — do not report as a value binder.
            walk_binder_sites_in_expr(&signature.annotation, visit);
        }
        Item::Expr(expr) => walk_binder_sites_in_expr(expr, visit),
    }
}

fn walk_binder_sites_in_record_entries<'a>(
    entries: &'a [RecordEntry],
    visit: &mut impl FnMut(BinderSite<'a>),
) {
    // Iteration binders need entry-level access; child exprs share the single
    // RecordEntry enumeration in walk_record_entry_exprs.
    walk_iteration_binders(entries, visit);
    walk_record_entry_exprs(entries, &mut |e| walk_binder_sites_in_expr(e, visit));
}

/// Emit `Iteration` binder sites (including nested comprehension bodies).
fn walk_iteration_binders<'a>(entries: &'a [RecordEntry], visit: &mut impl FnMut(BinderSite<'a>)) {
    for entry in entries {
        if let RecordEntry::Iteration {
            binder,
            binder_span,
            body,
            ..
        } = entry
        {
            visit(BinderSite {
                name: binder.as_str(),
                span: *binder_span,
                role: BinderRole::Iteration,
                is_callable: false,
            });
            walk_iteration_binders(body, visit);
        }
    }
}

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
        ExprKind::PrimitiveFamily { base, members } => {
            visit(base);
            walk_record_entry_exprs(members, visit);
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
                    Item::MethodAttachment(attachment) => {
                        visit(&attachment.owner);
                        walk_record_entry_exprs(&attachment.members, visit);
                    }
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

/// Visit every child expression under a list of record entries.
pub fn walk_record_entry_exprs<'a>(entries: &'a [RecordEntry], visit: &mut impl FnMut(&'a Expr)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_module;

    #[test]
    fn walk_binder_sites_covers_binding_lambda_match_and_pattern() {
        let parse = parse_module(concat!(
            "value = 1\n",
            "f = (item) => item\n",
            "result = x ?>\n",
            "  n => n\n",
            "  _ => 0\n",
            "{ a } = { a: 1 }\n",
            "picked = { @{\"name\"} -> k; (k, k) }\n",
            "block =\n",
            "  nested = 2\n",
            "  nested\n",
        ));
        let mut sites = Vec::new();
        walk_binder_sites_in_items(&parse.module.items, &mut |site| {
            sites.push((site.name, site.role, site.is_callable));
        });

        let expected = [
            ("value", BinderRole::Binding, false),
            ("f", BinderRole::Binding, true),
            ("item", BinderRole::Parameter, false),
            ("result", BinderRole::Binding, false),
            ("n", BinderRole::Pattern, false),
            ("a", BinderRole::Pattern, false),
            ("picked", BinderRole::Binding, false),
            ("k", BinderRole::Iteration, false),
            ("block", BinderRole::Binding, false),
            ("nested", BinderRole::Binding, false),
        ];
        for site in expected {
            let count = sites.iter().filter(|s| **s == site).count();
            assert_eq!(
                count, 1,
                "expected binder {site:?} exactly once, got {sites:?}"
            );
        }
        // Wildcard match arms do not introduce binders.
        assert!(!sites.iter().any(|(name, _, _)| *name == "_"));
    }
}
