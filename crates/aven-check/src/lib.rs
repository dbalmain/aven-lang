use std::collections::{HashMap, HashSet, hash_map::Entry};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, Item, Literal, MatchArm, MergedItem,
    Module, Param, RecordEntry, Signature, collect_declarations, merged_items, pattern_bindings,
    walk_expr_children,
};

const BUILTIN_TYPES: &[&str] = &[
    "Bool", "Float", "Int", "Nil", "Text", "Unit",
    // Seeded std names until import resolution provides them.
    "Array", "Json", "Result", "Set", "Yaml",
];

const CHECKED_NAMED_TYPES: &[&str] = &["Bool", "Float", "Int", "Nil", "Text", "Unit"];

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
    /// A unification variable used only during value inference. It never appears
    /// in a lowered annotation or any checked output; synthesis resolves it away
    /// (or defers) before a type reaches `value_types`.
    Meta(u32),
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
    let mut checker = Checker::with_module(known_types, type_definitions, module);

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalValueType {
    Known(Type),
    Unknown,
}

type TypeEnv = HashMap<String, LocalValueType>;

#[derive(Debug, Default)]
struct LocalTypeScopes {
    scopes: Vec<TypeEnv>,
}

impl LocalTypeScopes {
    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: &str, ty: LocalValueType) {
        if name == "_" {
            return;
        }

        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_owned(), ty);
        }
    }

    fn get(&self, name: &str) -> Option<&LocalValueType> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn inference_env(&self) -> TypeEnv {
        let mut env = TypeEnv::new();
        for scope in &self.scopes {
            env.extend(scope.clone());
        }
        env
    }
}

