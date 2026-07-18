mod checker;
mod comptime;
mod env;
mod host_comptime;
mod lower;
mod productivity;
mod ty;
mod unify;

use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Span};
use aven_parser::{Expr, ExprKind, Item, Module, RecordEntry};

pub use comptime::{ComptimeExport, ComptimeModuleIdentity, ComptimeOrigin, SpecializationKey};
pub use host_comptime::{
    ComptimeArg, ComptimeError, HostComptimeFn, HostComptimeFnSpec, HostComptimeParam, HostGlobals,
    HostStatics,
};
pub use lower::{AnnotationLowerer, DeclaredAnnotation, TypeLowering};
pub use ty::build;
pub use ty::{
    MethodConstraint, QualifiedType, RecordField, RecursiveTypeId, Row, RowEntry, RowTail, Type,
    function_required_arity, function_signature, is_text_type, literal_union_members,
    record_fields, render_type, type_contains_deferred, variant_tags,
};

/// Clone the completed one-level head for a recursive reference. Nested
/// back-edge references in that head stay atomic.
pub fn unfold_recursive_type_once(ty: &Type, unfoldings: &HashMap<RecursiveTypeId, Type>) -> Type {
    match ty {
        Type::Recursive(id) => unfoldings.get(id).cloned().unwrap_or_else(|| ty.clone()),
        _ => ty.clone(),
    }
}

/// Builtin comptime type functions. Shared with tooling (LSP hover) so the
/// checker's name binding and the hover descriptions cannot drift apart.
pub const COMPTIME_BUILTIN_FUNCTIONS: &[&str] = &["keysOf", "tagsOf", "typeOf", "pick", "omit"];

