mod checker;
mod comptime;
mod env;
mod host_comptime;
mod lower;
mod ty;
mod unify;

use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Span};
use aven_parser::{Expr, ExprKind, Item, Module, RecordEntry};

pub use comptime::ComptimeExport;
pub use host_comptime::{
    ComptimeArg, ComptimeError, HostComptimeFn, HostComptimeFnSpec, HostComptimeParam, HostGlobals,
    HostStatics,
};
pub use lower::{AnnotationLowerer, DeclaredAnnotation, TypeLowering};
pub use ty::build;
pub use ty::{
    RecordField, Row, RowEntry, RowTail, Type, function_required_arity, function_signature,
    is_text_type, literal_union_members, record_fields, render_type, type_contains_deferred,
    variant_tags,
};

/// Builtin comptime type functions. Shared with tooling (LSP hover) so the
/// checker's name binding and the hover descriptions cannot drift apart.
pub const COMPTIME_BUILTIN_FUNCTIONS: &[&str] = &["keysOf", "tagsOf", "typeOf", "pick", "omit"];

pub(crate) use checker::Checker;
pub(crate) use lower::{
    cyclic_alias_diagnostics, known_type_names, reserved_type_diagnostic, type_definitions,
    type_definitions_excluding,
};

const BUILTIN_TYPES: &[&str] = &[
    "Bool",
    "Float",
    "Int",
    "Null",
    "Text",
    "Type",
    "Undefined",
    "Unit",
    // Seeded std names until import resolution provides them.
    "Array",
    "Data",
    "Json",
    "JsonError",
    "Map",
    "Result",
    "Set",
    "Toml",
    "TomlError",
    "Yaml",
    "YamlError",
];