#[derive(Debug)]
struct Checker<'a> {
    known_types: HashSet<String>,
    type_definitions: HashMap<String, Type>,
    value_types: HashMap<String, Option<Type>>,
    local_types: LocalTypeScopes,
    bindings: HashMap<String, Option<&'a Binding>>,
    annotations: HashMap<String, &'a Expr>,
    memo: HashMap<String, Type>,
    in_progress: HashSet<String>,
    unifier: Unifier,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Checker<'a> {
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
            value_types: HashMap::new(),
            local_types: LocalTypeScopes::default(),
            bindings: HashMap::new(),
            annotations: HashMap::new(),
            memo: HashMap::new(),
            in_progress: HashSet::new(),
            unifier: Unifier::default(),
            diagnostics: Vec::new(),
        }
    }

    fn with_module(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
    ) -> Self {
        let mut checker = Self::with_type_definitions(known_types, type_definitions);
        checker.collect_top_level_environment(module);
        checker.build_value_types(module);
        checker
    }

    fn collect_top_level_environment(&mut self, module: &'a Module) {
        for declaration in collect_declarations(module) {
            if declaration.phase != DeclarationPhase::Runtime {
                continue;
            }

            if let Some(source) = declared_annotation_for_declaration(module, &declaration) {
                self.annotations
                    .insert(declaration.name.clone(), source.annotation);
            }

            match self.bindings.entry(declaration.name.clone()) {
                Entry::Occupied(mut entry) => {
                    entry.insert(None);
                }
                Entry::Vacant(entry) => {
                    entry.insert(binding_for_declaration(module, &declaration));
                }
            }
        }
    }

    fn build_value_types(&mut self, module: &Module) {
        let mut types = HashMap::new();

        for declaration in collect_declarations(module) {
            if declaration.phase != DeclarationPhase::Runtime {
                continue;
            }

            let name = declaration.name.clone();
            match types.entry(name.clone()) {
                Entry::Occupied(mut entry) => {
                    // A duplicate name is an overload: defer its published
                    // type until overload selection exists.
                    entry.insert(None);
                    continue;
                }
                Entry::Vacant(entry) => {
                    entry.insert(None);
                }
            }

            if binding_for_declaration(module, &declaration).is_none() {
                continue;
            }

            if let Some(annotation) = self.clean_declared_annotation(&name) {
                types.insert(name.clone(), Some(annotation));
            } else if let Some(inferred) = self.infer_top_level_value(&name) {
                types.insert(name.clone(), Some(inferred));
            }
        }

        self.value_types = types;
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
        let mut checked_value = false;

        if let Some(source) = declared_annotation_for_declaration(module, declaration) {
            let declared_type = self.lower_annotation(source.annotation);
            let expected_type = self.normalize(&declared_type);

            if let Some(binding) = binding {
                self.check_value_against(&expected_type, &binding.value);
                checked_value = true;
            }
        }

        if !checked_value && let Some(binding) = binding {
            self.check_value_expr(&binding.value);
        }
    }

    fn check_items(&mut self, items: &[Item]) {
        self.local_types.push();

        for item in merged_items(items) {
            match item {
                MergedItem::Binding { signature, binding } => {
                    self.check_local_binding(binding, signature);
                }
                MergedItem::Signature(signature) => {
                    let ty = self.lower_normalized_annotation(&signature.annotation);
                    self.local_types
                        .define(&signature.name, LocalValueType::Known(ty));
                }
                MergedItem::Expr(expr) => self.check_value_expr(expr),
            }
        }

        self.local_types.pop();
    }

    fn check_local_binding(&mut self, binding: &Binding, signature: Option<&Signature>) {
        let signature_type =
            signature.map(|signature| self.lower_normalized_annotation(&signature.annotation));
        let binding_type = binding
            .annotation
            .as_ref()
            .map(|annotation| self.lower_normalized_annotation(annotation));
        let declared_type = signature_type.as_ref().or(binding_type.as_ref());

        if let Some(expected) = declared_type {
            self.check_value_against(expected, &binding.value);
        }

        let inferred_type = if declared_type.is_none() {
            let env = self.local_types.inference_env();
            let inferred = self.infer_local_value(&env, &binding.value);
            self.check_value_expr(&binding.value);
            inferred
        } else {
            None
        };

        self.local_types.define(
            &binding.name,
            declared_type
                .cloned()
                .or(inferred_type)
                .map(LocalValueType::Known)
                .unwrap_or(LocalValueType::Unknown),
        );
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
            } => self.check_lambda_value_expr(params, return_annotation.as_deref(), body),
            ExprKind::Block(items) => self.check_items(items),
            ExprKind::Match { subject, arms, .. } => {
                self.check_match_arms(subject, arms, None);
            }
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Name(_)
            | ExprKind::ComptimeName(_) => {}
            _ => walk_expr_children(expr, &mut |child| {
                self.check_value_expr(child);
            }),
        }
    }

    fn check_lambda_value_expr(
        &mut self,
        params: &[Param],
        return_annotation: Option<&Expr>,
        body: &Expr,
    ) {
        let param_types: Vec<_> = params
            .iter()
            .map(|param| {
                param
                    .annotation
                    .as_ref()
                    .map(|annotation| {
                        LocalValueType::Known(self.lower_normalized_annotation(annotation))
                    })
                    .unwrap_or(LocalValueType::Unknown)
            })
            .collect();
        if let Some(annotation) = return_annotation {
            self.lower_annotation(annotation);
        }

        self.local_types.push();
        for (param, ty) in params.iter().zip(param_types) {
            self.local_types.define(&param.name, ty);
        }
        self.check_value_expr(body);
        self.local_types.pop();
    }

    fn check_value_exprs(&mut self, items: &[Expr]) {
        for item in items {
            self.check_value_expr(item);
        }
    }

    fn check_value_record_entries(&mut self, entries: &[RecordEntry]) {
        self.report_value_record_markers(entries);
        self.walk_value_record_values(entries);
    }

    fn report_value_record_markers(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Field {
                    optional: true,
                    name_span,
                    ..
                } => {
                    self.diagnostics.push(
                        Diagnostic::error("optional record fields are only valid in type position")
                            .with_code(codes::ty::TYPE_ONLY_RECORD_ENTRY)
                            .with_label(Label::primary(*name_span, "optional field marker here"))
                            .with_note("remove `?` in value records; use `field = Nil` when the value is absent"),
                    );
                }
                RecordEntry::Open { span } => {
                    self.diagnostics.push(
                        Diagnostic::error("open row markers are only valid in type position")
                            .with_code(codes::ty::TYPE_ONLY_RECORD_ENTRY)
                            .with_label(Label::primary(*span, "open row marker here"))
                            .with_note("remove `.._` from value records"),
                    );
                }
                RecordEntry::Field { .. }
                | RecordEntry::Shorthand { .. }
                | RecordEntry::Spread { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Element(_) => {}
            }
        }
    }

    fn walk_value_record_values(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Field { value, .. }
                | RecordEntry::Spread { value, .. }
                | RecordEntry::Element(value) => {
                    self.check_value_expr(value);
                }
                RecordEntry::Shorthand { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Open { .. } => {}
            }
        }
    }

    fn check_match_arms(&mut self, subject: &Expr, arms: &[MatchArm], expected: Option<&Type>) {
        self.check_value_expr(subject);
        let env = self.local_types.inference_env();
        let subject_type = self.infer_local_value(&env, subject);

        for arm in arms {
            self.local_types.push();
            for (name, ty) in pattern_local_types(&arm.pattern, subject_type.as_ref()) {
                self.local_types.define(&name, ty);
            }
            let bool_type = named_builtin("Bool");
            for guard in &arm.guards {
                self.check_value_against(&bool_type, guard);
            }
            if let Some(expected) = expected {
                self.check_value_against(expected, &arm.body);
            } else {
                self.check_value_expr(&arm.body);
            }
            self.local_types.pop();
        }
    }

    fn lower_normalized_annotation(&mut self, annotation: &Expr) -> Type {
        let ty = self.lower_annotation(annotation);
        self.normalize(&ty)
    }

    fn check_value_against(&mut self, expected: &Type, value: &Expr) {
        match (&value.kind, expected) {
            (ExprKind::Group(inner), _) => self.check_value_against(expected, inner),
            (ExprKind::Block(items), _) => self.check_block_against(expected, items),
            (
                ExprKind::Lambda {
                    params,
                    return_annotation,
                    body,
                },
                Type::Function {
                    params: expected_params,
                    result: expected_result,
                },
            ) => self.check_lambda_against_function(
                value.span,
                params,
                return_annotation.as_deref(),
                body,
                expected_params,
                expected_result,
            ),
            (ExprKind::Name(name), _) => match self.local_types.get(name).cloned() {
                Some(LocalValueType::Known(actual)) => {
                    self.check_type_against_type(expected, &actual, value.span);
                }
                Some(LocalValueType::Unknown) => {}
                None => {
                    if let Some(Some(actual)) = self.value_types.get(name).cloned() {
                        self.check_type_against_type(expected, &actual, value.span);
                    }
                }
            },
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
                    self.check_value_exprs(elements);
                } else {
                    for (element, element_type) in elements.iter().zip(element_types) {
                        self.check_value_against(element_type, element);
                    }
                }
            }
            (ExprKind::Record(value_entries), Type::Record(type_entries)) => {
                self.check_record_value_against(type_entries, value_entries, value.span);
            }
            (ExprKind::ComptimeName(tag), Type::Variant(type_entries)) => {
                self.check_variant_value_against(type_entries, tag, &[], value.span);
            }
            (ExprKind::Call { callee, args }, Type::Variant(type_entries))
                if matches!(&callee.kind, ExprKind::ComptimeName(_)) =>
            {
                let ExprKind::ComptimeName(tag) = &callee.kind else {
                    return;
                };
                self.check_variant_value_against(type_entries, tag, args, value.span);
            }
            (ExprKind::Match { subject, arms, .. }, _) => {
                self.check_match_arms(subject, arms, Some(expected));
            }
            (
                ExprKind::Array(elements),
                Type::Apply {
                    callee,
                    args: element_types,
                },
            ) if matches!(callee.as_ref(), Type::Named(name) if name == "Array")
                && element_types.len() == 1 =>
            {
                self.check_collection_elements(&element_types[0], elements);
            }
            (
                ExprKind::Set(entries),
                Type::Apply {
                    callee,
                    args: element_types,
                },
            ) if matches!(callee.as_ref(), Type::Named(name) if name == "Set")
                && element_types.len() == 1 =>
            {
                self.report_value_record_markers(entries);
                if let Some(elements) = literal_set_elements(entries) {
                    self.check_collection_elements(&element_types[0], elements);
                } else {
                    self.walk_value_record_values(entries);
                }
            }
            _ => {
                self.check_value_expr(value);
                let env = self.local_types.inference_env();
                if let Some(actual) = self.infer_local_value(&env, value) {
                    self.check_type_against_type(expected, &actual, value.span);
                }
            }
        }
    }

    fn check_block_against(&mut self, expected: &Type, items: &[Item]) {
        self.local_types.push();

        let final_expr = match items.last() {
            Some(Item::Expr(expr)) => Some(expr),
            _ => None,
        };
        let prefix_len = if final_expr.is_some() {
            items.len().saturating_sub(1)
        } else {
            items.len()
        };

        for item in merged_items(&items[..prefix_len]) {
            match item {
                MergedItem::Binding { signature, binding } => {
                    self.check_local_binding(binding, signature);
                }
                MergedItem::Signature(signature) => {
                    let ty = self.lower_normalized_annotation(&signature.annotation);
                    self.local_types
                        .define(&signature.name, LocalValueType::Known(ty));
                }
                MergedItem::Expr(expr) => self.check_value_expr(expr),
            }
        }

        if let Some(expr) = final_expr {
            self.check_value_against(expected, expr);
        }

        self.local_types.pop();
    }

    fn check_lambda_against_function(
        &mut self,
        lambda_span: Span,
        params: &[Param],
        return_annotation: Option<&Expr>,
        body: &Expr,
        expected_params: &[Type],
        expected_result: &Type,
    ) {
        if params.len() != expected_params.len() {
            self.report_function_arity_mismatch(expected_params.len(), params.len(), lambda_span);
            self.check_lambda_value_expr(params, return_annotation, body);
            return;
        }

        let mut param_types = Vec::new();
        for (param, expected) in params.iter().zip(expected_params) {
            let actual = param
                .annotation
                .as_ref()
                .map(|annotation| {
                    let actual = self.lower_normalized_annotation(annotation);
                    // Function parameters are contravariant. A lambda's
                    // explicit parameter annotation is the actual accepted type,
                    // so compare it in the same swapped direction as
                    // Function-vs-Function comparison.
                    self.check_type_against_type(&actual, expected, annotation.span);
                    actual
                })
                .unwrap_or_else(|| expected.clone());
            param_types.push(actual);
        }

        let body_expected = if let Some(annotation) = return_annotation {
            let actual = self.lower_normalized_annotation(annotation);
            self.check_type_against_type(expected_result, &actual, annotation.span);
            actual
        } else {
            expected_result.clone()
        };

        self.local_types.push();
        for (param, ty) in params.iter().zip(param_types) {
            self.local_types
                .define(&param.name, LocalValueType::Known(ty));
        }
        self.check_value_against(&body_expected, body);
        self.local_types.pop();
    }

    fn check_collection_elements<'b>(
        &mut self,
        element_type: &Type,
        elements: impl IntoIterator<Item = &'b Expr>,
    ) {
        for element in elements {
            self.check_value_against(element_type, element);
        }
    }

    fn check_type_against_type(&mut self, expected: &Type, actual: &Type, span: Span) {
        if expected == actual {
            return;
        }

        match (expected, actual) {
            (Type::Nullable(_), Type::Named(name)) if name == "Nil" => {}
            (Type::Nullable(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Named(expected), Type::Named(actual)) => {
                if named_type_mismatch(expected, actual) {
                    self.report_type_mismatch_between_types(expected, actual, span);
                }
            }
            (Type::Named(expected), Type::Nullable(actual)) => {
                if let Type::Named(actual) = actual.as_ref()
                    && (named_type_mismatch(expected, actual) || expected == actual)
                {
                    self.report_type_mismatch_between_types(expected, &format!("{actual}?"), span);
                }
            }
            (Type::Tuple(expected), Type::Tuple(actual)) => {
                if expected.len() != actual.len() {
                    self.report_tuple_arity_mismatch(expected.len(), actual.len(), span);
                } else {
                    for (expected, actual) in expected.iter().zip(actual) {
                        self.check_type_against_type(expected, actual, span);
                    }
                }
            }
            (Type::Tuple(expected), Type::Named(actual)) if actual == "Unit" => {
                if !expected.is_empty() {
                    self.report_tuple_arity_mismatch(expected.len(), 0, span);
                }
            }
            (
                Type::Function {
                    params: expected_params,
                    result: expected_result,
                },
                Type::Function {
                    params: actual_params,
                    result: actual_result,
                },
            ) => {
                if expected_params.len() != actual_params.len() {
                    self.report_function_arity_mismatch(
                        expected_params.len(),
                        actual_params.len(),
                        span,
                    );
                } else {
                    for (expected, actual) in expected_params.iter().zip(actual_params) {
                        // Function parameters are contravariant: the actual
                        // function may accept a wider type than callers of the
                        // expected function promise to pass.
                        self.check_type_against_type(actual, expected, span);
                    }
                    self.check_type_against_type(expected_result, actual_result, span);
                }
            }
            (
                Type::Apply {
                    callee: expected_callee,
                    args: expected_args,
                },
                Type::Apply {
                    callee: actual_callee,
                    args: actual_args,
                },
            ) if expected_args.len() == actual_args.len() => {
                self.check_type_against_type(expected_callee, actual_callee, span);
                for (expected, actual) in expected_args.iter().zip(actual_args) {
                    self.check_type_against_type(expected, actual, span);
                }
            }
            (Type::Record(expected), Type::Record(actual)) => {
                let (Some(expected), Some(actual)) =
                    (literal_record_type(expected), literal_record_type(actual))
                else {
                    return;
                };
                if actual.open || actual.fields.iter().any(|field| field.optional) {
                    return;
                }

                let actual_fields: Vec<_> = actual
                    .fields
                    .iter()
                    .map(|field| (field.name, span, FieldValue::Type(field.ty)))
                    .collect();
                self.compare_record(&expected, &actual_fields, span);
            }
            (Type::Variant(expected), Type::Variant(actual)) => {
                self.check_variant_type_against_type(expected, actual, span);
            }
            _ => {}
        }
    }

    fn check_variant_type_against_type(
        &mut self,
        expected_entries: &[TypeRowEntry],
        actual_entries: &[TypeRowEntry],
        span: Span,
    ) {
        let Some(actual_tags) = literal_variant_tags(actual_entries) else {
            return;
        };

        for tag in actual_tags {
            let Some(payload) = literal_variant_payload_lookup(expected_entries, tag.name) else {
                return;
            };

            let Some(expected_payload) = payload else {
                self.report_variant_tag_mismatch(tag.name, span);
                continue;
            };

            if expected_payload.len() != tag.payload.len() {
                self.report_variant_payload_arity_mismatch(
                    tag.name,
                    expected_payload.len(),
                    tag.payload.len(),
                    span,
                );
                continue;
            }

            for (expected, actual) in expected_payload.iter().zip(tag.payload) {
                self.check_type_against_type(expected, actual, span);
            }
        }
    }

    fn check_record_value_against(
        &mut self,
        type_entries: &[TypeRowEntry],
        value_entries: &[RecordEntry],
        value_span: Span,
    ) {
        self.report_value_record_markers(value_entries);

        let (Some(expected), Some(actual)) = (
            literal_record_type(type_entries),
            literal_record_value(value_entries, value_span),
        ) else {
            self.walk_value_record_values(value_entries);
            return;
        };

        let actual_fields: Vec<_> = actual
            .fields
            .iter()
            .map(|field| (field.name, field.name_span, FieldValue::Value(field.value)))
            .collect();
        self.compare_record(&expected, &actual_fields, actual.span);
    }

    fn check_variant_value_against(
        &mut self,
        type_entries: &[TypeRowEntry],
        tag: &str,
        args: &[Expr],
        value_span: Span,
    ) {
        let Some(payload) = literal_variant_payload_lookup(type_entries, tag) else {
            self.check_value_exprs(args);
            return;
        };

        let Some(expected_payload) = payload else {
            self.report_variant_tag_mismatch(tag, value_span);
            self.check_value_exprs(args);
            return;
        };

        if expected_payload.len() != args.len() {
            self.report_variant_payload_arity_mismatch(
                tag,
                expected_payload.len(),
                args.len(),
                value_span,
            );
            self.check_value_exprs(args);
            return;
        }

        for (arg, expected) in args.iter().zip(expected_payload) {
            self.check_value_against(expected, arg);
        }
    }

    fn compare_record(
        &mut self,
        expected: &ExpectedRecordShape<'_>,
        actual: &[(&str, Span, FieldValue<'_>)],
        record_span: Span,
    ) {
        let actual_fields: HashMap<_, _> = actual
            .iter()
            .map(|(name, _, payload)| (*name, *payload))
            .collect();
        let expected_field_names: HashSet<_> =
            expected.fields.iter().map(|field| field.name).collect();

        for field in &expected.fields {
            match actual_fields.get(field.name).copied() {
                Some(FieldValue::Value(Some(value))) => {
                    self.check_value_against(field.ty, value);
                }
                Some(FieldValue::Value(None)) => {}
                Some(FieldValue::Type(ty)) => {
                    self.check_type_against_type(field.ty, ty, record_span)
                }
                None if field.optional => {}
                None => self.report_missing_field(field.name, record_span),
            }
        }

        for (name, blame_span, payload) in actual {
            if !expected_field_names.contains(name) {
                if !expected.open {
                    self.report_unexpected_field(name, *blame_span);
                }
                if let FieldValue::Value(Some(value)) = payload {
                    self.check_value_expr(value);
                }
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
            Type::Meta(id) => Type::Meta(*id),
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

    fn report_function_arity_mismatch(&mut self, expected: usize, found: usize, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected a function with {expected} parameter{}, found one with {found}",
                if expected == 1 { "" } else { "s" },
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "function parameter count does not match annotation",
            ))
            .with_note("add or remove parameters to match the annotation"),
        );
    }

    fn report_variant_tag_mismatch(&mut self, tag: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("unexpected variant tag `{tag}`"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, "this tag is not in the variant type"))
                .with_note("use a tag listed by the annotation, or change the annotation"),
        );
    }

    fn report_variant_payload_arity_mismatch(
        &mut self,
        tag: &str,
        expected: usize,
        found: usize,
        span: Span,
    ) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected variant tag `{tag}` with {expected} payload value{}, found {found}",
                if expected == 1 { "" } else { "s" },
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "variant payload count does not match annotation",
            ))
            .with_note("add or remove payload values to match the variant annotation"),
        );
    }

    fn report_type_mismatch_between_types(&mut self, expected: &str, actual: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("expected `{expected}`, found `{actual}`"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    span,
                    format!("this value has type `{actual}`"),
                ))
                .with_note(format!(
                    "change the value to produce `{expected}`, or change the annotation to `{actual}`"
                )),
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

