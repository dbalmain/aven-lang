use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, Item, Module, collect_declarations,
};

use crate::{BUILTIN_TYPES, Checker, Type};

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
}

impl AnnotationLowerer {
    pub fn new(module: &Module) -> Self {
        Self {
            known_types: known_type_names(module),
        }
    }

    pub fn lower_declaration(
        &self,
        module: &Module,
        declaration: &aven_parser::Declaration,
    ) -> Option<DeclaredAnnotation> {
        let source = declared_annotation_for_declaration(module, declaration)?;
        let mut checker = Checker::new(self.known_types.clone());

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
    let mut definitions = HashMap::new();
    let mut checker = Checker::new(known_types.clone());

    for declaration in collect_declarations(module) {
        if declaration.phase != DeclarationPhase::Comptime {
            continue;
        }

        let Some(binding) = binding_for_declaration(module, &declaration) else {
            continue;
        };

        definitions.insert(declaration.name, checker.lower_annotation(&binding.value));
    }

    definitions
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
            Item::Binding(_) | Item::Signature(_) | Item::Expr(_) => {}
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
        Item::Binding(_) | Item::Signature(_) | Item::Expr(_) => None,
    })
}
