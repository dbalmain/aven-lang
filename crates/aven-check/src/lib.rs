mod checker;
mod comptime;
mod env;
mod lower;
mod ty;
mod unify;

use aven_core::{Diagnostic, Span};
use aven_parser::{Expr, Module};

pub use lower::{AnnotationLowerer, DeclaredAnnotation, TypeLowering};
pub use ty::{RecordField, Row, RowEntry, RowTail, Type, record_fields, render_type};

pub(crate) use checker::Checker;
pub(crate) use lower::{cyclic_alias_diagnostics, known_type_names, type_definitions};

const BUILTIN_TYPES: &[&str] = &[
    "Bool",
    "Float",
    "Int",
    "Null",
    "Text",
    "Undefined",
    "Unit",
    // Seeded std names until import resolution provides them.
    "Array",
    "Json",
    "Result",
    "Set",
    "Yaml",
];

const CHECKED_NAMED_TYPES: &[&str] = &["Bool", "Float", "Int", "Null", "Text", "Undefined", "Unit"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
    pub inferred_types: Vec<InferredType>,
}

impl CheckOutput {
    pub fn type_at(&self, span: Span) -> Option<&Type> {
        self.inferred_types
            .iter()
            .find(|inferred| inferred.name_span.contains(span))
            .map(|inferred| &inferred.ty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredType {
    pub name_span: Span,
    pub ty: Type,
}

impl InferredType {
    pub fn render(&self) -> String {
        self.ty.render()
    }
}

pub fn check_module(module: &Module) -> CheckOutput {
    let known_types = known_type_names(module);
    let type_definitions = type_definitions(module, &known_types);
    let alias_diagnostics = cyclic_alias_diagnostics(module, &type_definitions);
    let mut checker = Checker::with_module(known_types, type_definitions, module);

    checker.diagnostics.extend(alias_diagnostics);
    checker.check_module(module);

    CheckOutput {
        diagnostics: checker.diagnostics,
        inferred_types: checker.inferred_types,
    }
}

pub fn lower_annotation(module: &Module, annotation: &Expr) -> TypeLowering {
    let known_types = known_type_names(module);
    let type_definitions = type_definitions(module, &known_types);
    let mut checker = Checker::with_module_environment(known_types, type_definitions, module);

    checker.lower_annotation_with_diagnostics(annotation)
}

#[cfg(test)]
mod tests;
