use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, Item, Module, collect_declarations,
};

use crate::{BUILTIN_TYPES, Checker, HostGlobals, ModuleImports, Type, comptime};

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
    Checker::with_module_and_host_globals_and_imports(
        known_types.clone(),
        HashMap::new(),
        module,
        &HostGlobals::default(),
        &ModuleImports::default(),
        comptime::ComptimeModuleIdentity::Current,
    )
    .type_definitions
}

pub(crate) fn type_definition_names(
    module: &Module,
    known_types: &HashSet<String>,
    reserved_names: &HashSet<String>,
) -> HashSet<String> {
    collect_declarations(module)
        .into_iter()
        .filter(|declaration| declaration.phase == DeclarationPhase::Comptime)
        .filter_map(|declaration| {
            let binding = binding_for_declaration(module, &declaration)?;
            if crate::checker::is_import_call(&binding.value)
                || crate::checker::is_method_requirement_row(&binding.value)
                || aven_parser::is_named_method_provider(&binding.value)
                || (declaration
                    .name
                    .chars()
                    .next()
                    .is_some_and(char::is_uppercase)
                    && aven_parser::lambda_parts(&binding.value).is_some())
                || (declared_annotation_for_declaration(module, &declaration).is_none()
                    && bare_lowercase_unknown_name(&binding.value, known_types).is_some())
                || reserved_names.contains(&declaration.name)
            {
                return None;
            }
            Some(declaration.name)
        })
        .collect()
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

pub(crate) struct CyclicAliases {
    pub(crate) names: HashSet<String>,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

pub(crate) fn cyclic_aliases(module: &Module, names: &HashSet<String>) -> CyclicAliases {
    let edges: HashMap<String, String> = collect_declarations(module)
        .into_iter()
        .filter(|declaration| names.contains(&declaration.name))
        .filter_map(|declaration| {
            let binding = binding_for_declaration(module, &declaration)?;
            let target = match &ungroup_expr(&binding.value).kind {
                ExprKind::Name(target) | ExprKind::ComptimeName(target) => target,
                _ => return None,
            };
            names
                .contains(target)
                .then_some((declaration.name, target.clone()))
        })
        .collect();

    let mut cyclic = HashSet::new();
    let mut finished = HashSet::new();

    for name in edges.keys() {
        if finished.contains(name.as_str()) {
            continue;
        }

        let mut path: Vec<&str> = Vec::new();
        let mut positions = HashMap::new();
        let mut current = name.as_str();

        while !finished.contains(current) {
            if let Some(&cycle_start) = positions.get(current) {
                cyclic.extend(path[cycle_start..].iter().copied());
                break;
            }

            positions.insert(current, path.len());
            path.push(current);

            let Some(next) = edges.get(current).map(String::as_str) else {
                break;
            };
            current = next;
        }

        finished.extend(path);
    }

    let names = collect_declarations(module)
        .into_iter()
        .filter(|declaration| cyclic.contains(declaration.name.as_str()))
        .map(|declaration| declaration.name)
        .collect::<HashSet<_>>();
    let mut emitted = HashSet::new();
    let diagnostics = collect_declarations(module)
        .into_iter()
        .filter_map(|declaration| {
            let name = declaration.name;
            if !names.contains(&name) || !emitted.insert(name.clone()) {
                return None;
            }

            let mut cycle = vec![name.clone()];
            let mut current = edges[name.as_str()].clone();
            while current != name {
                cycle.push(current.clone());
                current = edges[current.as_str()].clone();
            }
            cycle.push(name.clone());

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
        .collect();

    CyclicAliases { names, diagnostics }
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
