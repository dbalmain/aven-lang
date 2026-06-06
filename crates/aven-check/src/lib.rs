use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, Item, Literal, MatchArm, Module, Param,
    RecordEntry, Signature, collect_declarations, walk_expr_children,
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
pub struct DeclaredAnnotation {
    pub name: String,
    pub declaration_span: Span,
    pub annotation_span: Span,
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
    let type_definitions = type_definitions(module, &known_types);
    let mut checker = Checker::with_type_definitions(known_types, type_definitions);

    checker.check_module(module);

    CheckOutput {
        diagnostics: checker.diagnostics,
    }
}

pub fn lower_annotation(module: &Module, annotation: &Expr) -> TypeLowering {
    let known_types = known_type_names(module);
    let mut checker = Checker::new(known_types);

    checker.lower_annotation_with_diagnostics(annotation)
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

fn type_definitions(module: &Module, known_types: &HashSet<String>) -> HashMap<String, Type> {
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

#[derive(Debug, Clone)]
struct DeclaredAnnotationSource<'a> {
    name: String,
    declaration_span: Span,
    annotation: &'a Expr,
}

fn declared_annotation_for_declaration<'a>(
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

fn binding_for_declaration<'a>(
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

#[derive(Debug)]
struct Checker {
    known_types: HashSet<String>,
    type_definitions: HashMap<String, Type>,
    diagnostics: Vec<Diagnostic>,
}

impl Checker {
    fn new(known_types: HashSet<String>) -> Self {
        Self::with_type_definitions(known_types, HashMap::new())
    }

    fn with_type_definitions(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
    ) -> Self {
        Self {
            known_types,
            type_definitions,
            diagnostics: Vec::new(),
        }
    }

    fn check_module(&mut self, module: &Module) {
        // Top-level declared annotations go through declarations so inline and
        // adjacent signature+binding forms share one lookup path.
        for declaration in collect_declarations(module) {
            self.check_declaration(module, &declaration);
        }

        for item in &module.items {
            if let Item::Expr(expr) = item {
                self.check_value_expr(expr);
            }
        }
    }

    fn check_declaration(&mut self, module: &Module, declaration: &Declaration) {
        let binding = binding_for_declaration(module, declaration);

        if let Some(source) = declared_annotation_for_declaration(module, declaration) {
            let declared_type = self.lower_annotation(source.annotation);
            let expected_type = self.normalize(&declared_type);

            if let Some(binding) = binding {
                self.check_value_against(&expected_type, &binding.value);
            }
        }

        if let Some(binding) = binding {
            self.check_value_expr(&binding.value);
        }
    }

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
            let declared_type = self.lower_annotation(annotation);
            let expected_type = self.normalize(&declared_type);
            self.check_value_against(&expected_type, &binding.value);
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

    fn check_value_against(&mut self, expected: &Type, value: &Expr) {
        match (&value.kind, expected) {
            (ExprKind::Group(inner), _) => self.check_value_against(expected, inner),
            (_, Type::Nullable(inner)) => {
                if !is_nil_value(value) {
                    self.check_value_against(inner, value);
                }
            }
            (ExprKind::Literal(literal), Type::Named(name)) => {
                if let Some(found) = mismatched_literal_kind(name, literal) {
                    self.report_type_mismatch(name, found, value.span);
                }
            }
            (ExprKind::Tuple(elements), Type::Tuple(element_types)) => {
                if elements.len() != element_types.len() {
                    self.report_tuple_arity_mismatch(
                        element_types.len(),
                        elements.len(),
                        value.span,
                    );
                } else {
                    for (element, element_type) in elements.iter().zip(element_types) {
                        self.check_value_against(element_type, element);
                    }
                }
            }
            (ExprKind::Record(value_entries), Type::Record(type_entries)) => {
                self.check_record_value_against(type_entries, value_entries, value.span);
            }
            _ => {}
        }
    }

    fn check_record_value_against(
        &mut self,
        type_entries: &[TypeRowEntry],
        value_entries: &[RecordEntry],
        value_span: Span,
    ) {
        let Some(expected) = literal_record_type(type_entries) else {
            return;
        };
        let Some(actual) = literal_record_value(value_entries, value_span) else {
            return;
        };

        let value_fields: HashMap<_, _> = actual
            .fields
            .iter()
            .map(|field| (field.name, field.value))
            .collect();
        let expected_field_names: HashSet<_> =
            expected.fields.iter().map(|field| field.name).collect();

        for field in &expected.fields {
            match value_fields.get(field.name) {
                Some(Some(value)) => self.check_value_against(field.ty, value),
                Some(None) => {}
                None if field.optional => {}
                None => self.report_missing_field(field.name, actual.span),
            }
        }

        if expected.open {
            return;
        }

        for field in &actual.fields {
            if !expected_field_names.contains(field.name) {
                self.report_unexpected_field(field.name, field.name_span);
            }
        }
    }

    fn lower_declared_annotation(
        &mut self,
        source: DeclaredAnnotationSource<'_>,
    ) -> DeclaredAnnotation {
        let lowering = self.lower_annotation_with_diagnostics(source.annotation);

        DeclaredAnnotation {
            name: source.name.to_owned(),
            declaration_span: source.declaration_span,
            annotation_span: source.annotation.span,
            ty: lowering.ty,
            diagnostics: lowering.diagnostics,
        }
    }

    fn lower_annotation_with_diagnostics(&mut self, annotation: &Expr) -> TypeLowering {
        let start = self.diagnostics.len();
        let ty = self.lower_annotation(annotation);
        let diagnostics = self.diagnostics[start..].to_vec();

        TypeLowering { ty, diagnostics }
    }

    fn normalize(&self, ty: &Type) -> Type {
        self.normalize_with_visited(ty, HashSet::new())
    }

    fn normalize_with_visited(&self, ty: &Type, visited: HashSet<String>) -> Type {
        match ty {
            Type::Named(name) => {
                let Some(definition) = self.type_definitions.get(name) else {
                    return Type::Named(name.clone());
                };

                if visited.contains(name) {
                    return Type::Named(name.clone());
                }

                let mut next_visited = visited;
                next_visited.insert(name.clone());
                self.normalize_with_visited(definition, next_visited)
            }
            Type::Deferred => Type::Deferred,
            Type::Variable(name) => Type::Variable(name.clone()),
            Type::Apply { callee, args } => Type::Apply {
                callee: Box::new(self.normalize_with_visited(callee, visited.clone())),
                args: self.normalize_types(args, &visited),
            },
            Type::Function { params, result } => Type::Function {
                params: self.normalize_types(params, &visited),
                result: Box::new(self.normalize_with_visited(result, visited)),
            },
            Type::Nullable(inner) => {
                Type::Nullable(Box::new(self.normalize_with_visited(inner, visited)))
            }
            Type::Tuple(items) => Type::Tuple(self.normalize_types(items, &visited)),
            Type::Record(entries) => Type::Record(self.normalize_row_entries(entries, &visited)),
            Type::Variant(entries) => Type::Variant(self.normalize_row_entries(entries, &visited)),
        }
    }

    fn normalize_types(&self, types: &[Type], visited: &HashSet<String>) -> Vec<Type> {
        types
            .iter()
            .map(|ty| self.normalize_with_visited(ty, visited.clone()))
            .collect()
    }

    fn normalize_row_entries(
        &self,
        entries: &[TypeRowEntry],
        visited: &HashSet<String>,
    ) -> Vec<TypeRowEntry> {
        entries
            .iter()
            .map(|entry| self.normalize_row_entry(entry, visited))
            .collect()
    }

    fn normalize_row_entry(&self, entry: &TypeRowEntry, visited: &HashSet<String>) -> TypeRowEntry {
        match entry {
            TypeRowEntry::Field {
                name,
                ty,
                overwrite,
                optional,
            } => TypeRowEntry::Field {
                name: name.clone(),
                ty: self.normalize_with_visited(ty, visited.clone()),
                overwrite: *overwrite,
                optional: *optional,
            },
            TypeRowEntry::Tag { name, payload } => TypeRowEntry::Tag {
                name: name.clone(),
                payload: self.normalize_types(payload, visited),
            },
            TypeRowEntry::Spread { ty, overwrite } => TypeRowEntry::Spread {
                ty: self.normalize_with_visited(ty, visited.clone()),
                overwrite: *overwrite,
            },
            TypeRowEntry::Element(ty) => {
                TypeRowEntry::Element(self.normalize_with_visited(ty, visited.clone()))
            }
            TypeRowEntry::Delete(name) => TypeRowEntry::Delete(name.clone()),
            TypeRowEntry::Rename { from, to } => TypeRowEntry::Rename {
                from: from.clone(),
                to: to.clone(),
            },
            TypeRowEntry::Shorthand(name) => TypeRowEntry::Shorthand(name.clone()),
            TypeRowEntry::Open => TypeRowEntry::Open,
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

    fn report_type_mismatch(&mut self, expected: &str, found: &'static str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("expected `{expected}`, found a {found}"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, format!("this is a {found}")))
                .with_note(format!(
                    "change the value to produce `{expected}`, or change the annotation to match the literal"
                )),
        );
    }

    fn report_tuple_arity_mismatch(&mut self, expected: usize, found: usize, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected a {expected}-element tuple, found a {found}-element tuple"
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "tuple length does not match annotation",
            ))
            .with_note("add or remove tuple elements to match the annotation"),
        );
    }

    fn report_missing_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("missing field `{name}`"))
                .with_code(codes::ty::MISSING_FIELD)
                .with_label(Label::primary(
                    span,
                    "this record is missing a required field",
                ))
                .with_note(format!(
                    "add `{name} = ...`, or make the field optional with `{name}?` in the type"
                )),
        );
    }

    fn report_unexpected_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("unexpected field `{name}`"))
                .with_code(codes::ty::UNEXPECTED_FIELD)
                .with_label(Label::primary(span, "this field is not in the record type"))
                .with_note(
                    "remove the field, or open the record type with `.._` to allow extra fields",
                ),
        );
    }
}