#[derive(Debug, Clone, Copy)]
enum FieldValue<'a> {
    Value(Option<&'a Expr>),
    Type(&'a Type),
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

#[derive(Debug)]
struct VariantTagShape<'a> {
    name: &'a str,
    payload: &'a [Type],
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

fn literal_set_elements(entries: &[RecordEntry]) -> Option<Vec<&Expr>> {
    entries
        .iter()
        .map(|entry| match entry {
            RecordEntry::Element(value) => Some(value),
            RecordEntry::Field { .. }
            | RecordEntry::Shorthand { .. }
            | RecordEntry::Spread { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Open { .. } => None,
        })
        .collect()
}

fn pattern_local_types(pattern: &Expr, expected: Option<&Type>) -> Vec<(String, LocalValueType)> {
    let mut known = HashMap::new();
    if let Some(expected) = expected {
        collect_known_pattern_types(pattern, expected, &mut known);
    }

    pattern_bindings(pattern)
        .into_iter()
        .map(|binding| {
            let ty = known
                .get(binding.name)
                .cloned()
                .map(LocalValueType::Known)
                .unwrap_or(LocalValueType::Unknown);
            (binding.name.to_owned(), ty)
        })
        .collect()
}

fn collect_known_pattern_types(pattern: &Expr, expected: &Type, known: &mut HashMap<String, Type>) {
    match (&pattern.kind, expected) {
        (ExprKind::Group(inner), _) => collect_known_pattern_types(inner, expected, known),
        (ExprKind::Name(name), _) if name != "_" && is_concrete_type(expected) => {
            known.insert(name.clone(), expected.clone());
        }
        (ExprKind::Call { callee, args }, Type::Variant(entries)) => {
            let ExprKind::ComptimeName(tag) = &callee.kind else {
                return;
            };
            let Some(payload) = literal_variant_payload(entries, tag) else {
                return;
            };
            if payload.len() != args.len() {
                return;
            }
            for (arg, ty) in args.iter().zip(payload) {
                collect_known_pattern_types(arg, ty, known);
            }
        }
        (ExprKind::ComptimeName(_), Type::Variant(_)) => {}
        _ => {}
    }
}

fn literal_variant_payload<'a>(entries: &'a [TypeRowEntry], tag: &str) -> Option<&'a [Type]> {
    literal_variant_payload_lookup(entries, tag).flatten()
}

fn literal_variant_payload_lookup<'a>(
    entries: &'a [TypeRowEntry],
    tag: &str,
) -> Option<Option<&'a [Type]>> {
    let mut found = None;

    for entry in entries {
        match entry {
            TypeRowEntry::Tag { name, payload } if name == tag => {
                found = Some(payload.as_slice());
            }
            TypeRowEntry::Tag { .. } => {}
            TypeRowEntry::Field { .. }
            | TypeRowEntry::Spread { .. }
            | TypeRowEntry::Delete(_)
            | TypeRowEntry::Rename { .. }
            | TypeRowEntry::Shorthand(_)
            | TypeRowEntry::Open
            | TypeRowEntry::Element(_) => return None,
        }
    }

    Some(found)
}

fn literal_variant_tags(entries: &[TypeRowEntry]) -> Option<Vec<VariantTagShape<'_>>> {
    let mut tags = Vec::new();

    for entry in entries {
        match entry {
            TypeRowEntry::Tag { name, payload } => tags.push(VariantTagShape {
                name,
                payload: payload.as_slice(),
            }),
            TypeRowEntry::Field { .. }
            | TypeRowEntry::Spread { .. }
            | TypeRowEntry::Delete(_)
            | TypeRowEntry::Rename { .. }
            | TypeRowEntry::Shorthand(_)
            | TypeRowEntry::Open
            | TypeRowEntry::Element(_) => return None,
        }
    }

    Some(tags)
}

#[derive(Debug, Default)]
struct Unifier {
    substitution: Vec<Option<Type>>,
}

impl Unifier {
    fn fresh(&mut self) -> Type {
        let id = self.substitution.len() as u32;
        self.substitution.push(None);
        Type::Meta(id)
    }

    fn resolve(&self, ty: &Type) -> Type {
        map_type(ty, &mut |node| match node {
            Type::Meta(id) => match self.substitution.get(*id as usize) {
                Some(Some(bound)) => Some(self.resolve(bound)),
                _ => None,
            },
            _ => None,
        })
    }

