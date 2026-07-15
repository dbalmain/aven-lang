use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, Item, Module, collect_declarations,
};

use crate::{BUILTIN_TYPES, Checker, ModuleImports, RowEntry, RowTail, Type};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeLowering {
    pub ty: Type,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredAnnotation {
    pub name: String,
    pub declaration_span: Span,
    pub annotation_span: Span,
    pub ty: Type,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone)]
pub struct AnnotationLowerer {
    known_types: HashSet<String>,
    type_definitions: HashMap<String, Type>,
}

impl AnnotationLowerer {
    pub fn new(module: &Module) -> Self {
        let known_types = known_type_names(module);
        let type_definitions = type_definitions(module, &known_types);

        Self {
            known_types,
            type_definitions,
        }
    }

    pub fn lower_declaration(
        &self,
        module: &Module,
        declaration: &aven_parser::Declaration,
    ) -> Option<DeclaredAnnotation> {
        let source = declared_annotation_for_declaration(module, declaration)?;
        let mut checker = Checker::with_module_environment(
            self.known_types.clone(),
            self.type_definitions.clone(),
            module,
        );

        Some(checker.lower_declared_annotation(source))
    }
}

pub(crate) fn known_type_names(module: &Module) -> HashSet<String> {
    let mut names: HashSet<_> = BUILTIN_TYPES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();

    for declaration in collect_declarations(module) {
        if declaration.phase == DeclarationPhase::Comptime {
            names.insert(declaration.name);
        }
    }

    names
}

pub(crate) fn type_definitions(
    module: &Module,
    known_types: &HashSet<String>,
) -> HashMap<String, Type> {
    let reserved_names = BUILTIN_TYPES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    type_definitions_excluding(
        module,
        known_types,
        &reserved_names,
        &ModuleImports::default(),
        &HashMap::new(),
    )
}

/// `seed_definitions` are definitions that exist before this module's own
/// comptime declarations — host-global and pattern-imported types. They must
/// be visible while declarations lower so comptime type functions applied to
/// them reify instead of deferring.
pub(crate) fn type_definitions_excluding(
    module: &Module,
    known_types: &HashSet<String>,
    reserved_names: &HashSet<String>,
    imports: &ModuleImports,
    seed_definitions: &HashMap<String, Type>,
) -> HashMap<String, Type> {
    let mut definitions = seed_definitions.clone();
    let declarations: Vec<_> = collect_declarations(module)
        .into_iter()
        .filter(|declaration| declaration.phase == DeclarationPhase::Comptime)
        .collect();

    for _ in 0..=declarations.len() {
        let mut next = seed_definitions.clone();

        for declaration in &declarations {
            let Some(binding) = binding_for_declaration(module, declaration) else {
                continue;
            };

            // An import binds a module *value*, never a type alias. Keep it
            // out of the definitions map so a binding named like a builtin
            // type (`Text = import(...)`) cannot shadow that type during
            // normalization; the checker reports the binding itself as
            // `name.uppercase-module-binding`.
            if crate::checker::is_import_call(&binding.value) {
                continue;
            }

            // An uppercase lambda is a comptime function definition, not a
            // lowered type alias. Its applications specialize through the
            // shared comptime evaluator below.
            if declaration
                .name
                .chars()
                .next()
                .is_some_and(char::is_uppercase)
                && aven_parser::lambda_parts(&binding.value).is_some()
            {
                continue;
            }

            // A bare lowercase name is a runtime reference, not a type alias.
            // Lowercase names remain valid type variables inside structured
            // aliases such as `{ value: a }`.
            if (declared_annotation_for_declaration(module, declaration).is_none()
                && bare_lowercase_unknown_name(&binding.value, known_types).is_some())
                || reserved_names.contains(&declaration.name)
            {
                continue;
            }

            // Lower each definition without its own entry so self-references
            // stay nominal (`Data` keeps `Named("Data")` payload leaves) —
            // recursive definitions unfold lazily at use sites instead of
            // expanding here. Imports are available so `PairInt = pair(Int)`
            // expands when `pair` is a pattern-imported comptime export.
            let mut visible = definitions.clone();
            visible.remove(&declaration.name);
            let mut checker =
                Checker::with_module_environment(known_types.clone(), visible, module);
            checker.imports = imports.clone();

            next.insert(
                declaration.name.clone(),
                checker.lower_annotation(&binding.value),
            );
        }

        if next == definitions {
            break;
        }

        definitions = next;
    }

    definitions
}