#[derive(Debug)]
struct ExpectedRecordShape<'a> {
    fields: Vec<ExpectedRecordField<'a>>,
    open: bool,
}

#[derive(Debug)]
struct ExpectedRecordField<'a> {
    name: &'a str,
    ty: &'a Type,
    optional: bool,
}

#[derive(Debug)]
struct ValueRecordShape<'a> {
    fields: Vec<ValueRecordField<'a>>,
    span: Span,
}

#[derive(Debug)]
struct ValueRecordField<'a> {
    name: &'a str,
    name_span: Span,
    value: Option<&'a Expr>,
}

fn literal_record_type(entries: &[TypeRowEntry]) -> Option<ExpectedRecordShape<'_>> {
    let mut fields = Vec::new();
    let mut open = false;

    for entry in entries {
        match entry {
            TypeRowEntry::Open => open = true,
            TypeRowEntry::Field {
                name,
                ty,
                overwrite: false,
                optional,
            } => fields.push(ExpectedRecordField {
                name,
                ty,
                optional: *optional,
            }),
            TypeRowEntry::Field {
                overwrite: true, ..
            }
            | TypeRowEntry::Tag { .. }
            | TypeRowEntry::Spread { .. }
            | TypeRowEntry::Delete(_)
            | TypeRowEntry::Rename { .. }
            | TypeRowEntry::Shorthand(_)
            | TypeRowEntry::Element(_) => return None,
        }
    }

    Some(ExpectedRecordShape { fields, open })
}

