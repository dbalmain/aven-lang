use std::collections::{HashMap, HashSet};

use aven_core::Span;

use crate::ty::{
    LiteralBase, RecursiveTypeId, Row, RowEntry, RowMergeConstraint, RowMergeSource, RowTail, Type,
    TypeScheme, free_row_vars, literal_variant_base, map_type, map_type_with_rows,
    open_literal_variant_base, render_literal_value, type_contains_meta,
};

#[derive(Debug, Default)]
pub(crate) struct Unifier {
    substitution: Vec<Option<Type>>,
    row_subst: Vec<Option<Row>>,
    row_merges: Vec<RowMergeConstraint>,
    numeric: HashSet<u32>,
    recursive_type_unfoldings: HashMap<RecursiveTypeId, Type>,
}

#[derive(Clone)]
pub(crate) struct UnifierSnapshot {
    substitution: Vec<Option<Type>>,
    row_subst: Vec<Option<Row>>,
    row_merges: Vec<RowMergeConstraint>,
    numeric: HashSet<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowMergeConflict {
    pub(crate) label: String,
    pub(crate) span: Span,
}

impl Unifier {
    pub(crate) fn fresh(&mut self) -> Type {
        let id = self.substitution.len() as u32;
        self.substitution.push(None);
        Type::Meta(id)
    }

    pub(crate) fn fresh_numeric(&mut self) -> Type {
        let Type::Meta(id) = self.fresh() else {
            unreachable!("fresh types are metavariables");
        };
        self.numeric.insert(id);
        Type::Meta(id)
    }

    pub(crate) fn fresh_row_var(&mut self) -> u32 {
        let id = self.row_subst.len() as u32;
        self.row_subst.push(None);
        id
    }

    pub(crate) fn fresh_row_merge(&mut self, sources: Vec<RowMergeSource>) -> u32 {
        let result = self.fresh_row_var();
        self.row_merges.push(RowMergeConstraint { result, sources });
        result
    }

    pub(crate) fn resolve(&self, ty: &Type) -> Type {
        let mut visiting_row_merges = HashSet::new();
        self.resolve_with_visited(ty, &mut visiting_row_merges)
    }

    fn resolve_with_visited(&self, ty: &Type, visiting_row_merges: &mut HashSet<u32>) -> Type {
        map_type_with_rows(
            ty,
            &mut |node| match node {
                Type::Meta(id) => match self.substitution.get(*id as usize) {
                    Some(Some(bound)) => Some(self.resolve(bound)),
                    _ => None,
                },
                _ => None,
            },
            &mut |tail| match tail {
                RowTail::Var(id) => self
                    .row_subst
                    .get(id as usize)
                    .and_then(|bound| bound.clone())
                    .or_else(|| {
                        self.resolve_row_merge(id, visiting_row_merges)
                            .ok()
                            .flatten()
                    }),
                RowTail::Closed | RowTail::Open => None,
            },
        )
    }

    pub(crate) fn is_numeric_meta(&self, ty: &Type) -> bool {
        matches!(self.resolve(ty), Type::Meta(id) if self.numeric.contains(&id))
    }

    pub(crate) fn default_numerics(&self, ty: &Type) -> Type {
        let resolved = self.resolve(ty);
        map_type(&resolved, &mut |node| match node {
            Type::Meta(id) if self.numeric.contains(id) => Some(Type::Named("Int".to_owned())),
            _ => None,
        })
    }

    /// Capture the current substitution so a speculative sequence of
    /// unifications can be rolled back with [`Unifier::restore`].
    pub(crate) fn snapshot(&self) -> UnifierSnapshot {
        UnifierSnapshot {
            substitution: self.substitution.clone(),
            row_subst: self.row_subst.clone(),
            row_merges: self.row_merges.clone(),
            numeric: self.numeric.clone(),
        }
    }

    pub(crate) fn restore(&mut self, snapshot: UnifierSnapshot) {
        self.substitution = snapshot.substitution;
        self.row_subst = snapshot.row_subst;
        self.row_merges = snapshot.row_merges;
        self.numeric = snapshot.numeric;
    }