    /// Capture the current substitution so a speculative sequence of
    /// unifications can be rolled back with [`Unifier::restore`].
    fn snapshot(&self) -> Vec<Option<Type>> {
        self.substitution.clone()
    }

    fn restore(&mut self, snapshot: Vec<Option<Type>>) {
        self.substitution = snapshot;
    }

    fn unify(&mut self, left: &Type, right: &Type) -> Result<(), ()> {
        let snapshot = self.snapshot();
        if self.unify_inner(left, right).is_err() {
            self.restore(snapshot);
            Err(())
        } else {
            Ok(())
        }
    }

    fn unify_inner(&mut self, left: &Type, right: &Type) -> Result<(), ()> {
        let left = self.resolve(left);
        let right = self.resolve(right);

        match (&left, &right) {
            (Type::Meta(left), Type::Meta(right)) if left == right => Ok(()),
            (Type::Meta(id), ty) | (ty, Type::Meta(id)) => self.bind(*id, ty),
            (Type::Named(left), Type::Named(right)) if left == right => Ok(()),
            (Type::Variable(left), Type::Variable(right)) if left == right => Ok(()),
            (
                Type::Apply {
                    callee: left_callee,
                    args: left_args,
                },
                Type::Apply {
                    callee: right_callee,
                    args: right_args,
                },
            ) if left_args.len() == right_args.len() => {
                self.unify_inner(left_callee, right_callee)?;
                self.unify_many(left_args, right_args)
            }
            (
                Type::Function {
                    params: left_params,
                    result: left_result,
                },
                Type::Function {
                    params: right_params,
                    result: right_result,
                },
            ) if left_params.len() == right_params.len() => {
                self.unify_many(left_params, right_params)?;
                self.unify_inner(left_result, right_result)
            }
            (Type::Nullable(left), Type::Nullable(right)) => self.unify_inner(left, right),
            (Type::Tuple(left), Type::Tuple(right)) if left.len() == right.len() => {
                self.unify_many(left, right)
            }
            _ => Err(()),
        }
    }

    fn unify_many(&mut self, left: &[Type], right: &[Type]) -> Result<(), ()> {
        for (left, right) in left.iter().zip(right) {
            self.unify_inner(left, right)?;
        }
        Ok(())
    }

    fn bind(&mut self, id: u32, ty: &Type) -> Result<(), ()> {
        let ty = self.resolve(ty);
        if ty == Type::Meta(id) {
            return Ok(());
        }
        if type_contains_meta(&ty, id) {
            return Err(());
        }

        let Some(slot) = self.substitution.get_mut(id as usize) else {
            return Err(());
        };
        *slot = Some(ty);
        Ok(())
    }

    fn instantiate(&mut self, ty: &Type) -> Type {
        // Memoized binding types are stored fully resolved, so any `Meta` left
        // here is a generic placeholder. Replacing each with a fresh meta lets a
        // top-level binding be applied at more than one type without its generics
        // leaking between uses.
        let mut replacements: HashMap<u32, Type> = HashMap::new();
        map_type(ty, &mut |node| match node {
            Type::Meta(id) => Some(if let Some(existing) = replacements.get(id) {
                existing.clone()
            } else {
                let fresh = self.fresh();
                replacements.insert(*id, fresh.clone());
                fresh
            }),
            _ => None,
        })
    }
}

impl<'a> Checker<'a> {
    fn infer_top_level_value(&mut self, name: &str) -> Option<Type> {
        let ty = self.infer_top_level(name)?;
        self.resolve_if_concrete(&ty)
    }

    fn infer_local_value(&mut self, env: &TypeEnv, value: &Expr) -> Option<Type> {
        let ty = self.infer(env, value);
        self.resolve_if_concrete(&ty)
    }

    /// Fully resolve `ty`; keep it only when no metavariable remains, so a
    /// synthesized value type never leaks an unsolved meta into checking.
    fn resolve_if_concrete(&self, ty: &Type) -> Option<Type> {
        let ty = self.unifier.resolve(ty);
        is_concrete_type(&ty).then_some(ty)
    }

    fn infer_top_level(&mut self, name: &str) -> Option<Type> {
        if let Some(ty) = self.memo.get(name).cloned() {
            return Some(ty);
        }
        if self.in_progress.contains(name) {
            return Some(Type::Deferred);
        }

        let binding = (*self.bindings.get(name)?)?;
        self.in_progress.insert(name.to_owned());

        let ty = if let Some(annotation) = self.clean_declared_annotation(name) {
            annotation
        } else {
            self.infer(&TypeEnv::new(), &binding.value)
        };
        let ty = self.unifier.resolve(&ty);

        self.in_progress.remove(name);
        self.memo.insert(name.to_owned(), ty.clone());
        Some(ty)
    }

    fn clean_declared_annotation(&self, name: &str) -> Option<Type> {
        let annotation = *self.annotations.get(name)?;
        let mut checker =
            Checker::with_type_definitions(self.known_types.clone(), self.type_definitions.clone());
        let lowering = checker.lower_annotation_with_diagnostics(annotation);
        if lowering.diagnostics.is_empty() {
            Some(checker.normalize(&lowering.ty))
        } else {
            None
        }
    }

