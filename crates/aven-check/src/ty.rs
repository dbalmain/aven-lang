use std::collections::HashSet;

use aven_parser::{Expr, ExprKind, Literal};

use crate::CHECKED_NAMED_TYPES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    /// A type expression that is valid to keep for a later comptime/type phase
    /// but is not part of the core lowered type grammar yet.
    Deferred,
    Named(String),
    Variable(String),
    /// A unification variable used only during value inference. It never appears
    /// in a lowered annotation or checked output; published schemes quantify any
    /// metas that remain after inference.
    Meta(u32),
    Apply {
        callee: Box<Type>,
        args: Vec<Type>,
    },
    Function {
        params: Vec<Type>,
        result: Box<Type>,
    },
    Nullable(Box<Type>),
    Tuple(Vec<Type>),
    Record(Row),
    Variant(Row),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeScheme {
    pub(crate) vars: Vec<u32>,
    pub(crate) row_vars: Vec<u32>,
    pub(crate) ty: Type,
}

impl TypeScheme {
    pub(crate) fn mono(ty: Type) -> Self {
        Self {
            vars: Vec::new(),
            row_vars: Vec::new(),
            ty,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub entries: Vec<RowEntry>,
    pub tail: RowTail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEntry {
    Field {
        name: String,
        ty: Type,
        optional: bool,
    },
    Tag {
        name: String,
        payload: Vec<Type>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTail {
    Closed,
    Open,
    Var(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Record,
    Variant,
}

/// Rebuild a type, letting `leaf` replace any node (used for substitution and
/// instantiation). Returning `None` keeps the node and recurses structurally.
pub(crate) fn map_type(ty: &Type, leaf: &mut impl FnMut(&Type) -> Option<Type>) -> Type {
    map_type_with_rows(ty, leaf, &mut |_| None)
}

/// Rebuild a type while allowing a row tail to expand into a complete row.
pub(crate) fn map_type_with_rows(
    ty: &Type,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
    tail: &mut impl FnMut(RowTail) -> Option<Row>,
) -> Type {
    if let Some(replaced) = leaf(ty) {
        return replaced;
    }
    match ty {
        Type::Apply { callee, args } => Type::Apply {
            callee: Box::new(map_type_with_rows(callee, leaf, tail)),
            args: args
                .iter()
                .map(|arg| map_type_with_rows(arg, leaf, tail))
                .collect(),
        },
        Type::Function { params, result } => Type::Function {
            params: params
                .iter()
                .map(|param| map_type_with_rows(param, leaf, tail))
                .collect(),
            result: Box::new(map_type_with_rows(result, leaf, tail)),
        },
        Type::Nullable(inner) => Type::Nullable(Box::new(map_type_with_rows(inner, leaf, tail))),
        Type::Tuple(items) => Type::Tuple(
            items
                .iter()
                .map(|item| map_type_with_rows(item, leaf, tail))
                .collect(),
        ),
        Type::Record(row) => Type::Record(map_row(row, leaf, tail)),
        Type::Variant(row) => Type::Variant(map_row(row, leaf, tail)),
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => ty.clone(),
    }
}

fn map_row(
    row: &Row,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
    map_tail: &mut impl FnMut(RowTail) -> Option<Row>,
) -> Row {
    let mut entries: Vec<_> = row
        .entries
        .iter()
        .map(|entry| map_row_entry(entry, leaf, map_tail))
        .collect();
    let tail = if let Some(replacement) = map_tail(row.tail) {
        let replacement = map_row(&replacement, leaf, map_tail);
        entries.extend(replacement.entries);
        replacement.tail
    } else {
        row.tail
    };

    Row { entries, tail }
}

fn map_row_entry(
    entry: &RowEntry,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
    tail: &mut impl FnMut(RowTail) -> Option<Row>,
) -> RowEntry {
    match entry {
        RowEntry::Field { name, ty, optional } => RowEntry::Field {
            name: name.clone(),
            ty: map_type_with_rows(ty, leaf, tail),
            optional: *optional,
        },
        RowEntry::Tag { name, payload } => RowEntry::Tag {
            name: name.clone(),
            payload: payload
                .iter()
                .map(|ty| map_type_with_rows(ty, leaf, tail))
                .collect(),
        },
    }
}

/// Visit every nested type in pre-order (used by the structural predicates).
fn visit_type(ty: &Type, visit: &mut impl FnMut(&Type)) {
    visit_type_with_rows(ty, visit, &mut |_| {});
}

fn visit_type_with_rows(
    ty: &Type,
    visit: &mut impl FnMut(&Type),
    visit_tail: &mut impl FnMut(RowTail),
) {
    visit(ty);
    match ty {
        Type::Apply { callee, args } => {
            visit_type_with_rows(callee, visit, visit_tail);
            args.iter()
                .for_each(|arg| visit_type_with_rows(arg, visit, visit_tail));
        }
        Type::Function { params, result } => {
            params
                .iter()
                .for_each(|param| visit_type_with_rows(param, visit, visit_tail));
            visit_type_with_rows(result, visit, visit_tail);
        }
        Type::Nullable(inner) => visit_type_with_rows(inner, visit, visit_tail),
        Type::Tuple(items) => items
            .iter()
            .for_each(|item| visit_type_with_rows(item, visit, visit_tail)),
        Type::Record(row) | Type::Variant(row) => {
            row.entries
                .iter()
                .for_each(|entry| visit_row_entry(entry, visit, visit_tail));
            visit_tail(row.tail);
        }
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => {}
    }
}

fn visit_row_entry(
    entry: &RowEntry,
    visit: &mut impl FnMut(&Type),
    visit_tail: &mut impl FnMut(RowTail),
) {
    match entry {
        RowEntry::Field { ty, .. } => visit_type_with_rows(ty, visit, visit_tail),
        RowEntry::Tag { payload, .. } => payload
            .iter()
            .for_each(|ty| visit_type_with_rows(ty, visit, visit_tail)),
    }
}

pub(crate) fn free_metas(ty: &Type) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut metas = Vec::new();
    visit_type(ty, &mut |node| {
        if let Type::Meta(id) = node
            && seen.insert(*id)
        {
            metas.push(*id);
        }
    });
    metas
}

pub(crate) fn free_row_vars(ty: &Type) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut row_vars = Vec::new();
    visit_type_with_rows(ty, &mut |_| {}, &mut |tail| {
        if let RowTail::Var(id) = tail
            && seen.insert(id)
        {
            row_vars.push(id);
        }
    });
    row_vars
}

pub(crate) fn generalize(resolved: Type, env_metas: &[u32], env_row_vars: &[u32]) -> TypeScheme {
    let env_metas: HashSet<_> = env_metas.iter().copied().collect();
    let env_row_vars: HashSet<_> = env_row_vars.iter().copied().collect();
    let vars = free_metas(&resolved)
        .into_iter()
        .filter(|id| !env_metas.contains(id))
        .collect();
    let row_vars = free_row_vars(&resolved)
        .into_iter()
        .filter(|id| !env_row_vars.contains(id))
        .collect();
    TypeScheme {
        vars,
        row_vars,
        ty: resolved,
    }
}

pub(crate) fn type_contains_meta(ty: &Type, id: u32) -> bool {
    let mut found = false;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Meta(candidate) if *candidate == id) {
            found = true;
        }
    });
    found
}

pub(crate) fn is_concrete_type(ty: &Type) -> bool {
    let mut concrete_types = true;
    let mut concrete_rows = true;
    visit_type_with_rows(
        ty,
        &mut |node| {
            if matches!(node, Type::Deferred | Type::Variable(_) | Type::Meta(_)) {
                concrete_types = false;
            }
        },
        &mut |tail| {
            if matches!(tail, RowTail::Var(_)) {
                concrete_rows = false;
            }
        },
    );
    concrete_types && concrete_rows
}

pub(crate) fn has_only_meta_unknowns(ty: &Type) -> bool {
    let mut valid_types = true;
    let mut valid_rows = true;
    visit_type_with_rows(
        ty,
        &mut |node| {
            if matches!(node, Type::Deferred | Type::Variable(_)) {
                valid_types = false;
            }
        },
        &mut |tail| {
            if matches!(tail, RowTail::Var(_)) {
                valid_rows = false;
            }
        },
    );
    valid_types && valid_rows
}

pub(crate) fn type_contains_deferred(ty: &Type) -> bool {
    let mut found = false;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Deferred) {
            found = true;
        }
    });
    found
}

