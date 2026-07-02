mod checker;
mod comptime;
mod env;
mod host_comptime;
mod lower;
mod ty;
mod unify;

use aven_core::{Diagnostic, Span};
use aven_parser::{Expr, Module};

pub use host_comptime::{
    ComptimeArg, ComptimeError, HostComptimeFn, HostComptimeFnSpec, HostGlobals,
};
pub use lower::{AnnotationLowerer, DeclaredAnnotation, TypeLowering};
pub use ty::build;
pub use ty::{
    RecordField, Row, RowEntry, RowTail, Type, function_required_arity, function_signature,
    literal_union_members, record_fields, render_type, variant_tags,
};

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
    "JsonError",
    "Map",
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
            .filter(|inferred| type_span_contains(inferred.name_span, span))
            .min_by_key(|inferred| inferred.name_span.len())
            .map(|inferred| &inferred.ty)
    }
}

fn type_span_contains(outer: Span, inner: Span) -> bool {
    let outer_end = outer.end.max(outer.start.saturating_add(1));
    let inner_end = inner.end.max(inner.start.saturating_add(1));
    inner.start >= outer.start && inner_end <= outer_end
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
    check_module_with_globals(module, &[])
}

/// Check `module` with a set of host/library globals seeded into the top-level
/// value environment. Each `(name, ty)` is a monomorphic value available to the
/// module unless a user top-level declaration shadows it. Free references to a
/// seeded name are checked by the existing call/field/arity machinery.
pub fn check_module_with_globals(module: &Module, globals: &[(String, Type)]) -> CheckOutput {
    check_module_with_host_globals(module, &HostGlobals::types_only(globals))
}

/// Check `module` with host/library globals and host comptime resolvers. The
/// ordinary type globals bind names and validate arguments; resolver entries can
/// override a registered call's result type when the listed arguments are known
/// at compile time.
pub fn check_module_with_host_globals(module: &Module, globals: &HostGlobals) -> CheckOutput {
    let mut known_types = known_type_names(module);
    known_types.extend(
        globals
            .type_definitions
            .iter()
            .map(|(name, _)| name.clone()),
    );
    let mut type_definitions = type_definitions(module, &known_types);
    let alias_diagnostics = cyclic_alias_diagnostics(module, &type_definitions);
    for (name, ty) in &globals.type_definitions {
        type_definitions
            .entry(name.clone())
            .or_insert_with(|| ty.clone());
    }
    let mut checker =
        Checker::with_module_and_host_globals(known_types, type_definitions, module, globals);

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