fn literal_record_value(entries: &[RecordEntry], span: Span) -> Option<ValueRecordShape<'_>> {
    let mut fields = Vec::new();

    for entry in entries {
        match entry {
            RecordEntry::Field {
                name,
                name_span,
                value,
                overwrite: false,
                ..
            } => fields.push(ValueRecordField {
                name,
                name_span: *name_span,
                value: Some(value),
            }),
            RecordEntry::Shorthand {
                name, name_span, ..
            } => fields.push(ValueRecordField {
                name,
                name_span: *name_span,
                value: None,
            }),
            RecordEntry::Field {
                overwrite: true, ..
            }
            | RecordEntry::Spread { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Open { .. }
            | RecordEntry::Element(_) => return None,
        }
    }

    Some(ValueRecordShape { fields, span })
}

fn mismatched_literal_kind(expected: &str, literal: &Literal) -> Option<&'static str> {
    match (expected, literal) {
        ("Text", Literal::String(_)) | ("Int" | "Float", Literal::Number(_)) => None,
        ("Int" | "Float" | "Bool" | "Nil" | "Unit", Literal::String(_)) => Some("text literal"),
        ("Text" | "Bool" | "Nil" | "Unit", Literal::Number(_)) => Some("number literal"),
        _ => None,
    }
}

