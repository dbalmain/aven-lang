use aven_core::{Diagnostic, Label, Span};

use crate::declarations::{CallableShape, Declaration, DeclarationShape, collect_declarations};
use crate::lexer::is_comptime_identifier_name;
use crate::parser::{Expr, ExprKind, Item, MatchArm, Module};
use crate::resolve::{BindingSite, pattern_bindings};
use crate::walk::walk_expr_children;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameAnalysis {
    pub declarations: Vec<Declaration>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeBinding {
    name: String,
    span: Span,
}

#[derive(Debug, Default)]
struct ScopeStack {
    scopes: Vec<Vec<ScopeBinding>>,
}

pub fn analyze_names(module: &Module) -> NameAnalysis {
    let declarations = collect_declarations(module);
    let mut diagnostics = duplicate_top_level_diagnostics(&declarations);
    let mut scopes = ScopeStack::default();

    for item in &module.items {
        analyze_item(item, &mut scopes, &mut diagnostics);
    }

    NameAnalysis {
        declarations,
        diagnostics,
    }
}

fn duplicate_top_level_diagnostics(declarations: &[Declaration]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for (index, declaration) in declarations.iter().enumerate() {
        let Some(previous) = declarations[..index]
            .iter()
            .find(|candidate| candidate.name == declaration.name)
        else {
            continue;
        };

        if is_plausible_typed_overload(previous, declaration) {
            continue;
        }

        diagnostics.push(
            Diagnostic::error(format!("duplicate declaration `{}`", declaration.name))
                .with_code("name.duplicate-declaration")
                .with_label(Label::primary(
                    declaration.name_span,
                    "declaration repeated here",
                ))
                .with_label(Label::primary(
                    previous.name_span,
                    "previous declaration with the same name",
                ))
                .with_note(
                    "typed overload disjointness is checked later; untyped or value declarations must use distinct names",
                ),
        );
    }

    diagnostics
}

fn is_plausible_typed_overload(left: &Declaration, right: &Declaration) -> bool {
    matches!(
        (&left.shape, &right.shape),
        (DeclarationShape::Callable(left), DeclarationShape::Callable(right))
            if is_fully_annotated_callable(left) && is_fully_annotated_callable(right)
    )
}

fn is_fully_annotated_callable(shape: &CallableShape) -> bool {
    shape.has_result_annotation
        && shape
            .parameter_annotations
            .iter()
            .all(|is_annotated| *is_annotated)
}

fn analyze_item(item: &Item, scopes: &mut ScopeStack, diagnostics: &mut Vec<Diagnostic>) {
    match item {
        Item::Binding(binding) => {
            if let Some(annotation) = &binding.annotation {
                analyze_expr(annotation, scopes, diagnostics);
            }
            analyze_expr(&binding.value, scopes, diagnostics);
        }
        Item::Signature(signature) => analyze_expr(&signature.annotation, scopes, diagnostics),
        Item::Expr(expr) => analyze_expr(expr, scopes, diagnostics),
    }
}

fn analyze_expr(expr: &Expr, scopes: &mut ScopeStack, diagnostics: &mut Vec<Diagnostic>) {
    match &expr.kind {
        ExprKind::Match { subject, arms, .. } => analyze_match(subject, arms, scopes, diagnostics),
        ExprKind::Lambda {
            params,
            return_annotation,
            body,
        } => {
            for param in params {
                if let Some(annotation) = &param.annotation {
                    analyze_expr(annotation, scopes, diagnostics);
                }
            }

            if let Some(annotation) = return_annotation {
                analyze_expr(annotation, scopes, diagnostics);
            }

            scopes.push();
            for param in params {
                diagnose_uppercase_runtime_name(&param.name, param.name_span, diagnostics);
                scopes.define(&param.name, param.name_span, diagnostics);
            }
            analyze_expr(body, scopes, diagnostics);
            scopes.pop();
        }
        ExprKind::Block(items) => analyze_block(items, scopes, diagnostics),
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Name(_)
        | ExprKind::ComptimeName(_) => {}
        _ => walk_expr_children(expr, &mut |child| {
            analyze_expr(child, scopes, diagnostics);
        }),
    }
}

fn analyze_block(items: &[Item], scopes: &mut ScopeStack, diagnostics: &mut Vec<Diagnostic>) {
    scopes.push();
    let mut items = items.iter().peekable();

    while let Some(item) = items.next() {
        match item {
            Item::Signature(signature) => {
                analyze_expr(&signature.annotation, scopes, diagnostics);

                if let Some(Item::Binding(binding)) = items.peek()
                    && binding.name == signature.name
                {
                    if let Some(annotation) = &binding.annotation {
                        analyze_expr(annotation, scopes, diagnostics);
                    }
                    analyze_expr(&binding.value, scopes, diagnostics);
                    scopes.define(&binding.name, binding.name_span, diagnostics);
                    items.next();
                    continue;
                }

                scopes.define(&signature.name, signature.name_span, diagnostics);
            }
            Item::Binding(binding) => {
                if let Some(annotation) = &binding.annotation {
                    analyze_expr(annotation, scopes, diagnostics);
                }
                analyze_expr(&binding.value, scopes, diagnostics);
                scopes.define(&binding.name, binding.name_span, diagnostics);
            }
            Item::Expr(expr) => analyze_expr(expr, scopes, diagnostics),
        }
    }

    scopes.pop();
}

fn analyze_match(
    subject: &Expr,
    arms: &[MatchArm],
    scopes: &mut ScopeStack,
    diagnostics: &mut Vec<Diagnostic>,
) {
    analyze_expr(subject, scopes, diagnostics);

    for arm in arms {
        scopes.push();
        define_pattern_bindings(pattern_bindings(&arm.pattern), scopes, diagnostics);
        analyze_exprs(&arm.guards, scopes, diagnostics);
        analyze_expr(&arm.body, scopes, diagnostics);
        scopes.pop();
    }
}

fn analyze_exprs(items: &[Expr], scopes: &mut ScopeStack, diagnostics: &mut Vec<Diagnostic>) {
    for item in items {
        analyze_expr(item, scopes, diagnostics);
    }
}

fn define_pattern_bindings(
    bindings: Vec<BindingSite<'_>>,
    scopes: &mut ScopeStack,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for binding in bindings {
        scopes.define(binding.name, binding.span, diagnostics);
    }
}

fn diagnose_uppercase_runtime_name(name: &str, span: Span, diagnostics: &mut Vec<Diagnostic>) {
    if !is_comptime_identifier_name(name) {
        return;
    }

    diagnostics.push(
        Diagnostic::error(format!(
            "uppercase parameter `{name}` cannot bind a runtime argument"
        ))
        .with_code("name.uppercase-runtime-binding")
        .with_label(Label::primary(span, "runtime binding introduced here"))
        .with_note("runtime values use lowercase names; uppercase names are reserved for comptime identifiers"),
    );
}

impl ScopeStack {
    fn push(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: &str, span: Span, diagnostics: &mut Vec<Diagnostic>) {
        if name == "_" {
            return;
        }

        if let Some(previous) = self.find_current(name) {
            diagnostics.push(
                Diagnostic::error(format!("duplicate local binding `{name}`"))
                    .with_code("name.duplicate-local")
                    .with_label(Label::primary(span, "binding repeated here"))
                    .with_label(Label::primary(
                        previous.span,
                        "previous local binding with the same name",
                    ))
                    .with_note(
                        "rename one binding, or use explicit shadowing syntax once it exists",
                    ),
            );
        } else if let Some(previous) = self.find_visible(name) {
            diagnostics.push(
                Diagnostic::error(format!("accidental shadowing of `{name}`"))
                    .with_code("name.accidental-shadowing")
                    .with_label(Label::primary(span, "new binding shadows this name"))
                    .with_label(Label::primary(
                        previous.span,
                        "existing binding with the same name",
                    ))
                    .with_note(
                        "rename the binding, or use explicit shadowing syntax once it exists",
                    ),
            );
        }

        if let Some(scope) = self.scopes.last_mut() {
            scope.push(ScopeBinding {
                name: name.to_owned(),
                span,
            });
        }
    }

    fn find_current(&self, name: &str) -> Option<&ScopeBinding> {
        self.scopes
            .last()
            .and_then(|scope| scope.iter().rev().find(|binding| binding.name == name))
    }

    fn find_visible(&self, name: &str) -> Option<&ScopeBinding> {
        self.scopes
            .iter()
            .rev()
            // The current scope was already checked by `find_current`; this
            // pass only reports bindings visible from enclosing scopes.
            .skip(1)
            .find_map(|scope| scope.iter().rev().find(|binding| binding.name == name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_module;

    #[test]
    fn reports_duplicate_top_level_values() {
        let output = parse_module("value = 1\nvalue = 2\n");
        let analysis = analyze_names(&output.module);

        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].code.as_deref(),
            Some("name.duplicate-declaration")
        );
    }

    #[test]
    fn defers_fully_typed_overload_disjointness() {
        let output = parse_module(
            "zero = (): NonEmptyText => \"-\"\nzero = (): NonEmptyArray[a] => [zero()]\n",
        );
        let analysis = analyze_names(&output.module);

        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn merges_local_signature_and_binding_for_duplicate_checks() {
        let output = parse_module("f = () =>\n  total : Int\n  total = 1\n  total\n");
        let analysis = analyze_names(&output.module);

        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn allows_local_bindings_to_shadow_top_level_declarations() {
        let output = parse_module("value = 1\nf = (value) => value\n");
        let analysis = analyze_names(&output.module);

        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn leaves_type_shaped_uppercase_bindings_to_the_semantic_phase() {
        let output =
            parse_module("HttpOk = 200\nUser = { name = Text }\nColor = @{ Red, Green }\n");
        let analysis = analyze_names(&output.module);

        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn reports_duplicate_lambda_parameters() {
        let output = parse_module("f = (value, value) => value\n");
        let analysis = analyze_names(&output.module);

        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].code.as_deref(),
            Some("name.duplicate-local")
        );
    }

    #[test]
    fn reports_accidental_shadowing() {
        let output = parse_module("f = (value) =>\n  inner = (value) => value\n  inner\n");
        let analysis = analyze_names(&output.module);

        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].code.as_deref(),
            Some("name.accidental-shadowing")
        );
    }
}
