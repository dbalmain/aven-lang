use std::collections::{HashMap, HashSet};

use crate::ty::{
    Row, RowEntry, RowTail, Type, TypeScheme, free_row_vars, map_type, map_type_with_rows,
    render_literal_value, type_contains_meta,
};

#[derive(Debug, Default)]
pub(crate) struct Unifier {
    substitution: Vec<Option<Type>>,
    row_subst: Vec<Option<Row>>,
    numeric: HashSet<u32>,
}

#[derive(Clone)]
pub(crate) struct UnifierSnapshot {
    substitution: Vec<Option<Type>>,
    row_subst: Vec<Option<Row>>,
    numeric: HashSet<u32>,
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

    pub(crate) fn resolve(&self, ty: &Type) -> Type {
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
                    .and_then(|bound| bound.clone()),
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
            numeric: self.numeric.clone(),
        }
    }

    pub(crate) fn restore(&mut self, snapshot: UnifierSnapshot) {
        self.substitution = snapshot.substitution;
        self.row_subst = snapshot.row_subst;
        self.numeric = snapshot.numeric;
    }

    pub(crate) fn unify(&mut self, left: &Type, right: &Type) -> Result<(), ()> {
        let snapshot = self.snapshot();
        if self.unify_inner(left, right).is_err() {
            self.restore(snapshot);
            Err(())
        } else {
            Ok(())
        }
    }

    fn unify_inner(&mut self, left: &Type, right: &Type) -> Result<(), ()> {
        let left = self.resolve(left);
        let right = self.resolve(right);

        match (&left, &right) {
            (Type::Meta(left), Type::Meta(right)) if left == right => Ok(()),
            (Type::Meta(id), ty) | (ty, Type::Meta(id)) => self.bind(*id, ty),
            (Type::Named(left), Type::Named(right)) if left == right => Ok(()),
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
                self.unify_inner(left_callee, right_callee)?;
                self.unify_many(left_args, right_args)
            }
            (
                Type::Function {
                    params: left_params,
                    result: left_result,
                },
                Type::Function {
                    params: right_params,
                    result: right_result,
                },
            ) if left_params.len() == right_params.len() => {
                self.unify_many(left_params, right_params)?;
                self.unify_inner(left_result, right_result)
            }
            (Type::Nullable(left), Type::Nullable(right)) => self.unify_inner(left, right),
            (Type::Tuple(left), Type::Tuple(right)) if left.len() == right.len() => {
                self.unify_many(left, right)
            }
            (Type::Record(left), Type::Record(right)) => self.unify_rows(left, right),
            (Type::Variant(left), Type::Variant(right)) => self.unify_rows(left, right),
            _ => Err(()),
        }
    }

    fn unify_many(&mut self, left: &[Type], right: &[Type]) -> Result<(), ()> {
        for (left, right) in left.iter().zip(right) {
            self.unify_inner(left, right)?;
        }
        Ok(())
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

    fn unify_rows(&mut self, left: &Row, right: &Row) -> Result<(), ()> {
        let left = self.resolve_row(left);
        let right = self.resolve_row(right);
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
            self.unify_row_entries(&left_entry, &right_entry)?;
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
            return self.unify_rows(&resolved_left, &resolved_right);
        }

        let right_tail = self.supply_entries(right_remainder.tail, &left_remainder.entries)?;
        let left_tail = self.supply_entries(left_remainder.tail, &right_remainder.entries)?;
        self.unify_row_tails(left_tail, right_tail)
    }

    fn unify_row_entries(&mut self, left: &RowEntry, right: &RowEntry) -> Result<(), ()> {
        match (left, right) {
            (RowEntry::Field { ty: left_type, .. }, RowEntry::Field { ty: right_type, .. }) => {
                self.unify_inner(left_type, right_type)
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
                self.unify_many(left_payload, right_payload)
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

    pub(crate) fn instantiate_scheme(&mut self, scheme: &TypeScheme) -> Type {
        let mut replacements: HashMap<u32, Type> = HashMap::new();
        for id in &scheme.vars {
            replacements.insert(*id, self.fresh());
        }

        let mut row_replacements: HashMap<u32, u32> = HashMap::new();
        for id in &scheme.row_vars {
            row_replacements.insert(*id, self.fresh_row_var());
        }

        map_type_with_rows(
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
        )
    }
}

fn row_entry_label(entry: &RowEntry) -> &str {
    match entry {
        RowEntry::Field { name, .. } | RowEntry::Tag { name, .. } => name,
        RowEntry::Literal { value } => render_literal_value(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Type {
        Type::Named(name.to_owned())
    }

    fn field(name: &str, ty: Type) -> RowEntry {
        RowEntry::Field {
            name: name.to_owned(),
            ty,
            optional: false,
        }
    }

    fn tag(name: &str, payload: Vec<Type>) -> RowEntry {
        RowEntry::Tag {
            name: name.to_owned(),
            payload,
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
}