pub(crate) fn bare_lowercase_unknown_name<'a>(
    expr: &'a Expr,
    known_types: &HashSet<String>,
) -> Option<&'a str> {
    let ExprKind::Name(name) = &ungroup_expr(expr).kind else {
        return None;
    };
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_lowercase)
        .then_some(name)
        .filter(|name| !known_types.contains(*name))
        .map(String::as_str)
}

pub(crate) fn reserved_type_diagnostic(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("type name `{name}` is reserved"))
        .with_code(codes::name::RESERVED_TYPE)
        .with_label(Label::primary(
            span,
            "this name is reserved by Aven or the host",
        ))
        .with_note("pick another type name so the builtin or host-provided type remains available")
}

fn ungroup_expr(mut expr: &Expr) -> &Expr {
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    expr
}

pub(crate) fn cyclic_alias_diagnostics(
    module: &Module,
    definitions: &HashMap<String, Type>,
) -> Vec<Diagnostic> {
    let edges: HashMap<&str, &str> = definitions
        .iter()
        .filter_map(|(name, ty)| match ty {
            Type::Named(target) if definitions.contains_key(target) => {
                Some((name.as_str(), target.as_str()))
            }
            _ => None,
        })
        .collect();

    let mut cyclic = HashSet::new();
    let mut finished = HashSet::new();

    for name in edges.keys().copied() {
        if finished.contains(name) {
            continue;
        }

        let mut path = Vec::new();
        let mut positions = HashMap::new();
        let mut current = name;

        while !finished.contains(current) {
            if let Some(&cycle_start) = positions.get(current) {
                cyclic.extend(path[cycle_start..].iter().copied());
                break;
            }

            positions.insert(current, path.len());
            path.push(current);

            let Some(next) = edges.get(current).copied() else {
                break;
            };
            current = next;
        }

        finished.extend(path);
    }

    let mut emitted = HashSet::new();
    collect_declarations(module)
        .into_iter()
        .filter_map(|declaration| {
            let name = declaration.name;
            if !cyclic.contains(name.as_str()) || !emitted.insert(name.clone()) {
                return None;
            }

            let mut cycle = vec![name.as_str()];
            let mut current = edges[name.as_str()];
            while current != name {
                cycle.push(current);
                current = edges[current];
            }
            cycle.push(name.as_str());

            Some(
                Diagnostic::error(format!(
                    "type alias `{name}` is defined as a cycle: {}",
                    cycle.join(" -> ")
                ))
                .with_code(codes::ty::CYCLIC_ALIAS)
                .with_label(Label::primary(
                    declaration.span,
                    "cyclic type alias declared here",
                ))
                .with_note(
                    "wrap one member in a record or variant to make the recursion well-founded, or remove the alias",
                ),
            )
        })
        .collect()
}

