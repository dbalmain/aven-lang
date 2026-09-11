use std::collections::{HashMap, HashSet};

use aven_parser::Expr;

use crate::{
    Type,
    ty::{self, TypeScheme},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalValueType {
    Known(Type),
    Scheme(TypeScheme),
    Unknown,
}

pub(crate) type TypeEnv = HashMap<String, LocalValueType>;

#[derive(Debug, Default)]
pub(crate) struct LocalTypeScopes {
    scopes: Vec<TypeEnv>,
    /// Right-hand sides of local `name = comptime(value)` bindings, scoped
    /// alongside `scopes` so every push/pop site covers both.
    pins: Vec<HashMap<String, Expr>>,
    /// Initializers of ordinary local bindings, scoped alongside `scopes` so
    /// every push/pop site covers all three.
    values: Vec<HashMap<String, LocalValue>>,
}

/// An ordinary local binding's initializer. A comptime demand may evaluate one
/// of these; a runtime *parameter* may not, which is why parameters are never
/// recorded and stay blocked from evaluation.
///
/// There is no initialization boundary here, and that is deliberate. A module
/// binding needs one because a demand can reach it from anywhere in the file.
/// A local is registered as its block is walked, in source order, so a demand
/// only ever sees the locals written above it — ordering is enforced by when
/// the binding enters scope rather than by comparing offsets.
#[derive(Debug, Clone)]
pub(crate) struct LocalValue {
    pub(crate) initializer: Expr,
}

impl LocalTypeScopes {
    pub(crate) fn push(&mut self) {
        self.scopes.push(HashMap::new());
        self.pins.push(HashMap::new());
        self.values.push(HashMap::new());
    }

    pub(crate) fn pop(&mut self) {
        self.scopes.pop();
        self.pins.pop();
        self.values.pop();
    }

    /// Open a scope for local *values* only. `infer_block` walks a block
    /// without pushing a type scope — its names live in a cloned `TypeEnv` —
    /// but its bindings are still evaluable definitions, and a demand reached
    /// during inference must see them exactly as one reached during checking.
    pub(crate) fn push_values(&mut self) {
        self.values.push(HashMap::new());
    }

    pub(crate) fn pop_values(&mut self) {
        self.values.pop();
    }

    pub(crate) fn define_value(&mut self, name: &str, initializer: Expr) {
        if name == "_" {
            return;
        }
        if let Some(scope) = self.values.last_mut() {
            scope.insert(name.to_owned(), LocalValue { initializer });
        }
    }

    /// Every local value in scope, outermost first, so inserting them in order
    /// leaves the nearest binding of a shadowed name in place.
    pub(crate) fn values_in_scope(&self) -> impl Iterator<Item = (&String, &LocalValue)> {
        self.values.iter().flat_map(HashMap::iter)
    }

    pub(crate) fn define_pin(&mut self, name: &str, value: Expr) {
        if let Some(scope) = self.pins.last_mut() {
            scope.insert(name.to_owned(), value);
        }
    }

    pub(crate) fn pin(&self, name: &str) -> Option<&Expr> {
        self.pins.iter().rev().find_map(|scope| scope.get(name))
    }

    pub(crate) fn define(&mut self, name: &str, ty: LocalValueType) {
        if name == "_" {
            return;
        }

        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_owned(), ty);
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&LocalValueType> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    pub(crate) fn free_metas(&self, resolve: impl FnMut(&Type) -> Type) -> Vec<u32> {
        free_metas_in_local_values(self.scopes.iter().flat_map(|scope| scope.values()), resolve)
    }

    pub(crate) fn free_row_vars(&self, resolve: impl FnMut(&Type) -> Type) -> Vec<u32> {
        free_row_vars_in_local_values(self.scopes.iter().flat_map(|scope| scope.values()), resolve)
    }

    pub(crate) fn inference_env(&self) -> TypeEnv {
        let mut env = TypeEnv::new();
        for scope in &self.scopes {
            env.extend(scope.clone());
        }
        env
    }
}

pub(crate) fn free_metas_in_local_values<'a>(
    values: impl IntoIterator<Item = &'a LocalValueType>,
    mut resolve: impl FnMut(&Type) -> Type,
) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut metas = Vec::new();

    for value in values {
        let (ty, quantified) = match value {
            LocalValueType::Known(ty) => (ty, &[][..]),
            LocalValueType::Scheme(scheme) => (&scheme.ty, scheme.vars.as_slice()),
            LocalValueType::Unknown => continue,
        };

        for id in ty::free_metas(&resolve(ty)) {
            if !quantified.contains(&id) && seen.insert(id) {
                metas.push(id);
            }
        }
    }

    metas
}

pub(crate) fn free_row_vars_in_local_values<'a>(
    values: impl IntoIterator<Item = &'a LocalValueType>,
    mut resolve: impl FnMut(&Type) -> Type,
) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut row_vars = Vec::new();

    for value in values {
        let (ty, quantified) = match value {
            LocalValueType::Known(ty) => (ty, &[][..]),
            LocalValueType::Scheme(scheme) => (&scheme.ty, scheme.row_vars.as_slice()),
            LocalValueType::Unknown => continue,
        };

        for id in ty::free_row_vars(&resolve(ty)) {
            if !quantified.contains(&id) && seen.insert(id) {
                row_vars.push(id);
            }
        }
    }

    row_vars
}
