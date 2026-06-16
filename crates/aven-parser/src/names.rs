use aven_core::{Diagnostic, Label, Span, codes};

use crate::declarations::{CallableShape, Declaration, DeclarationShape, collect_declarations};
use crate::items::{MergedItem, merged_items};
use crate::lexer::is_comptime_identifier_name;
use crate::parser::{Expr, ExprKind, Item, MatchArm, Module, RecordEntry};
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
    kind: BindingKind,
    used: bool,
}

#[derive(Debug, Default)]
struct ScopeStack {
    scopes: Vec<Vec<ScopeBinding>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    Local,
    Parameter,
    Pattern,
    Signature,
}

pub fn analyze_names(module: &Module) -> NameAnalysis {
    let declarations = collect_declarations(module);
    let mut diagnostics = duplicate_top_level_diagnostics(&declarations);
    let mut scopes = ScopeStack::default();

    for item in &module.items {
        analyze_item(item, &mut scopes, &mut diagnostics);
    }

    if diagnostics.iter().any(Diagnostic::is_error) {
        diagnostics.retain(Diagnostic::is_error);
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
                .with_code(codes::name::DUPLICATE_DECLARATION)
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
                scopes.define(
                    &param.name,
                    param.name_span,
                    BindingKind::Parameter,
                    diagnostics,
                );
            }
            analyze_expr(body, scopes, diagnostics);
            scopes.pop(diagnostics);
        }
        ExprKind::Block(items) => analyze_block(items, scopes, diagnostics),
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => scopes.mark_used(name),
        ExprKind::Record(entries) | ExprKind::Set(entries) => {
            analyze_record_entries(entries, scopes, diagnostics);
        }
        ExprKind::Missing | ExprKind::Literal(_) | ExprKind::Tag(_) => {}
        _ => walk_expr_children(expr, &mut |child| {
            analyze_expr(child, scopes, diagnostics);
        }),
    }
}

fn analyze_block(items: &[Item], scopes: &mut ScopeStack, diagnostics: &mut Vec<Diagnostic>) {
    scopes.push();

    for item in merged_items(items) {
        match item {
            MergedItem::Binding { signature, binding } => {
                if let Some(signature) = signature {
                    analyze_expr(&signature.annotation, scopes, diagnostics);
                }

                if let Some(annotation) = &binding.annotation {
                    analyze_expr(annotation, scopes, diagnostics);
                }
                analyze_expr(&binding.value, scopes, diagnostics);
                scopes.define(
                    &binding.name,
                    binding.name_span,
                    BindingKind::Local,
                    diagnostics,
                );
            }
            MergedItem::Signature(signature) => {
                analyze_expr(&signature.annotation, scopes, diagnostics);
                scopes.define(
                    &signature.name,
                    signature.name_span,
                    BindingKind::Signature,
                    diagnostics,
                );
            }
            MergedItem::Expr(expr) => analyze_expr(expr, scopes, diagnostics),
        }
    }

    scopes.pop(diagnostics);
}

fn analyze_record_entries(
    entries: &[RecordEntry],
    scopes: &mut ScopeStack,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for entry in entries {
        match entry {
            RecordEntry::Field { value, .. }
            | RecordEntry::Spread { value, .. }
            | RecordEntry::Element(value) => analyze_expr(value, scopes, diagnostics),
            RecordEntry::Shorthand { name, .. } => scopes.mark_used(name),
            RecordEntry::Delete { .. } | RecordEntry::Rename { .. } | RecordEntry::Open { .. } => {}
        }
    }
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
        scopes.pop(diagnostics);
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
        scopes.define(
            binding.name,
            binding.span,
            BindingKind::Pattern,
            diagnostics,
        );
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
        .with_code(codes::name::UPPERCASE_RUNTIME_BINDING)
        .with_label(Label::primary(span, "runtime binding introduced here"))
        .with_note("runtime values use lowercase names; uppercase names are reserved for comptime identifiers"),
    );
}

impl ScopeStack {
    fn push(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        let Some(scope) = self.scopes.pop() else {
            return;
        };

        for binding in scope {
            if !binding.used && binding.kind.reports_unused() && !binding.name.starts_with('_') {
                diagnostics.push(unused_binding_diagnostic(&binding));
            }
        }
    }

    fn define(
        &mut self,
        name: &str,
        span: Span,
        kind: BindingKind,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        if name == "_" {
            return;
        }

        if let Some(previous) = self.find_current(name) {
            diagnostics.push(
                Diagnostic::error(format!("duplicate local binding `{name}`"))
                    .with_code(codes::name::DUPLICATE_LOCAL)
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
                    .with_code(codes::name::ACCIDENTAL_SHADOWING)
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
                kind,
                used: false,
            });
        }
    }

    fn mark_used(&mut self, name: &str) {
        if name == "_" {
            return;
        }

        if let Some(binding) = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.iter_mut().rev().find(|binding| binding.name == name))
        {
            binding.used = true;
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

impl BindingKind {
    fn reports_unused(self) -> bool {
        matches!(self, Self::Local | Self::Parameter | Self::Pattern)
    }
}

fn unused_binding_diagnostic(binding: &ScopeBinding) -> Diagnostic {
    let (message, label, note) = match binding.kind {
        BindingKind::Local => (
            format!("unused local binding `{}`", binding.name),
            "binding is never used",
            "remove the binding or use its value",
        ),
        BindingKind::Parameter => (
            format!("unused parameter `{}`", binding.name),
            "parameter is never used",
            "replace the parameter with `_` if the argument is intentionally ignored",
        ),
        BindingKind::Pattern => (
            format!("unused pattern binding `{}`", binding.name),
            "pattern binding is never used",
            "replace the binding with `_` in the pattern if the value is intentionally ignored",
        ),
        BindingKind::Signature => {
            unreachable!("unused_binding_diagnostic only receives kinds accepted by reports_unused")
        }
    };

    Diagnostic::warning(message)
        .with_code(codes::name::UNUSED_BINDING)
        .with_label(Label::primary(binding.span, label))
        .with_note(note)
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
            parse_module("HttpOk = 200\nUser = { name: Text }\nColor = @{ @Red, @Green }\n");
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

    #[test]
    fn reports_unused_lambda_parameters() {
        let output = parse_module("f = (value) => 1\n");
        let analysis = analyze_names(&output.module);

        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].code.as_deref(),
            Some("name.unused-binding")
        );
    }

    #[test]
    fn treats_record_shorthand_as_a_local_use() {
        let output = parse_module("f = (name) => { name }\n");
        let analysis = analyze_names(&output.module);

        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn ignores_unused_underscore_prefixed_bindings() {
        let output = parse_module("f = () =>\n  _scratch = 1\n  2\n");
        let analysis = analyze_names(&output.module);

        assert!(analysis.diagnostics.is_empty());
    }

    #[test]
    fn suppresses_unused_warnings_when_name_errors_exist() {
        let output = parse_module("f = (value, value) => 1\n");
        let analysis = analyze_names(&output.module);

        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(
            analysis.diagnostics[0].code.as_deref(),
            Some("name.duplicate-local")
        );
    }
}
