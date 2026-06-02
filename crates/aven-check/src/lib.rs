use std::collections::HashSet;

use aven_core::{Diagnostic, Label, Span};
use aven_parser::{
    Binding, DeclarationPhase, Expr, ExprKind, Item, MatchArm, Module, Param, RecordEntry,
    Signature, collect_declarations, walk_expr_children,
};

const BUILTIN_TYPES: &[&str] = &[
    "Bool", "Float", "Int", "Nil", "Text", "Unit",
    // Seeded std names until import resolution provides them.
    "Array", "Json", "Result", "Yaml",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
}

pub fn check_module(module: &Module) -> CheckOutput {
    let known_types = known_type_names(module);
    let mut checker = Checker {
        known_types,
        diagnostics: Vec::new(),
    };

    checker.check_items(&module.items);

    CheckOutput {
        diagnostics: checker.diagnostics,
    }
}

fn known_type_names(module: &Module) -> HashSet<String> {
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

#[derive(Debug)]
struct Checker {
    known_types: HashSet<String>,
    diagnostics: Vec<Diagnostic>,
}

impl Checker {
    fn check_items(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::Binding(binding) => self.check_binding(binding),
                Item::Signature(signature) => self.check_signature(signature),
                Item::Expr(expr) => self.check_value_expr(expr),
            }
        }
    }

    fn check_binding(&mut self, binding: &Binding) {
        if let Some(annotation) = &binding.annotation {
            self.check_annotation(annotation);
        }

        self.check_value_expr(&binding.value);
    }

    fn check_signature(&mut self, signature: &Signature) {
        self.check_annotation(&signature.annotation);
    }

    fn check_value_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Record(entries) | ExprKind::Set(entries) => {
                self.check_value_record_entries(entries);
            }
            ExprKind::Lambda {
                params,
                return_annotation,
                body,
            } => {
                self.check_params(params);
                if let Some(annotation) = return_annotation {
                    self.check_annotation(annotation);
                }
                self.check_value_expr(body);
            }
            ExprKind::Block(items) => self.check_items(items),
            ExprKind::Match { subject, arms, .. } => self.check_match(subject, arms),
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Name(_)
            | ExprKind::ComptimeName(_) => {}
            _ => walk_expr_children(expr, &mut |child| self.check_value_expr(child)),
        }
    }

    fn check_value_exprs(&mut self, items: &[Expr]) {
        for item in items {
            self.check_value_expr(item);
        }
    }

    fn check_value_record_entries(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Field {
                    value,
                    optional,
                    name_span,
                    ..
                } => {
                    if *optional {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "optional record fields are only valid in type position",
                            )
                            .with_code("type.type-only-record-entry")
                            .with_label(Label::primary(*name_span, "optional field marker here"))
                            .with_note("remove `?` in value records; use `field = Nil` when the value is absent"),
                        );
                    }
                    self.check_value_expr(value);
                }
                RecordEntry::Open { span } => {
                    self.diagnostics.push(
                        Diagnostic::error("open row markers are only valid in type position")
                            .with_code("type.type-only-record-entry")
                            .with_label(Label::primary(*span, "open row marker here"))
                            .with_note("remove `.._` from value records"),
                    );
                }
                RecordEntry::Spread { value, .. } | RecordEntry::Element(value) => {
                    self.check_value_expr(value);
                }
                RecordEntry::Shorthand { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::Rename { .. } => {}
            }
        }
    }

    fn check_match(&mut self, subject: &Expr, arms: &[MatchArm]) {
        self.check_value_expr(subject);

        for arm in arms {
            self.check_value_exprs(&arm.guards);
            self.check_value_expr(&arm.body);
        }
    }

    fn check_params(&mut self, params: &[Param]) {
        for param in params {
            if let Some(annotation) = &param.annotation {
                self.check_annotation(annotation);
            }
        }
    }

    fn check_annotation(&mut self, annotation: &Expr) {
        match &annotation.kind {
            ExprKind::ComptimeName(name) => self.check_type_name(name, annotation.span),
            ExprKind::Name(_) => {}
            ExprKind::Record(entries) => self.check_record_type_entries(entries),
            ExprKind::Set(entries) => self.check_variant_type_entries(entries),
            ExprKind::Match { subject, arms, .. } => {
                self.check_annotation(subject);
                for arm in arms {
                    self.check_annotation(&arm.pattern);
                    self.check_annotations(&arm.guards);
                    self.check_annotation(&arm.body);
                }
            }
            ExprKind::Lambda {
                params,
                return_annotation,
                body,
            } => {
                self.check_params(params);
                if let Some(annotation) = return_annotation {
                    self.check_annotation(annotation);
                }
                self.check_annotation(body);
            }
            ExprKind::Block(items) => self.check_items(items),
            ExprKind::Missing | ExprKind::Literal(_) => {}
            _ => walk_expr_children(annotation, &mut |child| self.check_annotation(child)),
        }
    }

    fn check_annotations(&mut self, items: &[Expr]) {
        for item in items {
            self.check_annotation(item);
        }
    }

    fn check_record_type_entries(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Field { value, .. }
                | RecordEntry::Spread { value, .. }
                | RecordEntry::Element(value) => self.check_annotation(value),
                RecordEntry::Shorthand { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Open { .. } => {}
            }
        }
    }

    fn check_variant_type_entries(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Element(value) => self.check_variant_tag(value),
                RecordEntry::Field { value, .. } | RecordEntry::Spread { value, .. } => {
                    self.check_annotation(value);
                }
                RecordEntry::Shorthand { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Open { .. } => {}
            }
        }
    }

    fn check_variant_tag(&mut self, tag: &Expr) {
        match &tag.kind {
            ExprKind::Name(name) => self.report_lowercase_variant_tag(name, tag.span),
            ExprKind::Call { callee, args } => {
                match &callee.kind {
                    ExprKind::Name(name) => self.report_lowercase_variant_tag(name, callee.span),
                    ExprKind::ComptimeName(_) => {}
                    _ => self.check_annotation(callee),
                }
                self.check_annotations(args);
            }
            ExprKind::ComptimeName(_) => {}
            _ => self.check_annotation(tag),
        }
    }

    fn check_type_name(&mut self, name: &str, span: Span) {
        if self.known_types.contains(name) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error(format!("unknown type name `{name}`"))
                .with_code("type.unknown-name")
                .with_label(Label::primary(span, "type name not found"))
                .with_note("define the type, import it, or use a lowercase type variable for a generic type"),
        );
    }

    fn report_lowercase_variant_tag(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("variant tag `{name}` must start with uppercase"))
                .with_code("type.lowercase-variant-tag")
                .with_label(Label::primary(span, "lowercase variant tag"))
                .with_note("variant tags use uppercase names, for example `Ok` or `Err`"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aven_parser::parse_module;

    #[test]
    fn lowercase_type_variables_are_not_unknown_names() {
        let output = parse_module("id : (a) -> a\nid = (value) => value\n");
        let check = check_module(&output.module);

        assert!(check.diagnostics.is_empty());
    }

    #[test]
    fn top_level_comptime_declarations_are_known_type_names() {
        let output = parse_module("User = { name = Text }\nvalue : User = user\n");
        let check = check_module(&output.module);

        assert!(check.diagnostics.is_empty());
    }

    #[test]
    fn reports_unknown_uppercase_type_names() {
        let output = parse_module("value : Missing = value\n");
        let check = check_module(&output.module);

        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(
            check.diagnostics[0].code.as_deref(),
            Some("type.unknown-name")
        );
    }
}
