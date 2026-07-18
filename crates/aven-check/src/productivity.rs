use std::collections::HashSet;

use crate::ty::{RecursiveTypeId, RowEntry, RowTail, Type};

/// Apply the recursive-type productivity constructor rules to one completed
/// head. `recursive_status` identifies references in the SCC and supplies the
/// member's current least-fixed-point status; references outside it are
/// conservatively productive.
pub(crate) fn is_productive(
    ty: &Type,
    recursive_status: &mut impl FnMut(&Type) -> Option<bool>,
) -> bool {
    if let Some(productive) = recursive_status(ty) {
        return productive;
    }

    match ty {
        Type::Optional(_) | Type::Nullable(_) | Type::Function { .. } => true,
        Type::Apply { callee, .. } if matches!(callee.as_ref(), Type::Named(name) if matches!(name.as_str(), "Array" | "Map" | "Set" | "Stream")) => {
            true
        }
        Type::Tuple(items) => items
            .iter()
            .all(|item| is_productive(item, recursive_status)),
        Type::Record(row) => {
            row.tail != RowTail::Closed
                || row.entries.iter().all(|entry| match entry {
                    RowEntry::Field { ty, .. } => is_productive(ty, recursive_status),
                    RowEntry::Literal { .. } | RowEntry::Tag { .. } => true,
                })
        }
        Type::SlotRecord { data, slots } => [data, slots].into_iter().all(|row| {
            row.tail != RowTail::Closed
                || row.entries.iter().all(|entry| match entry {
                    RowEntry::Field { ty, .. } => is_productive(ty, recursive_status),
                    RowEntry::Literal { .. } | RowEntry::Tag { .. } => true,
                })
        }),
        Type::Variant(row) => {
            row.tail != RowTail::Closed
                || row.entries.iter().any(|entry| match entry {
                    RowEntry::Tag { payload, .. } => {
                        payload.iter().all(|ty| is_productive(ty, recursive_status))
                    }
                    RowEntry::Literal { .. } | RowEntry::Field { .. } => true,
                })
        }
        // Deferred forms, variables, metas, names, recursive references outside
        // the current SCC, and non-collection applications are intentionally
        // conservative: productivity diagnostics must not false-positive.
        Type::Deferred
        | Type::Named(_)
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Recursive(_)
        | Type::Apply { .. } => true,
    }
}

/// Describe the first strict constructor that forces another unproductive
/// recursive component member. This only formats the unified LFP result; it does not decide
/// productivity independently.
pub(crate) fn forcing_step(ty: &Type, unproductive: &HashSet<RecursiveTypeId>) -> Option<String> {
    match ty {
        Type::Recursive(id) if unproductive.contains(id) => Some(format!("type `{}`", ty.render())),
        Type::Tuple(items) => items.iter().enumerate().find_map(|(index, item)| {
            forcing_step(item, unproductive).map(|_| format!("tuple item {}", index + 1))
        }),
        Type::Record(row) if row.tail == RowTail::Closed => row.entries.iter().find_map(|entry| {
            let RowEntry::Field { name, ty } = entry else {
                return None;
            };
            forcing_step(ty, unproductive).map(|_| format!("field `{name}`"))
        }),
        Type::Variant(row) if row.tail == RowTail::Closed => row.entries.iter().find_map(|entry| {
            let RowEntry::Tag { name, payload } = entry else {
                return None;
            };
            payload
                .iter()
                .find_map(|ty| forcing_step(ty, unproductive))
                .map(|_| format!("alternative `@{name}`"))
        }),
        _ => None,
    }
}
