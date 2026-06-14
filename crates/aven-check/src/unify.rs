use std::collections::HashMap;

use crate::ty::{Type, map_type, type_contains_meta};

#[derive(Debug, Default)]
pub(crate) struct Unifier {
    substitution: Vec<Option<Type>>,
}

impl Unifier {
    pub(crate) fn fresh(&mut self) -> Type {
        let id = self.substitution.len() as u32;
        self.substitution.push(None);
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

    /// Capture the current substitution so a speculative sequence of
    /// unifications can be rolled back with [`Unifier::restore`].
    pub(crate) fn snapshot(&self) -> Vec<Option<Type>> {
        self.substitution.clone()
    }

    pub(crate) fn restore(&mut self, snapshot: Vec<Option<Type>>) {
        self.substitution = snapshot;
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

        let Some(slot) = self.substitution.get_mut(id as usize) else {
            return Err(());
        };
        *slot = Some(ty);
        Ok(())
    }

    pub(crate) fn instantiate(&mut self, ty: &Type) -> Type {
        // Memoized binding types are stored fully resolved, so any `Meta` left
        // here is a generic placeholder. Replacing each with a fresh meta lets a
        // top-level binding be applied at more than one type without its generics
        // leaking between uses.
        let mut replacements: HashMap<u32, Type> = HashMap::new();
        map_type(ty, &mut |node| match node {
            Type::Meta(id) => Some(if let Some(existing) = replacements.get(id) {
                existing.clone()
            } else {
                let fresh = self.fresh();
                replacements.insert(*id, fresh.clone());
                fresh
            }),
            _ => None,
        })
    }
}
