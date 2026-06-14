use std::collections::{HashMap, HashSet, hash_map::Entry};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, Item, Literal, MatchArm, MergedItem,
    Module, Param, RecordEntry, Signature, collect_declarations, merged_items, pattern_bindings,
    walk_expr_children,
};

use crate::env::{LocalTypeScopes, LocalValueType, TypeEnv, free_metas_in_local_values};
use crate::lower::{
    DeclaredAnnotation, DeclaredAnnotationSource, TypeLowering, binding_for_declaration,
    declared_annotation_for_declaration,
};
use crate::ty::{
    Row, RowEntry, RowKind, RowTail, Type, TypeScheme, free_metas, generalize,
    has_only_meta_unknowns, is_concrete_type, is_meta_type, is_nil_value, mismatched_literal_kind,
    named_builtin, named_type_mismatch, named_type_name, numeric_type_name, type_contains_deferred,
};
use crate::unify::Unifier;

pub(crate) struct Checker<'a> {
    known_types: HashSet<String>,
    type_definitions: HashMap<String, Type>,
    value_types: HashMap<String, Option<TypeScheme>>,
    local_types: LocalTypeScopes,
    bindings: HashMap<String, Option<&'a Binding>>,
    annotations: HashMap<String, &'a Expr>,
    memo: HashMap<String, TypeScheme>,
    in_progress: HashSet<String>,
    unifier: Unifier,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl<'a> Checker<'a> {
    pub(crate) fn new(known_types: HashSet<String>) -> Self {
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

    pub(crate) fn with_module(
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
                types.insert(name.clone(), Some(TypeScheme::mono(annotation)));
            } else if let Some(inferred) = self.infer_top_level(&name)
                && !type_contains_deferred(&inferred.ty)
            {
                types.insert(name.clone(), Some(inferred));
            }
        }

        self.value_types = types;
    }

    pub(crate) fn check_module(&mut self, module: &Module) {
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
            let inferred = self.infer(&env, &binding.value);
            let resolved = self.resolve_and_default(&inferred);
            let env_metas = self.local_types.free_metas();
            let scheme = generalize(resolved, &env_metas);
            self.check_value_expr(&binding.value);
            if type_contains_deferred(&scheme.ty) {
                LocalValueType::Unknown
            } else {
                LocalValueType::Scheme(scheme)
            }
        } else {
            declared_type
                .cloned()
                .map(LocalValueType::Known)
                .unwrap_or(LocalValueType::Unknown)
        };

        self.local_types.define(&binding.name, inferred_type);
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
                            .with_note("remove `?` in value records; use `field: Nil` when the value is absent"),
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
                Some(LocalValueType::Scheme(scheme)) => {
                    let actual = self.unifier.instantiate_scheme(&scheme);
                    self.check_type_against_type(expected, &actual, value.span);
                }
                Some(LocalValueType::Unknown) => {}
                None => {
                    if let Some(Some(scheme)) = self.value_types.get(name).cloned() {
                        let actual = self.unifier.instantiate_scheme(&scheme);
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
            (Type::Named(expected), Type::Named(actual))
                if named_type_mismatch(expected, actual) =>
            {
                self.report_type_mismatch_between_types(expected, actual, span);
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
            (Type::Tuple(expected), Type::Named(actual))
                if actual == "Unit" && !expected.is_empty() =>
            {
                self.report_tuple_arity_mismatch(expected.len(), 0, span);
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

    fn check_variant_type_against_type(&mut self, expected: &Row, actual: &Row, span: Span) {
        let Some(actual_tags) = literal_variant_tags(actual) else {
            return;
        };

        for tag in actual_tags {
            let Some(payload) = literal_variant_payload_lookup(expected, tag.name) else {
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
        row: &Row,
        value_entries: &[RecordEntry],
        value_span: Span,
    ) {
        self.report_value_record_markers(value_entries);

        let (Some(expected), Some(actual)) = (
            literal_record_type(row),
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
        row: &Row,
        tag: &str,
        args: &[Expr],
        value_span: Span,
    ) {
        let Some(payload) = literal_variant_payload_lookup(row, tag) else {
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

    pub(crate) fn lower_declared_annotation(
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

    pub(crate) fn lower_annotation_with_diagnostics(&mut self, annotation: &Expr) -> TypeLowering {
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
            Type::Record(row) => Type::Record(self.normalize_row(row, &visited)),
            Type::Variant(row) => Type::Variant(self.normalize_row(row, &visited)),
        }
    }

    fn normalize_types(&self, types: &[Type], visited: &HashSet<String>) -> Vec<Type> {
        types
            .iter()
            .map(|ty| self.normalize_with_visited(ty, visited.clone()))
            .collect()
    }

    fn normalize_row(&self, row: &Row, visited: &HashSet<String>) -> Row {
        Row {
            entries: row
                .entries
                .iter()
                .map(|entry| self.normalize_row_entry(entry, visited))
                .collect(),
            tail: row.tail,
        }
    }

    fn normalize_row_entry(&self, entry: &RowEntry, visited: &HashSet<String>) -> RowEntry {
        match entry {
            RowEntry::Field { name, ty, optional } => RowEntry::Field {
                name: name.clone(),
                ty: self.normalize_with_visited(ty, visited.clone()),
                optional: *optional,
            },
            RowEntry::Tag { name, payload } => RowEntry::Tag {
                name: name.clone(),
                payload: self.normalize_types(payload, visited),
            },
        }
    }

    pub(crate) fn lower_annotation(&mut self, annotation: &Expr) -> Type {
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
            ExprKind::Record(entries) => self.lower_row_entries(entries, RowKind::Record),
            ExprKind::Set(entries) => self.lower_row_entries(entries, RowKind::Variant),
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

    fn lower_row_entries(&mut self, entries: &[RecordEntry], kind: RowKind) -> Type {
        let mut lowered = Vec::new();
        let mut tail = RowTail::Closed;
        let mut normalizable = true;

        for entry in entries {
            match self.lower_row_entry(entry, kind) {
                Ok(Some(entry)) => lowered.push(entry),
                Ok(None) => tail = RowTail::Open,
                Err(()) => normalizable = false,
            }
        }

        if !normalizable {
            return Type::Deferred;
        }

        let row = Row {
            entries: lowered,
            tail,
        };
        match kind {
            RowKind::Record => Type::Record(row),
            RowKind::Variant => Type::Variant(row),
        }
    }

    fn lower_row_entry(
        &mut self,
        entry: &RecordEntry,
        kind: RowKind,
    ) -> Result<Option<RowEntry>, ()> {
        match entry {
            RecordEntry::Field {
                name,
                value,
                overwrite,
                optional,
                ..
            } => {
                let ty = self.lower_annotation(value);
                if kind == RowKind::Record && !overwrite {
                    Ok(Some(RowEntry::Field {
                        name: name.clone(),
                        ty,
                        optional: *optional,
                    }))
                } else {
                    Err(())
                }
            }
            RecordEntry::Shorthand { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. } => Err(()),
            RecordEntry::Spread { value, .. } => {
                self.lower_annotation(value);
                Err(())
            }
            RecordEntry::Open { .. } => Ok(None),
            RecordEntry::Element(value) => match kind {
                RowKind::Record => {
                    self.lower_annotation(value);
                    Err(())
                }
                RowKind::Variant => self.lower_variant_tag(value).map(Some).ok_or(()),
            },
        }
    }

    fn lower_variant_tag(&mut self, tag: &Expr) -> Option<RowEntry> {
        match &tag.kind {
            ExprKind::ComptimeName(name) => Some(RowEntry::Tag {
                name: name.clone(),
                payload: Vec::new(),
            }),
            ExprKind::Name(name) => {
                self.report_lowercase_variant_tag(name, tag.span);
                Some(RowEntry::Tag {
                    name: name.clone(),
                    payload: Vec::new(),
                })
            }
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::ComptimeName(name) => Some(RowEntry::Tag {
                    name: name.clone(),
                    payload: self.lower_annotations(args),
                }),
                ExprKind::Name(name) => {
                    self.report_lowercase_variant_tag(name, callee.span);
                    Some(RowEntry::Tag {
                        name: name.clone(),
                        payload: self.lower_annotations(args),
                    })
                }
                _ => {
                    self.lower_deferred_annotation(tag);
                    None
                }
            },
            _ => {
                self.lower_deferred_annotation(tag);
                None
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
                    "add `{name}: ...`, or make the field optional with `{name}?` in the type"
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

fn literal_record_type(row: &Row) -> Option<ExpectedRecordShape<'_>> {
    let mut fields = Vec::new();

    for entry in &row.entries {
        match entry {
            RowEntry::Field { name, ty, optional } => fields.push(ExpectedRecordField {
                name,
                ty,
                optional: *optional,
            }),
            RowEntry::Tag { .. } => return None,
        }
    }

    Some(ExpectedRecordShape {
        fields,
        open: row.tail == RowTail::Open,
    })
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

fn literal_variant_payload<'a>(row: &'a Row, tag: &str) -> Option<&'a [Type]> {
    literal_variant_payload_lookup(row, tag).flatten()
}

fn literal_variant_payload_lookup<'a>(row: &'a Row, tag: &str) -> Option<Option<&'a [Type]>> {
    if row.tail == RowTail::Open {
        return None;
    }

    let mut found = None;

    for entry in &row.entries {
        match entry {
            RowEntry::Tag { name, payload } if name == tag => {
                found = Some(payload.as_slice());
            }
            RowEntry::Tag { .. } => {}
            RowEntry::Field { .. } => return None,
        }
    }

    Some(found)
}

fn literal_variant_tags(row: &Row) -> Option<Vec<VariantTagShape<'_>>> {
    if row.tail == RowTail::Open {
        return None;
    }

    let mut tags = Vec::new();

    for entry in &row.entries {
        match entry {
            RowEntry::Tag { name, payload } => tags.push(VariantTagShape {
                name,
                payload: payload.as_slice(),
            }),
            RowEntry::Field { .. } => return None,
        }
    }

    Some(tags)
}

impl<'a> Checker<'a> {
    /// Instantiate and fully resolve a top-level binding's inferred type, used by
    /// white-box synthesis tests. Production code consumes the generalized scheme
    /// from `infer_top_level` directly.
    #[cfg(test)]
    pub(crate) fn infer_top_level_value(&mut self, name: &str) -> Option<Type> {
        let scheme = self.infer_top_level(name)?;
        let ty = self.unifier.instantiate_scheme(&scheme);
        self.resolve_if_concrete(&ty)
    }

    fn infer_local_value(&mut self, env: &TypeEnv, value: &Expr) -> Option<Type> {
        let ty = self.infer(env, value);
        self.resolve_if_concrete(&ty)
    }

    /// Fully resolve `ty`; keep it only when no metavariable remains, so a
    /// synthesized value type never leaks an unsolved meta into checking.
    fn resolve_if_concrete(&self, ty: &Type) -> Option<Type> {
        let ty = self.resolve_and_default(ty);
        is_concrete_type(&ty).then_some(ty)
    }

    fn resolve_and_default(&self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        self.unifier.default_numerics(&resolved)
    }

    fn infer_top_level(&mut self, name: &str) -> Option<TypeScheme> {
        if let Some(scheme) = self.memo.get(name).cloned() {
            return Some(scheme);
        }
        if self.in_progress.contains(name) {
            return Some(TypeScheme::mono(Type::Deferred));
        }

        let binding = (*self.bindings.get(name)?)?;
        self.in_progress.insert(name.to_owned());

        let scheme = if let Some(annotation) = self.clean_declared_annotation(name) {
            TypeScheme::mono(annotation)
        } else {
            let ty = self.infer(&TypeEnv::new(), &binding.value);
            generalize(self.resolve_and_default(&ty), &[])
        };

        self.in_progress.remove(name);
        self.memo.insert(name.to_owned(), scheme.clone());
        Some(scheme)
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
            ExprKind::Literal(Literal::Number(_)) => self.unifier.fresh_numeric(),
            ExprKind::Literal(Literal::String(_)) => named_builtin("Text"),
            ExprKind::ComptimeName(name) if name == "True" || name == "False" => {
                named_builtin("Bool")
            }
            ExprKind::ComptimeName(name) if name == "Nil" => named_builtin("Nil"),
            ExprKind::ComptimeName(name) => Type::Variant(Row {
                entries: vec![RowEntry::Tag {
                    name: name.clone(),
                    payload: Vec::new(),
                }],
                tail: RowTail::Closed,
            }),
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
                    fields.push(RowEntry::Field {
                        name: field.name.to_owned(),
                        ty: self.infer(env, value),
                        optional: false,
                    });
                }
                Type::Record(Row {
                    entries: fields,
                    tail: RowTail::Closed,
                })
            }
            ExprKind::Name(name) => {
                if let Some(local) = env.get(name).cloned() {
                    return match local {
                        LocalValueType::Known(ty) => ty,
                        LocalValueType::Scheme(scheme) => self.unifier.instantiate_scheme(&scheme),
                        LocalValueType::Unknown => Type::Deferred,
                    };
                }
                let Some(scheme) = self.infer_top_level(name) else {
                    return Type::Deferred;
                };
                self.unifier.instantiate_scheme(&scheme)
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

        if is_meta_type(&left)
            && is_meta_type(&right)
            && (self.unifier.is_numeric_meta(&left) || self.unifier.is_numeric_meta(&right))
        {
            self.unifier.unify(&left, &right).ok()?;
            return Some(self.unifier.resolve(&left));
        }

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
            if self.unifier.is_numeric_meta(&left) || self.unifier.is_numeric_meta(&right) {
                return self
                    .unifier
                    .unify(&left, &right)
                    .ok()
                    .map(|()| named_builtin("Bool"));
            }
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
        if let Some(name) = numeric_type_name(&value) {
            return Some(named_builtin(name));
        }
        self.unifier.is_numeric_meta(&value).then_some(value)
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
            let numeric_metas_only = has_only_meta_unknowns(&arg_type)
                && free_metas(&arg_type)
                    .into_iter()
                    .all(|id| self.unifier.is_numeric_meta(&Type::Meta(id)));
            if !is_concrete_type(&arg_type) && !numeric_metas_only {
                return Type::Deferred;
            }
            payload.push(arg_type);
        }

        Type::Variant(Row {
            entries: vec![RowEntry::Tag {
                name: tag.to_owned(),
                payload,
            }],
            tail: RowTail::Closed,
        })
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
                    let local_type = signature
                        .map(|signature| self.lower_annotation_for_inference(&signature.annotation))
                        .or_else(|| {
                            binding
                                .annotation
                                .as_ref()
                                .map(|annotation| self.lower_annotation_for_inference(annotation))
                        })
                        .map(LocalValueType::Known)
                        .unwrap_or_else(|| {
                            let inferred = self.infer(&next_env, &binding.value);
                            let resolved = self.resolve_and_default(&inferred);
                            let env_metas = free_metas_in_local_values(next_env.values());
                            let scheme = generalize(resolved, &env_metas);
                            if type_contains_deferred(&scheme.ty) {
                                LocalValueType::Unknown
                            } else {
                                LocalValueType::Scheme(scheme)
                            }
                        });
                    next_env.insert(binding.name.clone(), local_type);
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
