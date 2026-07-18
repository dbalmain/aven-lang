use aven_core::Span;

use crate::items::{MergedItem, merged_items};
use crate::lexer::is_comptime_identifier_name;
use crate::parser::{Binding, Expr, ExprKind, Module, Param, PatternBinding, Signature};
use crate::resolve::pattern_bindings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub kind: DeclarationKind,
    pub phase: DeclarationPhase,
    pub shape: DeclarationShape,
    pub is_annotated: bool,
    /// Span of the `:=` operator when this declaration came from an explicit
    /// shadow binding; `None` otherwise. Shadowing is meaningless at the top
    /// level, where this span anchors the error.
    pub shadow_span: Option<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationKind {
    Binding,
    Function,
    Signature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclarationPhase {
    Runtime,
    Comptime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationShape {
    Value,
    Callable(CallableShape),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallableShape {
    /// One entry per parameter. `true` means the parameter has an annotation.
    ///
    /// This is intentionally shallow parser-level information, not normalized
    /// type identity. Typed overload disjointness belongs to the type phase.
    pub parameter_annotations: Vec<bool>,
    /// Whether a result annotation is present. This keeps zero-argument
    /// result-type overloads from being flattened into untyped duplicates.
    pub has_result_annotation: bool,
}

pub fn collect_declarations(module: &Module) -> Vec<Declaration> {
    let mut declarations = Vec::new();

    for item in merged_items(&module.items) {
        match item {
            MergedItem::Binding { signature, binding } => {
                declarations.push(binding_declaration(binding, signature));
            }
            MergedItem::PatternBinding(binding) => {
                declarations.extend(pattern_binding_declarations(binding));
            }
            MergedItem::SpreadBinding(_) => {}
            MergedItem::MethodAttachment(_) => {}
            MergedItem::Signature(signature) => declarations.push(signature_declaration(signature)),
            MergedItem::Expr(_) => {}
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
        shape: binding_shape(binding, signature),
        is_annotated: signature.is_some(),
        shadow_span: binding.shadow_span,
    }
}

fn signature_declaration(signature: &Signature) -> Declaration {
    Declaration {
        name: signature.name.clone(),
        name_span: signature.name_span,
        span: signature.span,
        kind: DeclarationKind::Signature,
        phase: declaration_phase(&signature.name),
        shape: signature_shape(signature),
        is_annotated: false,
        shadow_span: None,
    }
}

fn pattern_binding_declarations(binding: &PatternBinding) -> Vec<Declaration> {
    pattern_bindings(&binding.pattern)
        .into_iter()
        .map(|site| Declaration {
            name: site.name.to_owned(),
            name_span: site.span,
            span: binding.span,
            kind: DeclarationKind::Binding,
            phase: declaration_phase(site.name),
            shape: DeclarationShape::Value,
            is_annotated: false,
            shadow_span: None,
        })
        .collect()
}

fn binding_kind(binding: &Binding) -> DeclarationKind {
    if binding
        .annotation
        .as_ref()
        .is_some_and(is_callable_annotation)
    {
        return DeclarationKind::Function;
    }

    if matches!(binding.value.kind, ExprKind::Lambda { .. }) {
        return DeclarationKind::Function;
    }

    DeclarationKind::Binding
}

fn binding_shape(binding: &Binding, signature: Option<&Signature>) -> DeclarationShape {
    if let Some(signature) = signature {
        return signature_shape(signature);
    }

    if let Some(annotation) = &binding.annotation {
        return annotation_shape(annotation);
    }

    if let ExprKind::Lambda {
        params,
        return_annotation,
        ..
    } = &binding.value.kind
    {
        return lambda_shape(params, return_annotation.is_some());
    }

    DeclarationShape::Value
}

fn signature_shape(signature: &Signature) -> DeclarationShape {
    annotation_shape(&signature.annotation)
}

fn annotation_shape(annotation: &Expr) -> DeclarationShape {
    if let ExprKind::Arrow { params, result } = &annotation.kind {
        return DeclarationShape::Callable(CallableShape {
            parameter_annotations: params.iter().map(is_present_annotation).collect(),
            has_result_annotation: is_present_annotation(result),
        });
    }

    DeclarationShape::Value
}

fn lambda_shape(params: &[Param], has_result_annotation: bool) -> DeclarationShape {
    DeclarationShape::Callable(CallableShape {
        parameter_annotations: params
            .iter()
            .map(|param| param.annotation.is_some())
            .collect(),
        has_result_annotation,
    })
}

fn is_callable_annotation(annotation: &Expr) -> bool {
    matches!(annotation.kind, ExprKind::Arrow { .. })
}

fn is_present_annotation(annotation: &Expr) -> bool {
    !matches!(annotation.kind, ExprKind::Missing)
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
        let output = parse_module("User = { name: Text }\ndouble = (x) => x\nvalue = 1\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[0].name, "User");
        assert_eq!(declarations[0].kind, DeclarationKind::Binding);
        assert_eq!(declarations[0].phase, DeclarationPhase::Comptime);
        assert_eq!(declarations[0].shape, DeclarationShape::Value);
        assert_eq!(declarations[1].name, "double");
        assert_eq!(declarations[1].kind, DeclarationKind::Function);
        assert_eq!(declarations[1].phase, DeclarationPhase::Runtime);
        assert_eq!(
            declarations[1].shape,
            DeclarationShape::Callable(CallableShape {
                parameter_annotations: vec![false],
                has_result_annotation: false,
            })
        );
        assert_eq!(declarations[2].name, "value");
        assert_eq!(declarations[2].kind, DeclarationKind::Binding);
        assert_eq!(declarations[2].shape, DeclarationShape::Value);
    }

    #[test]
    fn merges_adjacent_signature_and_binding() {
        let output = parse_module("double : (Int) -> Int\ndouble = (x) => x\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "double");
        assert_eq!(declarations[0].kind, DeclarationKind::Function);
        assert!(declarations[0].is_annotated);
        assert_eq!(
            declarations[0].shape,
            DeclarationShape::Callable(CallableShape {
                parameter_annotations: vec![true],
                has_result_annotation: true,
            })
        );
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
        assert_eq!(declarations[0].shape, DeclarationShape::Value);
        assert!(!declarations[0].is_annotated);
        assert_eq!(declarations[1].name, "other");
        assert_eq!(declarations[1].kind, DeclarationKind::Binding);
    }

    #[test]
    fn records_unannotated_lambda_shapes_without_parameter_types() {
        let output = parse_module("fallback = (value) => value\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(
            declarations[0].shape,
            DeclarationShape::Callable(CallableShape {
                parameter_annotations: vec![false],
                has_result_annotation: false,
            })
        );
    }

    #[test]
    fn records_result_annotations_for_zero_argument_lambdas() {
        let output = parse_module("zero = (): NonEmptyText => \"-\"\n");
        let declarations = collect_declarations(&output.module);

        assert_eq!(
            declarations[0].shape,
            DeclarationShape::Callable(CallableShape {
                parameter_annotations: Vec::new(),
                has_result_annotation: true,
            })
        );
    }
}
