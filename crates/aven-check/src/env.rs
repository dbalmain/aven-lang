use std::collections::{HashMap, HashSet};

use aven_parser::Expr;

use crate::checker::knowledge::Known;
use crate::comptime::ExecutionContext;
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
    ///
    /// Ordered, and shadowing appends rather than overwrites, because a local
    /// definition is a *lexical* thing: `get = () => x` written between
    /// `x = 1` and `x := 2` captures the first `x`, and a map keyed by name
    /// can only remember the last one. See `local_definition_layers`.
    values: Vec<Vec<(String, LocalValue)>>,
    /// Compile-time evidence for a binding's value, scoped alongside `scopes`
    /// so a proof cannot outlive the binding that earned it.
    ///
    /// `None` is a *mask*, not an absence: a parameter, a match binder, or a
    /// rebinding whose value proves nothing all record one, so that a lookup
    /// stops at the nearest binding of the name instead of reading a proof
    /// about some earlier, unrelated binding that happened to share a
    /// spelling.
    proofs: Vec<HashMap<String, Option<(ExecutionContext, Known)>>>,
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
    /// The binding's declared type, when it has one. Kept because a demand
    /// has to know whether evaluating this binding could involve a primitive
    /// family, and that is written in the annotation rather than the value.
    pub(crate) annotation: Option<Expr>,
    /// Written with `:=`. The initializer runs before this binding exists, so
    /// it resolves the binding being shadowed --- `x := x + 1` reads the
    /// previous `x` --- rather than itself.
    pub(crate) shadows: bool,
}

impl LocalTypeScopes {
    pub(crate) fn push(&mut self) {
        self.scopes.push(HashMap::new());
        self.pins.push(HashMap::new());
        self.values.push(Vec::new());
        self.proofs.push(HashMap::new());
    }

    pub(crate) fn pop(&mut self) {
        self.scopes.pop();
        self.pins.pop();
        self.values.pop();
        self.proofs.pop();
    }

    /// Open a scope for local *values* only. `infer_block` walks a block
    /// without pushing a type scope — its names live in a cloned `TypeEnv` —
    /// but its bindings are still evaluable definitions, and a demand reached
    /// during inference must see them exactly as one reached during checking.
    pub(crate) fn push_values(&mut self) {
        self.values.push(Vec::new());
    }

    pub(crate) fn pop_values(&mut self) {
        self.values.pop();
    }

    pub(crate) fn define_value(
        &mut self,
        name: &str,
        initializer: Expr,
        annotation: Option<Expr>,
        shadows: bool,
    ) {
        if name == "_" {
            return;
        }
        if let Some(scope) = self.values.last_mut() {
            scope.push((
                name.to_owned(),
                LocalValue {
                    initializer,
                    annotation,
                    shadows,
                },
            ));
        }
    }

    /// Every local value in scope, in the order it was written: outermost
    /// scope first, and within a scope, earliest binding first. A shadowed
    /// name appears more than once, and both entries matter --- the earlier
    /// one is what anything defined between them captured.
    pub(crate) fn values_in_scope(&self) -> impl Iterator<Item = (&String, &LocalValue)> {
        self.values
            .iter()
            .flat_map(|scope| scope.iter().map(|(name, value)| (name, value)))
    }

    pub(crate) fn define_pin(&mut self, name: &str, value: Expr) {
        if let Some(scope) = self.pins.last_mut() {
            scope.insert(name.to_owned(), value);
        }
    }

    pub(crate) fn pin(&self, name: &str) -> Option<&Expr> {
        self.pins.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Introduce a local name.
    ///
    /// Every local name --- binding, parameter, match binder, comprehension
    /// binder --- arrives here, which is why this is also where a stale proof
    /// is masked. A name that has just been bound to something new cannot go
    /// on answering demands with what an older binding of the same spelling
    /// was proved to hold. A binding that does prove something re-records it
    /// afterwards, through `propagate_local_binding_knowledge`.
    ///
    /// That propagation is itself total --- it records a mask when the new
    /// value proves nothing --- so for an ordinary binding the two overlap,
    /// and neutering either one alone leaves the tests green. The overlap is
    /// deliberate: propagation covers only `Binding`s, and this covers every
    /// other way a name is introduced, so the invariant holds here rather than
    /// depending on a caller remembering to re-establish it.
    pub(crate) fn define(&mut self, name: &str, ty: LocalValueType) {
        if name == "_" {
            return;
        }

        self.mask_proof(name);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_owned(), ty);
        }
    }

    /// Record that this name's nearest binding proves nothing.
    pub(crate) fn mask_proof(&mut self, name: &str) {
        if let Some(scope) = self.proofs.last_mut() {
            scope.insert(name.to_owned(), None);
        }
    }

    pub(crate) fn define_proof(&mut self, name: &str, origin: ExecutionContext, known: Known) {
        if let Some(scope) = self.proofs.last_mut() {
            scope.insert(name.to_owned(), Some((origin, known)));
        }
    }

    /// The nearest binding's evidence, or `None` when the nearest binding of
    /// this name has none. The distinction from "no binding at all" is the
    /// point: an outer proof must not show through an inner binding.
    pub(crate) fn proof(&self, name: &str) -> Option<Option<&(ExecutionContext, Known)>> {
        self.proofs
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .map(Option::as_ref)
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