fn is_nil_value(value: &Expr) -> bool {
    matches!(&value.kind, ExprKind::ComptimeName(name) if name == "Nil")
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
    fn annotation_lowerer_lowers_declaration_annotations() {
        let output = parse_module("value : Missing? = name\n");
        let declarations = collect_declarations(&output.module);
        let lowerer = AnnotationLowerer::new(&output.module);
        let declared = lowerer
            .lower_declaration(&output.module, &declarations[0])
            .expect("declared annotation");

        assert_eq!(declared.name, "value");
        assert_eq!(declared.ty, nullable(named("Missing")));
        assert_eq!(declared.diagnostics.len(), 1);
        assert_eq!(
            declared.diagnostics[0].code.as_deref(),
            Some(codes::ty::UNKNOWN_NAME)
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
    fn literal_bindings_accept_matching_scalar_annotations() {
        for source in [
            "value : Text = \"hi\"\n",
            "value : Int = 42\n",
            "value : Float = 42\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }
    }

    #[test]
    fn literal_bindings_report_definitive_scalar_mismatches() {
        for source in [
            "value : Int = \"hi\"\n",
            "value : Text = 42\n",
            "value : Text\nvalue = 42\n",
            "value : Int = (\"hi\")\n",
            "value : Bool = \"hi\"\n",
            "value : Nil = 42\n",
            "value : Unit = \"hi\"\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        }
    }

    #[test]
    fn literal_binding_mismatch_defers_non_literals_and_non_scalar_annotations() {
        for source in [
            "value : Int = other\n",
            "value : Float\nvalue = 42\n",
            "value : Int\nvalue = other\n",
            "value : { name = Text } = \"hi\"\n",
            "value : Missing = \"hi\"\n",
            "value : Missing\nvalue = \"hi\"\n",
            "value : (Int, Text) = pair\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }
    }

    #[test]
    fn separate_signature_binding_mismatch_reuses_declared_annotation_lookup() {
        let output = parse_module("value : Text\nvalue = 42\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn tuple_values_accept_matching_tuple_annotations() {
        for source in [
            "value : (Int, Text) = (1, \"a\")\n",
            "value : (Int, Float) = (1, 2)\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }
    }

    #[test]
    fn tuple_values_report_recursive_element_mismatches() {
        let output = parse_module("value : (Int, Text) = (1, 2)\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );
    }

    #[test]
    fn tuple_values_report_each_element_mismatch() {
        let output = parse_module("value : (Int, Text) = (\"a\", 2)\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 2);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Int`, found a text literal"
        );
        assert_eq!(
            check.diagnostics[1].message,
            "expected `Text`, found a number literal"
        );
    }

    #[test]
    fn parenthesized_values_are_checked_through_groups() {
        let output = parse_module("value : Int = (\"hi\")\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Int`, found a text literal"
        );
    }

    #[test]
    fn nullable_values_accept_nil_and_matching_inner_values() {
        for source in [
            "value : Text? = \"hi\"\n",
            "value : Text? = Nil\n",
            "value : Int? = Nil\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }
    }

    #[test]
    fn nullable_values_report_inner_mismatches() {
        let output = parse_module("value : Int? = \"hi\"\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Int`, found a text literal"
        );
    }

    #[test]
    fn nullable_values_defer_names() {
        let output = parse_module("value : Text? = other\n");
        let check = check_module(&output.module);

        assert!(!has_diagnostic_code(
            &check.diagnostics,
            codes::ty::MISMATCH
        ));
    }

    #[test]
    fn record_values_accept_exact_literal_record_annotations() {
        let output = parse_module("value : { name = Text } = { name = \"x\" }\n");
        let check = check_module(&output.module);

        assert!(check.diagnostics.is_empty());
    }

    #[test]
    fn record_values_report_field_value_mismatches() {
        let output = parse_module("value : { name = Text } = { name = 42 }\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );
    }

    #[test]
    fn record_values_report_missing_required_fields() {
        let output = parse_module("value : { name = Text, age = Int } = { name = \"x\" }\n");
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD),
            1
        );
    }

    #[test]
    fn record_values_report_unexpected_fields_in_closed_records() {
        let output = parse_module("value : { name = Text } = { name = \"x\", extra = 1 }\n");
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
            1
        );
    }

    #[test]
    fn open_record_types_allow_extra_value_fields() {
        let output = parse_module("value : { .._, name = Text } = { name = \"x\", extra = 1 }\n");
        let check = check_module(&output.module);

        assert!(check.diagnostics.is_empty());
    }

    #[test]
    fn optional_record_fields_may_be_absent_or_checked_when_present() {
        let output = parse_module("value : { name = Text, phone? = Text } = { name = \"x\" }\n");
        let check = check_module(&output.module);
        assert!(check.diagnostics.is_empty());

        let output = parse_module("value : { phone? = Text } = { phone = 42 }\n");
        let check = check_module(&output.module);
        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn nullable_record_fields_accept_nil() {
        let output = parse_module("value : { email = Text? } = { email = Nil }\n");
        let check = check_module(&output.module);

        assert!(check.diagnostics.is_empty());
    }

    #[test]
    fn nested_record_values_are_checked_recursively() {
        let output =
            parse_module("value : { user = { name = Text } } = { user = { name = 42 } }\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );
    }

    #[test]
    fn record_value_checking_defers_computed_rows() {
        for source in [
            "Base = { id = Int }\nvalue : { ..Base, name = Text } = { name = \"x\" }\n",
            "value : { name = Text } = { ..other, extra = 1 }\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
                "{source} unexpectedly produced type.missing-field"
            );
            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
                "{source} unexpectedly produced type.unexpected-field"
            );
            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }
    }

    #[test]
    fn transparent_scalar_aliases_are_normalized_before_checking() {
        let output = parse_module("Username = Text\nvalue : Username = 42\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );

        let output = parse_module("Username = Text\nvalue : Username = \"dave\"\n");
        let check = check_module(&output.module);
        assert!(!has_diagnostic_code(
            &check.diagnostics,
            codes::ty::MISMATCH
        ));
    }

    #[test]
    fn transparent_tuple_aliases_are_normalized_before_checking() {
        let output = parse_module("Pair = (Int, Text)\nvalue : Pair = (1, 2)\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );
    }

    #[test]
    fn transparent_alias_chains_are_normalized_before_checking() {
        let output = parse_module("A = B\nB = Text\nvalue : A = 42\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );
    }

    #[test]
    fn deferred_alias_definitions_do_not_emit_mismatches() {
        let output = parse_module("Wrapped = opaque(Text)\nvalue : Wrapped = 42\n");
        let check = check_module(&output.module);

        assert!(!has_diagnostic_code(
            &check.diagnostics,
            codes::ty::MISMATCH
        ));
    }

    #[test]
    fn cyclic_alias_normalization_terminates() {
        let output = parse_module("A = B\nB = A\nvalue : A = 42\n");
        let check = check_module(&output.module);

        assert!(!has_diagnostic_code(
            &check.diagnostics,
            codes::ty::MISMATCH
        ));

        let output = parse_module("A = (A, Int)\nvalue : A = (1, 2)\n");
        let check = check_module(&output.module);

        assert!(!has_diagnostic_code(
            &check.diagnostics,
            codes::ty::MISMATCH
        ));
    }

    #[test]
    fn tuple_values_report_arity_mismatches() {
        let output = parse_module("value : (Int, Text) = (1, \"a\", 3)\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected a 2-element tuple, found a 3-element tuple"
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

    fn has_diagnostic_code(diagnostics: &[Diagnostic], code: &str) -> bool {
        matching_codes(diagnostics, code) > 0
    }

    fn matching_codes(diagnostics: &[Diagnostic], code: &str) -> usize {
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code.as_deref() == Some(code))
            .count()
    }
}