    pub(crate) fn unify(&mut self, left: &Type, right: &Type) -> Result<(), ()> {
        let snapshot = self.snapshot();
        if self.unify_inner(left, right, &mut HashSet::new()).is_err() {
            self.restore(snapshot);
            Err(())
        } else {
            Ok(())
        }
    }

    fn unify_inner(
        &mut self,
        left: &Type,
        right: &Type,
        unfolding: &mut HashSet<RecursiveTypeId>,
    ) -> Result<(), ()> {
        let left = self.resolve(left);
        let right = self.resolve(right);

        match (&left, &right) {
            (Type::Meta(left), Type::Meta(right)) if left == right => Ok(()),
            (Type::Recursive(left), Type::Recursive(right)) => {
                (left == right).then_some(()).ok_or(())
            }
            (Type::Recursive(id), other) | (other, Type::Recursive(id)) => {
                if !unfolding.insert(*id) {
                    return Ok(());
                }
                let head = self.recursive_type_unfoldings.get(id).cloned().ok_or(())?;
                let result = if matches!(left, Type::Recursive(_)) {
                    self.unify_inner(&head, other, unfolding)
                } else {
                    self.unify_inner(other, &head, unfolding)
                };
                unfolding.remove(id);
                result
            }
            (Type::Meta(id), Type::Variant(row)) | (Type::Variant(row), Type::Meta(id))
                if self.numeric.contains(id)
                    && open_literal_variant_base(row) == Some(LiteralBase::Number) =>
            {
                Ok(())
            }
            (Type::Meta(id), ty) | (ty, Type::Meta(id)) => self.bind(*id, ty),
            (Type::Named(left), Type::Named(right)) if left == right => Ok(()),
            (Type::Variant(row), Type::Named(name)) | (Type::Named(name), Type::Variant(row))
                if open_literal_variant_base(row).is_some_and(|base| base.matches_named(name)) =>
            {
                Ok(())
            }
            (Type::Variable(left), Type::Variable(right)) if left == right => Ok(()),
            (
                Type::Apply {
                    callee: left_callee,
                    args: left_args,
                },
                Type::Apply {
                    callee: right_callee,
                    args: right_args,
                },
            ) if left_args.len() == right_args.len() => {
                self.unify_inner(left_callee, right_callee, unfolding)?;
                self.unify_many(left_args, right_args, unfolding)
            }
            (
                Type::Function {
                    params: left_params,
                    result: left_result,
                    required: left_required,
                },
                Type::Function {
                    params: right_params,
                    result: right_result,
                    required: right_required,
                },
                // Conservative: two function types unify only with the same
                // total length and the same required-arity. Function subtyping
                // (accepting a fewer-required function where a more-required one
                // is expected) is deferred.
            ) if left_params.len() == right_params.len() && left_required == right_required => {
                self.unify_many(left_params, right_params, unfolding)?;
                self.unify_inner(left_result, right_result, unfolding)
            }
            (Type::Optional(left), Type::Optional(right)) => {
                self.unify_inner(left, right, unfolding)
            }
            (Type::Nullable(left), Type::Nullable(right)) => {
                self.unify_inner(left, right, unfolding)
            }
            (Type::Tuple(left), Type::Tuple(right)) if left.len() == right.len() => {
                self.unify_many(left, right, unfolding)
            }
            (Type::Record(left), Type::Record(right)) => self.unify_rows(left, right, unfolding),
            (Type::Variant(left), Type::Variant(right)) => self.unify_rows(left, right, unfolding),
            _ => Err(()),
        }
    }

    fn unify_many(
        &mut self,
        left: &[Type],
        right: &[Type],
        unfolding: &mut HashSet<RecursiveTypeId>,
    ) -> Result<(), ()> {
        for (left, right) in left.iter().zip(right) {
            self.unify_inner(left, right, unfolding)?;
        }
        Ok(())
    }

    pub(crate) fn insert_recursive_type(&mut self, id: RecursiveTypeId, head: Type) {
        self.recursive_type_unfoldings.insert(id, head);
    }