    fn infer(&mut self, env: &TypeEnv, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Literal(Literal::Number(_)) => named_builtin("Int"),
            ExprKind::Literal(Literal::String(_)) => named_builtin("Text"),
            ExprKind::ComptimeName(name) if name == "True" || name == "False" => {
                named_builtin("Bool")
            }
            ExprKind::ComptimeName(name) if name == "Nil" => named_builtin("Nil"),
            ExprKind::ComptimeName(name) => Type::Variant(vec![TypeRowEntry::Tag {
                name: name.clone(),
                payload: Vec::new(),
            }]),
            ExprKind::Group(inner) => self.infer(env, inner),
            ExprKind::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.infer(env, element))
                    .collect(),
            ),
            ExprKind::Array(elements) => self.infer_array(env, elements),
            ExprKind::Set(entries) => self.infer_set(env, entries),
            ExprKind::Record(entries) => {
                let Some(shape) = literal_record_value(entries, expr.span) else {
                    return Type::Deferred;
                };
                let mut fields = Vec::new();
                for field in &shape.fields {
                    let Some(value) = field.value else {
                        return Type::Deferred;
                    };
                    fields.push(TypeRowEntry::Field {
                        name: field.name.to_owned(),
                        ty: self.infer(env, value),
                        overwrite: false,
                        optional: false,
                    });
                }
                Type::Record(fields)
            }
            ExprKind::Name(name) => {
                if let Some(local) = env.get(name) {
                    return match local {
                        LocalValueType::Known(ty) => ty.clone(),
                        LocalValueType::Unknown => Type::Deferred,
                    };
                }
                let Some(ty) = self.infer_top_level(name) else {
                    return Type::Deferred;
                };
                self.unifier.instantiate(&ty)
            }
            ExprKind::Lambda {
                params,
                return_annotation,
                body,
            } => self.infer_lambda(env, params, return_annotation.as_deref(), body),
            ExprKind::Call { callee, args } => self.infer_call(env, callee, args),
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            } => self.infer_binary(env, left, operator, right),
            ExprKind::Unary {
                operator, value, ..
            } => self.infer_unary(env, operator, value),
            ExprKind::Block(items) => self.infer_block(env, items),
            ExprKind::Match { subject, arms, .. } => self.infer_match(env, subject, arms),
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Index { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::Nullable(_)
            | ExprKind::Arrow { .. }
            | ExprKind::Propagate { .. } => Type::Deferred,
        }
    }

    fn infer_binary(&mut self, env: &TypeEnv, left: &Expr, operator: &str, right: &Expr) -> Type {
        let snapshot = self.unifier.snapshot();
        let left_type = self.infer(env, left);
        let right_type = self.infer(env, right);

        if let Some(result) = self.infer_binary_type(operator, &left_type, &right_type) {
            result
        } else {
            self.unifier.restore(snapshot);
            Type::Deferred
        }
    }

    fn infer_binary_type(&mut self, operator: &str, left: &Type, right: &Type) -> Option<Type> {
        match operator {
            "+" => self
                .infer_numeric_binary_type(left, right)
                .or_else(|| self.infer_same_named_binary_type(left, right, "Text")),
            "-" | "*" | "/" | "%" | "^" => self.infer_numeric_binary_type(left, right),
            "<" | "<=" | ">" | ">=" => self.infer_numeric_comparison_type(left, right),
            "==" | "!=" => self.infer_equality_type(left, right),
            "&&" | "||" => self.infer_same_named_binary_type(left, right, "Bool"),
            _ => None,
        }
    }

    fn infer_numeric_binary_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        let left = self.unifier.resolve(left);
        let right = self.unifier.resolve(right);

        match (numeric_type_name(&left), numeric_type_name(&right)) {
            (Some("Float"), Some(_)) | (Some(_), Some("Float")) => Some(named_builtin("Float")),
            (Some("Int"), Some("Int")) => Some(named_builtin("Int")),
            (None, Some(right_name)) if is_meta_type(&left) => self
                .unifier
                .unify(&left, &named_builtin(right_name))
                .ok()
                .map(|()| named_builtin(right_name)),
            (Some(left_name), None) if is_meta_type(&right) => self
                .unifier
                .unify(&right, &named_builtin(left_name))
                .ok()
                .map(|()| named_builtin(left_name)),
            _ => None,
        }
    }

    fn infer_numeric_comparison_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        self.infer_numeric_binary_type(left, right)
            .map(|_| named_builtin("Bool"))
    }

    fn infer_same_named_binary_type(
        &mut self,
        left: &Type,
        right: &Type,
        name: &'static str,
    ) -> Option<Type> {
        let left = self.unifier.resolve(left);
        let right = self.unifier.resolve(right);

        match (named_type_name(&left), named_type_name(&right)) {
            (Some(left_name), Some(right_name)) if left_name == name && right_name == name => {
                Some(named_builtin(name))
            }
            (None, Some(right_name)) if right_name == name && is_meta_type(&left) => self
                .unifier
                .unify(&left, &named_builtin(name))
                .ok()
                .map(|()| named_builtin(name)),
            (Some(left_name), None) if left_name == name && is_meta_type(&right) => self
                .unifier
                .unify(&right, &named_builtin(name))
                .ok()
                .map(|()| named_builtin(name)),
            _ => None,
        }
    }

    fn infer_equality_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        let left = self.unifier.resolve(left);
        let right = self.unifier.resolve(right);

        if is_meta_type(&left) && is_meta_type(&right) {
            return None;
        }

        if numeric_type_name(&left).is_some() && numeric_type_name(&right).is_some() {
            return Some(named_builtin("Bool"));
        }

        if is_meta_type(&left) && is_concrete_type(&right) {
            return self
                .unifier
                .unify(&left, &right)
                .ok()
                .map(|()| named_builtin("Bool"));
        }

        if is_meta_type(&right) && is_concrete_type(&left) {
            return self
                .unifier
                .unify(&right, &left)
                .ok()
                .map(|()| named_builtin("Bool"));
        }

        if is_concrete_type(&left) && is_concrete_type(&right) {
            return self
                .unifier
                .unify(&left, &right)
                .ok()
                .map(|()| named_builtin("Bool"));
        }

        None
    }

    fn infer_unary(&mut self, env: &TypeEnv, operator: &str, value: &Expr) -> Type {
        let snapshot = self.unifier.snapshot();
        let value_type = self.infer(env, value);

        let result = match operator {
            "-" => self.infer_numeric_unary_type(&value_type),
            _ => None,
        };

        if let Some(result) = result {
            result
        } else {
            self.unifier.restore(snapshot);
            Type::Deferred
        }
    }

    fn infer_numeric_unary_type(&mut self, value: &Type) -> Option<Type> {
        let value = self.unifier.resolve(value);
        numeric_type_name(&value).map(named_builtin)
    }

    fn infer_lambda(
        &mut self,
        env: &TypeEnv,
        params: &[Param],
        return_annotation: Option<&Expr>,
        body: &Expr,
    ) -> Type {
        let mut next_env = env.clone();
        let mut param_types = Vec::new();

        for param in params {
            let ty = if let Some(annotation) = &param.annotation {
                self.lower_annotation_for_inference(annotation)
            } else {
                self.unifier.fresh()
            };
            next_env.insert(param.name.clone(), LocalValueType::Known(ty.clone()));
            param_types.push(ty);
        }

        let body_type = self.infer(&next_env, body);
        let result_type = if let Some(annotation) = return_annotation {
            // A body that contradicts its return annotation defers rather than
            // reporting here: inference only synthesizes types, and diagnosing
            // the mismatch is a later return-annotation-checking slice.
            let expected = self.lower_annotation_for_inference(annotation);
            if self.unifier.unify(&body_type, &expected).is_err() {
                Type::Deferred
            } else {
                expected
            }
        } else {
            body_type
        };

        Type::Function {
            params: param_types,
            result: Box::new(result_type),
        }
    }

    fn infer_call(&mut self, env: &TypeEnv, callee: &Expr, args: &[Expr]) -> Type {
        if let ExprKind::ComptimeName(tag) = &callee.kind {
            return self.infer_variant_constructor(env, tag, args);
        }

        let callee_type = self.infer(env, callee);
        let arg_types: Vec<_> = args.iter().map(|arg| self.infer(env, arg)).collect();
        let result_type = self.unifier.fresh();
        let expected_callee = Type::Function {
            params: arg_types,
            result: Box::new(result_type.clone()),
        };

        if self.unifier.unify(&callee_type, &expected_callee).is_err() {
            Type::Deferred
        } else {
            result_type
        }
    }

    fn infer_variant_constructor(&mut self, env: &TypeEnv, tag: &str, args: &[Expr]) -> Type {
        let mut payload = Vec::new();

        for arg in args {
            let arg_type = self.infer(env, arg);
            let arg_type = self.unifier.resolve(&arg_type);
            if !is_concrete_type(&arg_type) {
                return Type::Deferred;
            }
            payload.push(arg_type);
        }

        Type::Variant(vec![TypeRowEntry::Tag {
            name: tag.to_owned(),
            payload,
        }])
    }

    fn infer_array(&mut self, env: &TypeEnv, elements: &[Expr]) -> Type {
        self.infer_collection(env, elements, "Array")
    }

    fn infer_set(&mut self, env: &TypeEnv, entries: &[RecordEntry]) -> Type {
        let Some(elements) = literal_set_elements(entries) else {
            return Type::Deferred;
        };
        self.infer_collection(env, elements, "Set")
    }

    fn infer_collection<'b>(
        &mut self,
        env: &TypeEnv,
        elements: impl IntoIterator<Item = &'b Expr>,
        name: &str,
    ) -> Type {
        let element_type = self.unifier.fresh();
        for element in elements {
            let item_type = self.infer(env, element);
            if self.unifier.unify(&element_type, &item_type).is_err() {
                return Type::Deferred;
            }
        }

        Type::Apply {
            callee: Box::new(Type::Named(name.to_owned())),
            args: vec![element_type],
        }
    }

    fn infer_block(&mut self, env: &TypeEnv, items: &[Item]) -> Type {
        let mut next_env = env.clone();

        for item in merged_items(items) {
            match item {
                MergedItem::Binding { signature, binding } => {
                    let ty = signature
                        .map(|signature| self.lower_annotation_for_inference(&signature.annotation))
                        .or_else(|| {
                            binding
                                .annotation
                                .as_ref()
                                .map(|annotation| self.lower_annotation_for_inference(annotation))
                        })
                        .unwrap_or_else(|| self.infer(&next_env, &binding.value));
                    next_env.insert(binding.name.clone(), LocalValueType::Known(ty));
                }
                MergedItem::Signature(signature) => {
                    let ty = self.lower_annotation_for_inference(&signature.annotation);
                    next_env.insert(signature.name.clone(), LocalValueType::Known(ty));
                }
                MergedItem::Expr(_) => {}
            }
        }

        match items.last() {
            Some(Item::Expr(expr)) => self.infer(&next_env, expr),
            _ => Type::Deferred,
        }
    }

    fn infer_match(&mut self, env: &TypeEnv, subject: &Expr, arms: &[MatchArm]) -> Type {
        if arms.is_empty() {
            return Type::Deferred;
        }

        let snapshot = self.unifier.snapshot();
        let inferred_subject = self.infer(env, subject);
        let subject_type = self.resolve_if_concrete(&inferred_subject);
        let result_type = self.unifier.fresh();

        for arm in arms {
            let mut arm_env = env.clone();
            for (name, ty) in pattern_local_types(&arm.pattern, subject_type.as_ref()) {
                arm_env.insert(name, ty);
            }

            let body_type = self.infer(&arm_env, &arm.body);
            if self.unifier.unify(&result_type, &body_type).is_err() {
                self.unifier.restore(snapshot);
                return Type::Deferred;
            }
        }

        result_type
    }

    fn lower_annotation_for_inference(&self, annotation: &Expr) -> Type {
        let mut checker =
            Checker::with_type_definitions(self.known_types.clone(), self.type_definitions.clone());
        let ty = checker.lower_annotation(annotation);
        checker.normalize(&ty)
    }
}

/// Rebuild a type, letting `leaf` replace any node (used for substitution and
/// instantiation). Returning `None` keeps the node and recurses structurally.
fn map_type(ty: &Type, leaf: &mut impl FnMut(&Type) -> Option<Type>) -> Type {
    if let Some(replaced) = leaf(ty) {
        return replaced;
    }
    match ty {
        Type::Apply { callee, args } => Type::Apply {
            callee: Box::new(map_type(callee, leaf)),
            args: args.iter().map(|arg| map_type(arg, leaf)).collect(),
        },
        Type::Function { params, result } => Type::Function {
            params: params.iter().map(|param| map_type(param, leaf)).collect(),
            result: Box::new(map_type(result, leaf)),
        },
        Type::Nullable(inner) => Type::Nullable(Box::new(map_type(inner, leaf))),
        Type::Tuple(items) => Type::Tuple(items.iter().map(|item| map_type(item, leaf)).collect()),
        Type::Record(entries) => Type::Record(
            entries
                .iter()
                .map(|entry| map_row_entry(entry, leaf))
                .collect(),
        ),
        Type::Variant(entries) => Type::Variant(
            entries
                .iter()
                .map(|entry| map_row_entry(entry, leaf))
                .collect(),
        ),
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => ty.clone(),
    }
}

