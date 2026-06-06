use std::collections::HashSet;

use aven_core::{Diagnostic, Label, Span, codes};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeLowering {
    pub ty: Type,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    /// A type expression that is valid to keep for a later comptime/type phase
    /// but is not part of the core lowered type grammar yet.
    Deferred,
    Named(String),
    Variable(String),
    Apply {
        callee: Box<Type>,
        args: Vec<Type>,
    },
    Function {
        params: Vec<Type>,
        result: Box<Type>,
    },
    Nullable(Box<Type>),
    Tuple(Vec<Type>),
    Record(Vec<TypeRowEntry>),
    Variant(Vec<TypeRowEntry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRowEntry {
    Field {
        name: String,
        ty: Type,
        overwrite: bool,
        optional: bool,
    },
    Tag {
        name: String,
        payload: Vec<Type>,
    },
    Spread {
        ty: Type,
        overwrite: bool,
    },
    Delete(String),
    Rename {
        from: String,
        to: String,
    },
    Shorthand(String),
    Open,
    Element(Type),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Record,
    Variant,
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

pub fn lower_annotation(module: &Module, annotation: &Expr) -> TypeLowering {
    let known_types = known_type_names(module);
    let mut checker = Checker {
        known_types,
        diagnostics: Vec::new(),
    };
    let ty = checker.lower_annotation(annotation);

    TypeLowering {
        ty,
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
            self.lower_annotation(annotation);
        }

        self.check_value_expr(&binding.value);
    }

    fn check_signature(&mut self, signature: &Signature) {
        self.lower_annotation(&signature.annotation);
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
                    self.lower_annotation(annotation);
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
                            .with_code(codes::ty::TYPE_ONLY_RECORD_ENTRY)
                            .with_label(Label::primary(*name_span, "optional field marker here"))
                            .with_note("remove `?` in value records; use `field = Nil` when the value is absent"),
                        );
                    }
                    self.check_value_expr(value);
                }
                RecordEntry::Open { span } => {
                    self.diagnostics.push(
                        Diagnostic::error("open row markers are only valid in type position")
                            .with_code(codes::ty::TYPE_ONLY_RECORD_ENTRY)
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
                self.lower_annotation(annotation);
            }
        }
    }

    fn lower_annotation(&mut self, annotation: &Expr) -> Type {
        match &annotation.kind {
            ExprKind::ComptimeName(name) => {
                self.check_type_name(name, annotation.span);
                Type::Named(name.clone())
            }
            ExprKind::Name(name) => Type::Variable(name.clone()),
            ExprKind::Group(inner) => self.lower_annotation(inner),
            ExprKind::Index { callee, args } => Type::Apply {
                callee: Box::new(self.lower_annotation(callee)),
                args: self.lower_annotations(args),
            },
            ExprKind::Nullable(inner) => Type::Nullable(Box::new(self.lower_annotation(inner))),
            ExprKind::Arrow { params, result } => Type::Function {
                params: self.lower_annotations(params),
                result: Box::new(self.lower_annotation(result)),
            },
            ExprKind::Tuple(items) => Type::Tuple(self.lower_annotations(items)),
            ExprKind::Record(entries) => {
                Type::Record(self.lower_row_entries(entries, RowKind::Record))
            }
            ExprKind::Set(entries) => {
                Type::Variant(self.lower_row_entries(entries, RowKind::Variant))
            }
            ExprKind::Missing => Type::Deferred,
            ExprKind::Literal(_)
            | ExprKind::Array(_)
            | ExprKind::FieldAccess { .. }
            | ExprKind::Call { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Propagate { .. }
            | ExprKind::Match { .. }
            | ExprKind::Lambda { .. }
            | ExprKind::Block(_) => {
                self.lower_deferred_annotation(annotation);
                Type::Deferred
            }
        }
    }

    fn lower_annotations(&mut self, items: &[Expr]) -> Vec<Type> {
        items
            .iter()
            .map(|item| self.lower_annotation(item))
            .collect()
    }

    fn lower_deferred_annotation(&mut self, annotation: &Expr) {
        walk_expr_children(annotation, &mut |child| {
            self.lower_annotation(child);
        });
    }

    fn lower_row_entries(&mut self, entries: &[RecordEntry], kind: RowKind) -> Vec<TypeRowEntry> {
        entries
            .iter()
            .map(|entry| self.lower_row_entry(entry, kind))
            .collect()
    }

    fn lower_row_entry(&mut self, entry: &RecordEntry, kind: RowKind) -> TypeRowEntry {
        match entry {
            RecordEntry::Field {
                name,
                value,
                overwrite,
                optional,
                ..
            } => TypeRowEntry::Field {
                name: name.clone(),
                ty: self.lower_annotation(value),
                overwrite: *overwrite,
                optional: *optional,
            },
            RecordEntry::Shorthand { name, .. } => TypeRowEntry::Shorthand(name.clone()),
            RecordEntry::Spread {
                value, overwrite, ..
            } => TypeRowEntry::Spread {
                ty: self.lower_annotation(value),
                overwrite: *overwrite,
            },
            RecordEntry::Delete { name, .. } => TypeRowEntry::Delete(name.clone()),
            RecordEntry::Rename { from, to, .. } => TypeRowEntry::Rename {
                from: from.clone(),
                to: to.clone(),
            },
            RecordEntry::Open { .. } => TypeRowEntry::Open,
            RecordEntry::Element(value) => match kind {
                RowKind::Record => TypeRowEntry::Element(self.lower_annotation(value)),
                RowKind::Variant => self.lower_variant_tag(value),
            },
        }
    }

    fn lower_variant_tag(&mut self, tag: &Expr) -> TypeRowEntry {
        match &tag.kind {
            ExprKind::ComptimeName(name) => TypeRowEntry::Tag {
                name: name.clone(),
                payload: Vec::new(),
            },
            ExprKind::Name(name) => {
                self.report_lowercase_variant_tag(name, tag.span);
                TypeRowEntry::Tag {
                    name: name.clone(),
                    payload: Vec::new(),
                }
            }
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::ComptimeName(name) => TypeRowEntry::Tag {
                    name: name.clone(),
                    payload: self.lower_annotations(args),
                },
                ExprKind::Name(name) => {
                    self.report_lowercase_variant_tag(name, callee.span);
                    TypeRowEntry::Tag {
                        name: name.clone(),
                        payload: self.lower_annotations(args),
                    }
                }
                _ => {
                    self.lower_deferred_annotation(tag);
                    TypeRowEntry::Element(Type::Deferred)
                }
            },
            _ => {
                self.lower_deferred_annotation(tag);
                TypeRowEntry::Element(Type::Deferred)
            }
        }
    }

    fn check_type_name(&mut self, name: &str, span: Span) {
        if self.known_types.contains(name) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error(format!("unknown type name `{name}`"))
                .with_code(codes::ty::UNKNOWN_NAME)
                .with_label(Label::primary(span, "type name not found"))
                .with_note("define the type, import it, or use a lowercase type variable for a generic type"),
        );
    }

    fn report_lowercase_variant_tag(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("variant tag `{name}` must start with uppercase"))
                .with_code(codes::ty::LOWERCASE_VARIANT_TAG)
                .with_label(Label::primary(span, "lowercase variant tag"))
                .with_note("variant tags use uppercase names, for example `Ok` or `Err`"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aven_parser::{Item, Module, parse_module};

    fn annotation<'a>(module: &'a Module, name: &str) -> &'a Expr {
        module
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.annotation.as_ref(),
                Item::Signature(signature) if signature.name == name => Some(&signature.annotation),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected annotation for {name}"))
    }

    fn named(name: &str) -> Type {
        Type::Named(name.to_owned())
    }

    fn variable(name: &str) -> Type {
        Type::Variable(name.to_owned())
    }

    fn apply(callee: Type, args: Vec<Type>) -> Type {
        Type::Apply {
            callee: Box::new(callee),
            args,
        }
    }

    fn function(params: Vec<Type>, result: Type) -> Type {
        Type::Function {
            params,
            result: Box::new(result),
        }
    }

    fn nullable(ty: Type) -> Type {
        Type::Nullable(Box::new(ty))
    }

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

    #[test]
    fn lowers_function_application_and_nullable_annotations() {
        let output =
            parse_module("mapper : (Array[a], a -> b) -> Array[b]\nvalue : Text? = name\n");

        let mapper = lower_annotation(&output.module, annotation(&output.module, "mapper"));
        let value = lower_annotation(&output.module, annotation(&output.module, "value"));

        assert_eq!(
            mapper.ty,
            function(
                vec![
                    apply(named("Array"), vec![variable("a")]),
                    function(vec![variable("a")], variable("b")),
                ],
                apply(named("Array"), vec![variable("b")]),
            )
        );
        assert!(mapper.diagnostics.is_empty());
        assert_eq!(value.ty, nullable(named("Text")));
        assert!(value.diagnostics.is_empty());
    }

    #[test]
    fn lowers_record_and_variant_annotations() {
        let output = parse_module(
            "FileError = @{Io}\n\
             user : { .._, name = Text, email = Text?, phone? = Text, -password } = current\n\
             error : @{ParseError(Text), NotFound, ..FileError, -Internal} = value\n",
        );

        let user = lower_annotation(&output.module, annotation(&output.module, "user"));
        let error = lower_annotation(&output.module, annotation(&output.module, "error"));

        assert_eq!(
            user.ty,
            Type::Record(vec![
                TypeRowEntry::Open,
                TypeRowEntry::Field {
                    name: "name".to_owned(),
                    ty: named("Text"),
                    overwrite: false,
                    optional: false,
                },
                TypeRowEntry::Field {
                    name: "email".to_owned(),
                    ty: nullable(named("Text")),
                    overwrite: false,
                    optional: false,
                },
                TypeRowEntry::Field {
                    name: "phone".to_owned(),
                    ty: named("Text"),
                    overwrite: false,
                    optional: true,
                },
                TypeRowEntry::Delete("password".to_owned()),
            ])
        );
        assert!(user.diagnostics.is_empty());

        assert_eq!(
            error.ty,
            Type::Variant(vec![
                TypeRowEntry::Tag {
                    name: "ParseError".to_owned(),
                    payload: vec![named("Text")],
                },
                TypeRowEntry::Tag {
                    name: "NotFound".to_owned(),
                    payload: Vec::new(),
                },
                TypeRowEntry::Spread {
                    ty: named("FileError"),
                    overwrite: false,
                },
                TypeRowEntry::Delete("Internal".to_owned()),
            ])
        );
        assert!(error.diagnostics.is_empty());
    }

    #[test]
    fn lower_annotation_reports_lowercase_variant_tags() {
        let output = parse_module("value : @{io} = value\n");
        let lowering = lower_annotation(&output.module, annotation(&output.module, "value"));

        assert_eq!(lowering.diagnostics.len(), 1);
        assert_eq!(
            lowering.diagnostics[0].code.as_deref(),
            Some(codes::ty::LOWERCASE_VARIANT_TAG)
        );
    }

    #[test]
    fn check_module_reports_type_only_entries_in_value_records() {
        let output = parse_module("value = { name? = 1 }\n");
        let check = check_module(&output.module);

        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(
            check.diagnostics[0].code.as_deref(),
            Some(codes::ty::TYPE_ONLY_RECORD_ENTRY)
        );
    }
}