    pub(crate) fn set_recursive_type_unfoldings(
        &mut self,
        unfoldings: HashMap<RecursiveTypeId, Type>,
    ) {
        self.recursive_type_unfoldings = unfoldings;
    }

    fn bind(&mut self, id: u32, ty: &Type) -> Result<(), ()> {
        let ty = self.resolve(ty);
        if ty == Type::Meta(id) {
            return Ok(());
        }
        if type_contains_meta(&ty, id) {
            return Err(());
        }

        if self.numeric.contains(&id) {
            match &ty {
                Type::Named(name) if name == "Int" || name == "Float" => {}
                Type::Meta(other) => {
                    self.numeric.insert(*other);
                }
                _ => return Err(()),
            }
        }

        let Some(slot) = self.substitution.get_mut(id as usize) else {
            return Err(());
        };
        *slot = Some(ty);
        Ok(())
    }

    fn unify_rows(
        &mut self,
        left: &Row,
        right: &Row,
        unfolding: &mut HashSet<RecursiveTypeId>,
    ) -> Result<(), ()> {
        let left = self.resolve_row(left);
        let right = self.resolve_row(right);
        if let (Some(left_base), Some(right_base)) =
            (literal_variant_base(&left), literal_variant_base(&right))
            && left_base != right_base
        {
            return Err(());
        }
        let mut right_entries = right.entries;
        let mut left_only = Vec::new();

        for left_entry in left.entries {
            let left_name = row_entry_label(&left_entry);
            let Some(position) = right_entries
                .iter()
                .position(|entry| row_entry_label(entry) == left_name)
            else {
                left_only.push(left_entry);
                continue;
            };

            let right_entry = right_entries.remove(position);
            self.unify_row_entries(&left_entry, &right_entry, unfolding)?;
        }

        let left_remainder = Row {
            entries: left_only,
            tail: left.tail,
        };
        let right_remainder = Row {
            entries: right_entries,
            tail: right.tail,
        };
        let resolved_left = self.resolve_row(&left_remainder);
        let resolved_right = self.resolve_row(&right_remainder);

        if resolved_left != left_remainder || resolved_right != right_remainder {
            return self.unify_rows(&resolved_left, &resolved_right, unfolding);
        }

        let right_tail = self.supply_entries(right_remainder.tail, &left_remainder.entries)?;
        let left_tail = self.supply_entries(left_remainder.tail, &right_remainder.entries)?;
        self.unify_row_tails(left_tail, right_tail)
    }

    fn unify_row_entries(
        &mut self,
        left: &RowEntry,
        right: &RowEntry,
        unfolding: &mut HashSet<RecursiveTypeId>,
    ) -> Result<(), ()> {
        match (left, right) {
            (RowEntry::Field { ty: left_type, .. }, RowEntry::Field { ty: right_type, .. }) => {
                self.unify_inner(left_type, right_type, unfolding)
            }
            (
                RowEntry::Tag {
                    payload: left_payload,
                    ..
                },
                RowEntry::Tag {
                    payload: right_payload,
                    ..
                },
            ) if left_payload.len() == right_payload.len() => {
                self.unify_many(left_payload, right_payload, unfolding)
            }
            (RowEntry::Literal { value: left }, RowEntry::Literal { value: right })
                if left == right =>
            {
                Ok(())
            }
            _ => Err(()),
        }
    }

    fn supply_entries(&mut self, tail: RowTail, entries: &[RowEntry]) -> Result<RowTail, ()> {
        if entries.is_empty() {
            return Ok(tail);
        }

        match tail {
            RowTail::Closed => Err(()),
            RowTail::Open => Ok(RowTail::Open),
            RowTail::Var(id) => {
                let remainder = self.fresh_row_var();
                self.bind_row(
                    id,
                    &Row {
                        entries: entries.to_vec(),
                        tail: RowTail::Var(remainder),
                    },
                )?;
                Ok(RowTail::Var(remainder))
            }
        }
    }