fn map_row_entry(
    entry: &TypeRowEntry,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
) -> TypeRowEntry {
    match entry {
        TypeRowEntry::Field {
            name,
            ty,
            overwrite,
            optional,
        } => TypeRowEntry::Field {
            name: name.clone(),
            ty: map_type(ty, leaf),
            overwrite: *overwrite,
            optional: *optional,
        },
        TypeRowEntry::Tag { name, payload } => TypeRowEntry::Tag {
            name: name.clone(),
            payload: payload.iter().map(|ty| map_type(ty, leaf)).collect(),
        },
        TypeRowEntry::Spread { ty, overwrite } => TypeRowEntry::Spread {
            ty: map_type(ty, leaf),
            overwrite: *overwrite,
        },
        TypeRowEntry::Delete(name) => TypeRowEntry::Delete(name.clone()),
        TypeRowEntry::Rename { from, to } => TypeRowEntry::Rename {
            from: from.clone(),
            to: to.clone(),
        },
        TypeRowEntry::Shorthand(name) => TypeRowEntry::Shorthand(name.clone()),
        TypeRowEntry::Open => TypeRowEntry::Open,
        TypeRowEntry::Element(ty) => TypeRowEntry::Element(map_type(ty, leaf)),
    }
}

/// Visit every nested type in pre-order (used by the structural predicates).
fn visit_type(ty: &Type, visit: &mut impl FnMut(&Type)) {
    visit(ty);
    match ty {
        Type::Apply { callee, args } => {
            visit_type(callee, visit);
            args.iter().for_each(|arg| visit_type(arg, visit));
        }
        Type::Function { params, result } => {
            params.iter().for_each(|param| visit_type(param, visit));
            visit_type(result, visit);
        }
        Type::Nullable(inner) => visit_type(inner, visit),
        Type::Tuple(items) => items.iter().for_each(|item| visit_type(item, visit)),
        Type::Record(entries) | Type::Variant(entries) => {
            entries
                .iter()
                .for_each(|entry| visit_row_entry(entry, visit));
        }
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => {}
    }
}

fn visit_row_entry(entry: &TypeRowEntry, visit: &mut impl FnMut(&Type)) {
    match entry {
        TypeRowEntry::Field { ty, .. }
        | TypeRowEntry::Spread { ty, .. }
        | TypeRowEntry::Element(ty) => visit_type(ty, visit),
        TypeRowEntry::Tag { payload, .. } => payload.iter().for_each(|ty| visit_type(ty, visit)),
        TypeRowEntry::Delete(_)
        | TypeRowEntry::Rename { .. }
        | TypeRowEntry::Shorthand(_)
        | TypeRowEntry::Open => {}
    }
}

fn type_contains_meta(ty: &Type, id: u32) -> bool {
    let mut found = false;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Meta(candidate) if *candidate == id) {
            found = true;
        }
    });
    found
}

fn is_concrete_type(ty: &Type) -> bool {
    let mut concrete = true;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Deferred | Type::Variable(_) | Type::Meta(_)) {
            concrete = false;
        }
    });
    concrete
}

fn named_builtin(name: &str) -> Type {
    Type::Named(name.to_owned())
}

fn named_type_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) => Some(name),
        Type::Deferred
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Apply { .. }
        | Type::Function { .. }
        | Type::Nullable(_)
        | Type::Tuple(_)
        | Type::Record(_)
        | Type::Variant(_) => None,
    }
}

fn numeric_type_name(ty: &Type) -> Option<&'static str> {
    match named_type_name(ty) {
        Some("Int") => Some("Int"),
        Some("Float") => Some("Float"),
        _ => None,
    }
}

fn is_meta_type(ty: &Type) -> bool {
    matches!(ty, Type::Meta(_))
}

fn mismatched_literal_kind(expected: &str, literal: &Literal) -> Option<&'static str> {
    match (expected, literal) {
        ("Text", Literal::String(_)) | ("Int" | "Float", Literal::Number(_)) => None,
        ("Int" | "Float" | "Bool" | "Nil" | "Unit", Literal::String(_)) => Some("text literal"),
        ("Text" | "Bool" | "Nil" | "Unit", Literal::Number(_)) => Some("number literal"),
        _ => None,
    }
}