/// Reports recursive aliases whose strict structure cannot produce a finite
/// value. Unlike transparent alias cycles, these definitions retain a nominal
/// `Named` knot while lowering, so their productivity is decided here from the
/// least fixed point of each local recursive SCC.
pub(crate) fn unproductive_recursion_diagnostics(
    module: &Module,
    definitions: &HashMap<String, Type>,
) -> Vec<Diagnostic> {
    let names: Vec<_> = collect_declarations(module)
        .into_iter()
        .filter(|declaration| definitions.contains_key(&declaration.name))
        .map(|declaration| declaration.name)
        .collect();
    let local: HashSet<_> = names.iter().map(String::as_str).collect();
    let edges: HashMap<_, _> = names
        .iter()
        .map(|name| {
            let mut targets = HashSet::new();
            collect_named_references(&definitions[name], &local, &mut targets);
            (name.as_str(), targets)
        })
        .collect();
    let reverse = reverse_edges(&edges);
    let mut assigned: HashSet<&str> = HashSet::new();
    let mut unproductive: HashSet<&str> = HashSet::new();

    for name in &names {
        if assigned.contains(name.as_str()) {
            continue;
        }
        let forward = reachable(name, &edges);
        let backward = reachable(name, &reverse);
        let component: HashSet<_> = forward.intersection(&backward).copied().collect();
        assigned.extend(&component);

        let recursive = component.len() > 1
            || edges
                .get(name.as_str())
                .is_some_and(|targets| targets.contains(name.as_str()));
        if !recursive {
            continue;
        }
        // A wholly transparent cycle is owned by `cyclic_alias_diagnostics`.
        // Keeping it there preserves both its established diagnostic and its
        // clearer alias-chain explanation.
        if component.iter().all(|member| {
            matches!(definitions[*member], Type::Named(ref target) if component.contains(target.as_str()))
        }) {
            continue;
        }

        let mut productive = HashSet::new();
        loop {
            let added: Vec<_> = component
                .iter()
                .copied()
                .filter(|member| {
                    !productive.contains(*member)
                        && is_productive(&definitions[*member], &component, &productive)
                })
                .collect();
            if added.is_empty() {
                break;
            }
            productive.extend(added);
        }
        unproductive.extend(component.difference(&productive).copied());
    }

    collect_declarations(module)
        .into_iter()
        .filter(|declaration| unproductive.contains(declaration.name.as_str()))
        .map(|declaration| {
            let forcing = forcing_step(
                &definitions[&declaration.name],
                &unproductive,
                &HashSet::new(),
            )
            .unwrap_or_else(|| "strict recursion".to_owned());
            Diagnostic::error(format!(
                "recursive type `{}` has no finite value",
                declaration.name
            ))
            .with_code(codes::ty::UNPRODUCTIVE_RECURSION)
            .with_label(Label::primary(
                declaration.span,
                "unproductive recursive type declared here",
            ))
            .with_note(format!(
                "every value of `{}` requires another recursive value via {forcing}",
                declaration.name
            ))
        })
        .collect()
}

fn collect_named_references<'a>(
    ty: &'a Type,
    local: &HashSet<&'a str>,
    targets: &mut HashSet<&'a str>,
) {
    match ty {
        Type::Named(name) => {
            if local.contains(name.as_str()) {
                targets.insert(name);
            }
        }
        Type::Apply { callee, args } => {
            collect_named_references(callee, local, targets);
            for arg in args {
                collect_named_references(arg, local, targets);
            }
        }
        Type::Function { params, result, .. } => {
            for param in params {
                collect_named_references(param, local, targets);
            }
            collect_named_references(result, local, targets);
        }
        Type::Optional(inner) | Type::Nullable(inner) => {
            collect_named_references(inner, local, targets)
        }
        Type::Tuple(items) => {
            for item in items {
                collect_named_references(item, local, targets);
            }
        }
        Type::Record(row) | Type::Variant(row) => {
            for entry in &row.entries {
                match entry {
                    RowEntry::Field { ty, .. } => collect_named_references(ty, local, targets),
                    RowEntry::Tag { payload, .. } => {
                        for ty in payload {
                            collect_named_references(ty, local, targets);
                        }
                    }
                    RowEntry::Literal { .. } => {}
                }
            }
        }
        Type::Deferred | Type::Variable(_) | Type::Meta(_) => {}
    }
}

fn reverse_edges<'a>(
    edges: &HashMap<&'a str, HashSet<&'a str>>,
) -> HashMap<&'a str, HashSet<&'a str>> {
    let mut reverse: HashMap<_, HashSet<_>> =
        edges.keys().map(|name| (*name, HashSet::new())).collect();
    for (from, targets) in edges {
        for target in targets {
            reverse.entry(*target).or_default().insert(*from);
        }
    }
    reverse
}

fn reachable<'a>(start: &'a str, edges: &HashMap<&'a str, HashSet<&'a str>>) -> HashSet<&'a str> {
    let mut seen = HashSet::new();
    let mut pending = vec![start];
    while let Some(name) = pending.pop() {
        if !seen.insert(name) {
            continue;
        }
        if let Some(targets) = edges.get(name) {
            pending.extend(targets.iter().copied());
        }
    }
    seen
}