const CHECKED_NAMED_TYPES: &[&str] = &["Bool", "Float", "Int", "Null", "Text", "Undefined", "Unit"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
    pub inferred_types: Vec<InferredType>,
    pub type_definitions: HashMap<String, Type>,
    pub top_level_types: HashMap<String, Type>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleImports {
    types: HashMap<String, Option<Type>>,
    type_exports: HashMap<String, HashMap<String, Type>>,
    /// Comptime-evaluable function exports keyed by import specifier then export
    /// name. Carries owned AST so importers can specialize type applications
    /// such as `pair(Int)` without borrowing the dependency's module.
    comptime_exports: HashMap<String, HashMap<String, ComptimeExport>>,
}

impl ModuleImports {
    pub fn new(types: impl IntoIterator<Item = (String, Type)>) -> Self {
        Self {
            types: types
                .into_iter()
                .map(|(specifier, ty)| (specifier, Some(ty)))
                .collect(),
            type_exports: HashMap::new(),
            comptime_exports: HashMap::new(),
        }
    }

    pub fn with_failed(specifiers: impl IntoIterator<Item = String>) -> Self {
        Self {
            types: specifiers
                .into_iter()
                .map(|specifier| (specifier, None))
                .collect(),
            type_exports: HashMap::new(),
            comptime_exports: HashMap::new(),
        }
    }

    pub fn insert(&mut self, specifier: impl Into<String>, ty: Type) {
        self.types.insert(specifier.into(), Some(ty));
    }

    pub fn insert_failed(&mut self, specifier: impl Into<String>) {
        self.types.insert(specifier.into(), None);
    }

    pub fn insert_type_exports(
        &mut self,
        specifier: impl Into<String>,
        exports: HashMap<String, Type>,
    ) {
        self.type_exports.insert(specifier.into(), exports);
    }

    pub fn insert_comptime_exports(
        &mut self,
        specifier: impl Into<String>,
        exports: HashMap<String, ComptimeExport>,
    ) {
        self.comptime_exports.insert(specifier.into(), exports);
    }

    pub fn get(&self, specifier: &str) -> Option<Option<&Type>> {
        self.types.get(specifier).map(Option::as_ref)
    }

    pub fn type_export(&self, specifier: &str, name: &str) -> Option<&Type> {
        self.type_exports.get(specifier)?.get(name)
    }

    pub fn comptime_export(&self, specifier: &str, name: &str) -> Option<&ComptimeExport> {
        self.comptime_exports.get(specifier)?.get(name)
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

/// The statics a named type carries, as record-like `(name, type)` fields:
/// compiler builtins (`Map.empty`/`Map.from`) merged with the host-registered
/// statics in `globals`. Tooling (completion, hover) reads these to present a
/// type's statics the same way it presents record fields. `None` when `name`
/// carries no statics.
pub fn type_statics(globals: &HostGlobals, name: &str) -> Option<Vec<RecordField>> {
    checker::builtin_type_statics()
        .into_iter()
        .chain(globals.statics.iter().cloned())
        .find(|(type_name, _)| type_name == name)
        .map(|(_, members)| {
            members
                .into_iter()
                .map(|(name, ty)| RecordField { name, ty })
                .collect()
        })
}

/// Check `module` with host/library globals and host comptime resolvers. The
/// ordinary type globals bind names and validate arguments; resolver entries can
/// override a registered call's result type when the listed arguments are known
/// at compile time.
pub fn check_module_with_host_globals(module: &Module, globals: &HostGlobals) -> CheckOutput {
    check_module_with_host_globals_and_imports(module, globals, &ModuleImports::default())
}

pub fn check_module_with_host_globals_and_imports(
    module: &Module,
    globals: &HostGlobals,
    imports: &ModuleImports,
) -> CheckOutput {
    let mut reserved_type_names: HashSet<_> = BUILTIN_TYPES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    reserved_type_names.extend(
        globals
            .type_definitions
            .iter()
            .map(|(name, _)| name.clone()),
    );
    let mut known_types = known_type_names(module);
    known_types.extend(
        globals
            .type_definitions
            .iter()
            .map(|(name, _)| name.clone()),
    );
    let mut reserved_diagnostics = aven_parser::collect_declarations(module)
        .into_iter()
        .filter(|declaration| declaration.phase == aven_parser::DeclarationPhase::Comptime)
        .filter(|declaration| reserved_type_names.contains(&declaration.name))
        .filter_map(|declaration| {
            let binding = lower::binding_for_declaration(module, &declaration)?;
            (!checker::is_import_call(&binding.value))
                .then(|| reserved_type_diagnostic(&declaration.name, declaration.name_span))
        })
        .collect::<Vec<_>>();
    // Host-global and pattern-imported type definitions must be visible while
    // comptime bindings evaluate, so a local comptime type function applied to
    // an imported type (`Draft = partial(User)` with `{ User } = import(...)`)
    // reifies instead of silently deferring.
    let mut seed_definitions: HashMap<String, Type> = globals
        .type_definitions
        .iter()
        .map(|(name, ty)| (name.clone(), ty.clone()))
        .collect();
    for item in &module.items {
        let Item::PatternBinding(binding) = item else {
            continue;
        };
        if !checker::is_import_call(&binding.value) {
            continue;
        }
        let Some(specifier) = aven_parser::static_import_specifier(&binding.value) else {
            continue;
        };
        let Some(entries) = record_pattern_entries(&binding.pattern) else {
            continue;
        };
        for entry in entries {
            let (source, target, target_span) = match entry {
                RecordEntry::Shorthand {
                    name, name_span, ..
                } => (name, name, *name_span),
                RecordEntry::Rename {
                    from, to, to_span, ..
                } => (from, to, *to_span),
                _ => continue,
            };
            if let Some(ty) = imports.type_export(&specifier, source) {
                if reserved_type_names.contains(target) {
                    // Extracting a host re-export under its own name rebinds
                    // the same definition (`{ Instant } = import("std/time")`)
                    // — only a *different* definition shadows the reserved
                    // name.
                    let rebinds_host_type = globals
                        .type_definitions
                        .iter()
                        .any(|(name, host_ty)| name == target && host_ty == ty);
                    if !rebinds_host_type {
                        reserved_diagnostics.push(reserved_type_diagnostic(target, target_span));
                    }
                    continue;
                }
                known_types.insert(target.clone());
                seed_definitions.insert(target.clone(), ty.clone());
            }
        }
    }
    let type_definitions = type_definitions_excluding(
        module,
        &known_types,
        &reserved_type_names,
        imports,
        &seed_definitions,
    );
    let alias_diagnostics = cyclic_alias_diagnostics(module, &type_definitions);
    let mut checker = Checker::with_module_and_host_globals_and_imports(
        known_types,
        type_definitions.clone(),
        module,
        globals,
        imports,
    );

    checker.diagnostics.extend(alias_diagnostics);
    checker.diagnostics.extend(reserved_diagnostics);
    checker.check_module(module);
    let export_names = final_record_names(module);
    let top_level_types = aven_parser::collect_declarations(module)
        .into_iter()
        .filter(|declaration| export_names.contains(&declaration.name))
        .filter_map(|declaration| {
            checker
                .infer_top_level_value_for_output(&declaration.name)
                .map(|ty| (declaration.name, ty))
        })
        .collect();

    CheckOutput {
        diagnostics: checker.diagnostics,
        inferred_types: checker.inferred_types,
        type_definitions,
        top_level_types,
    }
}

fn final_record_names(module: &Module) -> HashSet<String> {
    let Some(Item::Expr(expr)) = module.items.last() else {
        return HashSet::new();
    };
    let ExprKind::Record(entries) = &expr.kind else {
        return HashSet::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            RecordEntry::Field { name, .. } | RecordEntry::Shorthand { name, .. } => {
                Some(name.clone())
            }
            RecordEntry::Rename { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect()
}

fn record_pattern_entries(expr: &Expr) -> Option<&[RecordEntry]> {
    match &expr.kind {
        ExprKind::Record(entries) => Some(entries),
        ExprKind::Group(inner) => record_pattern_entries(inner),
        _ => None,
    }
}

/// Return whether `actual` fits `expected` at a normal checking boundary.
///
/// Host comptime resolvers use this when they need to validate reified types
/// with the same literal-row widening and subsumption rules as annotations and
/// call arguments.
pub fn type_fits_boundary(expected: &Type, actual: &Type) -> bool {
    let known_types = BUILTIN_TYPES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let mut checker = Checker::with_type_definitions(known_types, Default::default());
    checker.type_fits_boundary_without_reporting(expected, actual)
}

pub fn lower_annotation(module: &Module, annotation: &Expr) -> TypeLowering {
    let known_types = known_type_names(module);
    let type_definitions = type_definitions(module, &known_types);
    let mut checker = Checker::with_module_environment(known_types, type_definitions, module);

    checker.lower_annotation_with_diagnostics(annotation)
}

#[cfg(test)]
mod tests;