    fn unify_row_tails(&mut self, left: RowTail, right: RowTail) -> Result<(), ()> {
        match (left, right) {
            (RowTail::Var(left), RowTail::Var(right)) if left == right => Ok(()),
            (RowTail::Var(id), RowTail::Var(other)) => self.bind_row(
                id,
                &Row {
                    entries: Vec::new(),
                    tail: RowTail::Var(other),
                },
            ),
            (RowTail::Var(id), RowTail::Closed) | (RowTail::Closed, RowTail::Var(id)) => self
                .bind_row(
                    id,
                    &Row {
                        entries: Vec::new(),
                        tail: RowTail::Closed,
                    },
                ),
            (RowTail::Closed, RowTail::Closed) | (RowTail::Open, _) | (_, RowTail::Open) => Ok(()),
        }
    }

    fn bind_row(&mut self, id: u32, row: &Row) -> Result<(), ()> {
        let row = self.resolve_row(row);
        if row.entries.is_empty() && row.tail == RowTail::Var(id) {
            return Ok(());
        }
        if free_row_vars(&Type::Record(row.clone())).contains(&id) {
            return Err(());
        }

        let Some(slot) = self.row_subst.get_mut(id as usize) else {
            return Err(());
        };
        if slot.is_some() {
            return Err(());
        }
        *slot = Some(row);
        Ok(())
    }

    fn resolve_row(&self, row: &Row) -> Row {
        let Type::Record(row) = self.resolve(&Type::Record(row.clone())) else {
            unreachable!("record resolution preserves the outer type")
        };
        row
    }

    pub(crate) fn row_merge_conflict_in_type(&self, ty: &Type) -> Option<RowMergeConflict> {
        let mut conflict = None;
        visit_type_row_tails(ty, &mut |tail| {
            if conflict.is_some() {
                return;
            }
            let RowTail::Var(id) = tail else {
                return;
            };
            let mut visiting = HashSet::new();
            if let Err(found) = self.resolve_row_merge(id, &mut visiting) {
                conflict = Some(found);
            }
        });
        conflict
    }

    pub(crate) fn row_merge_closure(
        &self,
        roots: &[u32],
        env_row_vars: &[u32],
    ) -> Vec<RowMergeConstraint> {
        let env_row_vars: HashSet<_> = env_row_vars.iter().copied().collect();
        let mut needed: HashSet<_> = roots.iter().copied().collect();
        let mut constraints = Vec::new();

        let mut changed = true;
        while changed {
            changed = false;
            for constraint in &self.row_merges {
                if !needed.contains(&constraint.result)
                    || constraints
                        .iter()
                        .any(|included: &RowMergeConstraint| included.result == constraint.result)
                {
                    continue;
                }

                let mut constraint_vars = free_row_vars_in_merge_constraint(constraint);
                if constraint_vars.iter().any(|id| env_row_vars.contains(id)) {
                    continue;
                }

                constraint_vars.retain(|id| needed.insert(*id));
                changed = changed || !constraint_vars.is_empty();
                constraints.push(constraint.clone());
            }
        }

        constraints
    }

    fn resolve_row_merge(
        &self,
        id: u32,
        visiting: &mut HashSet<u32>,
    ) -> Result<Option<Row>, RowMergeConflict> {
        let Some(constraint) = self
            .row_merges
            .iter()
            .rev()
            .find(|constraint| constraint.result == id)
        else {
            return Ok(None);
        };
        if !visiting.insert(id) {
            return Ok(None);
        }

        let result = (|| {
            let mut row = Row {
                entries: Vec::new(),
                tail: RowTail::Closed,
            };
            for source in &constraint.sources {
                let source_row = self.resolve_merge_source_row(&source.row, visiting);
                if matches!(source_row.tail, RowTail::Var(_)) {
                    return Ok(None);
                }
                merge_resolved_row(&mut row, source_row, source.overwrite, source.span)?;
            }

            if matches!(row.tail, RowTail::Var(_)) {
                Ok(None)
            } else {
                Ok(Some(row))
            }
        })();
        visiting.remove(&id);
        result
    }

