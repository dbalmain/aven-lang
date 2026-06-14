use std::collections::HashMap;

use crate::Type;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalValueType {
    Known(Type),
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

    pub(crate) fn inference_env(&self) -> TypeEnv {
        let mut env = TypeEnv::new();
        for scope in &self.scopes {
            env.extend(scope.clone());
        }
        env
    }
}
