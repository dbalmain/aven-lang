use super::*;

impl<'a> Checker<'a> {
    pub(super) fn lower_normalized_annotation(&mut self, annotation: &Expr) -> Type {
        let ty = self.lower_annotation(annotation);
        self.normalize(&ty)
    }

    /// Check a binding value against its declared annotation. Polymorphic
    /// function signatures push their type variables as rigid (skolem) while
    /// the body is checked so the body cannot pin caller-chosen parameters.
    pub(super) fn check_value_against_declared_type(&mut self, expected: &Type, value: &Expr) {
        let rigid =
            matches!(expected, Type::Function { .. }).then(|| type_variable_names(expected));
        if let Some(vars) = rigid.filter(|vars| !vars.is_empty()) {
            self.rigid_type_var_scopes.push(vars);
            self.check_value_against(expected, value);
            self.rigid_type_var_scopes.pop();
        } else {
            self.check_value_against(expected, value);
        }
    }

    pub(super) fn is_rigid_type_var(&self, name: &str) -> bool {
        self.rigid_type_var_scopes
            .iter()
            .any(|scope| scope.contains(name))
    }

    pub(super) fn push_inline_lambda_type_var_scope(&mut self) {
        self.inline_lambda_type_var_scopes.push(HashMap::new());
    }

    pub(super) fn pop_inline_lambda_type_var_scope(&mut self) {
        self.inline_lambda_type_var_scopes.pop();
    }

    /// Lower an inline lambda annotation, turning free lowercase names into
    /// shared inference metas. Declared signature binders remain rigid.
    pub(super) fn lower_inline_lambda_annotation(&mut self, annotation: &Expr) -> Type {
        let ty = self.lower_normalized_annotation(annotation);
        self.resolve_inline_lambda_annotation_variables(&ty)
    }