    fn resolve_merge_source_row(&self, row: &Row, visiting: &mut HashSet<u32>) -> Row {
        let Type::Record(row) = self.resolve_with_visited(&Type::Record(row.clone()), visiting)
        else {
            unreachable!("record resolution preserves the outer type")
        };
        row
    }

    pub(crate) fn instantiate_scheme(&mut self, scheme: &TypeScheme) -> Type {
        let mut replacements: HashMap<u32, Type> = HashMap::new();
        for id in &scheme.vars {
            replacements.insert(*id, self.fresh());
        }

        let mut row_replacements: HashMap<u32, u32> = HashMap::new();
        for id in &scheme.row_vars {
            row_replacements.insert(*id, self.fresh_row_var());
        }

        let ty = map_type_with_rows(
            &scheme.ty,
            &mut |node| match node {
                Type::Meta(id) => replacements.get(id).cloned(),
                _ => None,
            },
            &mut |tail| match tail {
                RowTail::Var(id) => row_replacements.get(&id).map(|replacement| Row {
                    entries: Vec::new(),
                    tail: RowTail::Var(*replacement),
                }),
                RowTail::Closed | RowTail::Open => None,
            },
        );

        let instantiated_merges = scheme
            .row_merges
            .iter()
            .map(|constraint| {
                instantiate_row_merge_constraint(constraint, &replacements, &row_replacements)
            })
            .collect::<Vec<_>>();
        self.row_merges.extend(instantiated_merges);

        ty
    }
}

fn instantiate_row_merge_constraint(
    constraint: &RowMergeConstraint,
    replacements: &HashMap<u32, Type>,
    row_replacements: &HashMap<u32, u32>,
) -> RowMergeConstraint {
    RowMergeConstraint {
        result: row_replacements
            .get(&constraint.result)
            .copied()
            .unwrap_or(constraint.result),
        sources: constraint
            .sources
            .iter()
            .map(|source| RowMergeSource {
                row: instantiate_row_merge_source(&source.row, replacements, row_replacements),
                overwrite: source.overwrite,
                span: source.span,
            })
            .collect(),
    }
}

fn instantiate_row_merge_source(
    row: &Row,
    replacements: &HashMap<u32, Type>,
    row_replacements: &HashMap<u32, u32>,
) -> Row {
    let Type::Record(row) = map_type_with_rows(
        &Type::Record(row.clone()),
        &mut |node| match node {
            Type::Meta(id) => replacements.get(id).cloned(),
            _ => None,
        },
        &mut |tail| match tail {
            RowTail::Var(id) => row_replacements.get(&id).map(|replacement| Row {
                entries: Vec::new(),
                tail: RowTail::Var(*replacement),
            }),
            RowTail::Closed | RowTail::Open => None,
        },
    ) else {
        unreachable!("record mapping preserves the outer type")
    };
    row
}

fn free_row_vars_in_merge_constraint(constraint: &RowMergeConstraint) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut vars = Vec::new();
    if seen.insert(constraint.result) {
        vars.push(constraint.result);
    }
    for source in &constraint.sources {
        for id in free_row_vars(&Type::Record(source.row.clone())) {
            if seen.insert(id) {
                vars.push(id);
            }
        }
    }
    vars
}

fn merge_resolved_row(
    row: &mut Row,
    source: Row,
    overwrite: bool,
    span: Span,
) -> Result<(), RowMergeConflict> {
    for entry in source.entries {
        let label = row_entry_label(&entry).to_owned();
        if let Some(index) = row_entry_index(&row.entries, &label) {
            if optional_record_patch_field_matches(&row.entries[index], &entry) {
                continue;
            }
            if !overwrite {
                return Err(RowMergeConflict { label, span });
            }
            row.entries[index] = entry;
        } else {
            row.entries.push(entry);
        }
    }

    row.tail = merge_resolved_row_tails(row.tail, source.tail);
    Ok(())
}

fn optional_record_patch_field_matches(base: &RowEntry, incoming: &RowEntry) -> bool {
    let (
        RowEntry::Field { ty: base_ty, .. },
        RowEntry::Field {
            ty: Type::Optional(incoming_inner),
            ..
        },
    ) = (base, incoming)
    else {
        return false;
    };

    base_ty == incoming_inner.as_ref()
}

