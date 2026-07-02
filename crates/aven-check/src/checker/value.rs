use super::*;

impl<'a> Checker<'a> {
    pub(super) fn check_value_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Record(entries) => {
                self.check_value_record_entries(entries);
            }
            ExprKind::Set(entries) => {
                self.report_value_record_markers(entries);
                self.walk_value_record_values(entries);
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
            ExprKind::Propagate { value, .. } => {
                self.check_propagate_value_expr(value);
            }
            ExprKind::Call { callee, args } => self.check_value_call(callee, args),
            ExprKind::FieldAccess {
                receiver, field, ..
            } => {
                self.check_value_field_access(receiver, field, expr.span);
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
    pub(super) fn check_value_call(&mut self, callee: &Expr, args: &[Expr]) {
        self.check_value_expr(callee);

        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, callee);
        let callee_type = self.normalize(&self.resolve_and_default(&inferred));
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

    /// Check a field-access expression in statement position. A concretely-known
    /// closed record receiver that lacks the field is a real missing-field
    /// error; an unknown/open receiver stays deferred as before.
    pub(super) fn check_value_field_access(&mut self, receiver: &Expr, field: &str, span: Span) {
        self.check_value_expr(receiver);

        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, receiver);
        let receiver_type = self.normalize(&self.resolve_and_default(&inferred));
        let Type::Record(row) = &receiver_type else {
            return;
        };
        if row.tail != RowTail::Closed || !is_concrete_type(&receiver_type) {
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
        self.push_local_comptime_param_scope(params);
        for (param, ty) in params.iter().zip(param_types) {
            self.record_local_value_type(param.name_span, &ty);
            self.local_types.define(&param.name, ty);
        }
        self.check_value_expr(body);
        self.local_comptime_params.pop();
        self.local_types.pop();
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
                | RecordEntry::Spread { value, .. }
                | RecordEntry::DeleteComputed { key: value, .. }
                | RecordEntry::Element(value) => {
                    self.check_value_expr(value);
                }
                RecordEntry::FieldComputed { key, value, .. } => {
                    self.check_value_expr(key);
                    self.check_value_expr(value);
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
