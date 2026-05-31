use aven_core::Span;

use crate::lexer::is_comptime_identifier_name;
use crate::parser::{Binding, ExprKind, Item, Module, Signature};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub kind: DeclarationKind,
    pub phase: DeclarationPhase,
    pub is_annotated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationKind {
    Binding,
    Function,
    Signature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationPhase {
    Runtime,
    Comptime,
}

pub fn collect_declarations(module: &Module) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    let mut items = module.items.iter().peekable();

    while let Some(item) = items.next() {
        match item {
            Item::Signature(signature) => {
                if let Some(Item::Binding(binding)) = items.peek()
                    && binding.name == signature.name
                {
                    declarations.push(binding_declaration(binding, Some(signature)));
                    items.next();
                    continue;
                }

                declarations.push(signature_declaration(signature));
            }
            Item::Binding(binding) => declarations.push(binding_declaration(binding, None)),
            Item::Expr(_) => {}
        }
    }

    declarations
}

fn binding_declaration(binding: &Binding, signature: Option<&Signature>) -> Declaration {
    let span = signature.map_or(binding.span, |signature| signature.span.merge(binding.span));

    Declaration {
        name: binding.name.clone(),
        name_span: binding.name_span,
        span,
        kind: binding_kind(binding),
        phase: declaration_phase(&binding.name),
        is_annotated: signature.is_some(),
    }
}

fn signature_declaration(signature: &Signature) -> Declaration {
    Declaration {
        name: signature.name.clone(),
        name_span: signature.name_span,
        span: signature.span,
        kind: DeclarationKind::Signature,
        phase: declaration_phase(&signature.name),
        is_annotated: false,
    }
}

fn binding_kind(binding: &Binding) -> DeclarationKind {
    if matches!(binding.value.kind, ExprKind::Lambda { .. }) {
        return DeclarationKind::Function;
    }

    DeclarationKind::Binding
}

fn declaration_phase(name: &str) -> DeclarationPhase {
    if is_comptime_identifier_name(name) {
        return DeclarationPhase::Comptime;
    }

    DeclarationPhase::Runtime
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_module;

    #[test]
    fn collects_top_level_declarations() {
        let output = parse_module("User = { name = Text }\ndouble = (x) => x\nvalue = 1\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[0].name, "User");
        assert_eq!(declarations[0].kind, DeclarationKind::Binding);
        assert_eq!(declarations[0].phase, DeclarationPhase::Comptime);
        assert_eq!(declarations[1].name, "double");
        assert_eq!(declarations[1].kind, DeclarationKind::Function);
        assert_eq!(declarations[1].phase, DeclarationPhase::Runtime);
        assert_eq!(declarations[2].name, "value");
        assert_eq!(declarations[2].kind, DeclarationKind::Binding);
    }

    #[test]
    fn merges_adjacent_signature_and_binding() {
        let output = parse_module("double : (Int) -> Int\ndouble = (x) => x\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "double");
        assert_eq!(declarations[0].kind, DeclarationKind::Function);
        assert!(declarations[0].is_annotated);
        assert_eq!(declarations[0].span.start, 0);
        assert_eq!(declarations[0].name_span.start, 22);
    }

    #[test]
    fn keeps_unmatched_signatures_separate() {
        let output = parse_module("value : Int\nother = 1\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "value");
        assert_eq!(declarations[0].kind, DeclarationKind::Signature);
        assert!(!declarations[0].is_annotated);
        assert_eq!(declarations[1].name, "other");
        assert_eq!(declarations[1].kind, DeclarationKind::Binding);
    }
}