fn is_productive(ty: &Type, component: &HashSet<&str>, productive: &HashSet<&str>) -> bool {
    match ty {
        Type::Optional(_) | Type::Nullable(_) | Type::Function { .. } => true,
        Type::Apply { callee, .. } if matches!(callee.as_ref(), Type::Named(name) if matches!(name.as_str(), "Array" | "Map" | "Set" | "Stream")) => {
            true
        }
        Type::Named(name) => {
            !component.contains(name.as_str()) || productive.contains(name.as_str())
        }
        Type::Tuple(items) => items
            .iter()
            .all(|item| is_productive(item, component, productive)),
        Type::Record(row) => {
            row.tail != RowTail::Closed
                || row.entries.iter().all(|entry| match entry {
                    RowEntry::Field { ty, .. } => is_productive(ty, component, productive),
                    RowEntry::Literal { .. } | RowEntry::Tag { .. } => true,
                })
        }
        Type::Variant(row) => {
            row.tail != RowTail::Closed
                || row.entries.iter().any(|entry| match entry {
                    RowEntry::Tag { payload, .. } => payload
                        .iter()
                        .all(|ty| is_productive(ty, component, productive)),
                    RowEntry::Literal { .. } => true,
                    RowEntry::Field { .. } => true,
                })
        }
        // Deferred forms, variables, metas, non-collection applications, and
        // unresolved names are intentionally conservative: no false positive.
        Type::Deferred | Type::Variable(_) | Type::Meta(_) | Type::Apply { .. } => true,
    }
}

fn forcing_step(ty: &Type, unproductive: &HashSet<&str>, seen: &HashSet<&str>) -> Option<String> {
    match ty {
        Type::Named(name)
            if unproductive.contains(name.as_str()) && !seen.contains(name.as_str()) =>
        {
            Some(format!("type `{name}`"))
        }
        Type::Tuple(items) => items.iter().enumerate().find_map(|(index, item)| {
            forcing_step(item, unproductive, seen).map(|_| format!("tuple item {}", index + 1))
        }),
        Type::Record(row) if row.tail == RowTail::Closed => row.entries.iter().find_map(|entry| {
            let RowEntry::Field { name, ty } = entry else {
                return None;
            };
            forcing_step(ty, unproductive, seen).map(|_| format!("field `{name}`"))
        }),
        Type::Variant(row) if row.tail == RowTail::Closed => row.entries.iter().find_map(|entry| {
            let RowEntry::Tag { name, payload } = entry else {
                return None;
            };
            payload
                .iter()
                .find_map(|ty| forcing_step(ty, unproductive, seen))
                .map(|_| format!("alternative `@{name}`"))
        }),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DeclaredAnnotationSource<'a> {
    pub(crate) name: String,
    pub(crate) declaration_span: Span,
    pub(crate) annotation: &'a Expr,
}

pub(crate) fn declared_annotation_for_declaration<'a>(
    module: &'a Module,
    declaration: &aven_parser::Declaration,
) -> Option<DeclaredAnnotationSource<'a>> {
    for item in &module.items {
        match item {
            Item::Signature(signature)
                if signature.name == declaration.name
                    && declaration.span.contains(signature.span) =>
            {
                return Some(DeclaredAnnotationSource {
                    name: declaration.name.clone(),
                    declaration_span: declaration.span,
                    annotation: &signature.annotation,
                });
            }
            Item::Binding(binding)
                if binding.name == declaration.name
                    && declaration.span.contains(binding.span)
                    && binding.annotation.is_some() =>
            {
                return Some(DeclaredAnnotationSource {
                    name: declaration.name.clone(),
                    declaration_span: declaration.span,
                    annotation: binding.annotation.as_ref()?,
                });
            }
            Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::SpreadBinding(_)
            | Item::Signature(_)
            | Item::Expr(_) => {}
        }
    }

    None
}

pub(crate) fn binding_for_declaration<'a>(
    module: &'a Module,
    declaration: &Declaration,
) -> Option<&'a Binding> {
    module.items.iter().find_map(|item| match item {
        Item::Binding(binding)
            if binding.name == declaration.name && declaration.span.contains(binding.span) =>
        {
            Some(binding)
        }
        Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::SpreadBinding(_)
        | Item::Signature(_)
        | Item::Expr(_) => None,
    })
}
