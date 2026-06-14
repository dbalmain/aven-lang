use std::collections::{HashMap, HashSet};

use crate::ty::{Type, TypeScheme, map_type, type_contains_meta};

#[derive(Debug, Default)]
pub(crate) struct Unifier {
    substitution: Vec<Option<Type>>,
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

    pub(crate) fn resolve(&self, ty: &Type) -> Type {
        map_type(ty, &mut |node| match node {
            Type::Meta(id) => match self.substitution.get(*id as usize) {
                Some(Some(bound)) => Some(self.resolve(bound)),
                _ => None,
            },
            _ => None,
        })
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
    pub(crate) fn snapshot(&self) -> (Vec<Option<Type>>, HashSet<u32>) {
        (self.substitution.clone(), self.numeric.clone())
    }

    pub(crate) fn restore(&mut self, snapshot: (Vec<Option<Type>>, HashSet<u32>)) {
        (self.substitution, self.numeric) = snapshot;
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

    pub(crate) fn instantiate_scheme(&mut self, scheme: &TypeScheme) -> Type {
        let mut replacements: HashMap<u32, Type> = HashMap::new();
        for id in &scheme.vars {
            replacements.insert(*id, self.fresh());
        }

        map_type(&scheme.ty, &mut |node| match node {
            Type::Meta(id) => replacements.get(id).cloned(),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Type {
        Type::Named(name.to_owned())
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
}
