use std::collections::{HashMap, HashSet};

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
}

impl LocalTypeScopes {
    pub(crate) fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop(&mut self) {
        self.scopes.pop();
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

    pub(crate) fn free_metas(&self) -> Vec<u32> {
        free_metas_in_local_values(self.scopes.iter().flat_map(|scope| scope.values()))
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
) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut metas = Vec::new();

    for value in values {
        let (ty, quantified) = match value {
            LocalValueType::Known(ty) => (ty, &[][..]),
            LocalValueType::Scheme(scheme) => (&scheme.ty, scheme.vars.as_slice()),
            LocalValueType::Unknown => continue,
        };

        for id in ty::free_metas(ty) {
            if !quantified.contains(&id) && seen.insert(id) {
                metas.push(id);
            }
        }
    }

    metas
}