fn merge_resolved_row_tails(accumulated: RowTail, incoming: RowTail) -> RowTail {
    match (accumulated, incoming) {
        (tail, RowTail::Closed) | (RowTail::Closed, tail) => tail,
        (RowTail::Open, _) | (_, RowTail::Open) => RowTail::Open,
        (RowTail::Var(left), RowTail::Var(right)) if left == right => RowTail::Var(left),
        (RowTail::Var(_), RowTail::Var(_)) => RowTail::Open,
    }
}

fn visit_type_row_tails(ty: &Type, visit: &mut impl FnMut(RowTail)) {
    match ty {
        Type::Apply { callee, args } => {
            visit_type_row_tails(callee, visit);
            args.iter().for_each(|arg| visit_type_row_tails(arg, visit));
        }
        Type::Function { params, result, .. } => {
            params
                .iter()
                .for_each(|param| visit_type_row_tails(param, visit));
            visit_type_row_tails(result, visit);
        }
        Type::Optional(inner) | Type::Nullable(inner) => visit_type_row_tails(inner, visit),
        Type::Tuple(items) => items
            .iter()
            .for_each(|item| visit_type_row_tails(item, visit)),
        Type::Record(row) | Type::Variant(row) => visit_row_tails(row, visit),
        Type::Deferred
        | Type::Named(_)
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Recursive(_) => {}
    }
}

fn visit_row_tails(row: &Row, visit: &mut impl FnMut(RowTail)) {
    for entry in &row.entries {
        match entry {
            RowEntry::Field { ty, .. } => visit_type_row_tails(ty, visit),
            RowEntry::Tag { payload, .. } => payload
                .iter()
                .for_each(|ty| visit_type_row_tails(ty, visit)),
            RowEntry::Literal { .. } => {}
        }
    }
    visit(row.tail);
}

fn row_entry_label(entry: &RowEntry) -> &str {
    match entry {
        RowEntry::Field { name, .. } | RowEntry::Tag { name, .. } => name,
        RowEntry::Literal { value } => render_literal_value(value),
    }
}

fn row_entry_index(entries: &[RowEntry], label: &str) -> Option<usize> {
    entries
        .iter()
        .position(|entry| row_entry_label(entry) == label)
}

#[cfg(test)]
mod tests {
    use aven_parser::Literal;

    use super::*;

    fn named(name: &str) -> Type {
        Type::Named(name.to_owned())
    }

    fn field(name: &str, ty: Type) -> RowEntry {
        RowEntry::Field {
            name: name.to_owned(),
            ty,
        }
    }

    fn tag(name: &str, payload: Vec<Type>) -> RowEntry {
        RowEntry::Tag {
            name: name.to_owned(),
            payload,
        }
    }

    fn literal_number(raw: &str) -> RowEntry {
        RowEntry::Literal {
            value: Literal::Number(raw.to_owned()),
        }
    }

    fn literal_string(raw: &str) -> RowEntry {
        RowEntry::Literal {
            value: Literal::String(raw.to_owned()),
        }
    }

    #[test]
    fn numeric_metas_unify_in_either_order() {
        for reverse in [false, true] {
            let mut unifier = Unifier::default();
            let left = unifier.fresh_numeric();
            let right = unifier.fresh_numeric();

            let result = if reverse {
                unifier.unify(&right, &left)
            } else {
                unifier.unify(&left, &right)
            };

            assert_eq!(result, Ok(()));
            assert!(unifier.is_numeric_meta(&left));
            assert!(unifier.is_numeric_meta(&right));
            assert_eq!(unifier.default_numerics(&left), named("Int"));
            assert_eq!(unifier.default_numerics(&right), named("Int"));
        }
    }

    #[test]
    fn numeric_meta_rejects_non_numeric_named_types_in_either_order() {
        for reverse in [false, true] {
            let mut unifier = Unifier::default();
            let numeric = unifier.fresh_numeric();
            let text = named("Text");

            let result = if reverse {
                unifier.unify(&text, &numeric)
            } else {
                unifier.unify(&numeric, &text)
            };

            assert_eq!(result, Err(()));
            assert!(unifier.is_numeric_meta(&numeric));
        }
    }