pub(crate) fn named_builtin(name: &str) -> Type {
    Type::Named(name.to_owned())
}

pub(crate) fn named_type_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) => Some(name),
        Type::Deferred
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Apply { .. }
        | Type::Function { .. }
        | Type::Nullable(_)
        | Type::Tuple(_)
        | Type::Record(_)
        | Type::Variant(_) => None,
    }
}

pub(crate) fn numeric_type_name(ty: &Type) -> Option<&'static str> {
    match named_type_name(ty) {
        Some("Int") => Some("Int"),
        Some("Float") => Some("Float"),
        _ => None,
    }
}

pub(crate) fn is_meta_type(ty: &Type) -> bool {
    matches!(ty, Type::Meta(_))
}

pub(crate) fn mismatched_literal_kind(expected: &str, literal: &Literal) -> Option<&'static str> {
    match (expected, literal) {
        ("Text", Literal::String(_)) | ("Int" | "Float", Literal::Number(_)) => None,
        ("Int" | "Float" | "Bool" | "Nil" | "Unit", Literal::String(_)) => Some("text literal"),
        ("Text" | "Bool" | "Nil" | "Unit", Literal::Number(_)) => Some("number literal"),
        _ => None,
    }
}

pub(crate) fn named_type_mismatch(expected: &str, actual: &str) -> bool {
    if !CHECKED_NAMED_TYPES.contains(&expected) || !CHECKED_NAMED_TYPES.contains(&actual) {
        return false;
    }

    expected != actual
}

pub(crate) fn is_nil_value(value: &Expr) -> bool {
    matches!(&value.kind, ExprKind::ComptimeName(name) if name == "Nil")
}