pub(crate) use checker::Checker;
pub(crate) use lower::{known_type_names, reserved_type_diagnostic, type_definitions};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
    pub inferred_types: Vec<InferredType>,
    pub type_definitions: HashMap<String, Type>,
    pub top_level_types: HashMap<String, Type>,
    /// Exported value types with any method constraints reified alongside
    /// their ordinary type.
    pub top_level_qualified_types: HashMap<String, QualifiedType>,
    pub named_families: HashMap<String, NamedFamilyType>,
    pub named_family_aliases: HashMap<String, String>,
    /// Source-defined builtin methods visible after this module was checked.
    /// Ambient graph wiring seals and forwards this environment to user nodes.
    pub builtin_methods: BuiltinMethodEnvironment,
    /// Known-target value expressions that must materialize a slot-record at
    /// runtime. The evaluator applies these conversions after evaluating the
    /// expression at the recorded source span.
    pub slot_reifications: HashMap<Span, SlotReificationTarget>,
    /// Checked root coercions which the evaluator applies without changing the
    /// source AST. Primitive-family branding and widening are deliberately
    /// boundary-directed rather than HM equations.
    pub primitive_family_coercions: HashMap<Span, PrimitiveFamilyCoercion>,
    /// Completed one-level heads for parameterized recursive type references.
    /// Keeping these in a side map makes `Type::Recursive` a small atomic node
    /// while allowing checker consumers to unfold only at structural demands.
    pub recursive_type_unfoldings: HashMap<RecursiveTypeId, Type>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotReificationTarget {
    pub fields: Vec<String>,
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltinMethodEnvironment {
    methods: Vec<BuiltinMethodType>,
}

impl BuiltinMethodEnvironment {
    pub fn methods(&self) -> &[BuiltinMethodType] {
        &self.methods
    }

    pub fn extend(&mut self, methods: impl IntoIterator<Item = BuiltinMethodType>) {
        self.methods.extend(methods);
    }
}

/// Record-like method fields contributed by the sealed builtin-method
/// environment for a concrete receiver. This is the tooling view of the same
/// one-way owner-pattern match used by the checker: fixed pattern components
/// never refine receiver variables.
pub fn builtin_method_fields(
    environment: &BuiltinMethodEnvironment,
    receiver: &Type,
) -> Vec<RecordField> {
    let receiver = ty::map_type(receiver, &mut |node| {
        let Type::Variant(row) = node else {
            return None;
        };
        match ty::literal_variant_base(row) {
            Some(ty::LiteralBase::Bool) => Some(Type::Named("Bool".to_owned())),
            Some(ty::LiteralBase::Text) => Some(Type::Named("Text".to_owned())),
            Some(ty::LiteralBase::Number) => {
                let float = row.entries.iter().any(|entry| {
                    matches!(
                        entry,
                        RowEntry::Literal {
                            value: aven_parser::Literal::Number(number)
                        } if number.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
                    )
                });
                Some(Type::Named(if float { "Float" } else { "Int" }.to_owned()))
            }
            None => None,
        }
    });
    let mut fields = Vec::new();
    let mut seen = HashSet::new();

    for method in environment.methods() {
        if !seen.insert(method.member.clone()) {
            continue;
        }
        let variables = method.owner_variables.iter().cloned().collect();
        let Some(substitutions) =
            builtin_owner_pattern_bindings(&method.owner, &receiver, &variables)
        else {
            seen.remove(&method.member);
            continue;
        };
        let instantiate = |ty: &Type| {
            ty::map_type(ty, &mut |node| match node {
                Type::Variable(name) => substitutions.get(name).cloned(),
                _ => None,
            })
        };
        let params = method.params.iter().map(instantiate).collect::<Vec<_>>();
        let result = instantiate(&method.result);
        fields.push(RecordField {
            name: method.member.clone(),
            ty: Type::Function {
                required: params.len(),
                params,
                result: Box::new(result),
            },
        });
    }

    fields
}

fn builtin_owner_pattern_bindings(
    pattern: &Type,
    receiver: &Type,
    variables: &HashSet<String>,
) -> Option<HashMap<String, Type>> {
    let mut bindings = HashMap::new();
    builtin_owner_pattern_matches(pattern, receiver, variables, &mut bindings).then_some(bindings)
}

fn builtin_owner_pattern_matches(
    pattern: &Type,
    receiver: &Type,
    variables: &HashSet<String>,
    bindings: &mut HashMap<String, Type>,
) -> bool {
    match pattern {
        Type::Variable(name) if variables.contains(name) => match bindings.get(name) {
            Some(bound) => bound == receiver,
            None => {
                bindings.insert(name.clone(), receiver.clone());
                true
            }
        },
        Type::Apply { callee, args } => {
            let Type::Apply {
                callee: receiver_callee,
                args: receiver_args,
            } = receiver
            else {
                return false;
            };
            args.len() == receiver_args.len()
                && builtin_owner_pattern_matches(callee, receiver_callee, variables, bindings)
                && args.iter().zip(receiver_args).all(|(pattern, receiver)| {
                    builtin_owner_pattern_matches(pattern, receiver, variables, bindings)
                })
        }
        Type::Optional(inner) => matches!(receiver, Type::Optional(receiver) if
            builtin_owner_pattern_matches(inner, receiver, variables, bindings)),
        Type::Nullable(inner) => matches!(receiver, Type::Nullable(receiver) if
            builtin_owner_pattern_matches(inner, receiver, variables, bindings)),
        Type::Tuple(items) => {
            let Type::Tuple(receiver_items) = receiver else {
                return false;
            };
            items.len() == receiver_items.len()
                && items.iter().zip(receiver_items).all(|(pattern, receiver)| {
                    builtin_owner_pattern_matches(pattern, receiver, variables, bindings)
                })
        }
        _ => pattern == receiver,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinMethodType {
    pub owner: Type,
    pub owner_variables: Vec<String>,
    pub member: String,
    pub params: Vec<Type>,
    pub result: Type,
    pub constraints: Vec<MethodConstraint>,
    pub owner_span: Span,
    pub member_span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedMethodType {
    pub params: Vec<Type>,
    pub result: Type,
    pub constraints: Vec<MethodConstraint>,
    pub variables: Vec<String>,
    pub origin: NamedMethodOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedMethodOrigin {
    Declared,
    Override {
        base_owner: Type,
        base_member: String,
    },
    Inherited {
        base_owner: Type,
        base_member: String,
        lifted_params: Vec<bool>,
        lifted_result: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedFamilyType {
    pub owner: String,
    pub data: Row,
    pub defaulted_fields: HashSet<String>,
    /// `Some(B)` for a named primitive-base family and `None` for an existing
    /// named record family. The payload remains the exact normalized builtin.
    pub primitive_base: Option<Type>,
    pub methods: HashMap<String, NamedMethodType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimitiveFamilyCoercion {
    Brand { owner: String },
    Widen,
}

impl CheckOutput {
    pub fn inferred_type_at(&self, span: Span) -> Option<&InferredType> {
        self.inferred_types
            .iter()
            .filter(|inferred| type_span_contains(inferred.name_span, span))
            .min_by_key(|inferred| inferred.name_span.len())
    }

    pub fn type_at(&self, span: Span) -> Option<&Type> {
        self.inferred_type_at(span).map(|inferred| &inferred.ty)
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
    pub qualified: Option<String>,
}

impl InferredType {
    pub fn render(&self) -> String {
        self.qualified.clone().unwrap_or_else(|| self.ty.render())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleImports {
    types: HashMap<String, Option<Type>>,
    type_exports: HashMap<String, HashMap<String, Type>>,
    qualified_exports: HashMap<String, HashMap<String, QualifiedType>>,
    named_family_exports: HashMap<String, HashMap<String, NamedFamilyType>>,
    /// Comptime-evaluable function exports keyed by import specifier then export
    /// name. Carries owned AST so importers can specialize type applications
    /// such as `pair(Int)` without borrowing the dependency's module.
    comptime_exports: HashMap<String, HashMap<String, ComptimeExport>>,
    recursive_type_unfoldings: HashMap<RecursiveTypeId, Type>,
    builtin_methods: BuiltinMethodEnvironment,
    trusted_builtin_method_source: bool,
}

impl ModuleImports {
    pub fn new(types: impl IntoIterator<Item = (String, Type)>) -> Self {
        Self {
            types: types
                .into_iter()
                .map(|(specifier, ty)| (specifier, Some(ty)))
                .collect(),
            type_exports: HashMap::new(),
            qualified_exports: HashMap::new(),
            named_family_exports: HashMap::new(),
            comptime_exports: HashMap::new(),
            recursive_type_unfoldings: HashMap::new(),
            builtin_methods: BuiltinMethodEnvironment::default(),
            trusted_builtin_method_source: false,
        }
    }

    pub fn with_failed(specifiers: impl IntoIterator<Item = String>) -> Self {
        Self {
            types: specifiers
                .into_iter()
                .map(|specifier| (specifier, None))
                .collect(),
            type_exports: HashMap::new(),
            qualified_exports: HashMap::new(),
            named_family_exports: HashMap::new(),
            comptime_exports: HashMap::new(),
            recursive_type_unfoldings: HashMap::new(),
            builtin_methods: BuiltinMethodEnvironment::default(),
            trusted_builtin_method_source: false,
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

    pub fn insert_qualified_exports(
        &mut self,
        specifier: impl Into<String>,
        exports: HashMap<String, QualifiedType>,
    ) {
        self.qualified_exports.insert(specifier.into(), exports);
    }

    pub fn insert_named_family_exports(
        &mut self,
        specifier: impl Into<String>,
        exports: HashMap<String, NamedFamilyType>,
    ) {
        self.named_family_exports.insert(specifier.into(), exports);
    }

    pub fn insert_comptime_exports(
        &mut self,
        specifier: impl Into<String>,
        exports: HashMap<String, ComptimeExport>,
    ) {
        let specifier = specifier.into();
        let module_identity = ComptimeModuleIdentity::specifier(specifier.clone());
        let exports = exports
            .into_iter()
            .map(|(name, export)| {
                (
                    name,
                    export.with_fallback_module_identity(module_identity.clone()),
                )
            })
            .collect();
        self.comptime_exports.insert(specifier, exports);
    }

    pub fn insert_recursive_type_unfoldings(
        &mut self,
        unfoldings: impl IntoIterator<Item = (RecursiveTypeId, Type)>,
    ) {
        self.recursive_type_unfoldings.extend(unfoldings);
    }

    pub fn set_builtin_method_environment(&mut self, methods: BuiltinMethodEnvironment) {
        self.builtin_methods = methods;
    }

    pub fn set_trusted_builtin_method_source(&mut self, trusted: bool) {
        self.trusted_builtin_method_source = trusted;
    }

    pub fn get(&self, specifier: &str) -> Option<Option<&Type>> {
        self.types.get(specifier).map(Option::as_ref)
    }

    pub fn type_export(&self, specifier: &str, name: &str) -> Option<&Type> {
        self.type_exports.get(specifier)?.get(name)
    }

    pub fn qualified_export(&self, specifier: &str, name: &str) -> Option<&QualifiedType> {
        self.qualified_exports.get(specifier)?.get(name)
    }

    pub fn named_family_export(&self, specifier: &str, name: &str) -> Option<&NamedFamilyType> {
        self.named_family_exports.get(specifier)?.get(name)
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
    check_module_with_host_globals_and_imports_in(
        module,
        globals,
        imports,
        ComptimeModuleIdentity::Current,
    )
}

/// Check a module with the canonical identity supplied by a module graph.
///
/// Direct checker callers should use [`check_module_with_host_globals_and_imports`],
/// which reserves [`ComptimeModuleIdentity::Current`] for their single module.
pub fn check_module_with_host_globals_and_imports_in(
    module: &Module,
    globals: &HostGlobals,
    imports: &ModuleImports,
    module_identity: ComptimeModuleIdentity,
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
                    let rebinds_host_type =
                        globals.type_definitions.iter().any(|(name, host_ty)| {
                            name == target
                                && (host_ty == ty
                                    || matches!(ty, Type::Named(exported) if exported == name))
                        });
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
    let mut checker = Checker::with_module_and_host_globals_and_imports(
        known_types,
        seed_definitions,
        module,
        globals,
        imports,
        module_identity,
    );

    checker.diagnostics.extend(reserved_diagnostics);
    checker.check_module(module);
    let export_names = final_record_names(module);
    let named_family_aliases = checker.named_family_aliases.clone();
    let top_level_qualified_types: HashMap<_, _> = aven_parser::collect_declarations(module)
        .into_iter()
        .filter(|declaration| export_names.contains(&declaration.name))
        .filter(|declaration| !named_family_aliases.contains_key(&declaration.name))
        .filter_map(|declaration| {
            checker
                .infer_top_level_qualified_type_for_output(&declaration.name)
                .map(|qualified| (declaration.name, qualified))
        })
        .collect();
    let top_level_types = top_level_qualified_types
        .iter()
        .map(|(name, qualified)| (name.clone(), qualified.ty.clone()))
        .collect();

    CheckOutput {
        recursive_type_unfoldings: checker.recursive_type_unfoldings.clone(),
        diagnostics: checker.diagnostics,
        inferred_types: checker.inferred_types,
        type_definitions: checker.type_definitions.clone(),
        top_level_types,
        top_level_qualified_types,
        named_families: checker.named_families.clone(),
        named_family_aliases: checker.named_family_aliases.clone(),
        builtin_methods: checker.builtin_methods.clone(),
        slot_reifications: checker.slot_reifications.clone(),
        primitive_family_coercions: checker.primitive_family_coercions.clone(),
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