    #[test]
    fn numericness_propagates_to_an_ordinary_meta() {
        let mut unifier = Unifier::default();
        let numeric = unifier.fresh_numeric();
        let ordinary = unifier.fresh();

        assert_eq!(unifier.unify(&numeric, &ordinary), Ok(()));
        assert!(unifier.is_numeric_meta(&ordinary));
        assert_eq!(unifier.unify(&ordinary, &named("Float")), Ok(()));
        assert_eq!(unifier.default_numerics(&numeric), named("Float"));
    }

    #[test]
    fn record_unification_rewrites_and_closes_a_row_variable() {
        let mut unifier = Unifier::default();
        let tail = unifier.fresh_row_var();
        let required = Type::Record(Row {
            entries: vec![field("x", named("Int"))],
            tail: RowTail::Var(tail),
        });
        let actual = Type::Record(Row {
            entries: vec![field("x", named("Int")), field("y", named("Text"))],
            tail: RowTail::Closed,
        });

        assert_eq!(unifier.unify(&required, &actual), Ok(()));
        assert_eq!(
            unifier.resolve(&required),
            Type::Record(Row {
                entries: vec![field("x", named("Int")), field("y", named("Text"))],
                tail: RowTail::Closed,
            })
        );
    }

    #[test]
    fn row_occurs_check_rejects_a_recursive_tail_binding() {
        let mut unifier = Unifier::default();
        let tail = unifier.fresh_row_var();
        let recursive_field = Type::Record(Row {
            entries: Vec::new(),
            tail: RowTail::Var(tail),
        });
        let left = Type::Record(Row {
            entries: Vec::new(),
            tail: RowTail::Var(tail),
        });
        let right = Type::Record(Row {
            entries: vec![field("next", recursive_field)],
            tail: RowTail::Closed,
        });

        assert_eq!(unifier.unify(&left, &right), Err(()));
        assert_eq!(unifier.resolve(&left), left);
    }

    #[test]
    fn variant_unification_merges_open_tag_rows() {
        let mut unifier = Unifier::default();
        let left_tail = unifier.fresh_row_var();
        let right_tail = unifier.fresh_row_var();
        let left = Type::Variant(Row {
            entries: vec![tag("Zero", Vec::new())],
            tail: RowTail::Var(left_tail),
        });
        let right = Type::Variant(Row {
            entries: vec![tag("Pos", Vec::new())],
            tail: RowTail::Var(right_tail),
        });

        assert_eq!(unifier.unify(&left, &right), Ok(()));
        let Type::Variant(resolved) = unifier.resolve(&left) else {
            panic!("variant resolution should preserve the outer type");
        };
        assert!(resolved.entries.contains(&tag("Zero", Vec::new())));
        assert!(resolved.entries.contains(&tag("Pos", Vec::new())));
        assert!(matches!(resolved.tail, RowTail::Var(_)));
    }

    #[test]
    fn variant_unification_merges_open_literal_rows() {
        let mut unifier = Unifier::default();
        let left_tail = unifier.fresh_row_var();
        let right_tail = unifier.fresh_row_var();
        let left = Type::Variant(Row {
            entries: vec![literal_number("1")],
            tail: RowTail::Var(left_tail),
        });
        let right = Type::Variant(Row {
            entries: vec![literal_number("2")],
            tail: RowTail::Var(right_tail),
        });

        assert_eq!(unifier.unify(&left, &right), Ok(()));
        let Type::Variant(resolved) = unifier.resolve(&left) else {
            panic!("variant resolution should preserve the outer type");
        };
        assert!(resolved.entries.contains(&literal_number("1")));
        assert!(resolved.entries.contains(&literal_number("2")));
        assert!(matches!(resolved.tail, RowTail::Var(_)));
    }

