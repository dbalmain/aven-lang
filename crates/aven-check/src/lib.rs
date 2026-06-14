mod checker;
mod env;
mod lower;
mod ty;
mod unify;

use aven_core::Diagnostic;
use aven_parser::{Expr, Module};

pub use lower::{AnnotationLowerer, DeclaredAnnotation, TypeLowering};
pub use ty::{Type, TypeRowEntry};

pub(crate) use checker::Checker;
pub(crate) use lower::{known_type_names, type_definitions};

const BUILTIN_TYPES: &[&str] = &[
    "Bool", "Float", "Int", "Nil", "Text", "Unit",
    // Seeded std names until import resolution provides them.
    "Array", "Json", "Result", "Set", "Yaml",
];

const CHECKED_NAMED_TYPES: &[&str] = &["Bool", "Float", "Int", "Nil", "Text", "Unit"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
}

pub fn check_module(module: &Module) -> CheckOutput {
    let known_types = known_type_names(module);
    let type_definitions = type_definitions(module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, module);

    checker.check_module(module);

    CheckOutput {
        diagnostics: checker.diagnostics,
    }
}

pub fn lower_annotation(module: &Module, annotation: &Expr) -> TypeLowering {
    let known_types = known_type_names(module);
    let mut checker = Checker::new(known_types);

    checker.lower_annotation_with_diagnostics(annotation)
}

#[cfg(test)]
mod tests;