fn named_type_mismatch(expected: &str, actual: &str) -> bool {
    if !CHECKED_NAMED_TYPES.contains(&expected) || !CHECKED_NAMED_TYPES.contains(&actual) {
        return false;
    }

    if matches!((expected, actual), ("Int", "Float") | ("Float", "Int")) {
        return false;
    }

    expected != actual
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
            "value : Float\nvalue = 42\n",
            "value : { name = Text } = \"hi\"\n",
            "value : Missing = \"hi\"\n",
            "value : Missing\nvalue = \"hi\"\n",
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
    fn inferred_identifier_values_are_checked_against_expected_types() {
        for source in [
            "other = 42\nvalue : Text = other\n",
            "other = \"hi\"\nvalue : Int = other\n",
            "other = (1, \"a\")\nvalue : (Text, Text) = other\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn inferred_identifier_values_accept_compatible_types() {
        for source in [
            "other = 42\nvalue : Int = other\n",
            "other = 42\nvalue : Float = other\n",
            "other = (1, \"a\")\nvalue : (Int, Text) = other\n",
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
    fn lambda_application_results_are_inferred_for_identifier_values() {
        let mismatch = parse_module("f = (x) => x\nresult = f(\"hi\")\nvalue : Int = result\n");
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            1
        );

        let accepted = parse_module("f = (x) => x\nresult = f(\"hi\")\nvalue : Text = result\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "lambda application result unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn lambda_application_results_are_instantiated_per_use() {
        let output =
            parse_module("f = (x) => x\na = f(1)\nb = f(\"hi\")\nx : Int = a\ny : Text = b\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "generic top-level lambda reused stale inference state"
        );
    }

    #[test]
    fn lambda_application_tuple_results_recurse_through_inferred_types() {
        let output = parse_module("g = (x) => (x, x)\nr = g(1)\nvalue : (Int, Text) = r\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn annotated_lambdas_are_checked_against_function_annotations() {
        for source in [
            "f : (Int) -> Int = (x: Int) => x\n",
            "f : (Int) -> Int = (x) => x\n",
            "f : (Int) -> Text = (x) => \"hi\"\n",
            "f : (Int) -> Int = (x) : Int => x\n",
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
    fn contextual_lambda_checking_reports_body_param_and_return_mismatches() {
        for source in [
            "f : (Int) -> Text = (x: Int) => x\n",
            "f : (Int) -> Text = (x) => x\n",
            "f : (Int) -> Int = (x: Text) => 1\n",
            "f : (Int) -> Text = (x) : Int => x\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn function_identifier_values_are_checked_against_function_annotations() {
        let output = parse_module("g = (x: Int) => x\nh : (Int) -> Text = g\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn function_parameters_are_compared_contravariantly() {
        let parameter_mismatch = parse_module("f : (Text) -> Int = (x: Int) => x\n");
        let parameter_mismatch_check = check_module(&parameter_mismatch.module);
        assert_eq!(
            matching_codes(&parameter_mismatch_check.diagnostics, codes::ty::MISMATCH),
            1
        );

        let nullable_parameter = parse_module("f : (Int) -> Int = (x: Int?) => 1\n");
        let nullable_parameter_check = check_module(&nullable_parameter.module);
        assert!(
            !has_diagnostic_code(&nullable_parameter_check.diagnostics, codes::ty::MISMATCH),
            "wider nullable parameter unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn function_comparison_reports_arity_mismatches() {
        for source in [
            "f : (Int, Int) -> Int = (x: Int) => x\n",
            "g = (x: Int) => x\nh : (Int, Int) -> Int = g\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one function arity mismatch"
            );
        }
    }

    #[test]
    fn direct_application_under_annotation_is_checked() {
        let mismatch = parse_module("f = (x) => x\nvalue : Int = f(\"hi\")\n");
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            1
        );

        let accepted = parse_module("f = (x) => x\nvalue : Text = f(\"hi\")\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "direct application unexpectedly produced type.mismatch"
        );

        let tuple = parse_module("g = (x) => (x, x)\nvalue : (Int, Text) = g(1)\n");
        let tuple_check = check_module(&tuple.module);
        assert_eq!(
            matching_codes(&tuple_check.diagnostics, codes::ty::MISMATCH),
            1
        );
    }

    #[test]
    fn synthesized_application_checks_do_not_duplicate_existing_paths() {
        for source in ["value : Text = 42\n", "other = 42\nvalue : Text = other\n"] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce exactly one type.mismatch"
            );
        }
    }

    #[test]
    fn direct_application_under_annotation_defers_non_concrete_synthesis() {
        let output = parse_module("h = (x) => missing(x)\nvalue : Text = h(1)\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "unsolved direct application unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn block_bodied_values_are_checked_against_annotations() {
        for source in [
            "value : (Int, Text) =\n  pair = (1, \"a\")\n  pair\n",
            "value : Int =\n  x = 1\n  x\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }

        for source in [
            "value : (Int, Int) =\n  pair = (1, \"a\")\n  pair\n",
            "value : Text =\n  x = 1\n  x\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn contextual_blocks_check_final_expressions() {
        for source in [
            "value : (Int) -> Text =\n  (x) => x\n",
            "value : { name = Text } =\n  { name = 1 }\n",
            "value : Array[Text] =\n  [1]\n",
            "identity = (x) => x\nvalue : Int =\n  identity(\"hi\")\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn contextual_blocks_do_not_duplicate_prefix_diagnostics() {
        let output = parse_module("value : Text =\n  first : Text = 1\n  first\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn contextual_block_prefix_bindings_see_seeded_lambda_params() {
        let output = parse_module("f : (Int) -> Text = (x) =>\n  y : Bool = x\n  y\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 2);
    }

    #[test]
    fn contextual_matches_check_arm_bodies_against_expected_type() {
        let output = parse_module("value : Text =\n  result ?>\n    Ok(_) => 1\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn contextual_matches_check_block_arm_bodies_against_expected_type() {
        let output = parse_module(
            "value : Text =\n  result ?>\n    Ok(_) =>\n      local = 1\n      local\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn contextual_matches_keep_pattern_binders_unknown() {
        let output = parse_module(
            "item : Text = \"hi\"\nvalue : Bool =\n  result ?>\n    Ok(item) => item\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "contextual match arm borrowed a top-level type for a pattern binder"
        );
    }

    #[test]
    fn match_guards_are_checked_as_bool() {
        for source in [
            "value : Text =\n  result ?>\n    Ok(_), 1 < 2 => \"ok\"\n",
            "flag : Bool = True\nvalue : Text =\n  result ?>\n    Ok(_), flag => \"ok\"\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }

        for source in [
            "value : Text =\n  result ?>\n    Ok(_), 1 => \"ok\"\n",
            "flag : Text = \"no\"\nvalue : Text =\n  result ?>\n    Ok(_), flag => \"ok\"\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one guard type mismatch"
            );
        }
    }

    #[test]
    fn match_guard_pattern_binders_stay_unknown() {
        let output = parse_module(
            "item : Text = \"hi\"\nvalue : Text =\n  result ?>\n    Ok(item), item => \"ok\"\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "match guard borrowed a top-level type for a pattern binder"
        );
    }

    #[test]
    fn variant_match_payload_binders_use_subject_types() {
        let body = parse_module(
            "source : @{Ok(Text), Err(Text)} = result\nvalue : Bool = source ?>\n  Ok(item) => item\n  Err(_) => False\n",
        );
        let body_check = check_module(&body.module);
        assert_eq!(
            matching_codes(&body_check.diagnostics, codes::ty::MISMATCH),
            1
        );

        let guard = parse_module(
            "source : @{Ok(Text), Err(Text)} = result\nvalue : Text = source ?>\n  Ok(item), item => \"ok\"\n  Err(_) => \"err\"\n",
        );
        let guard_check = check_module(&guard.module);
        assert_eq!(
            matching_codes(&guard_check.diagnostics, codes::ty::MISMATCH),
            1
        );
    }

    #[test]
    fn variant_match_payload_types_feed_result_inference() {
        let output = parse_module(
            "source : @{Ok(Text), Err(Text)} = result\nmatched = source ?>\n  Ok(item) => item\n  Err(error) => error\nvalue : Int = matched\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn match_results_are_inferred_for_identifier_values() {
        let mismatch = parse_module(
            "result = source ?>\n  Ok(_) => 1\n  Err(_) => 2\nvalue : Text = result\n",
        );
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            1
        );

        let accepted =
            parse_module("result = source ?>\n  Ok(_) => 1\n  Err(_) => 2\nvalue : Int = result\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "compatible inferred match result unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn match_result_inference_handles_block_arm_bodies() {
        let output = parse_module(
            "result = source ?>\n  Ok(_) =>\n    local = 1\n    local\nvalue : Text = result\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn match_result_inference_defers_mixed_arm_types() {
        let output = parse_module(
            "result = source ?>\n  Ok(_) => 1\n  Err(_) => \"no\"\nvalue : Text = result\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "mixed match arm types should defer instead of reporting"
        );
    }

    #[test]
    fn match_result_inference_keeps_pattern_binders_unknown() {
        let output = parse_module(
            "item : Text = \"hi\"\nresult = source ?>\n  Ok(item) => item\nvalue : Bool = result\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "inferred match result borrowed a top-level type for a pattern binder"
        );
    }

    #[test]
    fn unannotated_block_values_feed_identifier_checks() {
        let output = parse_module("data =\n  x = 1\n  (x, x)\nvalue : (Int, Text) = data\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn block_inference_defers_unsolved_values() {
        for source in [
            "value : Text =\n  x = 1\n",
            "value : Text =\n  missing(1)\n",
            "value : Text =\n  x = missing\n  x + 1\n",
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
    fn block_inference_prefers_local_bindings_over_top_level_bindings() {
        let output = parse_module("name = 1\nvalue : Text =\n  name = \"hi\"\n  name\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "block local binding did not shadow top-level value during inference"
        );
    }

    #[test]
    fn array_literals_are_checked_against_annotations() {
        let accepted = parse_module("value : Array[Int] = [1, 2, 3]\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "compatible array literal unexpectedly produced type.mismatch"
        );

        let mismatch = parse_module("value : Array[Text] = [1, 2, 3]\n");
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            3
        );
    }

    #[test]
    fn inferred_array_identifier_values_are_checked_against_annotations() {
        let output = parse_module("nums = [1, 2]\nvalue : Array[Text] = nums\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn array_element_types_reuse_structural_type_comparison() {
        let accepted = parse_module("value : Array[(Int, Text)] = [(1, \"a\")]\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "compatible nested array literal unexpectedly produced type.mismatch"
        );

        let mismatch = parse_module("value : Array[(Int, Int)] = [(1, \"a\")]\n");
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            1
        );
    }

    #[test]
    fn array_literals_report_per_element_mismatches() {
        let output = parse_module("value : Array[Text] = [\"a\", 2, \"b\"]\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn array_inference_defers_empty_literals() {
        let output = parse_module("value : Array[Int] = []\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "empty array unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn set_literals_are_checked_against_annotations() {
        let accepted = parse_module("value : Set[Int] = @{1, 2, 3}\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "compatible set literal unexpectedly produced type.mismatch"
        );

        let mismatch = parse_module("value : Set[Text] = @{1, 2, 3}\n");
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            3
        );
    }

    #[test]
    fn inferred_set_identifier_values_are_checked_against_annotations() {
        let output = parse_module("nums = @{1, 2}\nvalue : Set[Text] = nums\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn set_literals_report_per_element_mismatches() {
        let output = parse_module("value : Set[Text] = @{\"a\", 2, \"b\"}\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn set_inference_defers_empty_tag_and_spread_literals() {
        for source in [
            "value : Set[Int] = @{}\n",
            "value : Set[Int] = @{Red, Green}\n",
            "other = @{2}\nvalue : Set[Int] = @{..other, 1}\n",
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
    fn variant_values_are_checked_against_annotations() {
        for source in [
            "value : @{Ok(Int), Err(Text)} = Ok(1)\n",
            "value : @{Done} = Done\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }

        for source in [
            "value : @{Ok(Text)} = Ok(1)\n",
            "value : @{Ok(Int)} = Err(1)\n",
            "value : @{Ok(Int)} = Ok(1, 2)\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn inferred_variant_identifier_values_are_checked_against_annotations() {
        for source in [
            "result = Ok(1)\nvalue : @{Ok(Int), Err(Text)} = result\n",
            "done = Done\nvalue : @{Done} = done\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }

        for source in [
            "result = Ok(1)\nvalue : @{Ok(Text), Err(Text)} = result\n",
            "result = Err(\"no\")\nvalue : @{Ok(Int)} = result\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn variant_value_checking_defers_computed_rows() {
        let output =
            parse_module("Error = @{Err(Text)}\nvalue : @{..Error, Ok(Int)} = Ok(\"x\")\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "computed variant row unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn lambda_application_inference_defers_unsolved_values() {
        for source in [
            "f = (x) => f(x)\nr = f(1)\nvalue : Text = r\n",
            "f = (x) => x\nx = f\nvalue : Text = x\n",
            "f = (x) => x(x)\nr = f(1)\nvalue : Text = r\n",
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
    fn builtin_operator_results_are_inferred() {
        for source in [
            "value : Int = 1 + 2\n",
            "value : Text = \"a\" + \"b\"\n",
            "value : Bool = 1 < 2\n",
            "left : Bool = True\nright : Bool = False\nvalue : Bool = left && right\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
        }

        for source in [
            "result = 1 + 2\nvalue : Text = result\n",
            "result = \"a\" + \"b\"\nvalue : Int = result\n",
            "result = 1 < 2\nvalue : Text = result\n",
            "left : Bool = True\nright : Bool = False\nresult = left && right\nvalue : Text = result\n",
            "h = (x) => x + 1\nr = h(1)\nvalue : Text = r\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn operator_inference_defers_unknown_operands() {
        for source in [
            "value : Text = missing + 1\n",
            "result = source ?>\n  Ok(item) => item + 1\nvalue : Text = result\n",
            "value : Text = unknown && missing\n",
            // An unsupported sub-expression stays deferred rather than being
            // constrained into a concrete type by a surrounding operator.
            "value : Text = (missing + 1) + 2\n",
            "value : Text = missing[0] + 1\n",
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
    fn infer_value_synthesizes_literal_record_types() {
        let output = parse_module("other = { id = 1, name = \"Ada\" }\n");
        let known_types = known_type_names(&output.module);
        let type_definitions = type_definitions(&output.module, &known_types);
        let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

        assert_eq!(
            checker.infer_top_level_value("other"),
            Some(Type::Record(vec![
                TypeRowEntry::Field {
                    name: "id".to_owned(),
                    ty: named("Int"),
                    overwrite: false,
                    optional: false,
                },
                TypeRowEntry::Field {
                    name: "name".to_owned(),
                    ty: named("Text"),
                    overwrite: false,
                    optional: false,
                },
            ]))
        );
    }

    #[test]
    fn inferred_record_identifier_values_report_field_type_mismatches() {
        for source in [
            "other = { id = 1 }\nvalue : { id = Text } = other\n",
            "other = { user = { name = 1 } }\nvalue : { user = { name = Text } } = other\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn inferred_record_identifier_values_report_missing_and_unexpected_fields() {
        let missing =
            parse_module("other = { id = 1 }\nvalue : { id = Int, name = Text } = other\n");
        let missing_check = check_module(&missing.module);
        assert_eq!(
            matching_codes(&missing_check.diagnostics, codes::ty::MISSING_FIELD),
            1
        );

        let unexpected =
            parse_module("other = { id = 1, name = \"Ada\" }\nvalue : { id = Int } = other\n");
        let unexpected_check = check_module(&unexpected.module);
        assert_eq!(
            matching_codes(&unexpected_check.diagnostics, codes::ty::UNEXPECTED_FIELD),
            1
        );
    }

    #[test]
    fn inferred_record_identifier_values_accept_compatible_records() {
        for source in [
            "other = { id = 1 }\nvalue : { id = Int } = other\n",
            "other = { id = 1, name = \"Ada\" }\nvalue : { .._, id = Int } = other\n",
            "other = { name = \"Ada\", id = 1 }\nvalue : { id = Int, name = Text } = other\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
                "{source} unexpectedly produced type.mismatch"
            );
            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
                "{source} unexpectedly produced type.missing-field"
            );
            assert!(
                !has_diagnostic_code(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
                "{source} unexpectedly produced type.unexpected-field"
            );
        }
    }

    #[test]
    fn record_identifier_value_checking_defers_open_actual_types() {
        let output =
            parse_module("other : { .._, id = Int } = rec\nvalue : { id = Int } = other\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "open actual record unexpectedly produced type.mismatch"
        );
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
            "open actual record unexpectedly produced type.missing-field"
        );
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
            "open actual record unexpectedly produced type.unexpected-field"
        );
    }

    #[test]
    fn annotated_identifier_values_are_checked_against_expected_types() {
        for source in [
            "other : Text = \"hi\"\nvalue : Int = other\n",
            "other : (Int, Text) = (1, \"a\")\nvalue : (Int, Int) = other\n",
            "other : Text? = Nil\nvalue : Text = other\n",
        ] {
            let output = parse_module(source);
            let check = check_module(&output.module);

            assert_eq!(
                matching_codes(&check.diagnostics, codes::ty::MISMATCH),
                1,
                "{source} should produce one type.mismatch"
            );
        }
    }

    #[test]
    fn annotated_identifier_values_accept_compatible_declared_types() {
        for source in [
            "other : Text = \"hi\"\nvalue : Text = other\n",
            "other : Text = \"hi\"\nvalue : Text? = other\n",
            "other : Nil = Nil\nvalue : Text? = other\n",
            "other : (Int, Text) = (1, \"a\")\nvalue : (Int, Text) = other\n",
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
    fn annotated_identifier_value_checking_defers_ambiguous_or_unstable_cases() {
        for source in [
            "other : Int = 1\nvalue : Float = other\n",
            "other : Float = 1\nvalue : Int = other\n",
            "other : Missing = value\nvalue : Text = other\n",
            "other : Text = \"hi\"\nother : Int = 1\nvalue : Int = other\n",
            "User = { name = Text }\nother : User = { name = \"a\" }\nvalue : { name = Text } = other\n",
            "other = name\nvalue : Int = other\n",
            "other = f(1)\nvalue : Int = other\n",
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
    fn shadowed_identifier_values_defer() {
        let output =
            parse_module("other : Text = \"hi\"\nf = (other : Bool) =>\n  x : Bool = other\n  x\n");
        let check = check_module(&output.module);

        assert!(!has_diagnostic_code(
            &check.diagnostics,
            codes::ty::MISMATCH
        ));
    }

    #[test]
    fn annotated_lambda_parameters_are_checked_in_local_bindings() {
        let output = parse_module("f = (x : Int) =>\n  y : Text = x\n  y\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn annotated_sequential_locals_are_checked_in_source_order() {
        let output =
            parse_module("f = () =>\n  first : Int = 1\n  second : Text = first\n  second\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn unannotated_local_literals_feed_later_checks() {
        let mismatch = parse_module("f = () =>\n  first = 1\n  second : Text = first\n  second\n");
        let mismatch_check = check_module(&mismatch.module);
        assert_eq!(
            matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
            1
        );

        let accepted = parse_module("f = () =>\n  first = 1\n  second : Int = first\n  second\n");
        let accepted_check = check_module(&accepted.module);
        assert!(
            !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
            "compatible inferred local unexpectedly produced type.mismatch"
        );
    }

    #[test]
    fn unannotated_local_applications_feed_later_checks() {
        let output = parse_module(
            "identity = (x) => x\nf = () =>\n  local = identity(\"hi\")\n  value : Int = local\n  value\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn annotated_parameters_feed_inferred_local_bindings() {
        let output = parse_module(
            "f = (input : Int) =>\n  local = input\n  value : Text = local\n  value\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn inferred_local_types_are_visible_in_nested_scopes() {
        let output = parse_module(
            "f = () =>\n  outer = 1\n  g = () =>\n    value : Text = outer\n    value\n  g\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn adjacent_local_signatures_supply_known_local_types() {
        let output = parse_module(
            "f = () =>\n  first : Int\n  first = 1\n  second : Text = first\n  second\n",
        );
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }

    #[test]
    fn unknown_lambda_parameters_shadow_top_level_types() {
        let output =
            parse_module("other : Text = \"hi\"\nf = (other) =>\n  x : Bool = other\n  x\n");
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "unannotated parameter borrowed a same-named top-level type"
        );
    }

    #[test]
    fn unknown_block_bindings_shadow_top_level_types() {
        let output = parse_module(
            "other : Text = \"hi\"\nf = () =>\n  other = missing\n  x : Bool = other\n  x\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "unsolved block binding borrowed a same-named top-level type"
        );
    }

    #[test]
    fn match_pattern_bindings_shadow_top_level_types() {
        let output = parse_module(
            "item : Text = \"hi\"\nf = (result) =>\n  result ?>\n    Ok(item) =>\n      value : Bool = item\n      value\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "pattern binding borrowed a same-named top-level type"
        );
    }

    #[test]
    fn inferred_pattern_dependent_locals_stay_unknown() {
        let output = parse_module(
            "item : Text = \"hi\"\nf = (result) =>\n  result ?>\n    Ok(item) =>\n      local = item\n      value : Bool = local\n      value\n",
        );
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "pattern-dependent local borrowed a top-level type during inference"
        );
    }

    #[test]
    fn nearest_annotated_local_type_wins_in_nested_scopes() {
        let output = parse_module(
            "f = (value : Int) =>\n  g = (value : Text) =>\n    result : Int = value\n    result\n  g\n",
        );
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
    fn nested_matched_record_markers_are_reported_once() {
        let output =
            parse_module("value : { r = { name = Text } } = { r = { name = 1, extra? = 2 } }\n");
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
            1
        );
    }

    #[test]
    fn set_element_record_markers_are_reported_once() {
        let output = parse_module("value : Set[{ name = Text }] = @{ { name = 1, extra? = 2 } }\n");
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
            1
        );
    }

    #[test]
    fn extra_field_record_markers_are_reported_once() {
        let output =
            parse_module("value : { name = Text } = { name = 1, blob = { inner? = 3 } }\n");
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
            1
        );
    }

    #[test]
    fn open_extra_field_record_markers_are_reported_once() {
        let output = parse_module(
            "value : { .._, name = Text } = { name = \"x\", blob = { inner? = 3 } }\n",
        );
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
            1
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
    fn aliased_record_types_are_normalized_before_field_checking() {
        let output = parse_module("Rec = { name = Text }\nvalue : Rec = { name = 42 }\n");
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
        assert_eq!(
            check.diagnostics[0].message,
            "expected `Text`, found a number literal"
        );
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