    #[test]
    fn variant_unification_rejects_mixed_literal_bases_without_merging() {
        let mut unifier = Unifier::default();
        let left_tail = unifier.fresh_row_var();
        let right_tail = unifier.fresh_row_var();
        let left = Type::Variant(Row {
            entries: vec![literal_number("1")],
            tail: RowTail::Var(left_tail),
        });
        let right = Type::Variant(Row {
            entries: vec![literal_string("\"one\"")],
            tail: RowTail::Var(right_tail),
        });

        assert_eq!(unifier.unify(&left, &right), Err(()));
        assert_eq!(unifier.resolve(&left), left);
    }

    #[test]
    fn open_literal_rows_widen_to_matching_base_types() {
        let mut unifier = Unifier::default();
        let tail = unifier.fresh_row_var();
        let open_text = Type::Variant(Row {
            entries: vec![literal_string("\"hi\"")],
            tail: RowTail::Var(tail),
        });
        let closed_text = Type::Variant(Row {
            entries: vec![literal_string("\"hi\"")],
            tail: RowTail::Closed,
        });
        let numeric = unifier.fresh_numeric();
        let number_tail = unifier.fresh_row_var();
        let open_number = Type::Variant(Row {
            entries: vec![literal_number("1")],
            tail: RowTail::Var(number_tail),
        });

        assert_eq!(unifier.unify(&open_text, &named("Text")), Ok(()));
        assert_eq!(unifier.unify(&closed_text, &named("Text")), Err(()));
        assert_eq!(unifier.unify(&open_number, &numeric), Ok(()));
        assert!(unifier.is_numeric_meta(&numeric));
    }

    #[test]
    fn variant_unification_checks_payloads_and_entry_kinds() {
        let mut unifier = Unifier::default();
        let int_tag = Type::Variant(Row {
            entries: vec![tag("Ok", vec![named("Int")])],
            tail: RowTail::Closed,
        });
        let text_tag = Type::Variant(Row {
            entries: vec![tag("Ok", vec![named("Text")])],
            tail: RowTail::Closed,
        });
        let arity_mismatch = Type::Variant(Row {
            entries: vec![tag("Ok", vec![named("Int"), named("Text")])],
            tail: RowTail::Closed,
        });

        assert_eq!(unifier.unify(&int_tag, &text_tag), Err(()));
        assert_eq!(unifier.unify(&int_tag, &arity_mismatch), Err(()));
        assert_eq!(
            unifier.unify(
                &Type::Variant(Row {
                    entries: vec![field("Ok", named("Int"))],
                    tail: RowTail::Closed,
                }),
                &int_tag,
            ),
            Err(())
        );
    }

    #[test]
    fn variant_unification_closes_sound_rows_and_rejects_extra_tags() {
        let mut unifier = Unifier::default();
        let tail = unifier.fresh_row_var();
        let open = Type::Variant(Row {
            entries: vec![tag("Zero", Vec::new()), tag("Pos", Vec::new())],
            tail: RowTail::Var(tail),
        });
        let closed = Type::Variant(Row {
            entries: vec![tag("Zero", Vec::new()), tag("Pos", Vec::new())],
            tail: RowTail::Closed,
        });

        assert_eq!(unifier.unify(&open, &closed), Ok(()));
        assert_eq!(unifier.resolve(&open), closed);

        let mut unifier = Unifier::default();
        let tail = unifier.fresh_row_var();
        let too_wide = Type::Variant(Row {
            entries: vec![tag("Zero", Vec::new()), tag("Pos", Vec::new())],
            tail: RowTail::Var(tail),
        });
        let only_zero = Type::Variant(Row {
            entries: vec![tag("Zero", Vec::new())],
            tail: RowTail::Closed,
        });

        assert_eq!(unifier.unify(&too_wide, &only_zero), Err(()));
    }

    #[test]
    fn occurs_check_still_rejects_an_unrelated_meta_cycle() {
        let mut unifier = Unifier::default();
        let meta = unifier.fresh();

        assert_eq!(
            unifier.unify(&meta, &Type::Tuple(vec![named("Int"), meta.clone()])),
            Err(())
        );
        assert_eq!(unifier.resolve(&meta), meta);
    }
}
