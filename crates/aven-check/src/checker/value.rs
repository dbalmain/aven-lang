use super::*;

use crate::checker::inference::pipe_call_expr;

impl<'a> Checker<'a> {
    pub(super) fn check_value_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Record(entries) => {
                self.check_value_record_entries(entries);
            }
            ExprKind::Set(entries) | ExprKind::Array(entries) => {
                self.report_value_record_markers(entries);
                self.walk_value_record_values(entries);
            }
            ExprKind::Lambda {
                params,
                return_annotation,
                requirements,
                body,
            } => self.check_lambda_value_expr(
                params,
                return_annotation.as_deref(),
                requirements,
                body,
            ),
            ExprKind::Block(items) => self.check_items(items),
            ExprKind::Match { subject, arms, .. } => {
                self.check_match_arms(subject, arms, None);
            }
            ExprKind::Propagate { value, .. } => {
                self.check_propagate_value_expr(value);
            }
            ExprKind::Call { callee, args } => self.check_value_call(expr, callee, args),
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            } if operator == "|>" => {
                let call = pipe_call_expr(left, right);
                self.check_value_expr(&call);
            }
            ExprKind::FieldAccess {
                receiver,
                field,
                field_span,
                ..
            } => {
                self.check_value_field_access(receiver, field, *field_span);
            }
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                self.check_name_reference(name, expr.span);
            }
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Undefined
            | ExprKind::Null
            | ExprKind::Tag(_) => {}
            _ => walk_expr_children(expr, &mut |child| {
                self.check_value_expr(child);
            }),
        }
    }

    pub(super) fn check_name_reference(&mut self, name: &str, span: Span) {
        let env = self.local_types.inference_env();
        self.infer_name_reference(&env, name, span);
    }

    /// Check a call expression in statement position. When the callee resolves
    /// to a concretely-known function type (e.g. a host global), surface
    /// argument/arity errors through the existing arity/mismatch machinery
    /// rather than letting inference silently defer them. A non-concrete callee
    /// (unknown/free name) keeps today's permissive behaviour.
    pub(super) fn check_value_call(&mut self, call: &Expr, callee: &Expr, args: &[Expr]) {
        if self.infer_import_call(callee, args).is_some() {
            return;
        }
        let env = self.local_types.inference_env();
        if self
            .infer_slot_conversion_call(&env, callee, args)
            .is_some()
        {
            return;
        }
        if self.named_family_constructor_owner(callee).is_some() {
            let _ = self.infer_call(&env, callee, args);
            return;
        }

        // Uppercase comptime functions validate their arguments while they
        // specialize. Their parameters describe comptime bounds (including
        // `Type`), not runtime call arguments, so the ordinary value-call
        // check would report the same failure a second time.
        if self.is_uppercase_comptime_function_callee(callee) {
            // Runtime type applications use the same checked specialization
            // selector as named aliases. Eagerly produce its recursive head so
            // the compiler can replace the source lambda with a finite runtime
            // descriptor graph.
            if !args
                .iter()
                .any(|arg| self.expr_references_unresolved_comptime_param(arg))
            {
                let _ = self.try_lower_comptime_annotation_for_eager_validation(call);
            }
            self.check_value_expr(callee);
            // Specialization owns the outer comptime-parameter validation, but
            // its evaluator deliberately does not descend into unsupported
            // value forms such as records and lambdas. Walk the arguments here
            // to retain name and structural diagnostics without comparing them
            // against the comptime parameters a second time.
            self.check_value_exprs(args);
            return;
        }

        if let Some((receiver, _)) = self.value_encode_sugar_receiver(&env, callee) {
            self.check_value_expr(receiver);
            let _ = self.infer_value_encode_call(&env, callee, args);
            return;
        }

        // `text.decode(Fmt, ...)` is format sugar, not a Text method field. Route
        // through the desugar before field-access checking would report missing
        // `decode` on Text (which now has a real method table).
        if self.infer_text_decode_call(&env, callee, args).is_some() {
            return;
        }

        let callee_obligation_marker = self.method_obligation_marker();
        self.check_value_expr(callee);

        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, callee);
        let callee_type = self.normalize(&self.resolve_and_default(&inferred));
        // The callee is inferred above only to choose the directed checking
        // path. Qualified schemes must be instantiated with the arguments in
        // `infer_call`; retaining these probe obligations would leave an
        // unrelated fresh candidate that can never discharge.
        let _ = self.take_method_obligations_since(callee_obligation_marker);
        let Type::Function {
            params, required, ..
        } = &callee_type
        else {
            self.check_value_exprs(args);
            return;
        };
        let required = *required;
        if !is_concrete_type(&callee_type) {
            if required <= args.len() && args.len() <= params.len() {
                let diagnostics_start = self.diagnostics.len();
                let _ = self.infer_call(&env, callee, args);
                self.check_value_exprs(args);
                self.deduplicate_diagnostics_since(diagnostics_start);
                return;
            }
            self.check_value_exprs(args);
            return;
        }

        if args.len() < required || args.len() > params.len() {
            self.report_function_arity_mismatch(required, params.len(), args.len(), callee.span);
            self.check_value_exprs(args);
            return;
        }

        // Omitted trailing optional params are simply not supplied; check each
        // provided argument against its corresponding param.
        let params = params.clone();
        for (expected, arg) in params.iter().zip(args) {
            self.check_call_arg_against_param(expected, arg);
        }
    }

    /// Check a field-access expression in statement position. A resolved
    /// receiver that lacks the field is a real missing-field error; an
    /// unknown/open receiver stays deferred as before.
    pub(super) fn check_value_field_access(&mut self, receiver: &Expr, field: &str, span: Span) {
        self.check_value_expr(receiver);

        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, receiver);
        let receiver_type = self.normalize(&self.resolve_and_default(&inferred));
        if self.exact_method_signature(&receiver_type, field).is_some() {
            return;
        }
        if self
            .attached_builtin_method_required_owner(&receiver_type, field)
            .is_some()
        {
            // The enclosing call inference reports the owner-pattern mismatch
            // after its receiver-first lookup.
            return;
        }
        if builtin_collection_method_type(&receiver_type, field).is_some() {
            return;
        }
        let (_, core) = peel_empty_values(&receiver_type);
        let receiver_type = self
            .named_family_data_view(core)
            .unwrap_or_else(|| self.unfold_recursive_type_once(core));
        if builtin_collection_method_type(&receiver_type, field).is_some() {
            return;
        }
        if is_concrete_type(&receiver_type)
            && (is_map_receiver_type(&receiver_type)
                || is_array_receiver_type(&receiver_type)
                || is_text_type(&receiver_type)
                || result_type_args(&receiver_type).is_some())
        {
            self.report_missing_field(field, span);
            return;
        }
        // Array methods (`has`, `push`) are not invented on non-array receivers.
        if is_concrete_type(&receiver_type)
            && crate::ty::ARRAY_METHOD_NAMES.contains(&field)
            && !is_array_receiver_type(&receiver_type)
        {
            self.report_missing_field(field, span);
            return;
        }
        // Text methods are not invented on non-Text receivers.
        if is_concrete_type(&receiver_type)
            && crate::ty::TEXT_METHOD_NAMES.contains(&field)
            && !is_text_type(&receiver_type)
        {
            self.report_missing_field(field, span);
            return;
        }

        let Type::Record(row) = &receiver_type else {
            if is_resolved_value_type(&receiver_type) {
                self.report_missing_field(field, span);
            }
            return;
        };
        if row.tail != RowTail::Closed || !is_resolved_value_type(&receiver_type) {
            return;
        }

        let has_field = row
            .entries
            .iter()
            .any(|entry| matches!(entry, RowEntry::Field { name, .. } if name == field));
        if !has_field {
            self.report_missing_field(field, span);
        }
    }

    pub(super) fn check_propagate_value_expr(&mut self, value: &Expr) {
        self.check_value_expr(value);

        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, value);
        let resolved = self.normalize(&self.resolve_and_default(&inferred));
        if result_type_args(&resolved).is_none() {
            self.report_propagate_not_result_if_concrete(&resolved, value.span);
        }
    }

    pub(super) fn check_lambda_value_expr(
        &mut self,
        params: &[Param],
        return_annotation: Option<&Expr>,
        requirements: &[Requirement],
        body: &Expr,
    ) {
        self.push_inline_lambda_type_var_scope();
        let param_types: Vec<_> = params
            .iter()
            .map(|param| {
                param
                    .annotation
                    .as_ref()
                    .map(|annotation| {
                        LocalValueType::Known(self.lower_inline_lambda_annotation(annotation))
                    })
                    .unwrap_or(LocalValueType::Unknown)
            })
            .collect();
        let body_expected =
            return_annotation.map(|annotation| self.lower_inline_lambda_annotation(annotation));

        self.local_types.push();
        self.push_local_comptime_param_scope(params);
        for (param, ty) in params.iter().zip(param_types) {
            self.record_local_value_type(param.name_span, &ty);
            self.local_types.define(&param.name, ty);
        }
        let assumptions = self.requirement_predicates(requirements);
        let obligation_marker = self.method_obligation_marker();
        self.push_method_assumptions(assumptions);
        if let Some(body_expected) = body_expected {
            // Mirror `check_lambda_against_function`: check the body against the
            // lowered return annotation with propagation context, so inline
            // `(x): T => body` mismatches surface the same way as binding-level
            // `f: (...) -> T = ...`.
            self.propagation_contexts
                .push(PropagationContext::default());
            self.check_value_against(&body_expected, body);
            let body_type = self.infer_body_type_for_propagation_check(body);
            let propagation = self.pop_propagation_context();
            let _ = self.apply_propagation_context_to_body_type(body, body_type, &propagation);
            self.report_propagated_errors_against_annotation(&body_expected, &propagation);
        } else {
            self.check_value_expr(body);
        }
        self.finish_checked_lambda_obligations(obligation_marker);
        self.pop_method_assumptions();
        self.local_comptime_params.pop();
        self.local_types.pop();
        self.pop_inline_lambda_type_var_scope();
    }

    pub(super) fn check_value_exprs(&mut self, items: &[Expr]) {
        for item in items {
            self.check_value_expr(item);
        }
    }

    pub(super) fn check_value_record_entries(&mut self, entries: &[RecordEntry]) {
        self.report_value_record_markers(entries);
        self.report_redundant_undefined_record_fields(entries);
        self.walk_value_record_values(entries);
    }

    pub(super) fn report_value_record_markers(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Open { span } => {
                    self.diagnostics.push(
                        Diagnostic::error("open row markers are only valid in type position")
                            .with_code(codes::ty::TYPE_ONLY_RECORD_ENTRY)
                            .with_label(Label::primary(*span, "open row marker here"))
                            .with_note("remove `..` from value records"),
                    );
                }
                RecordEntry::Field { .. }
                | RecordEntry::Method { .. }
                | RecordEntry::FieldDefault { .. }
                | RecordEntry::FieldComputed { .. }
                | RecordEntry::Shorthand { .. }
                | RecordEntry::Spread { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::DeleteComputed { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Iteration { .. }
                | RecordEntry::Element(_) => {}
            }
        }
    }

    pub(super) fn report_redundant_undefined_record_fields(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Field {
                    name, value, span, ..
                } if is_undefined_value(value) => {
                    self.report_redundant_undefined_field(*span, format!("`-{name}`"));
                }
                RecordEntry::FieldComputed { value, span, .. } if is_undefined_value(value) => {
                    self.report_redundant_undefined_field(*span, "`-[...]`");
                }
                RecordEntry::Iteration { body, .. } => {
                    self.report_redundant_undefined_record_fields(body);
                }
                RecordEntry::Open { .. }
                | RecordEntry::Method { .. }
                | RecordEntry::FieldDefault { .. }
                | RecordEntry::Field { .. }
                | RecordEntry::FieldComputed { .. }
                | RecordEntry::Shorthand { .. }
                | RecordEntry::Spread { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::DeleteComputed { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Element(_) => {}
            }
        }
    }

    pub(super) fn walk_value_record_values(&mut self, entries: &[RecordEntry]) {
        for entry in entries {
            match entry {
                RecordEntry::Field { value, .. }
                | RecordEntry::Method { value, .. }
                | RecordEntry::Spread { value, .. }
                | RecordEntry::DeleteComputed { key: value, .. }
                | RecordEntry::Element(value) => {
                    self.check_value_expr(value);
                }
                RecordEntry::FieldComputed { key, value, .. } => {
                    self.check_value_expr(key);
                    self.check_value_expr(value);
                }
                RecordEntry::FieldDefault {
                    annotation,
                    default,
                    ..
                } => {
                    self.lower_annotation(annotation);
                    self.check_value_expr(default);
                }
                RecordEntry::Iteration {
                    source,
                    binder,
                    binder_span,
                    guard,
                    body,
                    ..
                } => {
                    self.check_value_expr(source);
                    self.local_types.push();
                    self.local_types.define(binder, LocalValueType::Unknown);
                    self.record_local_value_type(*binder_span, &LocalValueType::Unknown);
                    if let Some(guard) = guard {
                        self.check_value_expr(guard);
                    }
                    self.walk_value_record_values(body);
                    self.local_types.pop();
                }
                RecordEntry::Shorthand {
                    name, name_span, ..
                } => {
                    self.check_name_reference(name, *name_span);
                }
                RecordEntry::Delete { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Open { .. } => {}
            }
        }
    }
}
