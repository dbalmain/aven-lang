use super::*;

impl<'a> Checker<'a> {
    pub(super) fn lower_normalized_annotation(&mut self, annotation: &Expr) -> Type {
        let ty = self.lower_annotation(annotation);
        self.normalize(&ty)
    }

    pub(super) fn check_value_against(&mut self, expected: &Type, value: &Expr) {
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
                    ..
                },
            ) => self.check_lambda_against_function(
                value.span,
                params,
                return_annotation.as_deref(),
                body,
                expected_params,
                expected_result,
            ),
            (ExprKind::Name(name) | ExprKind::ComptimeName(name), _) => {
                let env = self.local_types.inference_env();
                let actual = self.infer_name_reference(&env, name, value.span);
                if !type_contains_deferred(&actual) {
                    self.check_type_against_type(expected, &actual, value.span);
                }
            }
            // `undefined` / `null` inhabit Optional / Nullable without peeling.
            (_, Type::Optional(_)) if is_undefined_value(value) => {}
            (_, Type::Nullable(_)) if is_null_value(value) => {}
            // Structural forms peel so bare payloads check against the inner
            // type (N2 `T` → `?T` / `T?` widening). Inference forms — matches,
            // calls, indexes, ops — keep the full wrapper: match arms must see
            // `?T` (not stripped `T`), and already-`?T` actuals unify at the
            // type level via `check_type_against_type`.
            (_, Type::Optional(inner)) if peels_optional_or_nullable_expected(value) => {
                self.check_value_against(inner, value);
            }
            (_, Type::Nullable(inner)) if peels_optional_or_nullable_expected(value) => {
                self.check_value_against(inner, value);
            }
            (ExprKind::Literal(literal), Type::Named(name)) => {
                if let Some(found) = mismatched_literal_kind(name, literal) {
                    self.report_type_mismatch(name, found, value.span);
                }
            }
            (ExprKind::Literal(literal), Type::Variant(row)) => {
                self.check_literal_value_against_variant(row, literal, value.span);
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
            (ExprKind::Tag(tag), Type::Variant(type_entries)) => {
                self.check_variant_value_against(type_entries, tag, &[], value.span);
            }
            (ExprKind::Call { callee, args }, expected) if is_result_type(expected) => {
                if let Some(tag) = result_constructor_tag(callee) {
                    self.check_result_constructor_value_against(expected, tag, args, value.span);
                } else {
                    self.check_value_expr(value);
                    let env = self.local_types.inference_env();
                    let diagnostics_start = self.diagnostics.len();
                    let actual = self.infer_local_value(&env, value);
                    self.deduplicate_diagnostics_since(diagnostics_start);
                    if let Some(actual) = actual {
                        self.check_type_against_type(expected, &actual, value.span);
                    }
                }
            }
            (ExprKind::Call { callee, args }, Type::Variant(type_entries))
                if matches!(&callee.kind, ExprKind::Tag(_)) =>
            {
                let ExprKind::Tag(tag) = &callee.kind else {
                    return;
                };
                self.check_variant_value_against(type_entries, tag, args, value.span);
            }
            (ExprKind::Call { callee, args }, _) => {
                let env = self.local_types.inference_env();
                if let Some(actual) = self.infer_record_selection_builtin_call(&env, callee, args) {
                    self.record_expr_type(value.span, &actual);
                    if !type_contains_deferred(&actual) {
                        self.check_type_against_type(expected, &actual, value.span);
                    }
                } else {
                    self.check_value_expr(value);
                    let env = self.local_types.inference_env();
                    let diagnostics_start = self.diagnostics.len();
                    let actual = self.infer_local_value(&env, value);
                    self.deduplicate_diagnostics_since(diagnostics_start);
                    if let Some(actual) = actual {
                        self.check_type_against_type(expected, &actual, value.span);
                    }
                }
            }
            (ExprKind::Match { subject, arms, .. }, _) => {
                self.check_match_arms(subject, arms, Some(expected));
            }
            (
                ExprKind::Array(entries),
                Type::Apply {
                    callee,
                    args: element_types,
                },
            ) if matches!(callee.as_ref(), Type::Named(name) if name == "Array")
                && element_types.len() == 1 =>
            {
                self.report_value_record_markers(entries);
                self.check_collection_entries_against(expected, &element_types[0], entries);
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
                self.check_collection_entries_against(expected, &element_types[0], entries);
            }
            _ => {
                self.check_value_expr(value);
                let env = self.local_types.inference_env();
                let diagnostics_start = self.diagnostics.len();
                let actual = self.infer_local_value(&env, value);
                self.deduplicate_diagnostics_since(diagnostics_start);
                if let Some(actual) = actual {
                    self.check_type_against_type(expected, &actual, value.span);
                }
            }
        }
    }

    pub(super) fn check_block_against(&mut self, expected: &Type, items: &[Item]) {
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
                MergedItem::PatternBinding(binding) => {
                    self.check_local_pattern_binding(binding);
                }
                MergedItem::SpreadBinding(binding) => {
                    self.check_local_spread_binding(binding);
                }
                MergedItem::Signature(signature) => {
                    let ty = self.lower_normalized_annotation(&signature.annotation);
                    self.local_types
                        .define(&signature.name, LocalValueType::Known(ty));
                }
                MergedItem::Expr(expr) => {
                    let env = self.local_types.inference_env();
                    self.report_unused_result_if_dropped(&env, expr);
                    self.check_value_expr(expr);
                }
            }
        }

        if let Some(expr) = final_expr {
            self.check_value_against(expected, expr);
        }

        self.local_types.pop();
    }

    pub(super) fn check_lambda_against_function(
        &mut self,
        lambda_span: Span,
        params: &[Param],
        return_annotation: Option<&Expr>,
        body: &Expr,
        expected_params: &[Type],
        expected_result: &Type,
    ) {
        if params.len() != expected_params.len() {
            // The expected type is a function-type annotation, which has no
            // defaults: required == total.
            self.report_function_arity_mismatch(
                expected_params.len(),
                expected_params.len(),
                params.len(),
                lambda_span,
            );
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
        self.push_local_comptime_param_scope(params);
        for (param, ty) in params.iter().zip(param_types) {
            self.record_inferred_type(param.name_span, ty.clone());
            self.local_types
                .define(&param.name, LocalValueType::Known(ty));
        }
        self.propagation_contexts
            .push(PropagationContext::default());
        self.check_value_against(&body_expected, body);
        let body_type = self.infer_body_type_for_propagation_check(body);
        let propagation = self.pop_propagation_context();
        let _ = self.apply_propagation_context_to_body_type(body, body_type, &propagation);
        self.report_propagated_errors_against_annotation(&body_expected, &propagation);
        self.local_comptime_params.pop();
        self.local_types.pop();
    }

    /// Check array/set-literal entries against an expected collection type:
    /// elements against the element type, spread subjects against the whole
    /// collection type. Other entry kinds fall back to walking values.
    fn check_collection_entries_against(
        &mut self,
        expected: &Type,
        element_type: &Type,
        entries: &[RecordEntry],
    ) {
        for entry in entries {
            match entry {
                RecordEntry::Element(element) => self.check_value_against(element_type, element),
                RecordEntry::Spread { value, .. } => self.check_value_against(expected, value),
                entry => self.walk_value_record_values(std::slice::from_ref(entry)),
            }
        }
    }

    pub(super) fn check_call_arg_against_param(&mut self, expected: &Type, arg: &Expr) -> bool {
        let diagnostics_start = self.diagnostics.len();
        self.check_value_against(expected, arg);
        let reported_diagnostic = self.diagnostics.len() > diagnostics_start;
        self.deduplicate_diagnostics_since(diagnostics_start);
        !reported_diagnostic
    }

    pub(super) fn check_type_against_type(&mut self, expected: &Type, actual: &Type, span: Span) {
        if expected == actual {
            return;
        }

        match (expected, actual) {
            (Type::Optional(expected_inner), Type::Optional(actual_inner))
            | (Type::Nullable(expected_inner), Type::Nullable(actual_inner)) => {
                self.check_type_against_type(expected_inner, actual_inner, span);
            }
            (Type::Optional(_), Type::Named(name)) if name == "Undefined" => {}
            (Type::Nullable(_), Type::Named(name)) if name == "Null" => {}
            (Type::Optional(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Nullable(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Named(expected), Type::Named(actual))
                if named_type_mismatch(expected, actual) =>
            {
                self.report_type_mismatch_between_types(expected, actual, span);
            }
            (Type::Named(expected), actual @ (Type::Optional(_) | Type::Nullable(_))) => {
                let inner = match actual {
                    Type::Optional(inner) | Type::Nullable(inner) => inner,
                    _ => unreachable!("actual is constrained by the outer pattern"),
                };
                if let Type::Named(actual_name) = inner.as_ref()
                    && (named_type_mismatch(expected, actual_name) || expected == actual_name)
                {
                    self.report_type_mismatch_between_types(expected, &actual.render(), span);
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
                    ..
                },
                Type::Function {
                    params: actual_params,
                    result: actual_result,
                    ..
                },
            ) => {
                if expected_params.len() != actual_params.len() {
                    // Function-type annotations have no defaults: required ==
                    // total.
                    self.report_function_arity_mismatch(
                        expected_params.len(),
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
            ) => {
                if expected_args.len() != actual_args.len()
                    || applied_type_constructor_mismatch(expected_callee, actual_callee)
                {
                    self.report_type_mismatch_between_types(
                        &expected.render(),
                        &actual.render(),
                        span,
                    );
                    return;
                }
                self.check_type_against_type(expected_callee, actual_callee, span);
                for (expected, actual) in expected_args.iter().zip(actual_args) {
                    self.check_type_against_type(expected, actual, span);
                }
            }
            (expected, Type::Variant(actual)) if is_result_type(expected) => {
                self.check_result_variant_type_against_result(expected, actual, span);
            }
            (Type::Apply { .. }, actual) if reportable_type_shape(actual) => {
                self.report_type_mismatch_between_types(&expected.render(), &actual.render(), span);
            }
            (expected, Type::Apply { .. }) if reportable_type_shape(expected) => {
                self.report_type_mismatch_between_types(&expected.render(), &actual.render(), span);
            }
            (Type::Variant(expected), Type::Named(actual)) => {
                self.check_named_type_against_variant(expected, actual, span);
            }
            (Type::Named(expected), Type::Variant(actual)) => {
                self.check_variant_type_against_named(expected, actual, span);
            }
            (Type::Record(expected), Type::Record(actual)) => {
                let (Some(expected), Some(actual)) =
                    (literal_record_type(expected), literal_record_type(actual))
                else {
                    return;
                };
                if actual.open
                    || actual
                        .fields
                        .iter()
                        .any(|field| self.type_admits_undefined(field.ty))
                {
                    return;
                }

                let actual_fields: Vec<_> = actual
                    .fields
                    .iter()
                    .map(|field| (field.name, span, FieldValue::Type(field.ty)))
                    .collect();
                self.compare_record(&expected, &actual_fields, ExtraFields::Allow, span);
            }
            (Type::Variant(expected), Type::Variant(actual)) => {
                self.check_variant_type_against_type(expected, actual, span);
            }
            _ => {}
        }
    }

    pub(super) fn check_variant_type_against_type(
        &mut self,
        expected: &Row,
        actual: &Row,
        span: Span,
    ) {
        let expected = self.resolve_variant_row(expected);
        let actual = self.resolve_variant_row(actual);

        // A literal value row carries an open tail purely so distinct literals can
        // join during inference; at a boundary its known members are exhaustive,
        // so it subsumes into a literal union by membership (R3) — not by the
        // strict open-variant rule below, which exists for tag variants whose tail
        // could carry tags the annotation does not allow.
        if row_has_literal_entries(&actual) {
            let Some(actual_literals) = literal_variant_members(&actual) else {
                return;
            };
            let Some(expected_literals) = literal_variant_members(&expected) else {
                self.report_variant_entry_kind_mismatch(
                    &Type::Variant(expected.clone()),
                    &Type::Variant(actual.clone()),
                    span,
                );
                return;
            };

            for literal in actual_literals {
                if expected.tail == RowTail::Closed && !expected_literals.contains(&literal) {
                    self.report_literal_not_in_union(literal, &expected_literals, span);
                }
            }
            return;
        }

        if expected.tail == RowTail::Closed && actual.tail != RowTail::Closed {
            self.report_open_variant_not_assignable(span);
            return;
        }

        if actual.tail != RowTail::Open
            && self
                .unifier
                .unify(
                    &Type::Variant(expected.clone()),
                    &Type::Variant(actual.clone()),
                )
                .is_ok()
        {
            return;
        }

        let Some(actual_tags) = variant_tags(&actual) else {
            return;
        };

        for tag in actual_tags {
            let Some(payload) = variant_payload_lookup(&expected, tag.name) else {
                if row_has_literal_entries(&expected) {
                    self.report_variant_entry_kind_mismatch(
                        &Type::Variant(expected.clone()),
                        &Type::Variant(actual.clone()),
                        span,
                    );
                }
                return;
            };

            let Some(expected_payload) = payload else {
                if expected.tail == RowTail::Closed {
                    self.report_variant_tag_mismatch(tag.name, span);
                }
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

    pub(super) fn resolve_variant_row(&self, row: &Row) -> Row {
        let Type::Variant(row) = self.unifier.resolve(&Type::Variant(row.clone())) else {
            unreachable!("variant resolution preserves the outer type")
        };
        row
    }

    pub(super) fn check_named_type_against_variant(
        &mut self,
        expected: &Row,
        actual: &str,
        span: Span,
    ) {
        let expected = self.resolve_variant_row(expected);
        let Some(literals) = literal_variant_members(&expected) else {
            return;
        };

        let rendered_expected = Type::Variant(expected.clone()).render();
        if literal_union_accepts_base_type(&literals, actual) {
            self.report_wide_value_into_literal_union(&rendered_expected, actual, span);
        } else {
            self.report_type_mismatch_between_types(&rendered_expected, actual, span);
        }
    }

    pub(super) fn check_variant_type_against_named(
        &mut self,
        expected: &str,
        actual: &Row,
        span: Span,
    ) {
        if let Some(Type::Variant(expected_row)) = self.type_definitions.get(expected).cloned() {
            self.check_variant_type_against_type(&expected_row, actual, span);
            return;
        }

        let actual = self.resolve_variant_row(actual);
        let Some(base) = literal_variant_base(&actual) else {
            return;
        };

        if !base.matches_named(expected) {
            self.report_type_mismatch_between_types(
                expected,
                &display_inferred_type(&Type::Variant(actual)).render(),
                span,
            );
        }
    }

    pub(super) fn check_literal_value_against_variant(
        &mut self,
        row: &Row,
        literal: &Literal,
        span: Span,
    ) {
        let row = self.resolve_variant_row(row);
        let Some(literals) = literal_variant_members(&row) else {
            self.report_type_mismatch(
                &Type::Variant(row).render(),
                literal_kind_name(literal),
                span,
            );
            return;
        };

        let base_mismatch =
            literal_variant_base(&row).is_some_and(|expected_base| match literal_base(literal) {
                Some(actual_base) => actual_base != expected_base,
                None => true,
            });
        if base_mismatch {
            self.report_literal_not_in_union(literal, &literals, span);
            return;
        }

        if row.tail != RowTail::Closed || literals.contains(&literal) {
            return;
        }

        self.report_literal_not_in_union(literal, &literals, span);
    }

    pub(super) fn check_record_value_against(
        &mut self,
        row: &Row,
        value_entries: &[RecordEntry],
        value_span: Span,
    ) {
        self.report_value_record_markers(value_entries);
        self.report_redundant_undefined_record_fields(value_entries);

        let Some(expected) = literal_record_type(row) else {
            self.walk_value_record_values(value_entries);
            return;
        };

        if let Some(actual) = literal_record_value(value_entries, value_span) {
            self.check_literal_record_shorthands(&actual);
            let actual_fields: Vec<_> = actual
                .fields
                .iter()
                .map(|field| (field.name, field.name_span, FieldValue::Value(field.value)))
                .collect();
            self.compare_record(&expected, &actual_fields, ExtraFields::Reject, actual.span);
            return;
        }

        let env = self.local_types.inference_env();
        let actual = self.infer_record_entries(&env, value_entries);
        if !type_contains_deferred(&actual) {
            self.check_type_against_type(&Type::Record(row.clone()), &actual, value_span);
        }
        self.walk_value_record_values(value_entries);
    }

    pub(super) fn check_literal_record_shorthands(&mut self, record: &ValueRecordShape<'_>) {
        for field in &record.fields {
            if field.value.is_none() {
                self.check_name_reference(field.name, field.name_span);
            }
        }
    }

    pub(super) fn check_result_constructor_value_against(
        &mut self,
        expected: &Type,
        tag: &str,
        args: &[Expr],
        value_span: Span,
    ) {
        let Some((ok_ty, error_ty)) = result_type_args(expected) else {
            return;
        };
        let payload_ty = match tag {
            "Ok" => ok_ty.clone(),
            "Err" => error_ty.clone(),
            _ => {
                self.report_type_mismatch_between_types(
                    &expected.render(),
                    &result_constructor_type(tag, args).render(),
                    value_span,
                );
                self.check_value_exprs(args);
                return;
            }
        };

        if args.len() != 1 {
            self.report_type_mismatch_between_types(
                &expected.render(),
                &result_constructor_type(tag, args).render(),
                value_span,
            );
            self.check_value_exprs(args);
            return;
        }

        self.check_value_against(&payload_ty, &args[0]);
    }

    pub(super) fn check_result_variant_type_against_result(
        &mut self,
        expected: &Type,
        actual: &Row,
        span: Span,
    ) {
        let Some((ok_ty, error_ty)) = result_type_args(expected) else {
            return;
        };
        let ok_ty = ok_ty.clone();
        let error_ty = error_ty.clone();

        for entry in &actual.entries {
            let RowEntry::Tag { name, payload } = entry else {
                self.report_type_mismatch_between_types(
                    &expected.render(),
                    &Type::Variant(actual.clone()).render(),
                    span,
                );
                return;
            };
            let expected_payload_ty = match name.as_str() {
                "Ok" => &ok_ty,
                "Err" => &error_ty,
                _ => {
                    self.report_type_mismatch_between_types(
                        &expected.render(),
                        &Type::Variant(actual.clone()).render(),
                        span,
                    );
                    return;
                }
            };
            if payload.len() != 1 {
                self.report_type_mismatch_between_types(
                    &expected.render(),
                    &Type::Variant(actual.clone()).render(),
                    span,
                );
                continue;
            }
            self.check_type_against_type(expected_payload_ty, &payload[0], span);
        }
    }

    pub(super) fn check_variant_value_against(
        &mut self,
        row: &Row,
        tag: &str,
        args: &[Expr],
        value_span: Span,
    ) {
        let Some(payload) = variant_payload_lookup(row, tag) else {
            if row_has_literal_entries(row) {
                self.report_variant_entry_kind_mismatch(
                    &Type::Variant(row.clone()),
                    &Type::Variant(Row {
                        entries: vec![RowEntry::Tag {
                            name: tag.to_owned(),
                            payload: Vec::new(),
                        }],
                        tail: RowTail::Closed,
                    }),
                    value_span,
                );
            }
            self.check_value_exprs(args);
            return;
        };

        let Some(expected_payload) = payload else {
            if row.tail == RowTail::Closed {
                self.report_variant_tag_mismatch(tag, value_span);
            }
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

    pub(super) fn compare_record(
        &mut self,
        expected: &ExpectedRecordShape<'_>,
        actual: &[(&str, Span, FieldValue<'_>)],
        extra_fields: ExtraFields,
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
                None if self.type_admits_undefined(field.ty) => {}
                None => self.report_missing_field(field.name, record_span),
            }
        }

        for (name, blame_span, payload) in actual {
            if !expected_field_names.contains(name) {
                if !expected.open && matches!(extra_fields, ExtraFields::Reject) {
                    self.report_unexpected_field(name, *blame_span);
                }
                if let FieldValue::Value(Some(value)) = payload {
                    self.check_value_expr(value);
                }
            }
        }
    }
}

/// Structural value forms that check against an Optional/Nullable expected type
/// by peeling the wrapper and checking the payload. Everything else keeps the
/// full wrapper so type-level N2 subsumption and match-arm expectation flow work.
fn peels_optional_or_nullable_expected(value: &Expr) -> bool {
    matches!(
        &value.kind,
        ExprKind::Literal(_)
            | ExprKind::Tuple(_)
            | ExprKind::Record(_)
            | ExprKind::Tag(_)
            | ExprKind::Array(_)
            | ExprKind::Set(_)
    )
}
