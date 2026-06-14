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
    /// in a lowered annotation or any checked output; synthesis resolves it away
    /// (or defers) before a type reaches `value_types`.
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
    Record(Vec<TypeRowEntry>),
    Variant(Vec<TypeRowEntry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRowEntry {
    Field {
        name: String,
        ty: Type,
        overwrite: bool,
        optional: bool,
    },
    Tag {
        name: String,
        payload: Vec<Type>,
    },
    Spread {
        ty: Type,
        overwrite: bool,
    },
    Delete(String),
    Rename {
        from: String,
        to: String,
    },
    Shorthand(String),
    Open,
    Element(Type),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Record,
    Variant,
}

/// Rebuild a type, letting `leaf` replace any node (used for substitution and
/// instantiation). Returning `None` keeps the node and recurses structurally.
pub(crate) fn map_type(ty: &Type, leaf: &mut impl FnMut(&Type) -> Option<Type>) -> Type {
    if let Some(replaced) = leaf(ty) {
        return replaced;
    }
    match ty {
        Type::Apply { callee, args } => Type::Apply {
            callee: Box::new(map_type(callee, leaf)),
            args: args.iter().map(|arg| map_type(arg, leaf)).collect(),
        },
        Type::Function { params, result } => Type::Function {
            params: params.iter().map(|param| map_type(param, leaf)).collect(),
            result: Box::new(map_type(result, leaf)),
        },
        Type::Nullable(inner) => Type::Nullable(Box::new(map_type(inner, leaf))),
        Type::Tuple(items) => Type::Tuple(items.iter().map(|item| map_type(item, leaf)).collect()),
        Type::Record(entries) => Type::Record(
            entries
                .iter()
                .map(|entry| map_row_entry(entry, leaf))
                .collect(),
        ),
        Type::Variant(entries) => Type::Variant(
            entries
                .iter()
                .map(|entry| map_row_entry(entry, leaf))
                .collect(),
        ),
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => ty.clone(),
    }
}

fn map_row_entry(
    entry: &TypeRowEntry,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
) -> TypeRowEntry {
    match entry {
        TypeRowEntry::Field {
            name,
            ty,
            overwrite,
            optional,
        } => TypeRowEntry::Field {
            name: name.clone(),
            ty: map_type(ty, leaf),
            overwrite: *overwrite,
            optional: *optional,
        },
        TypeRowEntry::Tag { name, payload } => TypeRowEntry::Tag {
            name: name.clone(),
            payload: payload.iter().map(|ty| map_type(ty, leaf)).collect(),
        },
        TypeRowEntry::Spread { ty, overwrite } => TypeRowEntry::Spread {
            ty: map_type(ty, leaf),
            overwrite: *overwrite,
        },
        TypeRowEntry::Delete(name) => TypeRowEntry::Delete(name.clone()),
        TypeRowEntry::Rename { from, to } => TypeRowEntry::Rename {
            from: from.clone(),
            to: to.clone(),
        },
        TypeRowEntry::Shorthand(name) => TypeRowEntry::Shorthand(name.clone()),
        TypeRowEntry::Open => TypeRowEntry::Open,
        TypeRowEntry::Element(ty) => TypeRowEntry::Element(map_type(ty, leaf)),
    }
}

/// Visit every nested type in pre-order (used by the structural predicates).
fn visit_type(ty: &Type, visit: &mut impl FnMut(&Type)) {
    visit(ty);
    match ty {
        Type::Apply { callee, args } => {
            visit_type(callee, visit);
            args.iter().for_each(|arg| visit_type(arg, visit));
        }
        Type::Function { params, result } => {
            params.iter().for_each(|param| visit_type(param, visit));
            visit_type(result, visit);
        }
        Type::Nullable(inner) => visit_type(inner, visit),
        Type::Tuple(items) => items.iter().for_each(|item| visit_type(item, visit)),
        Type::Record(entries) | Type::Variant(entries) => {
            entries
                .iter()
                .for_each(|entry| visit_row_entry(entry, visit));
        }
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => {}
    }
}

fn visit_row_entry(entry: &TypeRowEntry, visit: &mut impl FnMut(&Type)) {
    match entry {
        TypeRowEntry::Field { ty, .. }
        | TypeRowEntry::Spread { ty, .. }
        | TypeRowEntry::Element(ty) => visit_type(ty, visit),
        TypeRowEntry::Tag { payload, .. } => payload.iter().for_each(|ty| visit_type(ty, visit)),
        TypeRowEntry::Delete(_)
        | TypeRowEntry::Rename { .. }
        | TypeRowEntry::Shorthand(_)
        | TypeRowEntry::Open => {}
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
    let mut concrete = true;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Deferred | Type::Variable(_) | Type::Meta(_)) {
            concrete = false;
        }
    });
    concrete
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

    if matches!((expected, actual), ("Int", "Float") | ("Float", "Int")) {
        return false;
    }

    expected != actual
}

pub(crate) fn is_nil_value(value: &Expr) -> bool {
    matches!(&value.kind, ExprKind::ComptimeName(name) if name == "Nil")
}