    pub(super) fn resolve_inline_lambda_annotation_variables(&mut self, ty: &Type) -> Type {
        map_type(ty, &mut |node| {
            let Type::Variable(name) = node else {
                return None;
            };
            if self.is_rigid_type_var(name) {
                return None;
            }

            let existing = self
                .inline_lambda_type_var_scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name).cloned());
            Some(existing.unwrap_or_else(|| {
                let meta = self.unifier.fresh();
                self.inline_lambda_type_var_scopes
                    .last_mut()
                    .expect("inline lambda annotations always have a type-variable scope")
                    .insert(name.clone(), meta.clone());
                meta
            }))
        })
    }

    pub(super) fn check_value_against(&mut self, expected: &Type, value: &Expr) {
        if let Type::SlotRecord { data, slots } = expected {
            match &value.kind {
                ExprKind::Group(inner) => self.check_value_against(expected, inner),
                ExprKind::Block(items) => self.check_block_against(expected, items),
                ExprKind::Match { subject, arms, .. } => {
                    self.check_match_arms(subject, arms, Some(expected));
                }
                _ => self.check_value_against_slot_record(data, slots, value),
            }
            return;
        }
        if matches!(expected, Type::Recursive(_))
            && let ExprKind::Name(name) | ExprKind::ComptimeName(name) = &value.kind
        {
            let env = self.local_types.inference_env();
            let actual = self.infer_name_reference(&env, name, value.span);
            if !type_contains_deferred(&actual) {
                self.check_type_against_type(expected, &actual, value.span);
            }
            return;
        }

        if matches!(expected, Type::Recursive(_)) {
            let Type::Recursive(id) = expected else {
                unreachable!("recursive guard established the variant")
            };
            if !self.recursive_type_comparisons.insert(*id) {
                return;
            }
            let unfolded = self.unfold_recursive_type_once(expected);
            if unfolded != *expected {
                self.check_value_against(&unfolded, value);
                self.recursive_type_comparisons.remove(id);
                return;
            }
            self.recursive_type_comparisons.remove(id);
        }

        if self.check_primitive_family_literal_branding(expected, value) {
            return;
        }

        match (&value.kind, expected) {
            (ExprKind::Group(inner), _) => self.check_value_against(expected, inner),
            (ExprKind::Block(items), _) => self.check_block_against(expected, items),
            (
                ExprKind::Lambda {
                    params,
                    return_annotation,
                    requirements,
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
                requirements,
                body,
                (expected_params, expected_result),
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
                if let Some(found) = mismatched_literal_kind(name, literal)
                    && self.known_types.contains(name)
                {
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

    fn check_value_against_slot_record(&mut self, data: &Row, slots: &Row, value: &Expr) {
        let diagnostics_start = self.diagnostics.len();
        let env = self.local_types.inference_env();
        let actual = self.infer(&env, value);
        let actual = self.normalize(&self.resolve_and_default(&actual));
        let expected = Type::SlotRecord {
            data: Box::new(data.clone()),
            slots: Box::new(slots.clone()),
        };

        if matches!(actual, Type::SlotRecord { .. }) {
            self.check_type_against_type(&expected, &actual, value.span);
            if self.diagnostics.len() == diagnostics_start && actual != expected {
                self.record_slot_reification(value.span, data, slots);
            }
            return;
        }

        if data.tail != RowTail::Closed || slots.tail != RowTail::Closed {
            self.diagnostics.push(
                Diagnostic::error("method-slot reification requires a closed target")
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(
                        value.span,
                        "the target still has an open row",
                    ))
                    .with_note("use a closed slot record at a deliberate forgetting boundary"),
            );
            return;
        }
        if matches!(actual, Type::Deferred | Type::Variable(_) | Type::Meta(_)) {
            self.diagnostics.push(
                Diagnostic::error("generic-source method-slot reification is not available")
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(
                        value.span,
                        "the source owner is not statically known here",
                    ))
                    .with_note(
                        "this boundary needs generic-source thunk materialization, which the current runtime does not provide",
                    ),
            );
            return;
        }

        let requested_slots = slots
            .entries
            .iter()
            .filter_map(|entry| match entry {
                RowEntry::Field { name, ty } => Some((name.clone(), ty.clone())),
                RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
            })
            .collect::<Vec<_>>();
        let source_provides_slot = requested_slots
            .iter()
            .any(|(name, _)| self.exact_method_signature(&actual, name).is_some());

        if !data.entries.is_empty() {
            if let Some(Type::Record(source_data)) = self.named_family_data_view(&actual) {
                self.check_type_against_type(
                    &Type::Record(data.clone()),
                    &Type::Record(source_data),
                    value.span,
                );
            } else if source_provides_slot {
                let fields = data
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        RowEntry::Field { name, .. } => Some(format!("`{name}`")),
                        RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.diagnostics.push(
                    Diagnostic::error("builtin values reify only to pure-behavior targets")
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(
                            value.span,
                            format!("target requires structural data field {fields}"),
                        ))
                        .with_note(
                            "builtin methods do not create structural data fields during reification",
                        ),
                );
                return;
            }
        }

        let marker = self.method_obligation_marker();
        for (name, requested) in &requested_slots {
            let Type::Function { params, result, .. } = requested else {
                continue;
            };
            if let Type::Record(row) = &actual
                && row.entries.iter().any(
                    |entry| matches!(entry, RowEntry::Field { name: field, .. } if field == name),
                )
            {
                self.diagnostics.push(
                    Diagnostic::error(format!(
                        "stored function field `{name}` cannot fill a method slot"
                    ))
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(value.span, "member kinds differ at this boundary"))
                    .with_note(format!(
                        "construct `{{ {name}: value.{name} }}` explicitly to store a function field"
                    )),
                );
                continue;
            }
            self.push_method_obligations_at(
                [MethodPredicate {
                    candidate: actual.clone(),
                    member: name.clone(),
                    params: params.clone(),
                    result: result.as_ref().clone(),
                    operator_span: value.span,
                    binding: None,
                    call_span: Some(value.span),
                    obligation_id: None,
                }],
                value.span,
            );
        }
        self.finish_non_generalizing_lambda_obligations(marker);

        if self.diagnostics.len() == diagnostics_start {
            self.record_slot_reification(value.span, data, slots);
        }
    }

    fn record_slot_reification(&mut self, span: Span, data: &Row, slots: &Row) {
        let names = |row: &Row| {
            row.entries
                .iter()
                .filter_map(|entry| match entry {
                    RowEntry::Field { name, .. } => Some(name.clone()),
                    RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
                })
                .collect()
        };
        self.slot_reifications.insert(
            span,
            SlotReificationTarget {
                fields: names(data),
                slots: names(slots),
            },
        );
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
                MergedItem::MethodAttachment(_) => {}
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
        requirements: &[Requirement],
        body: &Expr,
        expected: (&[Type], &Type),
    ) {
        let (expected_params, expected_result) = expected;
        if params.len() != expected_params.len() {
            // The expected type is a function-type annotation, which has no
            // defaults: required == total.
            self.report_function_arity_mismatch(
                expected_params.len(),
                expected_params.len(),
                params.len(),
                lambda_span,
            );
            self.check_lambda_value_expr(params, return_annotation, requirements, body);
            return;
        }

        self.push_inline_lambda_type_var_scope();
        let mut param_types = Vec::new();
        for (param, expected) in params.iter().zip(expected_params) {
            let actual = param
                .annotation
                .as_ref()
                .map(|annotation| {
                    let actual = self.lower_inline_lambda_annotation(annotation);
                    // Function parameters are contravariant. A lambda's
                    // explicit parameter annotation is the actual accepted type,
                    // so compare it in the same swapped direction as
                    // Function-vs-Function comparison.
                    self.check_type_against_type(&actual, expected, annotation.span);
                    if !free_metas(&actual).is_empty() {
                        let _ = self.unifier.unify(&actual, expected);
                    }
                    actual
                })
                .unwrap_or_else(|| expected.clone());
            param_types.push(actual);
        }

        let (body_expected, body_has_inline_metas) = if let Some(annotation) = return_annotation {
            let actual = self.lower_inline_lambda_annotation(annotation);
            let has_inline_metas = !free_metas(&actual).is_empty();
            self.check_type_against_type(expected_result, &actual, annotation.span);
            if has_inline_metas {
                let _ = self.unifier.unify(expected_result, &actual);
            }
            (actual, has_inline_metas)
        } else {
            (expected_result.clone(), false)
        };

        self.local_types.push();
        self.push_local_comptime_param_scope(params);
        for (param, ty) in params.iter().zip(param_types) {
            self.record_inferred_type(param.name_span, ty.clone());
            self.local_types
                .define(&param.name, LocalValueType::Known(ty));
        }
        let assumptions = self.requirement_predicates(requirements);
        let obligation_marker = self.method_obligation_marker();
        self.push_method_assumptions(assumptions);
        self.propagation_contexts
            .push(PropagationContext::default());
        self.check_value_against(&body_expected, body);
        let body_type = self.infer_body_type_for_propagation_check(body);
        let propagation = self.pop_propagation_context();
        let body_type = self.apply_propagation_context_to_body_type(body, body_type, &propagation);
        // A call-site function expectation can carry result metas even without
        // an inline return annotation. Feed the contextual body type back into
        // those metas so higher-order arguments refine their caller's result.
        let body_fits = free_metas(&body_expected).is_empty()
            || self.inferred_return_type_fits_annotation(&body_expected, &body_type);
        if body_has_inline_metas && !body_fits {
            let expected = self.normalize(&self.resolve_and_default(&body_expected));
            let actual = self.normalize(&self.resolve_and_default(&body_type));
            self.report_type_mismatch_between_types(
                &expected.render(),
                &actual.render(),
                body.span,
            );
        }
        self.report_propagated_errors_against_annotation(&body_expected, &propagation);
        self.finish_non_generalizing_lambda_obligations(obligation_marker);
        self.pop_method_assumptions();
        self.local_comptime_params.pop();
        self.local_types.pop();
        self.pop_inline_lambda_type_var_scope();
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

        if self
            .primitive_family_base_view(actual)
            .is_some_and(|base| base == *expected)
        {
            if !span.is_empty() {
                self.primitive_family_coercions
                    .insert(span, PrimitiveFamilyCoercion::Widen);
            }
            return;
        }

        if self.contains_nested_primitive_family_widening(expected, actual) {
            self.report_type_mismatch_between_types(&expected.render(), &actual.render(), span);
            return;
        }

        if matches!(expected, Type::Record(_))
            && let Some(data) = self.named_family_data_view(actual)
        {
            self.check_type_against_type(expected, &data, span);
            return;
        }
        if let Type::Named(owner) = expected
            && self.named_family_data_view(expected).is_some()
            && matches!(actual, Type::Record(_))
        {
            let display_owner = Type::Named(owner.to_owned()).render();
            self.diagnostics.push(
                Diagnostic::error(format!("construct `{display_owner}` explicitly"))
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(
                        span,
                        "a structural record does not carry this method owner",
                    ))
                    .with_note(format!(
                        "write `{display_owner}({{ ... }})` to attach its methods"
                    )),
            );
            return;
        }

        match (expected, actual) {
            (Type::Recursive(expected), Type::Recursive(actual)) => {
                self.report_type_mismatch_between_types(
                    &Type::Recursive(*expected).render(),
                    &Type::Recursive(*actual).render(),
                    span,
                );
                return;
            }
            (Type::Recursive(id), _) | (_, Type::Recursive(id)) => {
                if !self.recursive_type_comparisons.insert(*id) {
                    return;
                }
                let unfolded = self.unfold_recursive_type_once(&Type::Recursive(*id));
                if unfolded == Type::Recursive(*id) {
                    self.recursive_type_comparisons.remove(id);
                    self.report_type_mismatch_between_types(
                        &expected.render(),
                        &actual.render(),
                        span,
                    );
                    return;
                }
                if matches!(expected, Type::Recursive(_)) {
                    self.check_type_against_type(&unfolded, actual, span);
                } else {
                    self.check_type_against_type(expected, &unfolded, span);
                }
                self.recursive_type_comparisons.remove(id);
                return;
            }
            _ => {}
        }

        match (expected, actual) {
            (
                Type::SlotRecord {
                    data: expected_data,
                    slots: expected_slots,
                },
                Type::SlotRecord {
                    data: actual_data,
                    slots: actual_slots,
                },
            ) => {
                self.check_type_against_type(
                    &Type::Record(expected_data.as_ref().clone()),
                    &Type::Record(actual_data.as_ref().clone()),
                    span,
                );
                self.check_type_against_type(
                    &Type::Record(expected_slots.as_ref().clone()),
                    &Type::Record(actual_slots.as_ref().clone()),
                    span,
                );
            }
            (Type::SlotRecord { .. }, _) | (_, Type::SlotRecord { .. }) => {
                self.report_type_mismatch_between_types(&expected.render(), &actual.render(), span);
            }
            (Type::Optional(expected_inner), Type::Optional(actual_inner))
            | (Type::Nullable(expected_inner), Type::Nullable(actual_inner)) => {
                self.check_type_against_type(expected_inner, actual_inner, span);
            }
            (Type::Optional(_), Type::Named(name)) if name == "Undefined" => {}
            (Type::Nullable(_), Type::Named(name)) if name == "Null" => {}
            (Type::Optional(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Nullable(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Named(expected), Type::Named(actual))
                if named_type_mismatch(expected, actual)
                    && self.is_known_named_type(expected)
                    && self.is_known_named_type(actual) =>
            {
                self.report_type_mismatch_between_types(
                    &Type::Named(expected.clone()).render(),
                    &Type::Named(actual.clone()).render(),
                    span,
                );
            }
            // A bare named type is never Optional/Nullable; wrappers peel on the
            // expected side above, so an actual wrapper against a named expected
            // is always a shape mismatch (including `Int` vs `?Int`).
            (Type::Named(expected), actual @ (Type::Optional(_) | Type::Nullable(_)))
                if self.known_types.contains(expected) =>
            {
                self.report_type_mismatch_between_types(expected, &actual.render(), span);
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
                } else if type_variable_names(actual)
                    .iter()
                    .any(|name| !self.is_rigid_type_var(name))
                {
                    // A polymorphic actual must be checked with ONE
                    // instantiation shared across every occurrence of each
                    // variable — comparing params and result independently
                    // would let `(a) -> a` inhabit `(Int) -> Text`.
                    let instantiated =
                        self.instantiate_nonrigid_type_variables(actual, &mut HashMap::new());
                    if self.unifier.unify(expected, &instantiated).is_err() {
                        self.report_type_mismatch_between_types(
                            &expected.render(),
                            &actual.render(),
                            span,
                        );
                    }
                } else if !free_metas(actual).is_empty() {
                    // Instantiated schemes carry fresh metas rather than their
                    // original variable names. They still need one shared
                    // instantiation across the whole function comparison.
                    if self.unifier.unify(expected, actual).is_err() {
                        self.report_type_mismatch_between_types(
                            &expected.render(),
                            &actual.render(),
                            span,
                        );
                    }
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
            (
                expected @ (Type::Record(_) | Type::Tuple(_) | Type::Function { .. }),
                Type::Variant(actual),
            ) if literal_variant_base(actual).is_some() => {
                self.report_type_mismatch_between_types(
                    &expected.render(),
                    &display_inferred_type(&Type::Variant(actual.clone())).render(),
                    span,
                );
            }
            (Type::Record(expected), Type::Record(actual)) => {
                let (Some(expected), Some(actual)) =
                    (literal_record_type(expected), literal_record_type(actual))
                else {
                    return;
                };
                let actual_fields: Vec<_> = actual
                    .fields
                    .iter()
                    .map(|field| (field.name, span, FieldValue::Type(field.ty)))
                    .collect();
                let missing_fields = if actual.open {
                    MissingFields::Allow
                } else {
                    MissingFields::Reject
                };
                self.compare_record(
                    &expected,
                    &actual_fields,
                    ExtraFields::Allow,
                    missing_fields,
                    span,
                );
            }
            (Type::Variant(expected), Type::Variant(actual)) => {
                self.check_variant_type_against_type(expected, actual, span);
            }
            // Rigid annotation variables (from an enclosing polymorphic
            // function signature) unify only with themselves. Free variables
            // outside that scope keep today's silent deferral (type-alias
            // implicit vars, free local annotations). Meta/Deferred also stay
            // silent so incomplete inference does not false-positive.
            (Type::Variable(expected_name), Type::Variable(actual_name))
                if expected_name != actual_name
                    && (self.is_rigid_type_var(expected_name)
                        || self.is_rigid_type_var(actual_name)) =>
            {
                self.report_rigid_type_variable_mismatch(
                    if self.is_rigid_type_var(expected_name) {
                        expected_name
                    } else {
                        actual_name
                    },
                    &expected.render(),
                    &actual.render(),
                    span,
                );
            }
            (Type::Variable(name), actual)
                if reportable_type_shape(actual) && self.is_rigid_type_var(name) =>
            {
                self.report_rigid_type_variable_mismatch(
                    name,
                    &expected.render(),
                    &actual.render(),
                    span,
                );
            }
            (expected, Type::Variable(name))
                if reportable_type_shape(expected) && self.is_rigid_type_var(name) =>
            {
                self.report_rigid_type_variable_mismatch(
                    name,
                    &expected.render(),
                    &actual.render(),
                    span,
                );
            }
            // Nominal names that survive normalization do not admit structural
            // shapes (records, tuples, functions). Apply and Variant have their
            // own arms above; Deferred/Meta/Variable stay silent via
            // `reportable_type_shape` / non-match. Unknown names are left
            // unconstrained (they already report `type.unknown-name`).
            (
                Type::Named(expected),
                actual @ (Type::Record(_) | Type::Tuple(_) | Type::Function { .. }),
            ) if self.known_types.contains(expected) => {
                self.report_type_mismatch_between_types(expected, &actual.render(), span);
            }
            (
                expected @ (Type::Record(_) | Type::Tuple(_) | Type::Function { .. }),
                Type::Named(actual),
            ) if self.known_types.contains(actual) => {
                self.report_type_mismatch_between_types(&expected.render(), actual, span);
            }
            _ => {}
        }
    }

    fn check_primitive_family_literal_branding(&mut self, expected: &Type, value: &Expr) -> bool {
        let Type::Named(name) = expected else {
            return false;
        };
        let Some(owner) = self.named_family_aliases.get(name).cloned() else {
            return false;
        };
        let Some(base) = self
            .named_families
            .get(&owner)
            .and_then(|family| family.primitive_base.clone())
        else {
            return false;
        };

        if let ExprKind::Literal(literal) = &ungroup_expr(value).kind {
            if primitive_literal_matches_base(literal, &base) {
                self.primitive_family_coercions
                    .insert(value.span, PrimitiveFamilyCoercion::Brand { owner });
            } else if let Some(found) = primitive_literal_base_name(literal) {
                self.report_type_mismatch_between_types(
                    &Type::Named(name.clone()).render(),
                    found,
                    value.span,
                );
            } else {
                self.check_value_against(&base, value);
            }
            return true;
        }

        if !matches!(
            ungroup_expr(value).kind,
            ExprKind::Name(_) | ExprKind::ComptimeName(_)
        ) {
            return false;
        }
        let env = self.local_types.inference_env();
        let actual = self.infer(&env, value);
        let actual = self.normalize(&self.resolve_and_default(&actual));
        let Type::Variant(row) = &actual else {
            return false;
        };
        let [RowEntry::Literal { value: literal }] = row.entries.as_slice() else {
            return false;
        };
        if primitive_literal_matches_base(literal, &base) {
            self.primitive_family_coercions
                .insert(value.span, PrimitiveFamilyCoercion::Brand { owner });
        } else if let Some(found) = primitive_literal_base_name(literal) {
            self.report_type_mismatch_between_types(
                &Type::Named(name.clone()).render(),
                found,
                value.span,
            );
        } else {
            self.check_type_against_type(&base, &actual, value.span);
        }
        true
    }

    fn is_known_named_type(&self, name: &str) -> bool {
        self.known_types.contains(name) || self.named_families.contains_key(name)
    }

    fn contains_nested_primitive_family_widening(&self, expected: &Type, actual: &Type) -> bool {
        if self
            .primitive_family_base_view(actual)
            .is_some_and(|base| base == *expected)
        {
            return true;
        }
        match (expected, actual) {
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
                self.contains_nested_primitive_family_widening(expected_callee, actual_callee)
                    || expected_args
                        .iter()
                        .zip(actual_args)
                        .any(|(expected, actual)| {
                            self.contains_nested_primitive_family_widening(expected, actual)
                        })
            }
            (Type::Optional(expected), Type::Optional(actual))
            | (Type::Nullable(expected), Type::Nullable(actual)) => {
                self.contains_nested_primitive_family_widening(expected, actual)
            }
            (Type::Tuple(expected), Type::Tuple(actual)) => {
                expected.iter().zip(actual).any(|(expected, actual)| {
                    self.contains_nested_primitive_family_widening(expected, actual)
                })
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
                expected_params
                    .iter()
                    .zip(actual_params)
                    .any(|(expected, actual)| {
                        self.contains_nested_primitive_family_widening(expected, actual)
                    })
                    || self
                        .contains_nested_primitive_family_widening(expected_result, actual_result)
            }
            (Type::Record(expected), Type::Record(actual)) => {
                expected.entries.iter().any(|expected| {
                    let RowEntry::Field {
                        name: expected_name,
                        ty: expected_type,
                    } = expected
                    else {
                        return false;
                    };
                    actual.entries.iter().any(|actual| {
                        matches!(actual,
                        RowEntry::Field { name, ty }
                            if name == expected_name
                                && self.contains_nested_primitive_family_widening(
                                    expected_type,
                                    ty,
                                ))
                    })
                })
            }
            _ => false,
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
            // An open literal union is a join artifact from fresh literals
            // (users cannot write `0 | ..`), so a same-base value widens it —
            // e.g. an instantiated fold accumulator seeded with a literal.
            // Only closed (user-written) unions reject their base type.
            if expected.tail != RowTail::Closed {
                return;
            }
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
            self.compare_record(
                &expected,
                &actual_fields,
                ExtraFields::Reject,
                MissingFields::Reject,
                actual.span,
            );
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
        missing_fields: MissingFields,
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
                None if matches!(missing_fields, MissingFields::Reject) => {
                    self.report_missing_field(field.name, record_span)
                }
                None => {}
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

fn primitive_literal_matches_base(literal: &Literal, base: &Type) -> bool {
    matches!(base, Type::Named(name) if primitive_literal_base_name(literal) == Some(name))
}

fn primitive_literal_base_name(literal: &Literal) -> Option<&'static str> {
    match literal {
        Literal::Bool(_) => Some("Bool"),
        Literal::String(_) => Some("Text"),
        Literal::Number(number) if super::inference::is_float_literal_text(number) => Some("Float"),
        Literal::Number(_) => Some("Int"),
        Literal::Regex(_) => None,
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
