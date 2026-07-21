use std::cmp::Ordering;

use super::annotations::call_callee_name;
use super::method_sets::{builtin_method_signature, resolve_builtin_operator_signature};
use super::*;

/// Desugar `value |> f(args)` to `f(value, args)` for checking and inference.
/// Keeping the written subexpressions preserves their source spans in call
/// diagnostics and inferred-type output.
pub(super) fn pipe_call_expr(value: &Expr, target: &Expr) -> Expr {
    let (callee, trailing_args) = match &ungroup_expr(target).kind {
        ExprKind::Call { callee, args } => ((**callee).clone(), args.clone()),
        _ => (target.clone(), Vec::new()),
    };
    let mut args = Vec::with_capacity(trailing_args.len() + 1);
    args.push(value.clone());
    args.extend(trailing_args);

    Expr {
        kind: ExprKind::Call {
            callee: Box::new(callee),
            args,
        },
        span: value.span.merge(target.span),
    }
}

impl<'a> Checker<'a> {
    /// Instantiate and fully resolve a top-level binding's inferred type, used by
    /// white-box synthesis tests. Production code consumes the generalized scheme
    /// from `infer_top_level` directly.
    #[cfg(test)]
    pub(crate) fn comptime_rhs_is_non_liftable_artifact(&self, name: &str) -> bool {
        self.comptime_artifacts.get(name).copied().unwrap_or(false)
    }

    #[cfg(test)]
    pub(crate) fn infer_top_level_value(&mut self, name: &str) -> Option<Type> {
        let scheme = self.infer_top_level(name)?;
        let (ty, _) = self.unifier.instantiate_scheme(&scheme);
        self.resolve_if_concrete(&ty)
    }

    pub(crate) fn infer_top_level_qualified_type_for_output(
        &mut self,
        name: &str,
    ) -> Option<QualifiedType> {
        let scheme = self.infer_top_level(name)?;
        if scheme.vars.is_empty() && scheme.row_vars.is_empty() {
            // Annotation-sourced schemes are mono even when they carry
            // `Type::Variable` binders (host-style generics). Those are already
            // export-ready; `resolve_if_concrete` would reject them.
            if crate::ty::type_contains_variable(&scheme.ty) {
                return Some(QualifiedType {
                    ty: self.normalize(&scheme.ty),
                    constraints: export_method_constraints(&scheme, &HashMap::new()),
                });
            }
            return self
                .resolve_if_concrete(&scheme.ty)
                .map(|ty| QualifiedType {
                    ty,
                    constraints: export_method_constraints(&scheme, &HashMap::new()),
                });
        }
        // Reify quantified schemes as Type::Variable form so module exports
        // carry host-style generics (`scheme_from_global` understands them).
        let names = scheme
            .vars
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, Type::Variable(export_generic_name(index))))
            .collect::<HashMap<_, _>>();
        let ty = crate::ty::map_type_with_rows(
            &scheme.ty,
            &mut |node| match node {
                Type::Meta(id) => names.get(id).cloned(),
                _ => None,
            },
            &mut |tail| match tail {
                RowTail::Var(id) if scheme.row_vars.contains(&id) => Some(Row {
                    entries: Vec::new(),
                    tail: RowTail::Open,
                }),
                RowTail::Closed | RowTail::Open | RowTail::Var(_) => None,
            },
        );
        Some(QualifiedType {
            ty,
            constraints: export_method_constraints(&scheme, &names),
        })
    }

    #[cfg(test)]
    pub(crate) fn infer_top_level_scheme(&mut self, name: &str) -> Option<TypeScheme> {
        self.infer_top_level(name)
    }

    pub(super) fn infer_local_value(&mut self, env: &TypeEnv, value: &Expr) -> Option<Type> {
        let ty = self.infer(env, value);
        self.resolve_if_concrete(&ty)
    }

    /// Fully resolve `ty`; keep it only when no metavariable remains, so a
    /// synthesized value type never leaks an unsolved meta into checking.
    pub(super) fn resolve_if_concrete(&self, ty: &Type) -> Option<Type> {
        let ty = self.normalize(&self.resolve_and_default(ty));
        is_resolved_value_type(&ty).then_some(ty)
    }

    pub(super) fn resolve_and_default(&self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        self.unifier.default_numerics(&resolved)
    }

    pub(super) fn infer_top_level(&mut self, name: &str) -> Option<TypeScheme> {
        if let Some(scheme) = self.memo.get(name).cloned() {
            return Some(scheme);
        }
        if self.in_progress.contains(name) {
            return Some(TypeScheme::mono(Type::Deferred));
        }

        let binding = self.bindings.get(name).and_then(|binding| *binding);
        let pattern_binding = self.pattern_bindings.get(name).copied();
        if binding.is_none() && pattern_binding.is_none() {
            return None;
        }
        self.in_progress.insert(name.to_owned());

        let scheme = if let Some(annotation) = self.clean_declared_annotation(name) {
            // Polymorphic annotations use `Type::Variable` binders; publish them
            // as quantified schemes so each use instantiates fresh metas.
            if crate::ty::type_contains_variable(&annotation) {
                self.declared_method_scheme(name, &annotation)
            } else {
                TypeScheme::mono(annotation)
            }
        } else if let Some(binding) = binding {
            let obligation_marker = self.method_obligation_marker();
            let ty = self.infer(&TypeEnv::new(), &binding.value);
            self.generalize_method_obligations(
                self.resolve_and_default(&ty),
                &[],
                &[],
                obligation_marker,
                Some(name),
            )
        } else if let Some(binding) = pattern_binding {
            let qualified_import =
                aven_parser::static_import_specifier(&binding.value).and_then(|specifier| {
                    let source = super::import_pattern_source_for_binder(&binding.pattern, name)?;
                    self.imports.qualified_export(&specifier, source).cloned()
                });
            if let Some(qualified) = qualified_import {
                scheme_from_qualified_type(
                    &qualified,
                    name,
                    binding.pattern.span,
                    &mut self.unifier,
                )
            } else {
                let obligation_marker = self.method_obligation_marker();
                let ty = self.infer(&TypeEnv::new(), &binding.value);
                let resolved = self.normalize(&self.resolve_and_default(&ty));
                let local_types = pattern_local_types(
                    self.pattern_type_context(),
                    &binding.pattern,
                    Some(&resolved),
                );
                let ty = local_types
                    .into_iter()
                    .find_map(|(binding_name, ty)| (binding_name == name).then_some(ty))
                    .and_then(local_value_type_as_type)
                    .unwrap_or(Type::Deferred);
                // Module exports reify polymorphic functions as `Type::Variable`
                // binders. Treat them like host-generic globals so each use
                // instantiates fresh metas.
                if crate::ty::type_contains_variable(&ty) {
                    super::scheme_from_global(&ty, &mut self.unifier)
                } else {
                    self.generalize_method_obligations(ty, &[], &[], obligation_marker, Some(name))
                }
            }
        } else {
            TypeScheme::mono(Type::Deferred)
        };

        self.in_progress.remove(name);
        self.memo.insert(name.to_owned(), scheme.clone());
        Some(scheme)
    }

    pub(super) fn infer_top_level_without_unbound_names(
        &mut self,
        name: &str,
    ) -> Option<TypeScheme> {
        let previous = self.report_unbound_names;
        self.report_unbound_names = false;
        let scheme = self.infer_top_level(name);
        self.report_unbound_names = previous;
        scheme
    }

    pub(super) fn clean_declared_annotation(&mut self, name: &str) -> Option<Type> {
        let annotation = *self.annotations.get(name)?;
        let mut checker = self.fork_annotation_checker();
        let lowering = checker.lower_annotation_with_diagnostics(annotation);
        if !lowering.diagnostics.is_empty() {
            return None;
        }

        let ty = checker.normalize(&lowering.ty);
        self.comptime_specializations = checker.comptime_specializations;
        self.recursive_type_unfoldings = checker.recursive_type_unfoldings;
        self.unifier
            .set_recursive_type_unfoldings(self.recursive_type_unfoldings.clone());
        Some(ty)
    }

    pub(super) fn infer(&mut self, env: &TypeEnv, expr: &Expr) -> Type {
        let ty = match &expr.kind {
            ExprKind::Literal(
                literal @ (Literal::Bool(_) | Literal::Number(_) | Literal::String(_)),
            ) => self.open_literal_variant(literal),
            ExprKind::Undefined => named_builtin("Undefined"),
            ExprKind::Null => named_builtin("Null"),
            ExprKind::Tag(name) => Type::Variant(Row {
                entries: vec![RowEntry::Tag {
                    name: name.clone(),
                    payload: Vec::new(),
                }],
                tail: RowTail::Closed,
            }),
            ExprKind::ComptimeName(name) => self.infer_name_reference(env, name, expr.span),
            ExprKind::Group(inner) => self.infer(env, inner),
            ExprKind::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.infer(env, element))
                    .collect(),
            ),
            ExprKind::Array(entries) => self.infer_array(env, entries),
            ExprKind::Set(entries) => self.infer_set(env, entries),
            ExprKind::Record(entries) => {
                if let Some(shape) = literal_record_value(entries, expr.span) {
                    let mut fields = Vec::new();
                    for field in &shape.fields {
                        let ty = match field.value {
                            Some(value) => self.infer(env, value),
                            None => self.infer_name_reference(env, field.name, field.name_span),
                        };
                        fields.push(RowEntry::Field {
                            name: field.name.to_owned(),
                            ty,
                        });
                    }
                    Type::Record(Row {
                        entries: fields,
                        tail: RowTail::Closed,
                    })
                } else {
                    self.infer_record_entries(env, entries)
                }
            }
            ExprKind::Name(name) => self.infer_name_reference(env, name, expr.span),
            ExprKind::Lambda {
                params,
                return_annotation,
                requirements,
                body,
            } => self.infer_lambda(
                env,
                params,
                return_annotation.as_deref(),
                requirements,
                body,
            ),
            ExprKind::Call { callee, args } => self.infer_call(env, callee, args),
            ExprKind::Index { callee, args } => self.infer_value_index(env, callee, args),
            ExprKind::FieldAccess {
                receiver,
                field,
                field_span,
                null_safe,
            } => self.infer_field_access(env, receiver, field, *null_safe, *field_span),
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            } if operator == "|" => self.infer_set_union(env, expr),
            ExprKind::Binary {
                left,
                operator,
                operator_span,
                right,
            } => self.infer_binary(env, left, operator, *operator_span, right),
            ExprKind::Unary {
                operator, value, ..
            } => self.infer_unary(env, operator, value),
            ExprKind::Block(items) => self.infer_block(env, items),
            ExprKind::Match { subject, arms, .. } => self.infer_match(env, subject, arms),
            ExprKind::Interpolation(segments) => self.infer_interpolation(env, segments),
            ExprKind::Propagate {
                value,
                operator_span,
                mode,
            } => self.infer_propagate(env, value, *operator_span, *mode),
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::PrimitiveFamily { .. }
            | ExprKind::Optional(_)
            | ExprKind::Nullable(_)
            | ExprKind::NonNull(_)
            | ExprKind::Arrow { .. } => Type::Deferred,
        };
        self.record_expr_type(expr.span, &ty);
        ty
    }

    pub(super) fn open_literal_variant(&mut self, literal: &Literal) -> Type {
        Type::Variant(Row {
            entries: vec![RowEntry::Literal {
                value: literal.clone(),
            }],
            tail: RowTail::Var(self.unifier.fresh_row_var()),
        })
    }

    /// Infer `value?!` / `value?^`. Both unwrap a `Result(ok, err)` to its `ok`
    /// branch. `?^` additionally contributes the error type to the current
    /// lambda-body propagation context; top-level `?^` has no active context and
    /// keeps the old "unwrap to ok" typing behavior.
    pub(super) fn infer_propagate(
        &mut self,
        env: &TypeEnv,
        value: &Expr,
        operator_span: Span,
        mode: PropagationMode,
    ) -> Type {
        let inferred = self.infer(env, value);
        let resolved = self.normalize(&self.resolve_and_default(&inferred));
        if let Some((ok_ty, err_ty)) = result_type_args(&resolved) {
            if mode == PropagationMode::ReturnError {
                self.record_propagation_site(operator_span, err_ty.clone());
            }
            ok_ty.clone()
        } else {
            self.report_propagate_not_result_if_concrete(&resolved, value.span);
            Type::Deferred
        }
    }

    pub(super) fn infer_interpolation(
        &mut self,
        env: &TypeEnv,
        segments: &[InterpolationSegment],
    ) -> Type {
        for segment in segments {
            if let InterpolationSegment::Expr(expr) = segment {
                self.infer(env, expr);
            }
        }

        named_builtin("Text")
    }

    pub(super) fn infer_binary(
        &mut self,
        env: &TypeEnv,
        left: &Expr,
        operator: &str,
        operator_span: Span,
        right: &Expr,
    ) -> Type {
        if self.module_role == ModuleRole::Dependency && is_custom_operator_token(operator) {
            self.infer(env, left);
            self.infer(env, right);
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "custom operator `{operator}` cannot be used as bare infix in a dependency"
                ))
                .with_code(codes::parse::CUSTOM_INFIX_NOT_ROOT)
                .with_label(Label::primary(
                    operator_span,
                    "bare custom infix is allowed only in the compilation entry",
                ))
                .with_note(format!(
                    "rewrite this use as `left.{operator}(right)`, or move the expression into the designated entry"
                )),
            );
            return Type::Deferred;
        }

        if operator == "|>" {
            let call = pipe_call_expr(left, right);
            let ExprKind::Call { callee, args } = &call.kind else {
                return Type::Deferred;
            };
            return self.infer_call(env, callee, args);
        }

        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let left_type = self.infer(env, left);
        let right_type = self.infer(env, right);
        let divisor_context = IntegerDivisorContext {
            span: right.span,
            literal_is_zero: static_integer_literal_is_zero(right),
            right_type: right_type.clone(),
            parameter_index: self.integer_divisor_parameter_index(right),
        };
        let left_owner = self.normalize(&self.resolve_and_default(&left_type));

        if is_method_operator(operator)
            && self.is_named_family_owner(&left_owner)
            && let Some(signature) = self.exact_method_signature(&left_owner, operator)
        {
            self.push_method_obligations_at(signature.predicates, operator_span);
            if let [param] = signature.params.as_slice() {
                self.check_call_arg_against_param(param, right);
            }
            self.simplify_method_obligations(false);
            self.maybe_report_integer_divisor(operator, &left_type, &divisor_context);
            return self.resolve_and_default(&signature.result);
        }

        let right_owner = self.normalize(&self.resolve_and_default(&right_type));
        if !self.is_named_family_owner(&left_owner)
            && self.primitive_family_base_view(&right_owner).is_some()
            && !right.span.is_empty()
        {
            self.primitive_family_coercions
                .insert(right.span, PrimitiveFamilyCoercion::Widen);
        }

        // Integer `/` and `%` need a statically non-zero divisor. The check
        // consults both syntax (literal tokens) and the divisor's resolved type
        // (closed / inferred-open number literal unions).
        self.maybe_report_integer_divisor(operator, &left_type, &divisor_context);

        if let Some(literal) = self.fold_binary_literal(operator, &left_type, &right_type) {
            return self.open_literal_variant(&literal);
        }

        // Functions have no equality; the evaluator throws on `f == g`. Report
        // statically and still type the comparison as Bool so downstream
        // checking stays precise instead of deferring.
        if matches!(operator, "==" | "!=") {
            let function_operand = [(&left_type, left), (&right_type, right)]
                .into_iter()
                .find(|(ty, _)| matches!(self.unifier.resolve(ty), Type::Function { .. }));
            if let Some((_, operand)) = function_operand {
                self.diagnostics.push(
                    Diagnostic::error("functions are not comparable")
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(
                            operand.span,
                            "this operand is a function",
                        ))
                        .with_note(
                            "compare the results of calling the functions instead of the functions themselves",
                        ),
                );
                return named_builtin("Bool");
            }
        }

        let result = if let Some(result) = self.infer_binary_type(operator, &left_type, &right_type)
        {
            result
        } else if is_method_operator(operator)
            && matches!(
                self.normalize(&self.unifier.resolve(&left_type)),
                Type::Meta(_) | Type::Variable(_) | Type::Deferred
            )
        {
            let unresolved_deferred = matches!(
                self.normalize(&self.unifier.resolve(&left_type)),
                Type::Deferred
            );
            let infer_homogeneous =
                matches!((&left_type, &right_type), (Type::Meta(_), Type::Meta(_)))
                    && !self.inline_annotation_names_meta(&left_type)
                    && !self.inline_annotation_names_meta(&right_type);
            let obligation_right = if infer_homogeneous {
                let _ = self.unifier.unify(&left_type, &right_type);
                left_type.clone()
            } else {
                right_type.clone()
            };
            let result = if unresolved_deferred {
                Type::Deferred
            } else if matches!(operator, "<" | "<=" | ">" | ">=") {
                named_builtin("Bool")
            } else if infer_homogeneous {
                left_type.clone()
            } else {
                self.unifier.fresh()
            };
            self.add_operator_obligation(
                left_type.clone(),
                operator,
                obligation_right,
                result.clone(),
                operator_span,
                matches!(operator, "/" | "%").then_some(divisor_context.clone()),
            );
            result
        } else {
            let left_type = self.normalize(&self.resolve_and_default(&left_type));
            let right_type = self.normalize(&self.resolve_and_default(&right_type));
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            if is_resolved_operator_operand(&left_type) && is_resolved_operator_operand(&right_type)
            {
                if matches!(operator, "==" | "!=")
                    && let (Type::Record(left_row), Type::Record(right_row)) =
                        (&left_type, &right_type)
                {
                    let compatibility = self.record_equality_compatibility(left_row, right_row);
                    if compatibility != EqualityCompatibility::Mismatched
                        || self.expr_references_unresolved_comptime_param(left)
                        || self.expr_references_unresolved_comptime_param(right)
                    {
                        return named_builtin("Bool");
                    }
                }
                if operator == "??" {
                    self.report_null_coalesce_mismatch(&left_type, &right_type, right.span);
                } else {
                    self.report_invalid_operator_operands(
                        operator,
                        &left_type,
                        &right_type,
                        left.span.merge(right.span),
                    );
                }
                self.invalid_binary_result_type(operator, &left_type, &right_type)
            } else {
                Type::Deferred
            }
        };

        // After any snapshot restore above, so this warning is not discarded.
        if operator == "??" {
            let left_resolved = self.normalize(&self.unifier.resolve(&left_type));
            self.maybe_report_coalesce_never_empty(left, &left_resolved);
        }

        // Operator resolution can constrain an initially-unresolved operand to
        // Int. Recheck after those unifications; diagnostics are deduplicated.
        self.maybe_report_integer_divisor(operator, &left_type, &divisor_context);

        result
    }

    pub(super) fn maybe_report_integer_divisor(
        &mut self,
        operator: &str,
        left_type: &Type,
        context: &IntegerDivisorContext,
    ) {
        if !matches!(operator, "/" | "%")
            || !self.operator_operand_resolves_to_int(left_type)
            || !self.operator_operand_resolves_to_int(&context.right_type)
        {
            return;
        }

        let checked_method = if operator == "/" { "div" } else { "mod" };
        // Prefer the syntactic path (plain / grouped / negated literal tokens)
        // so positions that have not yet formed a literal type still accept.
        // Fall back to the divisor's resolved type: a number literal variant
        // whose known members are all non-zero integers is also legal.
        let (is_zero, zero_from_syntax) = match context.literal_is_zero {
            Some(is_zero) => (Some(is_zero), true),
            None => {
                let right_resolved = self.normalize(&self.resolve_and_default(&context.right_type));
                (static_integer_divisor_type_is_zero(&right_resolved), false)
            }
        };
        let diagnostic = match is_zero {
            Some(true) => {
                let label = if zero_from_syntax {
                    "this integer literal is zero"
                } else {
                    "this divisor is zero"
                };
                Diagnostic::error("division by zero")
                    .with_code(codes::ty::DIVISION_BY_ZERO)
                    .with_label(Label::primary(context.span, label))
                    .with_note(format!(
                        "use checked `x.{checked_method}(n)` (`?Int`) when the divisor may be zero"
                    ))
            }
            Some(false) => return,
            None => Diagnostic::error("divisor is not statically known to be non-zero")
                .with_code(codes::ty::DIVISOR_NOT_STATIC)
                .with_label(Label::primary(
                    context.span,
                    "this divisor is not a non-zero integer literal",
                ))
                .with_note(format!(
                    "use checked `x.{checked_method}(n)` (`?Int`) or convert to `Float`"
                )),
        };
        self.push_unique_diagnostic(diagnostic);
    }

    fn operator_operand_resolves_to_int(&self, ty: &Type) -> bool {
        let ty = self.normalize(&self.resolve_and_default(ty));
        matches!(&ty, Type::Named(name) if name == "Int")
            || self
                .primitive_family_base_view(&ty)
                .is_some_and(|base| matches!(base, Type::Named(name) if name == "Int"))
            || matches!(&ty, Type::Variant(row)
                if literal_variant_base(row) == Some(LiteralBase::Number)
                    && !row.entries.iter().any(|entry| matches!(entry,
                        RowEntry::Literal { value: Literal::Number(number) }
                            if is_float_literal_text(number))))
    }

    fn integer_divisor_parameter_index(&self, right: &Expr) -> Option<usize> {
        let ExprKind::Name(name) = &ungroup_expr(right).kind else {
            return None;
        };
        self.lambda_parameter_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    pub(super) fn infer_binary_type(
        &mut self,
        operator: &str,
        left: &Type,
        right: &Type,
    ) -> Option<Type> {
        let owner = self.normalize(&self.unifier.resolve(left));
        if self.is_named_family_owner(&owner)
            && let Some(signature) = self.exact_method_signature(&owner, operator)
            && let [param] = signature.params.as_slice()
            && self.unifier.unify(param, right).is_ok()
        {
            return Some(signature.result);
        }
        match operator {
            "+" => self
                .infer_numeric_binary_type(operator, left, right)
                .or_else(|| {
                    let owner = self.infer_same_named_binary_type(left, right, "Text")?;
                    builtin_method_signature(&owner, operator).map(|signature| signature.result)
                }),
            "-" | "*" | "/" | "%" | "^" | "<" | "<=" | ">" | ">=" => {
                self.infer_numeric_binary_type(operator, left, right)
            }
            "==" | "!=" => self.infer_equality_type(left, right),
            "&&" | "||" => self.infer_same_named_binary_type(left, right, "Bool"),
            "??" => self.infer_null_coalesce_type(left, right),
            _ => None,
        }
    }

    pub(super) fn infer_null_coalesce_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        let left = self.normalize(&self.unifier.resolve(left));
        let (_, payload) = peel_empty_values(&left);
        let payload = payload.clone();
        let right = self.normalize(&self.unifier.resolve(right));

        if self.type_fits_boundary_without_reporting(&payload, &right) {
            return Some(payload);
        }
        if self.type_fits_boundary_without_reporting(&right, &payload) {
            return Some(right);
        }

        None
    }

    fn report_null_coalesce_mismatch(&mut self, left: &Type, right: &Type, span: Span) {
        let (_, payload) = peel_empty_values(left);
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected `{}`, found `{}`",
                display_inferred_type(payload).render(),
                display_inferred_type(right).render()
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(span, "this fallback has the wrong type"))
            .with_note("the `??` fallback must match the value it replaces"),
        );
    }

    /// Warn when `left ?? fallback` has a left operand that can never be empty,
    /// so the fallback is dead. Conservative: unresolved / open / variable /
    /// meta left types stay silent (false negatives are preferred).
    fn maybe_report_coalesce_never_empty(&mut self, left: &Expr, left_type: &Type) {
        if self.expr_references_unresolved_comptime_param(left) {
            return;
        }
        if !is_resolved_operator_operand(left_type) {
            return;
        }
        if coalesce_left_can_be_empty(left_type) {
            return;
        }

        let rendered = display_inferred_type(left_type).render();
        self.diagnostics.push(
            Diagnostic::warning("left operand of `??` is never empty; the fallback is dead")
                .with_code(codes::ty::COALESCE_NEVER_EMPTY)
                .with_label(Label::primary(left.span, "this value is never empty"))
                .with_note(format!("type `{rendered}` cannot be `null` or `undefined`")),
        );
    }

    fn report_invalid_operator_operands(
        &mut self,
        operator: &str,
        left: &Type,
        right: &Type,
        span: Span,
    ) {
        let left = operator_operand_type(left);
        let right = operator_operand_type(right);
        let attempted_right_fallback =
            builtin_method_signature(&Type::Named(left.clone()), operator).is_none()
                && builtin_method_signature(&Type::Named(right.clone()), operator).is_some();
        let mut diagnostic = Diagnostic::error(format!(
            "operator `{operator}` is not defined for `{left}` and `{right}`"
        ))
        .with_code(codes::ty::INVALID_OPERATOR_OPERANDS)
        .with_label(Label::primary(
            span,
            "these operand types do not support this operator",
        ))
        .with_note(operator_operand_note(operator));
        if attempted_right_fallback {
            diagnostic = diagnostic.with_note(format!(
                "dispatch is left-biased: `{left}` is the sole method owner; Aven does not fall back to `{right}.{operator}`"
            ))
            .with_note(
                "reverse the operands only when doing so preserves the operation's intended semantics",
            );
        }
        self.diagnostics.push(diagnostic);
    }

    fn report_invalid_unary_operator_operand(&mut self, operator: &str, value: &Type, span: Span) {
        let value = operator_operand_type(value);
        self.diagnostics.push(
            Diagnostic::error(format!(
                "operator `{operator}` is not defined for `{value}`"
            ))
            .with_code(codes::ty::INVALID_OPERATOR_OPERANDS)
            .with_label(Label::primary(
                span,
                "this operand type does not support this operator",
            ))
            .with_note(operator_operand_note(operator)),
        );
    }

    fn invalid_binary_result_type(&self, operator: &str, left: &Type, right: &Type) -> Type {
        if matches!(
            operator,
            "<" | "<=" | ">" | ">=" | "==" | "!=" | "&&" | "||"
        ) {
            return named_builtin("Bool");
        }
        if matches!(operator, "+" | "-" | "*" | "/" | "%" | "^") {
            if binary_operand_is_float(left) || binary_operand_is_float(right) {
                return named_builtin("Float");
            }
            if binary_operand_is_numeric(left) || binary_operand_is_numeric(right) {
                return named_builtin("Int");
            }
            if operator == "+" && (binary_operand_is_text(left) || binary_operand_is_text(right)) {
                return named_builtin("Text");
            }
        }
        Type::Deferred
    }

    pub(super) fn fold_binary_literal(
        &self,
        operator: &str,
        left: &Type,
        right: &Type,
    ) -> Option<Literal> {
        let resolved_left = self.unifier.resolve(left);
        let resolved_right = self.unifier.resolve(right);
        let left = singleton_literal_type(&resolved_left)?;
        let right = singleton_literal_type(&resolved_right)?;
        fold_binary_literals(operator, left, right)
    }

    pub(super) fn infer_numeric_binary_type(
        &mut self,
        operator: &str,
        left: &Type,
        right: &Type,
    ) -> Option<Type> {
        let left = self.widen_numeric_operand(left);
        let right = self.widen_numeric_operand(right);

        if is_meta_type(&left)
            && is_meta_type(&right)
            && (self.unifier.is_numeric_meta(&left) || self.unifier.is_numeric_meta(&right))
        {
            self.unifier.unify(&left, &right).ok()?;
            let owner = self.unifier.resolve(&left);
            let int = named_builtin("Int");
            let signature = builtin_method_signature(&int, operator)?;
            return Some(if signature.result == int {
                owner
            } else {
                signature.result
            });
        }

        match (numeric_type_name(&left), numeric_type_name(&right)) {
            (Some(_), Some(_)) => resolve_builtin_operator_signature(&left, operator, &right)
                .map(|signature| signature.result),
            (None, Some(right_name)) if is_meta_type(&left) => {
                let right = named_builtin(right_name);
                self.unifier.unify(&left, &right).ok()?;
                resolve_builtin_operator_signature(&right, operator, &right)
                    .map(|signature| signature.result)
            }
            (Some(left_name), None) if is_meta_type(&right) => {
                let left = named_builtin(left_name);
                self.unifier.unify(&right, &left).ok()?;
                resolve_builtin_operator_signature(&left, operator, &left)
                    .map(|signature| signature.result)
            }
            _ => None,
        }
    }

    pub(super) fn infer_same_named_binary_type(
        &mut self,
        left: &Type,
        right: &Type,
        name: &'static str,
    ) -> Option<Type> {
        let left = self.widen_same_named_operand(left, name);
        let right = self.widen_same_named_operand(right, name);

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

    pub(super) fn widen_numeric_operand(&mut self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        if let Some(base) = self.primitive_family_base_view(&resolved) {
            return base;
        }
        if let Type::Variant(row) = &resolved
            && literal_variant_base(row) == Some(LiteralBase::Number)
        {
            if let Some(Literal::Number(number)) = singleton_literal_type(&resolved)
                && is_float_literal_text(number)
            {
                return named_builtin("Float");
            }
            return self.unifier.fresh_numeric();
        }

        resolved
    }

    pub(super) fn widen_same_named_operand(&mut self, ty: &Type, name: &'static str) -> Type {
        let resolved = self.unifier.resolve(ty);
        if let Some(base) = self.primitive_family_base_view(&resolved) {
            return base;
        }
        if name == "Text"
            && let Type::Variant(row) = &resolved
            && literal_variant_base(row) == Some(LiteralBase::Text)
        {
            return named_builtin("Text");
        }
        if name == "Bool"
            && let Type::Variant(row) = &resolved
            && literal_variant_base(row) == Some(LiteralBase::Bool)
        {
            return named_builtin("Bool");
        }

        resolved
    }

    pub(super) fn infer_equality_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        let left = self.widen_equality_operand(left);
        let right = self.widen_equality_operand(right);

        if let (Type::Record(left), Type::Record(right)) = (&left, &right) {
            return (self.record_equality_compatibility(left, right)
                != EqualityCompatibility::Mismatched)
                .then(|| named_builtin("Bool"));
        }

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
            let snapshot = self.unifier.snapshot();
            if self.unifier.unify(&left, &right).is_ok() {
                return Some(named_builtin("Bool"));
            }
            self.unifier.restore(snapshot);
            if left.render() == right.render() {
                return Some(named_builtin("Bool"));
            }
            if self.type_fits_boundary_without_reporting(&left, &right)
                || self.type_fits_boundary_without_reporting(&right, &left)
            {
                return Some(named_builtin("Bool"));
            }
        }

        None
    }

    pub(super) fn widen_equality_operand(&mut self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        if let Type::Variant(row) = &resolved {
            match literal_variant_base(row) {
                Some(LiteralBase::Bool) => return named_builtin("Bool"),
                Some(LiteralBase::Text) => return named_builtin("Text"),
                Some(LiteralBase::Number) => return self.widen_numeric_operand(&resolved),
                None => {}
            }
        }

        if let Type::Apply { callee, args } = &resolved {
            return Type::Apply {
                callee: callee.clone(),
                args: args
                    .iter()
                    .map(|arg| self.widen_equality_operand(arg))
                    .collect(),
            };
        }

        resolved
    }

    fn record_equality_compatibility(&mut self, left: &Row, right: &Row) -> EqualityCompatibility {
        if left.tail != RowTail::Closed || right.tail != RowTail::Closed {
            return EqualityCompatibility::Unknown;
        }

        if !left
            .entries
            .iter()
            .chain(&right.entries)
            .all(|entry| matches!(entry, RowEntry::Field { .. }))
        {
            return EqualityCompatibility::Unknown;
        }

        let mut compatibility = EqualityCompatibility::Comparable;
        for left_entry in &left.entries {
            let RowEntry::Field {
                name,
                ty: left_type,
            } = left_entry
            else {
                return EqualityCompatibility::Unknown;
            };
            let Some(right_type) = row_field_type(right, name) else {
                // An optional-key field may be absent at runtime, so the
                // records could still be equal; a required field cannot.
                if self.field_type_admits_absence(left_type) {
                    continue;
                }
                return EqualityCompatibility::Mismatched;
            };

            compatibility = compatibility.and(self.equality_compatibility(left_type, right_type));
            if compatibility == EqualityCompatibility::Mismatched {
                return compatibility;
            }
        }
        for right_entry in &right.entries {
            let RowEntry::Field {
                name,
                ty: right_type,
            } = right_entry
            else {
                return EqualityCompatibility::Unknown;
            };
            if row_field_type(left, name).is_none() && !self.field_type_admits_absence(right_type) {
                return EqualityCompatibility::Mismatched;
            }
        }
        compatibility
    }

    /// Whether a field of this type may be absent from the record at runtime
    /// (optional-key fields: `?T`).
    fn field_type_admits_absence(&mut self, ty: &Type) -> bool {
        matches!(self.normalize(&self.unifier.resolve(ty)), Type::Optional(_))
    }

    fn equality_compatibility(&mut self, left: &Type, right: &Type) -> EqualityCompatibility {
        let left = self.unifier.resolve(left);
        let right = self.unifier.resolve(right);
        if let (Type::Recursive(left), Type::Recursive(right)) = (&left, &right) {
            return if left == right {
                EqualityCompatibility::Comparable
            } else {
                EqualityCompatibility::Mismatched
            };
        }
        let left = self.normalize_for_demand(&left);
        let right = self.normalize_for_demand(&right);
        let (_, left) = peel_empty_values(&left);
        let (_, right) = peel_empty_values(&right);

        if let (Some(left_kind), Some(right_kind)) =
            (equality_base_kind(left), equality_base_kind(right))
        {
            return if left_kind == right_kind {
                EqualityCompatibility::Comparable
            } else {
                EqualityCompatibility::Mismatched
            };
        }

        if !is_concrete_type(left) || !is_concrete_type(right) {
            return EqualityCompatibility::Unknown;
        }

        match (left, right) {
            (Type::Record(left), Type::Record(right)) => {
                self.record_equality_compatibility(left, right)
            }
            (Type::Record(_), _) | (_, Type::Record(_)) => EqualityCompatibility::Mismatched,
            (Type::SlotRecord { .. }, Type::SlotRecord { .. }) => EqualityCompatibility::Unknown,
            (Type::SlotRecord { .. }, _) | (_, Type::SlotRecord { .. }) => {
                EqualityCompatibility::Mismatched
            }
            (Type::Tuple(left), Type::Tuple(right)) => {
                equality_sequence_compatibility(left, right, |left, right| {
                    self.equality_compatibility(left, right)
                })
            }
            (Type::Tuple(_), _) | (_, Type::Tuple(_)) => EqualityCompatibility::Mismatched,
            (
                Type::Apply {
                    callee: left_callee,
                    args: left_args,
                },
                Type::Apply {
                    callee: right_callee,
                    args: right_args,
                },
            ) if is_array_constructor(left_callee) && is_array_constructor(right_callee) => {
                match (left_args.as_slice(), right_args.as_slice()) {
                    ([left], [right]) => self.equality_compatibility(left, right),
                    _ => EqualityCompatibility::Unknown,
                }
            }
            (Type::Apply { callee, .. }, _) if is_array_constructor(callee) => {
                EqualityCompatibility::Mismatched
            }
            (_, Type::Apply { callee, .. }) if is_array_constructor(callee) => {
                EqualityCompatibility::Mismatched
            }
            (Type::Variant(_), _)
            | (_, Type::Variant(_))
            | (Type::Function { .. }, _)
            | (_, Type::Function { .. })
            | (Type::Apply { .. }, _)
            | (_, Type::Apply { .. }) => EqualityCompatibility::Unknown,
            (Type::Named(left), Type::Named(right)) => {
                if left == right {
                    EqualityCompatibility::Comparable
                } else {
                    EqualityCompatibility::Mismatched
                }
            }
            (Type::Recursive(left), Type::Recursive(right)) => {
                if left == right {
                    EqualityCompatibility::Comparable
                } else {
                    EqualityCompatibility::Mismatched
                }
            }
            (Type::Recursive(_), _) | (_, Type::Recursive(_)) => EqualityCompatibility::Unknown,
            (Type::Deferred | Type::Variable(_) | Type::Meta(_), _)
            | (_, Type::Deferred | Type::Variable(_) | Type::Meta(_)) => {
                EqualityCompatibility::Unknown
            }
            (Type::Optional(_) | Type::Nullable(_), _)
            | (_, Type::Optional(_) | Type::Nullable(_)) => {
                unreachable!("empty-value wrappers were peeled")
            }
        }
    }

    pub(super) fn infer_unary(&mut self, env: &TypeEnv, operator: &str, value: &Expr) -> Type {
        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let value_type = self.infer(env, value);

        if let Some(literal) = self.fold_unary_literal(operator, &value_type) {
            return self.open_literal_variant(&literal);
        }

        let result = match operator {
            "-" => self.infer_numeric_unary_type(&value_type),
            "!" => self.infer_same_named_unary_type(&value_type, "Bool"),
            _ => None,
        };

        if let Some(result) = result {
            result
        } else {
            let value_type = self.normalize(&self.resolve_and_default(&value_type));
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            if is_resolved_operator_operand(&value_type) {
                self.report_invalid_unary_operator_operand(operator, &value_type, value.span);
                match operator {
                    "-" if binary_operand_is_float(&value_type) => named_builtin("Float"),
                    "-" if binary_operand_is_numeric(&value_type) => named_builtin("Int"),
                    _ => Type::Deferred,
                }
            } else {
                Type::Deferred
            }
        }
    }

    pub(super) fn infer_numeric_unary_type(&mut self, value: &Type) -> Option<Type> {
        let value = self.widen_numeric_operand(value);
        if let Some(name) = numeric_type_name(&value) {
            return Some(named_builtin(name));
        }
        self.unifier.is_numeric_meta(&value).then_some(value)
    }

    pub(super) fn infer_same_named_unary_type(
        &mut self,
        value: &Type,
        name: &'static str,
    ) -> Option<Type> {
        let value = self.widen_same_named_operand(value, name);
        match named_type_name(&value) {
            Some(value_name) if value_name == name => Some(named_builtin(name)),
            None if is_meta_type(&value) => self
                .unifier
                .unify(&value, &named_builtin(name))
                .ok()
                .map(|()| named_builtin(name)),
            _ => None,
        }
    }

    pub(super) fn fold_unary_literal(&self, operator: &str, value: &Type) -> Option<Literal> {
        let resolved = self.unifier.resolve(value);
        let value = singleton_literal_type(&resolved)?;
        fold_unary_literal(operator, value)
    }

    pub(super) fn infer_lambda(
        &mut self,
        env: &TypeEnv,
        params: &[Param],
        return_annotation: Option<&Expr>,
        requirements: &[Requirement],
        body: &Expr,
    ) -> Type {
        let mut next_env = env.clone();
        let mut param_types = Vec::new();
        self.push_inline_lambda_type_var_scope();
        // Defaults are trailing (per D1): the required-arity is the count of
        // leading params without a default.
        let mut required = params.len();

        for (index, param) in params.iter().enumerate() {
            let ty = if let Some(annotation) = &param.annotation {
                self.lower_inline_lambda_annotation_for_inference(annotation)
            } else {
                self.unifier.fresh()
            };

            if let Some(default) = &param.default {
                required = required.min(index);
                // Check the default against the param's type. An annotated
                // param's default must match the annotation (a normal `type.*`
                // diagnostic on the default); an unannotated param infers its
                // type from the default. The default cannot reference the param
                // itself, so use the env without it bound.
                if param.annotation.is_some() {
                    self.check_value_against(&ty, default);
                } else {
                    let inferred = self.infer(&next_env, default);
                    let _ = self.unifier.unify(&ty, &inferred);
                }
            }

            next_env.insert(param.name.clone(), LocalValueType::Known(ty.clone()));
            param_types.push(ty);
        }

        let assumptions = self.requirement_predicates(requirements);
        let obligation_marker = self.method_obligation_marker();
        self.push_method_assumptions(assumptions.clone());
        self.lambda_parameter_scopes.push(
            params
                .iter()
                .enumerate()
                .map(|(index, param)| (param.name.clone(), index))
                .collect(),
        );
        self.push_local_comptime_param_scope(params);
        self.propagation_contexts
            .push(PropagationContext::default());
        let body_type = self.infer(&next_env, body);
        let propagation = self.pop_propagation_context();
        self.local_comptime_params.pop();
        self.lambda_parameter_scopes.pop();
        let body_type = self.apply_propagation_context_to_body_type(body, body_type, &propagation);
        let result_type = if let Some(annotation) = return_annotation {
            // Trust the return annotation as the lambda's result type so
            // downstream uses stay precise. Body-vs-annotation mismatch is
            // reported on the value-check path (`check_lambda_value_expr`), not
            // here, to avoid double-reporting when both paths run. Incomplete
            // body types still defer silently.
            let expected = self.lower_inline_lambda_annotation_for_inference(annotation);
            self.report_propagated_errors_against_annotation(&expected, &propagation);
            if self.inferred_return_type_fits_annotation(&expected, &body_type) {
                expected
            } else {
                let resolved_body = self.normalize(&self.resolve_and_default(&body_type));
                let resolved_expected = self.normalize(&self.resolve_and_default(&expected));
                // A fully resolved return annotation is authoritative for the
                // published function type: value-check validates the body. This
                // matters for bodies that only typecheck under an expected type
                // (e.g. direct slot-record initializers), which inference alone
                // cannot shape.
                if is_resolved_value_type(&resolved_body)
                    || is_resolved_value_type(&resolved_expected)
                {
                    expected
                } else {
                    Type::Deferred
                }
            }
        } else {
            body_type
        };

        let lambda_type = Type::Function {
            params: param_types,
            result: Box::new(result_type),
            required,
        };
        self.finalize_lambda_requirements(obligation_marker, requirements, assumptions);
        self.pop_method_assumptions();
        self.pop_inline_lambda_type_var_scope();
        lambda_type
    }

    fn inline_annotation_names_meta(&self, ty: &Type) -> bool {
        self.inline_lambda_type_var_scopes
            .iter()
            .any(|scope| scope.values().any(|candidate| candidate == ty))
    }

    pub(super) fn pop_propagation_context(&mut self) -> PropagationContext {
        self.propagation_contexts.pop().unwrap_or_default()
    }

    pub(super) fn record_propagation_site(&mut self, span: Span, error_ty: Type) {
        let Some(context) = self.propagation_contexts.last_mut() else {
            return;
        };
        if context.sites.iter().any(|site| site.span == span) {
            return;
        }

        context.sites.push(PropagationSite { span, error_ty });
    }

    pub(super) fn infer_body_type_for_propagation_check(&mut self, body: &Expr) -> Type {
        let diagnostics_len = self.diagnostics.len();
        let reported_unbound_name_spans = self.reported_unbound_name_spans.clone();
        let inferred_types_len = self.inferred_types.len();
        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, body);
        let body_type = self.resolve_and_default(&inferred);
        self.diagnostics.truncate(diagnostics_len);
        self.reported_unbound_name_spans = reported_unbound_name_spans;
        self.inferred_types.truncate(inferred_types_len);
        body_type
    }

    pub(super) fn apply_propagation_context_to_body_type(
        &mut self,
        body: &Expr,
        body_type: Type,
        propagation: &PropagationContext,
    ) -> Type {
        if propagation.sites.is_empty() {
            return body_type;
        }

        let Some(body_result) = self.propagation_body_result_type(body, &body_type) else {
            let resolved = self.normalize(&self.resolve_and_default(&body_type));
            if is_resolved_value_type(&resolved) {
                self.report_propagate_needs_result(final_result_span(body));
            }
            return body_type;
        };

        let Some((ok_ty, body_error_ty)) = result_type_args(&body_result) else {
            return body_result;
        };
        let ok_ty = ok_ty.clone();
        let body_error_ty = body_error_ty.clone();
        let error_ty = self.union_propagated_error_types(
            std::iter::once(body_error_ty)
                .chain(propagation.sites.iter().map(|site| site.error_ty.clone())),
        );
        result_type(ok_ty, error_ty)
    }

    pub(super) fn propagation_body_result_type(
        &self,
        body: &Expr,
        body_type: &Type,
    ) -> Option<Type> {
        let resolved = self.normalize(&self.resolve_and_default(body_type));
        if result_type_args(&resolved).is_some() {
            return Some(resolved);
        }

        self.final_result_constructor_type(body, &resolved)
    }

    pub(super) fn final_result_constructor_type(
        &self,
        body: &Expr,
        body_type: &Type,
    ) -> Option<Type> {
        let final_expr = final_value_expr(body)?;
        let ExprKind::Call { callee, args } = &ungroup_expr(final_expr).kind else {
            return None;
        };
        if args.len() != 1 {
            return None;
        }
        let ExprKind::Tag(tag) = &ungroup_expr(callee).kind else {
            return None;
        };
        let payload_ty = single_tag_payload_type(body_type, tag)?;

        match tag.as_str() {
            "Ok" => Some(result_type(payload_ty, empty_variant_type())),
            "Err" => Some(result_type(Type::Deferred, payload_ty)),
            _ => None,
        }
    }

    pub(super) fn union_propagated_error_types(
        &mut self,
        types: impl IntoIterator<Item = Type>,
    ) -> Type {
        let types: Vec<_> = types
            .into_iter()
            .map(|ty| self.normalize(&self.resolve_and_default(&ty)))
            .filter(|ty| !is_empty_closed_variant(ty))
            .collect();
        let Some(first) = types.first().cloned() else {
            return empty_variant_type();
        };
        if types.iter().all(|ty| ty == &first) {
            return first;
        }

        if types.iter().all(|ty| matches!(ty, Type::Variant(_))) {
            let body_types: Vec<_> = types
                .into_iter()
                .map(|ty| MatchArmBodyType {
                    span: Span::new(0, 0),
                    ty,
                })
                .collect();
            return match self.union_match_variant_arm_body_types(&body_types) {
                Some(MatchArmCombination::Joined(ty)) => {
                    self.normalize(&self.resolve_and_default(&ty))
                }
                Some(MatchArmCombination::Conflict(_)) | None => Type::Deferred,
            };
        }

        types
            .into_iter()
            .skip(1)
            .try_fold(first, |combined, ty| {
                if self.type_fits_boundary_without_reporting(&combined, &ty) {
                    Some(combined)
                } else if self.type_fits_boundary_without_reporting(&ty, &combined) {
                    Some(ty)
                } else {
                    None
                }
            })
            .unwrap_or(Type::Deferred)
    }

    pub(super) fn report_propagated_errors_against_annotation(
        &mut self,
        expected: &Type,
        propagation: &PropagationContext,
    ) {
        if propagation.sites.is_empty() {
            return;
        }

        let expected = self.normalize(&self.resolve_and_default(expected));
        let Some((_, expected_error_ty)) = result_type_args(&expected) else {
            return;
        };
        let expected_error_ty = expected_error_ty.clone();
        for site in &propagation.sites {
            let actual = self.normalize(&self.resolve_and_default(&site.error_ty));
            self.check_type_against_type(&expected_error_ty, &actual, site.span);
        }
    }

    pub(super) fn inferred_return_type_fits_annotation(
        &mut self,
        expected: &Type,
        actual: &Type,
    ) -> bool {
        let expected = self.normalize(&self.resolve_and_default(expected));
        let actual = self.normalize(&self.resolve_and_default(actual));
        let (Some((expected_ok, expected_error)), Some((actual_ok, actual_error))) =
            (result_type_args(&expected), result_type_args(&actual))
        else {
            // A variant-row body (e.g. a bare `@Ok(1)` final expression) fits a
            // `Result` annotation by the same boundary rule the checking
            // direction uses; raw unification cannot equate the two shapes.
            if is_result_type(&expected) && matches!(actual, Type::Variant(_)) {
                return self.type_fits_boundary_without_reporting(&expected, &actual);
            }
            return self.unifier.unify(&actual, &expected).is_ok();
        };
        let expected_ok = expected_ok.clone();
        let expected_error = expected_error.clone();
        let actual_ok = actual_ok.clone();
        let actual_error = actual_error.clone();

        self.unifier.unify(&actual_ok, &expected_ok).is_ok()
            && self.type_fits_boundary_without_reporting(&expected_error, &actual_error)
    }

    pub(crate) fn type_fits_boundary_without_reporting(
        &mut self,
        expected: &Type,
        actual: &Type,
    ) -> bool {
        let unifier_snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        self.check_type_against_type(expected, actual, Span::new(0, 0));
        let accepted = self.diagnostics.len() == diagnostic_snapshot.diagnostics_len;
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        if !accepted {
            self.unifier.restore(unifier_snapshot);
        }
        accepted
    }

    pub(super) fn infer_field_access(
        &mut self,
        env: &TypeEnv,
        receiver: &Expr,
        field: &str,
        null_safe: bool,
        field_span: Span,
    ) -> Type {
        if let ExprKind::Name(name) | ExprKind::ComptimeName(name) = &ungroup_expr(receiver).kind
            && let Some(owner) = self.unbound_method_owner_name(name)
            && let Some(signature) = self.exact_method_signature(&Type::Named(owner.clone()), field)
        {
            self.push_method_obligations_at(signature.predicates, field_span);
            let mut params = Vec::with_capacity(signature.params.len() + 1);
            params.push(Type::Named(owner));
            params.extend(signature.params);
            return Type::Function {
                required: params.len(),
                params,
                result: Box::new(signature.result),
            };
        }

        // A static-carrying type name (`Map`, `Json`, ...) resolves the field
        // through the statics table rather than a namespace record.
        if let ExprKind::Name(name) | ExprKind::ComptimeName(name) = &ungroup_expr(receiver).kind
            && let Some(scheme) = self.static_member_scheme(env, name, field)
        {
            return self.instantiate_scheme_at(&scheme, field_span);
        }

        // Bare `Array.sortBy` / `Pair.method` — parameterized type constructors
        // have no single unbound method value without an instantiation.
        if let ExprKind::Name(name) | ExprKind::ComptimeName(name) = &ungroup_expr(receiver).kind
            && self.is_parameterized_type_constructor_name(env, name)
        {
            self.report_unbound_method_parameterized_owner(name, field, field_span);
            return Type::Deferred;
        }

        if let Some(scheme) = self.imported_field_scheme(env, receiver, field, field_span) {
            return self.instantiate_scheme_at(&scheme, field_span);
        }

        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let receiver_type = self.infer(env, receiver);

        // A `?T` / `T?` receiver (e.g. an array element, `arr[0]`) carries the
        // field behind one or more empties. Peel them, read the field off the
        // underlying record, then re-wrap the result with the same empties so the
        // access propagates the emptiness (`?.email : ?Text`).
        let resolved = self.normalize(&self.unifier.resolve(&receiver_type));
        if let Some(signature) = self.exact_method_signature(&resolved, field) {
            self.push_method_obligations_at(signature.predicates, field_span);
            self.simplify_method_obligations(false);
            return Type::Function {
                required: signature.params.len(),
                params: signature.params,
                result: Box::new(signature.result),
            };
        }
        if let Some(Type::Record(data)) = self.named_family_data_view(&resolved)
            && let Some(ty) = data.entries.iter().find_map(|entry| match entry {
                RowEntry::Field { name, ty } if name == field => Some(ty.clone()),
                RowEntry::Field { .. } | RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
            })
        {
            return ty;
        }
        if let Some(method_type) = builtin_collection_method_type(&resolved, field) {
            return self.instantiate_annotation_type_variables(&method_type, &mut HashMap::new());
        }
        let (empties, core) = peel_empty_values(&resolved);
        let core = self
            .named_family_data_view(core)
            .unwrap_or_else(|| core.clone());

        if let Some(method_type) = builtin_collection_method_type(&core, field) {
            let method_type =
                self.instantiate_annotation_type_variables(&method_type, &mut HashMap::new());
            if empties.is_empty() {
                return method_type;
            }
            if !null_safe {
                self.report_unguarded_empty_field_access(receiver, field_span, &empties);
            }
            return rewrap_empty_values(method_type, &empties);
        }

        if is_map_receiver_type(&core) {
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            return Type::Deferred;
        }

        let field_type = self.unifier.fresh();
        let tail = self.unifier.fresh_row_var();
        let required = Type::Record(Row {
            entries: vec![RowEntry::Field {
                name: field.to_owned(),
                ty: field_type.clone(),
            }],
            tail: RowTail::Var(tail),
        });

        if self.unifier.unify(&core, &required).is_err() {
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            return Type::Deferred;
        }

        if empties.is_empty() {
            return field_type;
        }

        // The receiver may be empty: a plain `.field` would use the wrapped value
        // as its underlying `T`, which is unsound — require `?.`.
        if !null_safe {
            self.report_unguarded_empty_field_access(receiver, field_span, &empties);
        }
        rewrap_empty_values(field_type, &empties)
    }

    fn imported_field_scheme(
        &mut self,
        env: &TypeEnv,
        receiver: &Expr,
        field: &str,
        origin_span: Span,
    ) -> Option<TypeScheme> {
        let specifier = aven_parser::static_import_specifier(receiver).or_else(|| {
            let ExprKind::Name(name) = &ungroup_expr(receiver).kind else {
                return None;
            };
            if env.get(name).is_some() {
                return None;
            }
            self.bindings
                .get(name)
                .and_then(|binding| *binding)
                .and_then(|binding| aven_parser::static_import_specifier(&binding.value))
        })?;
        let qualified = self.imports.qualified_export(&specifier, field)?.clone();
        Some(scheme_from_qualified_type(
            &qualified,
            field,
            origin_span,
            &mut self.unifier,
        ))
    }

    pub(super) fn report_unguarded_empty_field_access(
        &mut self,
        receiver: &Expr,
        field_span: Span,
        empties: &[EmptyValue],
    ) {
        // Underline the access (`.field`), not just the field name, and name the
        // receiver when its shape is renderable (`headers[0] may be ...`).
        let span = Span::point(receiver.span.end).merge(field_span);
        let subject = describe_receiver_expr(receiver)
            .map_or_else(|| "this value".to_owned(), |text| format!("`{text}`"));
        self.diagnostics.push(
            Diagnostic::error(format!(
                "{subject} may be {}; accessing a field through it needs `?.`",
                render_empty_values(empties)
            ))
            .with_code(codes::ty::UNGUARDED_EMPTY_ACCESS)
            .with_label(Label::primary(
                span,
                "field accessed without guarding the empty",
            ))
            .with_note(
                "use `?.` to propagate the empty, `??` to supply a default, or match the empty before access",
            ),
        );
    }

    pub(super) fn infer_call(&mut self, env: &TypeEnv, callee: &Expr, args: &[Expr]) -> Type {
        if let ExprKind::Tag(tag) = &callee.kind {
            return self.infer_variant_constructor(env, tag, args);
        }

        if let Some(owner) = self.named_family_constructor_owner(callee) {
            return self.infer_named_family_constructor(env, &owner, args, callee.span);
        }

        if let Some(result) = self.infer_import_call(callee, args) {
            return result;
        }

        if let Some(result) = self.infer_slot_conversion_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_text_decode_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_value_encode_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_record_selection_builtin_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_map_constructor_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_host_comptime_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_comptime_param_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_named_or_constrained_method_call(env, callee, args) {
            return result;
        }

        let callee_obligation_start = self.method_obligation_marker();
        let callee_type = self.infer(env, callee);
        let callee_obligation_end = self.method_obligation_marker();
        let resolved_callee = self.unifier.resolve(&callee_type);
        let callee_type = if matches!(resolved_callee, Type::Function { .. }) {
            self.instantiate_nonrigid_type_variables(&resolved_callee, &mut HashMap::new())
        } else {
            callee_type
        };
        let arg_types: Vec<_> = args.iter().map(|arg| self.infer(env, arg)).collect();
        self.set_integer_divisor_call_types(
            callee_obligation_start,
            callee_obligation_end,
            &arg_types,
        );

        // When the callee already resolves to a function (e.g. a host global or
        // a lambda with defaults), unify each supplied argument against the
        // matching param and keep the function's own result. This admits an
        // omitted trailing optional param, which a fixed-arity synthetic
        // function type could not.
        let resolved = self.unifier.resolve(&callee_type);
        if let Type::Function {
            params,
            result,
            required,
        } = &resolved
            && *required <= arg_types.len()
            && arg_types.len() <= params.len()
        {
            self.check_call_arg_types_against_params(args, &arg_types, params);
            self.simplify_method_obligations(false);
            let result = self.resolve_row_merge_call_result(result);
            if is_to_result_call(callee)
                && let [error] = arg_types.as_slice()
                && let Some((ok, _)) = result_type_args(&result)
            {
                return result_type(
                    ok.clone(),
                    widen_to_result_error_type(&self.unifier.resolve(error)),
                );
            }
            if let Some(result) =
                self.infer_or_else_single_constructor_result(env, callee, &arg_types, &result)
            {
                return result;
            }
            return result;
        }

        let result_type = self.unifier.fresh();
        let expected_callee = Type::Function {
            params: arg_types,
            result: Box::new(result_type.clone()),
            required: args.len(),
        };

        if self.unifier.unify(&callee_type, &expected_callee).is_err() {
            Type::Deferred
        } else {
            self.simplify_method_obligations(false);
            self.resolve_row_merge_call_result(&result_type)
        }
    }

    pub(super) fn named_family_constructor_owner(&self, callee: &Expr) -> Option<String> {
        let (ExprKind::Name(name) | ExprKind::ComptimeName(name)) = &ungroup_expr(callee).kind
        else {
            return None;
        };
        self.named_family_aliases.get(name).cloned()
    }

    pub(super) fn infer_slot_conversion_call(
        &mut self,
        _env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let ExprKind::FieldAccess {
            receiver,
            field,
            null_safe: false,
            ..
        } = &ungroup_expr(callee).kind
        else {
            return None;
        };
        let [target] = args else {
            return None;
        };
        if field != "to" {
            return None;
        }
        let target = self.lower_normalized_annotation(target);
        if !matches!(target, Type::SlotRecord { .. }) {
            return None;
        }
        self.check_value_against(&target, receiver);
        Some(target)
    }

    fn infer_named_family_constructor(
        &mut self,
        env: &TypeEnv,
        owner: &str,
        args: &[Expr],
        callee_span: Span,
    ) -> Type {
        if args.len() != 1 {
            self.report_function_arity_mismatch(1, 1, args.len(), callee_span);
            for arg in args {
                self.infer(env, arg);
            }
            return Type::Named(owner.to_owned());
        }
        let family = self
            .named_families
            .get(owner)
            .cloned()
            .expect("named-family constructor owners have descriptors");
        if let Some(base) = family.primitive_base {
            self.check_value_against(&base, &args[0]);
            return Type::Named(owner.to_owned());
        }
        let data = family.data;
        let constructor_data = constructor_data_row(&data, &family.defaulted_fields);
        let payload = &args[0];
        if let ExprKind::Record(entries) = &ungroup_expr(payload).kind {
            self.check_record_value_against(&constructor_data, entries, payload.span);
            return Type::Named(owner.to_owned());
        }

        let actual = self.infer(env, payload);
        let actual = self.normalize(&self.resolve_and_default(&actual));
        let actual = self.named_family_data_view(&actual).unwrap_or(actual);
        let exact = matches!(&actual, Type::Record(row) if {
            let actual = record_label_set(row);
            let expected = record_label_set(&data);
            actual.is_subset(&expected)
                && expected.difference(&actual).all(|name| {
                    family.defaulted_fields.contains(name)
                        || data.entries.iter().any(|entry| {
                            matches!(entry, RowEntry::Field { name: field, ty } if field == name && self.type_admits_undefined(ty))
                        })
                })
        });
        if exact {
            self.check_type_against_type(&Type::Record(constructor_data), &actual, payload.span);
        } else {
            let display_owner = Type::Named(owner.to_owned()).render();
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "`{display_owner}` construction requires exactly its declared data fields"
                ))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    payload.span,
                    "payload has a different record shape",
                ))
                .with_note("pass a record with no missing or extra fields"),
            );
        }
        Type::Named(owner.to_owned())
    }

    fn infer_named_or_constrained_method_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let ExprKind::FieldAccess {
            receiver,
            field,
            field_span,
            null_safe: false,
        } = &ungroup_expr(callee).kind
        else {
            return None;
        };
        // Receiver-first selection: establish the exact owner and instantiate
        // its qualified method scheme before looking at any explicit argument.
        let candidate = self.infer(env, receiver);
        let probed = self.normalize(&self.resolve_and_default(&candidate));
        let known = self.exact_method_signature(&probed, field);
        let constrained = self
            .method_assumption_scopes
            .iter()
            .rev()
            .flatten()
            .any(|assumption| {
                assumption.member == *field
                    && self.normalize(&self.unifier.resolve(&assumption.candidate)) == probed
            });
        if known.is_none()
            && !constrained
            && let Some(required) = self.attached_builtin_method_required_owner(&probed, field)
        {
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "`{field}` requires receiver `{}`",
                    required.render()
                ))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    *field_span,
                    format!("receiver is `{}`", probed.render()),
                ))
                .with_note(
                    "fixed owner-pattern components never infer an unresolved receiver type",
                ),
            );
            for arg in args {
                self.infer(env, arg);
            }
            return Some(Type::Deferred);
        }
        if known.is_none() && !constrained {
            return None;
        }

        if let Some(signature) = known {
            self.push_method_obligations_at(signature.predicates, *field_span);
            if signature.params.len() != args.len() {
                self.report_function_arity_mismatch(
                    signature.params.len(),
                    signature.params.len(),
                    args.len(),
                    callee.span,
                );
            } else {
                for (expected, arg) in signature.params.iter().zip(args) {
                    self.check_call_arg_against_param(expected, arg);
                }
            }
            self.simplify_method_obligations(false);
            return Some(self.resolve_and_default(&signature.result));
        }

        let arg_types = args
            .iter()
            .map(|arg| self.infer(env, arg))
            .collect::<Vec<_>>();
        let result = self.unifier.fresh();
        self.push_new_method_obligations([MethodPredicate {
            candidate,
            member: field.clone(),
            params: arg_types,
            result: result.clone(),
            operator_span: *field_span,
            divisor_context: None,
            binding: None,
            call_span: None,
            obligation_id: None,
        }]);
        self.simplify_method_obligations(false);
        Some(result)
    }

    fn infer_or_else_single_constructor_result(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        arg_types: &[Type],
        call_result: &Type,
    ) -> Option<Type> {
        if is_resolved_value_type(call_result) {
            return None;
        }
        let ExprKind::FieldAccess {
            receiver, field, ..
        } = &ungroup_expr(callee).kind
        else {
            return None;
        };
        if field != "orElse" {
            return None;
        }
        let inferred_receiver = self.infer(env, receiver);
        let receiver_type = self.normalize(&self.resolve_and_default(&inferred_receiver));
        let (receiver_ok, _receiver_error) = result_type_args(&receiver_type)?;
        let receiver_ok = receiver_ok.clone();
        let [
            Type::Function {
                result: callback_result,
                ..
            },
        ] = arg_types
        else {
            return None;
        };
        let callback_result = self.normalize(&self.resolve_and_default(callback_result));
        let Type::Variant(Row {
            entries,
            tail: RowTail::Closed,
        }) = callback_result
        else {
            return None;
        };
        let [RowEntry::Tag { name, payload }] = entries.as_slice() else {
            return None;
        };
        let [payload] = payload.as_slice() else {
            return None;
        };

        match name.as_str() {
            // A callback that only ever returns `@Ok` recovers every error, so
            // the chain can no longer fail: the error side collapses to the
            // empty closed variant. The success type stays the receiver's.
            "Ok" => Some(result_type(receiver_ok, empty_variant_type())),
            // A callback that only returns `@Err` never contributes a success,
            // so the ok side stays the receiver's while the error type is
            // replaced by the callback's.
            "Err" => Some(result_type(receiver_ok, payload.clone())),
            _ => None,
        }
    }

    pub(super) fn infer_import_call(&mut self, callee: &Expr, args: &[Expr]) -> Option<Type> {
        let ExprKind::Name(name) = &ungroup_expr(callee).kind else {
            return None;
        };
        if name != "import" {
            return None;
        }

        let Some(arg) = args.first() else {
            self.report_dynamic_import(callee.span);
            return Some(Type::Deferred);
        };
        if args.len() != 1 {
            self.report_dynamic_import(callee.span);
            return Some(Type::Deferred);
        }

        let ExprKind::Literal(Literal::String(raw)) = &ungroup_expr(arg).kind else {
            self.report_dynamic_import(arg.span);
            return Some(Type::Deferred);
        };
        let specifier = decode_string_literal(raw);
        match self.imports.get(&specifier) {
            Some(Some(ty)) => Some(ty.clone()),
            Some(None) => Some(Type::Deferred),
            None if aven_core::is_local_import_specifier(&specifier) => {
                self.report_unresolved_import(&specifier, arg.span);
                Some(Type::Deferred)
            }
            None => {
                self.report_unsupported_import_root(&specifier, arg.span);
                Some(Type::Deferred)
            }
        }
    }

    /// `text.decode(Fmt, ...)` desugars to `Fmt.decode(text, ...)`: the format
    /// arrives as the first argument and supplies the decoder. Returns `None`
    /// when the callee is not `.decode` on a `Text`-typed receiver, so a
    /// non-`Text` receiver keeps today's unknown-field behavior.
    pub(super) fn infer_text_decode_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let ExprKind::FieldAccess {
            receiver,
            field,
            field_span,
            null_safe: false,
        } = &ungroup_expr(callee).kind
        else {
            return None;
        };
        if field != "decode" {
            return None;
        }

        // Only the method form dispatches: probe the receiver without leaking its
        // inference (unifier, diagnostics, and recorded types all restored).
        let resolved = self.probe_receiver_type(env, receiver);
        if !is_text_type(&resolved) {
            return None;
        }

        let Some(format) = args.first() else {
            self.report_decode_missing_format(*field_span);
            return Some(Type::Deferred);
        };
        let Some(format_name) = self.format_member_name(env, format, "decode") else {
            self.report_decode_invalid_format(format);
            return Some(Type::Deferred);
        };

        // Build the equivalent `Fmt.decode(receiver, ...rest)` and reuse the
        // existing static / host-comptime resolution unchanged. The synthetic
        // callee keeps the real `text.decode` and format spans so diagnostics
        // land on the written code, and the receiver and remaining arguments
        // keep their own spans for hover.
        let synth_callee = Expr {
            kind: ExprKind::FieldAccess {
                receiver: Box::new(Expr {
                    kind: ExprKind::Name(format_name.clone()),
                    span: format.span,
                }),
                field: "decode".to_owned(),
                field_span: *field_span,
                null_safe: false,
            },
            span: callee.span,
        };
        let mut synth_args = Vec::with_capacity(args.len());
        synth_args.push((**receiver).clone());
        synth_args.extend(args[1..].iter().cloned());

        let result = self.infer_call(env, &synth_callee, &synth_args);
        self.record_format_method_member_type(*field_span, &format_name, &args[1..], &result);
        // Record on the `.decode` access so hover shows the resolved call type.
        self.record_expr_type(callee.span, &result);
        Some(result)
    }

    /// `value.encode(Fmt, ...)` desugars to `Fmt.encode(value, ...)`: the format
    /// arrives as the first argument and supplies the encoder. Unlike decode,
    /// every receiver admits the sugar unless the receiver's own type already
    /// carries an `encode` member, in which case ordinary field-call semantics
    /// win.
    pub(super) fn infer_value_encode_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let (receiver, field_span) = self.value_encode_sugar_receiver(env, callee)?;

        let Some(format) = args.first() else {
            self.report_encode_missing_format(field_span);
            return Some(Type::Deferred);
        };
        let Some(format_name) = self.format_member_name(env, format, "encode") else {
            self.report_encode_invalid_format(format);
            return Some(Type::Deferred);
        };

        let mut synth_args = Vec::with_capacity(args.len());
        synth_args.push(receiver.clone());
        synth_args.extend(args[1..].iter().cloned());

        if self.report_static_member_arity_mismatch(
            env,
            &format_name,
            "encode",
            synth_args.len(),
            callee.span,
        ) {
            return Some(Type::Deferred);
        }

        let synth_callee = Expr {
            kind: ExprKind::FieldAccess {
                receiver: Box::new(Expr {
                    kind: ExprKind::Name(format_name.clone()),
                    span: format.span,
                }),
                field: "encode".to_owned(),
                field_span,
                null_safe: false,
            },
            span: callee.span,
        };

        let result = self.infer_call(env, &synth_callee, &synth_args);
        self.record_format_method_member_type(field_span, &format_name, &args[1..], &result);
        self.record_expr_type(callee.span, &result);
        Some(result)
    }

    fn record_format_method_member_type(
        &mut self,
        field_span: Span,
        format_name: &str,
        extra_args: &[Expr],
        result: &Type,
    ) {
        let mut params = Vec::with_capacity(extra_args.len() + 1);
        params.push(Type::Named(format_name.to_owned()));
        params.extend(
            extra_args
                .iter()
                .enumerate()
                .map(|(index, arg)| Self::method_view_arg_type(arg, index)),
        );
        let result = self.normalize(&self.resolve_and_default(result));
        let ty = Type::Function {
            required: params.len(),
            params,
            result: Box::new(result),
        };
        self.record_local_value_type(field_span, &LocalValueType::Known(ty));
    }

    pub(super) fn value_encode_sugar_receiver<'b>(
        &mut self,
        env: &TypeEnv,
        callee: &'b Expr,
    ) -> Option<(&'b Expr, Span)> {
        let ExprKind::FieldAccess {
            receiver,
            field,
            field_span,
            null_safe: false,
        } = &ungroup_expr(callee).kind
        else {
            return None;
        };
        if field != "encode" {
            return None;
        }

        if self.static_member_wins(env, receiver, field) {
            return None;
        }

        // Probe without leaking the field-sugar decision into the actual
        // inference pass. A record with an `encode` field keeps ordinary lookup;
        // an unconstrained or non-record receiver still gets the universal sugar.
        let resolved = self.probe_receiver_type(env, receiver);
        if receiver_type_carries_member(&resolved, field) {
            return None;
        }

        Some((receiver.as_ref(), *field_span))
    }

    fn probe_receiver_type(&mut self, env: &TypeEnv, receiver: &Expr) -> Type {
        let unifier_snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred_types_len = self.inferred_types.len();
        let receiver_type = self.infer(env, receiver);
        let resolved = self.normalize(&self.resolve_and_default(&receiver_type));
        self.unifier.restore(unifier_snapshot);
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        self.inferred_types.truncate(inferred_types_len);
        resolved
    }

    fn method_view_arg_type(arg: &Expr, index: usize) -> Type {
        match &ungroup_expr(arg).kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => Type::Named(name.clone()),
            _ => Type::Variable(format!("arg{}", index + 1)),
        }
    }

    /// The name of an unshadowed format type carrying `member` as a static
    /// (`Json`, `Yaml`, `Toml`, ...), when `format` names one. Resolution and
    /// dispatch go through the same statics table / `"Fmt.member"` resolver key
    /// the direct call uses, so any registered format works with no per-format
    /// cases.
    fn format_member_name(&self, env: &TypeEnv, format: &Expr, member: &str) -> Option<String> {
        let (ExprKind::Name(name) | ExprKind::ComptimeName(name)) = &ungroup_expr(format).kind
        else {
            return None;
        };
        let has_member = self.static_member_scheme(env, name, member).is_some()
            || (env.get(name).is_none()
                && !self.bindings.contains_key(name)
                && self
                    .host_comptime_fns
                    .contains_key(&format!("{name}.{member}")));
        has_member.then(|| name.clone())
    }

    /// Registered format types carrying a static or resolver named `member`,
    /// alphabetised, for the diagnostic hint — so it stays correct as formats
    /// are added.
    fn format_member_hint(&self, member: &str) -> String {
        let suffix = format!(".{member}");
        let mut formats: Vec<&str> = self
            .statics
            .iter()
            .filter(|(_, members)| members.contains_key(member))
            .map(|(name, _)| name.as_str())
            .chain(
                self.host_comptime_fns
                    .keys()
                    .filter_map(|key| key.strip_suffix(&suffix)),
            )
            .collect();
        formats.sort_unstable();
        formats.dedup();
        if formats.is_empty() {
            return "`Json`".to_owned();
        }
        formats
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn static_member_wins(&self, env: &TypeEnv, receiver: &Expr, field: &str) -> bool {
        let (ExprKind::Name(name) | ExprKind::ComptimeName(name)) = &ungroup_expr(receiver).kind
        else {
            return false;
        };
        self.static_member_scheme(env, name, field).is_some()
            || (env.get(name).is_none()
                && !self.bindings.contains_key(name)
                && self
                    .host_comptime_fns
                    .contains_key(&format!("{name}.{field}")))
    }

    fn report_static_member_arity_mismatch(
        &mut self,
        env: &TypeEnv,
        receiver_name: &str,
        field: &str,
        arg_count: usize,
        span: Span,
    ) -> bool {
        if self
            .host_comptime_fns
            .contains_key(&format!("{receiver_name}.{field}"))
        {
            return false;
        }

        let Some(scheme) = self.static_member_scheme(env, receiver_name, field) else {
            return false;
        };
        let snapshot = self.unifier.snapshot();
        let (ty, _) = self.unifier.instantiate_scheme(&scheme);
        let resolved = self.unifier.resolve(&ty);
        self.unifier.restore(snapshot);

        let Type::Function {
            params, required, ..
        } = resolved
        else {
            return false;
        };
        if required <= arg_count && arg_count <= params.len() {
            return false;
        }

        self.report_function_arity_mismatch(required, params.len(), arg_count, span);
        true
    }

    pub(super) fn report_decode_missing_format(&mut self, field_span: Span) {
        let hint = self.format_member_hint("decode");
        self.diagnostics.push(
            Diagnostic::error("`text.decode` needs a format as its first argument")
                .with_code(codes::ty::DECODE_FORMAT)
                .with_label(Label::primary(
                    field_span,
                    "missing the format to decode with",
                ))
                .with_note(format!(
                    "pass a format type as the first argument, such as {hint}"
                )),
        );
    }

    pub(super) fn report_decode_invalid_format(&mut self, format: &Expr) {
        let hint = self.format_member_hint("decode");
        self.diagnostics.push(
            Diagnostic::error("the first argument to `text.decode` must be a format type")
                .with_code(codes::ty::DECODE_FORMAT)
                .with_label(Label::primary(
                    format.span,
                    "not a format type carrying a `decode` implementation",
                ))
                .with_note(format!("use a format type such as {hint}")),
        );
    }

    pub(super) fn report_encode_missing_format(&mut self, field_span: Span) {
        let hint = self.format_member_hint("encode");
        self.diagnostics.push(
            Diagnostic::error("`value.encode` needs a format as its first argument")
                .with_code(codes::ty::ENCODE_FORMAT)
                .with_label(Label::primary(
                    field_span,
                    "missing the format to encode with",
                ))
                .with_note(format!(
                    "pass a format type as the first argument, such as {hint}"
                )),
        );
    }

    pub(super) fn report_encode_invalid_format(&mut self, format: &Expr) {
        let hint = self.format_member_hint("encode");
        self.diagnostics.push(
            Diagnostic::error("the first argument to `value.encode` must be a format type")
                .with_code(codes::ty::ENCODE_FORMAT)
                .with_label(Label::primary(
                    format.span,
                    "not a format type carrying an `encode` implementation",
                ))
                .with_note(format!("use a format type such as {hint}")),
        );
    }

    pub(super) fn resolve_row_merge_call_result(&mut self, result: &Type) -> Type {
        if let Some(conflict) = self.unifier.row_merge_conflict_in_type(result) {
            self.report_duplicate_row_label(
                &conflict.label,
                conflict.span,
                DuplicateRowLabelContext::Spread,
            );
            Type::Deferred
        } else {
            self.unifier.resolve(result)
        }
    }

    pub(super) fn check_call_arg_types_against_params(
        &mut self,
        args: &[Expr],
        arg_types: &[Type],
        params: &[Type],
    ) -> bool {
        let mut all_match = true;
        for ((arg, actual), expected) in args.iter().zip(arg_types).zip(params) {
            if self.unifier.unify(actual, expected).is_err() {
                let expected = self.normalize(&self.resolve_and_default(expected));
                if !self.check_call_arg_against_param(&expected, arg) {
                    all_match = false;
                }
            }
        }
        all_match
    }

    pub(super) fn infer_host_comptime_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let name = self.comptime_callee_name(env, callee)?;
        let spec = self.host_comptime_fn(env, &name)?;
        let callee_type = self.infer(env, callee);
        let arg_types: Vec<_> = args.iter().map(|arg| self.infer(env, arg)).collect();
        let resolved = self.unifier.resolve(&callee_type);

        let Type::Function {
            params, required, ..
        } = resolved
        else {
            return Some(Type::Deferred);
        };
        if required > arg_types.len() || arg_types.len() > params.len() {
            self.report_function_arity_mismatch(
                required,
                params.len(),
                arg_types.len(),
                callee.span,
            );
            return Some(Type::Deferred);
        }

        let args_match = self.check_call_arg_types_against_params(args, &arg_types, &params);
        if !self.host_comptime_args_match_params(&spec, args, &arg_types, &params) {
            return Some(Type::Deferred);
        }

        let bindings = self.current_comptime_value_bindings();
        let mut comptime_args = Vec::new();
        let mut error_span = callee.span;
        for param in &spec.comptime_params {
            let index = param.index();
            let Some(arg) = args.get(index) else {
                if index >= required {
                    continue;
                }
                return Some(Type::Deferred);
            };
            let Some(mut argument) = (match param {
                HostComptimeParam::Value(_) => self
                    .evaluate_comptime_param_argument(arg, &bindings)
                    .map(|argument| argument.value),
                HostComptimeParam::TypeOf(_) => arg_types.get(index).map(|actual| {
                    let actual = self.normalize(&self.resolve_and_default(actual));
                    comptime::ComptimeValue::ReifiedType(actual)
                }),
            }) else {
                return Some(Type::Deferred);
            };
            if matches!(param, HostComptimeParam::Value(_))
                && matches!(&argument, comptime::ComptimeValue::ReifiedType(_))
                && let Some(name) = call_callee_name(arg)
                && self.type_definitions.get(name).is_some_and(|definition| {
                    !matches!(definition, Type::Deferred | Type::Recursive(_))
                })
            {
                // Host resolvers preserve the written nominal leaf for a
                // direct non-recursive type target. Structural evaluation is
                // still used to prove that the argument is a type, while the
                // resolver result keeps established displays such as
                // `Result(User, JsonError)`.
                argument = comptime::ComptimeValue::ReifiedType(Type::Named(name.to_owned()));
            }
            error_span = arg.span;
            comptime_args.push(ComptimeArg::from_comptime_value(argument));
        }

        if !args_match {
            return Some(Type::Deferred);
        }

        let type_context = crate::ComptimeTypeContext {
            type_definitions: &self.type_definitions,
            named_families: &self.named_families,
            named_family_aliases: &self.named_family_aliases,
            recursive_type_unfoldings: &self.recursive_type_unfoldings,
        };
        match spec
            .resolver
            .resolve_with_type_context(&comptime_args, &type_context)
        {
            Ok(ty) => Some(ty),
            Err(error) => {
                self.report_host_comptime_error(error, error_span);
                Some(Type::Deferred)
            }
        }
    }

    /// The dotted key under which a callee's host comptime resolver is
    /// registered: a bare name (`open`) or a `Receiver.field` path
    /// (`File.open`) when `Receiver` is an unshadowed host-record global.
    /// `None` otherwise.
    pub(super) fn comptime_callee_name(&self, env: &TypeEnv, callee: &Expr) -> Option<String> {
        match &ungroup_expr(callee).kind {
            ExprKind::Name(name) => Some(name.clone()),
            ExprKind::FieldAccess {
                receiver,
                field,
                null_safe: false,
                ..
            } => {
                let receiver_name = match &ungroup_expr(receiver).kind {
                    ExprKind::Name(name) | ExprKind::ComptimeName(name) => name,
                    _ => return None,
                };
                if env.get(receiver_name).is_some() || self.bindings.contains_key(receiver_name) {
                    return None;
                }

                let key = format!("{receiver_name}.{field}");
                self.host_comptime_fns.contains_key(&key).then_some(key)
            }
            _ => None,
        }
    }

    /// The generalized scheme for `Type.field` when `Type` is an unshadowed
    /// static-carrying type name. Mirrors the host-comptime shadowing rule: a
    /// user binding of the receiver name takes precedence over the type.
    pub(super) fn static_member_scheme(
        &self,
        env: &TypeEnv,
        receiver_name: &str,
        field: &str,
    ) -> Option<TypeScheme> {
        if env.get(receiver_name).is_some() || self.bindings.contains_key(receiver_name) {
            return None;
        }

        self.statics.get(receiver_name)?.get(field).cloned()
    }

    /// Value-position `Map(pairs)` construction: a single argument of type
    /// `Array((k, v))` yields `Map(k, v)`. Shares the scheme of `Map.from` so
    /// the two stay in lockstep. Two-argument `Map(k, v)` is type application
    /// (type position via annotation lowering; value-position type values via
    /// the evaluator) and is intentionally not handled here.
    pub(super) fn infer_map_constructor_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let name = call_callee_name(callee)?;
        if name != "Map" {
            return None;
        }
        // Same shadowing rule as statics / host comptime: a user binding of
        // `Map` takes precedence over the builtin constructor.
        if env.get("Map").is_some() || self.bindings.contains_key("Map") {
            return None;
        }
        if args.len() != 1 {
            return None;
        }

        let scheme = self.statics.get("Map")?.get("from")?.clone();
        let from_type = self.instantiate_scheme_at(&scheme, callee.span);
        let resolved = self.unifier.resolve(&from_type);
        let Type::Function {
            params,
            result,
            required,
        } = resolved
        else {
            return None;
        };
        if required > args.len() || args.len() > params.len() {
            return None;
        }

        let arg_types: Vec<_> = args.iter().map(|arg| self.infer(env, arg)).collect();
        self.check_call_arg_types_against_params(args, &arg_types, &params);
        self.simplify_method_obligations(false);
        Some(self.resolve_row_merge_call_result(&result))
    }

    pub(super) fn host_comptime_fn(&self, env: &TypeEnv, name: &str) -> Option<HostComptimeFnSpec> {
        if env.get(name).is_some() || self.bindings.contains_key(name) {
            return None;
        }

        self.host_comptime_fns.get(name).cloned()
    }

    pub(super) fn host_comptime_args_match_params(
        &mut self,
        spec: &HostComptimeFnSpec,
        args: &[Expr],
        arg_types: &[Type],
        params: &[Type],
    ) -> bool {
        spec.comptime_params.iter().all(|param| {
            let index = param.index();
            if args.get(index).is_none() {
                return params.get(index).is_some_and(|_| index >= args.len());
            }
            let Some(actual) = arg_types.get(index) else {
                return false;
            };
            let Some(expected) = params.get(index) else {
                return false;
            };

            if matches!(expected, Type::Deferred) {
                return true;
            }

            let snapshot = self.unifier.snapshot();
            let matches = self.unifier.unify(actual, expected).is_ok();
            self.unifier.restore(snapshot);
            matches
        })
    }

    pub(super) fn report_host_comptime_error(&mut self, error: ComptimeError, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(error.message)
                .with_code(
                    error
                        .code
                        .unwrap_or_else(|| codes::comptime::HOST_FUNCTION.to_owned()),
                )
                .with_label(Label::primary(
                    span,
                    "this compile-time host function call could not be resolved",
                )),
        );
    }

    pub(super) fn infer_record_selection_builtin_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let name = expr_name(callee)?;
        let kind = comptime::RecordSelectionKind::from_name(name)?;
        if self.record_selection_builtin_is_shadowed(env, name) {
            return None;
        }

        let [subject_arg, labels_arg] = args else {
            return Some(Type::Deferred);
        };

        let subject = self.infer_record_selection_subject(env, subject_arg);
        let subject = self.unfold_recursive_type_once(&subject);
        let subject_is_unresolved = self.reflection_subject_is_unresolved(&subject);
        if subject_is_unresolved || !is_concrete_type(&subject) {
            return Some(Type::Deferred);
        }

        if !matches!(subject, Type::Record(_)) {
            let evaluation = comptime::evaluate_record_selection(
                &subject,
                &[],
                subject_arg.span,
                subject_is_unresolved,
                kind,
            );
            self.diagnostics.extend(evaluation.diagnostics);
            return Some(Type::Deferred);
        }

        let Some(labels) = self.evaluate_record_selection_labels(env, labels_arg) else {
            return Some(Type::Deferred);
        };

        let evaluation = comptime::evaluate_record_selection(
            &subject,
            &labels,
            subject_arg.span,
            subject_is_unresolved,
            kind,
        );
        self.diagnostics.extend(evaluation.diagnostics);

        match evaluation.evaluation {
            Evaluation::Evaluated(value) => value.into_reified_type().or(Some(Type::Deferred)),
            Evaluation::Deferred | Evaluation::Unsupported => Some(Type::Deferred),
        }
    }

    pub(super) fn record_selection_builtin_is_shadowed(&self, env: &TypeEnv, name: &str) -> bool {
        env.get(name).is_some()
            || self.bindings.contains_key(name)
            || self.value_types.contains_key(name)
    }

    pub(super) fn infer_record_selection_subject(&mut self, env: &TypeEnv, arg: &Expr) -> Type {
        let inferred = self.infer(env, arg);
        let subject = self.normalize(&self.resolve_and_default(&inferred));
        if is_concrete_type(&subject) {
            return subject;
        }

        let mut checker = self.fork_annotation_checker();
        let lowered = checker.lower_annotation(arg);
        if checker.diagnostics.is_empty() {
            let lowered = checker.normalize(&lowered);
            if is_concrete_type(&lowered) {
                return lowered;
            }
        }

        subject
    }

    pub(super) fn evaluate_record_selection_labels(
        &mut self,
        env: &TypeEnv,
        arg: &Expr,
    ) -> Option<Vec<String>> {
        let bindings = self.current_comptime_value_bindings();
        if let Some(argument) = self.evaluate_comptime_param_argument(arg, &bindings)
            && let comptime::ComptimeValue::LabelSet(labels) = argument.value
        {
            return Some(labels);
        }

        if let Some(labels) =
            self.comptime_known_label_set_for_mode(arg, RowFoldMode::Value { env })
        {
            return Some(labels);
        }

        let evaluation = comptime::evaluate_type_position_with_bindings(self, arg, &bindings);
        self.diagnostics.extend(evaluation.diagnostics);

        match evaluation.evaluation {
            Evaluation::Evaluated(comptime::ComptimeValue::LabelSet(labels)) => Some(labels),
            Evaluation::Evaluated(
                comptime::ComptimeValue::ReifiedType(_)
                | comptime::ComptimeValue::Literal(_)
                | comptime::ComptimeValue::Bool(_),
            )
            | Evaluation::Deferred
            | Evaluation::Unsupported => None,
        }
    }

    pub(super) fn infer_comptime_param_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let (params, body) = self.comptime_param_function(callee)?;
        let uppercase = call_callee_name(callee)
            .and_then(|name| name.chars().next())
            .is_some_and(char::is_uppercase);
        if params.len() != args.len() {
            let function = match &ungroup_expr(callee).kind {
                ExprKind::Name(name) | ExprKind::ComptimeName(name) => name,
                _ => "comptime function",
            };
            self.diagnostics
                .push(comptime::comptime_function_arity_mismatch(
                    callee.span,
                    function,
                    params.len(),
                    args.len(),
                ));
            return Some(Type::Deferred);
        }

        let mut type_bindings = HashMap::new();
        let mut body_env = TypeEnv::new();
        let mut param_var_metas = HashMap::new();

        for (param, arg) in params.iter().zip(args).filter(|(param, _)| !param.comptime) {
            let inferred = self.infer(env, arg);
            let actual = self.normalize(&self.resolve_and_default(&inferred));

            if let Some(annotation) = &param.annotation {
                collect_comptime_type_bindings(annotation, &actual, &mut type_bindings);
                // A lowercase type variable in the parameter annotation (e.g.
                // `variant: v`, linked to a comptime domain `tagsOf(v)`) is a
                // generic binder, not a concrete type. Instantiate its
                // variables to fresh unification metas — shared across params so
                // repeated names stay consistent — before checking the argument,
                // so a rigid `Type::Variable` does not spuriously reject every
                // call.
                let expected = self.lower_annotation_for_inference(annotation);
                let expected =
                    self.instantiate_annotation_type_variables(&expected, &mut param_var_metas);
                if self.unifier.unify(&expected, &actual).is_err() {
                    return Some(Type::Deferred);
                }
            }

            body_env.insert(param.name.clone(), LocalValueType::Known(actual));
        }

        let runtime_value_bindings = self.current_comptime_value_bindings();
        let mut body_comptime_values = HashMap::new();

        for (param, arg) in params.iter().zip(args).filter(|(param, _)| param.comptime) {
            let Some(argument) =
                self.evaluate_comptime_param_argument(arg, &runtime_value_bindings)
            else {
                // An unresolved enclosing comptime parameter is intentionally
                // deferred until its caller specializes this function. Other
                // unevaluable lowercase arguments still need ordinary value
                // checking so they cannot turn an annotation into a silent
                // `Deferred` accept.
                if !uppercase && !self.expr_references_unresolved_comptime_param(arg) {
                    let diagnostics_start = self.diagnostics.len();
                    self.check_value_expr(arg);
                    if self.diagnostics.len() == diagnostics_start
                        && self.is_runtime_computation_call(arg)
                    {
                        let function = call_callee_name(callee).unwrap_or("comptime function");
                        self.push_unique_diagnostic(comptime::comptime_argument_not_known(
                            arg.span, function,
                        ));
                    }
                }
                return Some(Type::Deferred);
            };
            let value = argument.value.clone();

            let domain = param.annotation.as_ref().and_then(|annotation| {
                self.evaluate_comptime_param_domain(annotation, &type_bindings)
            });

            let diagnostics_before_domain_check = self.diagnostics.len();
            if !uppercase && let Some(row) = domain.as_ref().and_then(literal_union_domain_row) {
                match &value {
                    comptime::ComptimeValue::Literal(literal) => {
                        self.check_literal_value_against_variant(row, literal, arg.span);
                    }
                    comptime::ComptimeValue::LabelSet(labels) => {
                        if let Some(members) = &argument.label_set_members {
                            for member in members {
                                self.check_literal_value_against_variant(
                                    row,
                                    &member.literal,
                                    member.span,
                                );
                            }
                        } else {
                            for label in labels {
                                let literal = label_literal(label);
                                self.check_literal_value_against_variant(row, &literal, arg.span);
                            }
                        }
                    }
                    comptime::ComptimeValue::Bool(value) => {
                        self.check_literal_value_against_variant(
                            row,
                            &Literal::Bool(*value),
                            arg.span,
                        );
                    }
                    comptime::ComptimeValue::ReifiedType(_) => {}
                }
            }
            if self.diagnostics.len() > diagnostics_before_domain_check {
                return Some(Type::Deferred);
            }

            let value_type = value
                .clone()
                .reify_type_position()
                .into_reified_type()
                .or(domain)
                .unwrap_or(Type::Deferred);

            body_env.insert(param.name.clone(), LocalValueType::Known(value_type));
            body_comptime_values.insert(param.name.clone(), value.clone());
        }

        self.local_comptime_values.push(body_comptime_values);
        let result = self.infer(&body_env, &body);
        self.local_comptime_values.pop();

        Some(self.resolve_and_default(&result))
    }

    pub(super) fn evaluate_comptime_param_argument(
        &mut self,
        arg: &Expr,
        bindings: &HashMap<String, comptime::ComptimeValue>,
    ) -> Option<ComptimeArgument> {
        // A call to a lowercase function with no `@` parameters is a runtime
        // computation, even if the evaluator can reduce its body. In
        // particular, `pick(bad())` must not execute `bad` while validating a
        // comptime argument.
        if self.is_runtime_computation_call(arg) {
            return None;
        }

        if let Some(argument) = self.evaluate_comptime_runtime_argument(arg, bindings) {
            return Some(argument);
        }

        let members = self.concrete_label_set_members(arg, bindings);
        if let Some(members) = members {
            let labels = members.iter().map(|member| member.label.clone()).collect();
            return Some(ComptimeArgument {
                value: comptime::ComptimeValue::LabelSet(labels),
                label_set_members: Some(members),
            });
        }

        if let Some(ty) = self.evaluate_comptime_type_argument(arg, bindings) {
            return Some(ComptimeArgument {
                value: comptime::ComptimeValue::ReifiedType(ty),
                label_set_members: None,
            });
        }
        None
    }

    pub(super) fn is_runtime_computation_call(&self, expr: &Expr) -> bool {
        matches!(&ungroup_expr(expr).kind, ExprKind::Call { callee, .. }
        if call_callee_name(callee).is_some_and(|name| {
            name.chars().next().is_some_and(char::is_lowercase)
                && self
                    .lookup_comptime_function_export(name)
                    .is_some_and(|function| {
                        function.params.iter().all(|param| !param.comptime)
                    })
        }))
    }

    pub(super) fn evaluate_comptime_runtime_argument(
        &self,
        arg: &Expr,
        bindings: &HashMap<String, comptime::ComptimeValue>,
    ) -> Option<ComptimeArgument> {
        match comptime::evaluate_runtime_value(arg, bindings).evaluation {
            Evaluation::Evaluated(value) => {
                return Some(ComptimeArgument {
                    value,
                    label_set_members: None,
                });
            }
            Evaluation::Deferred | Evaluation::Unsupported => {}
        }

        None
    }

    fn evaluate_comptime_type_argument(
        &mut self,
        arg: &Expr,
        bindings: &HashMap<String, comptime::ComptimeValue>,
    ) -> Option<Type> {
        let start = self.diagnostics.len();
        self.local_comptime_values.push(bindings.clone());
        let ty = self.lower_annotation(arg);
        self.local_comptime_values.pop();
        let diagnostics = self.diagnostics.split_off(start);
        let has_diagnostics = !diagnostics.is_empty();
        self.diagnostics.extend(diagnostics);
        if has_diagnostics {
            return None;
        }

        is_concrete_type(&ty).then_some(ty)
    }

    pub(super) fn comptime_param_function(&self, callee: &Expr) -> Option<(Vec<Param>, Expr)> {
        let name = call_callee_name(callee)?;
        let export = self.lookup_comptime_function_export(name)?;
        let uppercase = name.chars().next().is_some_and(char::is_uppercase);
        (uppercase || export.params.iter().any(|param| param.comptime)).then(|| {
            let params = export
                .params
                .into_iter()
                .map(|mut param| {
                    param.comptime |= uppercase;
                    param
                })
                .collect();
            (params, export.body)
        })
    }

    /// Resolve a comptime-evaluable function by local binding or imported export.
    pub(super) fn lookup_comptime_function_export(
        &self,
        name: &str,
    ) -> Option<comptime::ComptimeExport> {
        if let Some(binding) = self.bindings.get(name).and_then(|binding| *binding)
            && let Some((params, body)) = lambda_parts(&binding.value)
        {
            return Some(
                comptime::ComptimeExport::from_module_lambda(
                    binding.name.clone(),
                    params,
                    body,
                    self.type_definitions.clone(),
                    self.local_comptime_function_definitions(),
                )
                .with_module_identity(self.module_identity.clone()),
            );
        }

        let pattern_binding = *self.pattern_bindings.get(name)?;
        let specifier = aven_parser::static_import_specifier(&pattern_binding.value)?;
        let source = super::import_pattern_source_for_binder(&pattern_binding.pattern, name)?;
        let export = self.imports.comptime_export(&specifier, source)?;
        Some(export.renamed(name))
    }

    pub(super) fn local_comptime_function_definitions(&self) -> Vec<(String, Vec<Param>, Expr)> {
        self.bindings
            .iter()
            .filter_map(|(name, binding)| {
                let binding = *binding.as_ref()?;
                let (params, body) = lambda_parts(&binding.value)?;
                name.chars()
                    .next()
                    .is_some_and(char::is_uppercase)
                    .then(|| (name.clone(), params.to_vec(), body.clone()))
            })
            .collect()
    }

    /// True when `name` is bound from a static module import (pattern extract).
    pub(super) fn is_imported_name(&self, name: &str) -> bool {
        self.pattern_bindings
            .get(name)
            .is_some_and(|binding| aven_parser::static_import_specifier(&binding.value).is_some())
    }

    pub(super) fn evaluate_comptime_param_domain(
        &mut self,
        annotation: &Expr,
        type_bindings: &HashMap<String, comptime::ComptimeValue>,
    ) -> Option<Type> {
        let evaluation =
            comptime::evaluate_type_position_with_bindings(self, annotation, type_bindings);
        self.diagnostics.extend(evaluation.diagnostics);

        match evaluation.evaluation {
            Evaluation::Evaluated(value) => value.reify_type_position().into_reified_type(),
            Evaluation::Deferred | Evaluation::Unsupported => None,
        }
    }

    pub(super) fn infer_value_index(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Type {
        let [arg] = args else {
            return Type::Deferred;
        };

        let callee_type = self.infer(env, callee);
        let callee_type = self.normalize(&self.resolve_and_default(&callee_type));
        match callee_type {
            Type::Record(row) => self.infer_record_index(&row, arg),
            Type::Tuple(elements) => self.infer_tuple_index(&elements, arg),
            Type::Apply { callee, args }
                if args.len() == 1
                    && matches!(callee.as_ref(), Type::Named(name) if name == "Array") =>
            {
                // Array indexing accepts a runtime index that may be out of
                // bounds, so the result is optional (`?a`): an absent element
                // yields `undefined`.
                self.check_value_index_arg(env, arg, named_builtin("Int"));
                Type::Optional(Box::new(args[0].clone()))
            }
            Type::Apply { callee, args }
                if args.len() == 2
                    && matches!(callee.as_ref(), Type::Named(name) if name == "Map") =>
            {
                // `m[key]` sugars to `m.get(key)`: the index unifies with the
                // key type, and the result is optional (`?v`) since a missing
                // key yields `undefined` at runtime.
                let key_type = args[0].clone();
                let value_type = args[1].clone();
                self.check_value_index_arg(env, arg, key_type);
                Type::Optional(Box::new(value_type))
            }
            _ if is_resolved_value_type(&callee_type) => {
                self.report_not_indexable(&callee_type, callee.span);
                Type::Deferred
            }
            _ => Type::Deferred,
        }
    }

    /// Check a value-index argument so deferred index types remain deferred and
    /// concrete mismatches point to the index expression.
    fn check_value_index_arg(&mut self, env: &TypeEnv, arg: &Expr, expected: Type) {
        let actual = self.infer(env, arg);
        if matches!(&expected, Type::Named(name) if name == "Int") {
            let actual = self.widen_numeric_operand(&actual);
            if self.unifier.unify(&actual, &expected).is_err() {
                let expected = self.normalize(&self.resolve_and_default(&expected));
                self.check_type_against_type(&expected, &actual, arg.span);
            }
        } else {
            self.check_call_arg_types_against_params(
                std::slice::from_ref(arg),
                &[actual],
                &[expected],
            );
        }
    }

    /// Record indexing with a comptime-known key reads the exact field type,
    /// just like `record.field`.
    pub(super) fn infer_record_index(&mut self, row: &Row, arg: &Expr) -> Type {
        let Some(label) = self.comptime_known_label(arg) else {
            if self.expr_references_unresolved_comptime_param(arg) {
                return Type::Deferred;
            }
            self.report_record_index_not_comptime(arg.span);
            return Type::Deferred;
        };
        if let Some(ty) = row_field_type(row, &label) {
            return ty.clone();
        }
        if row.tail == RowTail::Closed {
            self.report_missing_field(&label, arg.span);
        }
        Type::Deferred
    }

    /// Tuple projection requires a comptime-known integer index and returns the
    /// element type directly (an in-range element is always present, so no `?`).
    pub(super) fn infer_tuple_index(&mut self, elements: &[Type], arg: &Expr) -> Type {
        let Some(index) = comptime_known_tuple_index(arg) else {
            self.report_tuple_index_not_comptime(arg.span);
            return Type::Deferred;
        };
        match elements.get(index) {
            Some(ty) => ty.clone(),
            None => {
                self.report_tuple_index_out_of_range(arg.span, index, elements.len());
                Type::Deferred
            }
        }
    }

    pub(super) fn comptime_known_label(&self, expr: &Expr) -> Option<String> {
        match &ungroup_expr(expr).kind {
            ExprKind::Literal(Literal::String(text)) => string_literal_label(text),
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => self
                .lookup_comptime_value(name)
                .and_then(comptime_value_label),
            _ => None,
        }
    }

    pub(super) fn comptime_known_label_set(&self, expr: &Expr) -> Option<Vec<String>> {
        match &ungroup_expr(expr).kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => self
                .lookup_comptime_value(name)
                .and_then(comptime_value_label_set),
            ExprKind::Set(_) => {
                let bindings = self.current_comptime_value_bindings();
                self.concrete_label_set_members(expr, &bindings)
                    .map(|members| members.into_iter().map(|member| member.label).collect())
            }
            _ => None,
        }
    }

    pub(super) fn comptime_known_label_set_for_mode(
        &self,
        expr: &Expr,
        mode: RowFoldMode<'_>,
    ) -> Option<Vec<String>> {
        self.comptime_known_label_set(expr).or_else(|| match mode {
            RowFoldMode::Annotation => self.comptime_known_reflection_reified_type(expr),
            RowFoldMode::Value { env } => self.comptime_known_reflection_value(expr, env),
        })
    }

    pub(super) fn comptime_known_reflection_reified_type(
        &self,
        expr: &Expr,
    ) -> Option<Vec<String>> {
        let ExprKind::Call { callee, args } = &ungroup_expr(expr).kind else {
            return None;
        };
        let reflection = LabelReflection::from_name(expr_name(callee)?)?;

        let [arg] = args.as_slice() else {
            return None;
        };
        let subject = self.lookup_comptime_reified_type_expr(arg)?;
        let subject = self.unfold_recursive_type_once(&self.normalize(&subject));

        let Evaluation::Evaluated(comptime::ComptimeValue::LabelSet(labels)) = reflection
            .evaluate(
                &subject,
                arg.span,
                self.reflection_subject_is_unresolved(&subject),
            )
            .evaluation
        else {
            return None;
        };

        Some(labels)
    }

    pub(super) fn comptime_known_reflection_value(
        &self,
        expr: &Expr,
        env: &TypeEnv,
    ) -> Option<Vec<String>> {
        let ExprKind::Call { callee, args } = &ungroup_expr(expr).kind else {
            return None;
        };
        let reflection = LabelReflection::from_name(expr_name(callee)?)?;

        let [arg] = args.as_slice() else {
            return None;
        };
        let name = expr_name(arg)?;
        let LocalValueType::Known(subject) = env.get(name)? else {
            return None;
        };
        let subject =
            self.unfold_recursive_type_once(&self.normalize(&self.unifier.resolve(subject)));

        let Evaluation::Evaluated(comptime::ComptimeValue::LabelSet(labels)) = reflection
            .evaluate(
                &subject,
                arg.span,
                self.reflection_subject_is_unresolved(&subject),
            )
            .evaluation
        else {
            return None;
        };

        Some(labels)
    }

    pub(super) fn lookup_comptime_value(&self, name: &str) -> Option<&comptime::ComptimeValue> {
        self.local_comptime_values
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
    }

    pub(super) fn current_comptime_value_bindings(
        &self,
    ) -> HashMap<String, comptime::ComptimeValue> {
        let mut bindings = HashMap::new();
        for scope in &self.local_comptime_values {
            bindings.extend(scope.clone());
        }
        bindings
    }

    pub(super) fn concrete_label_set_members(
        &self,
        expr: &Expr,
        bindings: &HashMap<String, comptime::ComptimeValue>,
    ) -> Option<Vec<LabelSetMember>> {
        let ExprKind::Set(entries) = &ungroup_expr(expr).kind else {
            return None;
        };
        let elements = literal_set_elements(entries)?;
        let mut members = Vec::new();

        for element in elements {
            let Evaluation::Evaluated(comptime::ComptimeValue::Literal(literal)) =
                comptime::evaluate_runtime_value(element, bindings).evaluation
            else {
                return None;
            };
            let Literal::String(text) = &literal else {
                return None;
            };
            let label = string_literal_label(text)?;
            members.push(LabelSetMember {
                label,
                literal,
                span: element.span,
            });
        }

        Some(members)
    }

    pub(super) fn infer_name_reference(&mut self, env: &TypeEnv, name: &str, span: Span) -> Type {
        if let Some(local) = env.get(name).cloned() {
            return match local {
                LocalValueType::Known(ty) => ty,
                LocalValueType::Scheme(scheme) => self.instantiate_scheme_at(&scheme, span),
                LocalValueType::Unknown => Type::Deferred,
            };
        }

        if self.named_family_aliases.contains_key(name) {
            return Type::Named("Type".to_owned());
        }

        if let Some(scheme) = self.infer_top_level(name) {
            return self.instantiate_scheme_at(&scheme, span);
        }

        // Seeded host globals have no binding to infer from; read their
        // published scheme so the inference path sees the same type as the
        // directed-checking path. A declared name whose published type was
        // withheld (e.g. a duplicate top-level declaration, deferred until
        // overload selection exists) is still *bound* — resolve it to a deferred
        // type rather than letting it fall through and be reported as unbound,
        // which would cascade an error onto every later use.
        if let Some(scheme) = self.value_types.get(name).cloned() {
            return match scheme {
                Some(scheme) => self.instantiate_scheme_at(&scheme, span),
                None => Type::Deferred,
            };
        }

        if name == aven_parser::METHOD_RECEIVER_NAME {
            if self.report_unbound_names && self.reported_unbound_name_spans.insert(span) {
                self.diagnostics.push(
                    Diagnostic::error("receiver focus `.` is only available inside a method body")
                        .with_code(codes::name::UNBOUND)
                        .with_label(Label::primary(span, "no hidden receiver is in scope")),
                );
            }
            return Type::Deferred;
        }

        if name_is_placeholder(name)
            || builtin_value_name_is_bound(name)
            || self.known_types.contains(name)
        {
            return Type::Deferred;
        }

        self.report_unbound_name(name, span);
        Type::Deferred
    }

    pub(super) fn report_unbound_name(&mut self, name: &str, span: Span) {
        if !self.report_unbound_names {
            return;
        }

        if !self.reported_unbound_name_spans.insert(span) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error(format!("unbound name `{name}`"))
                .with_code(codes::name::UNBOUND)
                .with_label(Label::primary(span, "this name is not bound"))
                .with_note(format!(
                    "check the spelling, or define `{name}` before it is used"
                )),
        );
    }

    pub(super) fn report_unused_result_if_dropped(&mut self, env: &TypeEnv, expr: &Expr) {
        let unifier_snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred_types_len = self.inferred_types.len();
        let inferred = self.infer(env, expr);
        let resolved = self.resolve_and_default(&inferred);
        self.unifier.restore(unifier_snapshot);
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        self.inferred_types.truncate(inferred_types_len);

        if is_result_type(&resolved) {
            self.report_unused_result(expr.span);
        }
    }

    pub(super) fn report_unused_result(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::warning("unused `Result`")
                .with_code(codes::ty::UNUSED_RESULT)
                .with_label(Label::primary(span, "this `Result` is unused"))
                .with_note(
                    "unwrap it with `?!` (panic on `@Err`), propagate it with `?^`, or discard it explicitly with `_ =`.",
                ),
        );
    }

    pub(super) fn infer_variant_constructor(
        &mut self,
        env: &TypeEnv,
        tag: &str,
        args: &[Expr],
    ) -> Type {
        let mut payload = Vec::new();

        for arg in args {
            let arg_type = self.infer(env, arg);
            let arg_type = self.unifier.resolve(&arg_type);
            if type_contains_deferred(&arg_type) {
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

    pub(super) fn infer_array(&mut self, env: &TypeEnv, entries: &[RecordEntry]) -> Type {
        self.infer_collection_entries(env, entries, "Array")
    }

    pub(super) fn infer_set(&mut self, env: &TypeEnv, entries: &[RecordEntry]) -> Type {
        self.infer_collection_entries(env, entries, "Set")
    }

    /// Infer `Array`/`Set` literals from Element + Spread entries (same shape).
    pub(super) fn infer_collection_entries(
        &mut self,
        env: &TypeEnv,
        entries: &[RecordEntry],
        name: &str,
    ) -> Type {
        let element_type = self.unifier.fresh();
        let collection_type = Type::Apply {
            callee: Box::new(Type::Named(name.to_owned())),
            args: vec![element_type.clone()],
        };

        for entry in entries {
            match entry {
                RecordEntry::Element(element) => {
                    let item_type = self.infer(env, element);
                    if self.unifier.unify(&element_type, &item_type).is_err() {
                        return Type::Deferred;
                    }
                }
                RecordEntry::Spread { value, .. } => {
                    let source_type = self.infer(env, value);
                    if self.unifier.unify(&collection_type, &source_type).is_err() {
                        return Type::Deferred;
                    }
                }
                _ => return Type::Deferred,
            }
        }

        collection_type
    }

    pub(super) fn infer_set_union(&mut self, env: &TypeEnv, expr: &Expr) -> Type {
        let Some(parts) = value_set_union_parts(expr) else {
            return Type::Deferred;
        };

        let element_type = self.unifier.fresh();
        for part in parts {
            let item_type = self.infer_set_union_part_type(env, part);
            if self.unifier.unify(&element_type, &item_type).is_err() {
                return Type::Deferred;
            }
        }

        Type::Apply {
            callee: Box::new(Type::Named("Set".to_owned())),
            args: vec![element_type],
        }
    }

    pub(super) fn infer_set_union_part_type(
        &mut self,
        env: &TypeEnv,
        part: SetUnionPart<'_>,
    ) -> Type {
        let ty = self.infer(env, part.expr());
        if part.promotes_singleton() {
            return ty;
        }

        self.set_operand_element_type(&ty).unwrap_or(ty)
    }

    pub(super) fn set_operand_element_type(&mut self, ty: &Type) -> Option<Type> {
        let resolved = self.resolve_and_default(ty);
        match self.normalize(&resolved) {
            Type::Apply { callee, args }
                if args.len() == 1
                    && matches!(callee.as_ref(), Type::Named(name) if name == "Set") =>
            {
                Some(args[0].clone())
            }
            _ => None,
        }
    }

    pub(super) fn infer_block(&mut self, env: &TypeEnv, items: &[Item]) -> Type {
        let mut next_env = env.clone();

        for item in merged_items(items) {
            match item {
                MergedItem::Binding { signature, binding } => {
                    let obligation_marker = self.method_obligation_marker();
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
                            let env_metas = free_metas_in_local_values(next_env.values(), |ty| {
                                self.unifier.resolve(ty)
                            });
                            let env_row_vars =
                                free_row_vars_in_local_values(next_env.values(), |ty| {
                                    self.unifier.resolve(ty)
                                });
                            let scheme = self.generalize_method_obligations(
                                resolved,
                                &env_metas,
                                &env_row_vars,
                                obligation_marker,
                                Some(&binding.name),
                            );
                            if type_contains_deferred(&scheme.ty) {
                                LocalValueType::Unknown
                            } else {
                                LocalValueType::Scheme(scheme)
                            }
                        });
                    next_env.insert(binding.name.clone(), local_type);
                }
                MergedItem::PatternBinding(binding) => {
                    let inferred = self.infer(&next_env, &binding.value);
                    let resolved = self.normalize(&self.resolve_and_default(&inferred));
                    for (name, ty) in pattern_local_types(
                        self.pattern_type_context(),
                        &binding.pattern,
                        Some(&resolved),
                    ) {
                        next_env.insert(name, ty);
                    }
                }
                MergedItem::SpreadBinding(binding) => {
                    if let Some(row) = self.closed_spread_row(binding, &next_env, false) {
                        for entry in row.entries {
                            let RowEntry::Field { name, ty } = entry else {
                                continue;
                            };
                            next_env.insert(name, LocalValueType::Known(ty));
                        }
                    }
                }
                MergedItem::MethodAttachment(_) => {}
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
    pub(super) fn lower_annotation_for_inference(&self, annotation: &Expr) -> Type {
        let mut checker = self.fork_annotation_checker();
        let ty = checker.lower_annotation(annotation);
        checker.normalize(&ty)
    }

    pub(super) fn lower_inline_lambda_annotation_for_inference(
        &mut self,
        annotation: &Expr,
    ) -> Type {
        let ty = self.lower_annotation_for_inference(annotation);
        self.resolve_inline_lambda_annotation_variables(&ty)
    }

    /// Replace each `Type::Variable` (a generic type binder from a parameter
    /// annotation) with a fresh unification meta, reusing `metas` so a name that
    /// appears in several positions instantiates to the same meta.
    pub(super) fn instantiate_annotation_type_variables(
        &mut self,
        ty: &Type,
        metas: &mut HashMap<String, Type>,
    ) -> Type {
        self.instantiate_type_variables(ty, metas, |_| true)
    }

    /// Instantiate free annotation variables at a call site, preserving the
    /// skolems that are rigid while checking an enclosing declaration body.
    pub(super) fn instantiate_nonrigid_type_variables(
        &mut self,
        ty: &Type,
        metas: &mut HashMap<String, Type>,
    ) -> Type {
        let rigid = self
            .rigid_type_var_scopes
            .iter()
            .flat_map(|scope| scope.iter().cloned())
            .collect::<HashSet<_>>();
        self.instantiate_type_variables(ty, metas, |name| !rigid.contains(name))
    }

    fn instantiate_type_variables(
        &mut self,
        ty: &Type,
        metas: &mut HashMap<String, Type>,
        should_instantiate: impl Fn(&str) -> bool,
    ) -> Type {
        map_type(ty, &mut |node| match node {
            Type::Variable(name) if should_instantiate(name) => Some(
                metas
                    .entry(name.clone())
                    .or_insert_with(|| self.unifier.fresh())
                    .clone(),
            ),
            _ => None,
        })
    }
}

fn record_label_set(row: &Row) -> HashSet<String> {
    row.entries
        .iter()
        .filter_map(|entry| match entry {
            RowEntry::Field { name, .. } => Some(name.clone()),
            RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
        })
        .collect()
}

fn constructor_data_row(data: &Row, defaulted_fields: &HashSet<String>) -> Row {
    Row {
        entries: data
            .entries
            .iter()
            .map(|entry| match entry {
                RowEntry::Field { name, ty } if defaulted_fields.contains(name) => {
                    RowEntry::Field {
                        name: name.clone(),
                        ty: Type::Optional(Box::new(ty.clone())),
                    }
                }
                entry => entry.clone(),
            })
            .collect(),
        tail: data.tail,
    }
}

fn export_generic_name(index: usize) -> String {
    const LETTERS: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";
    LETTERS
        .get(index)
        .map(|letter| char::from(*letter).to_string())
        .unwrap_or_else(|| format!("t{index}"))
}

fn receiver_type_carries_member(ty: &Type, member: &str) -> bool {
    if builtin_collection_method_type(ty, member).is_some() {
        return true;
    }
    let (_, core) = peel_empty_values(ty);
    if builtin_collection_method_type(core, member).is_some() {
        return true;
    }

    let Type::Record(row) = core else {
        return false;
    };
    row.entries
        .iter()
        .any(|entry| matches!(entry, RowEntry::Field { name, .. } if name == member))
}

fn is_to_result_call(callee: &Expr) -> bool {
    matches!(
        &ungroup_expr(callee).kind,
        ExprKind::FieldAccess { field, .. } if field == "toResult"
    )
}

fn widen_to_result_error_type(ty: &Type) -> Type {
    map_type(ty, &mut |node| {
        let Type::Variant(row) = node else {
            return None;
        };
        match literal_variant_base(row)? {
            LiteralBase::Text => Some(named_builtin("Text")),
            LiteralBase::Bool => Some(named_builtin("Bool")),
            LiteralBase::Number => {
                let is_float = row.entries.iter().any(|entry| {
                    matches!(entry, RowEntry::Literal { value: Literal::Number(number) } if is_float_literal_text(number))
                });
                Some(named_builtin(if is_float { "Float" } else { "Int" }))
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FoldNumber {
    Int(i64),
    Float(f64),
}

fn is_empty_closed_variant(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Variant(Row {
            entries,
            tail: RowTail::Closed,
        }) if entries.is_empty()
    )
}

fn singleton_literal_type(ty: &Type) -> Option<&Literal> {
    let Type::Variant(row) = ty else {
        return None;
    };
    if row.tail == RowTail::Open {
        return None;
    }

    let [RowEntry::Literal { value }] = row.entries.as_slice() else {
        return None;
    };
    Some(value)
}

fn static_integer_literal_is_zero(expr: &Expr) -> Option<bool> {
    match &ungroup_expr(expr).kind {
        ExprKind::Literal(Literal::Number(number)) if !is_float_literal_text(number) => {
            Some(integer_literal_text_is_zero(number))
        }
        ExprKind::Unary {
            operator, value, ..
        } if operator == "-" => static_integer_literal_is_zero(value),
        _ => None,
    }
}

/// Type-based static non-zero divisor check.
///
/// Returns `Some(true)` when every known member is the integer zero (report
/// division-by-zero), `Some(false)` when every known member is a non-zero
/// integer (accept), and `None` when the type does not prove a non-zero
/// divisor (plain `Int`, open rows, mixed zero/non-zero unions, floats, …).
///
/// Closed literal unions (`2 | 4`) are the annotated form. Inferred monomorphic
/// literals (`n = 10 / 2` → `5 | ρ`) use a free row-var tail; those are accepted
/// too because the known entries are the only inhabitants of the value. A truly
/// open row (`RowTail::Open`, as in `@{2, 4, ..}`) may still grow unknown
/// members and stays rejected.
fn static_integer_divisor_type_is_zero(ty: &Type) -> Option<bool> {
    let Type::Variant(row) = ty else {
        return None;
    };
    if row.tail == RowTail::Open || row.entries.is_empty() {
        return None;
    }
    if literal_variant_base(row) != Some(LiteralBase::Number) {
        return None;
    }

    let mut saw_zero = false;
    let mut saw_nonzero = false;
    for entry in &row.entries {
        let RowEntry::Literal {
            value: Literal::Number(number),
        } = entry
        else {
            return None;
        };
        if is_float_literal_text(number) {
            return None;
        }
        if integer_literal_text_is_zero(number) {
            saw_zero = true;
        } else {
            saw_nonzero = true;
        }
    }

    match (saw_zero, saw_nonzero) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

fn integer_literal_text_is_zero(number: &str) -> bool {
    number.bytes().all(|byte| matches!(byte, b'0' | b'_'))
}

fn operator_operand_type(ty: &Type) -> String {
    if binary_operand_is_text(ty) {
        return "Text".to_owned();
    }
    if binary_operand_is_numeric(ty) {
        return if binary_operand_is_float(ty) {
            "Float".to_owned()
        } else {
            "Int".to_owned()
        };
    }
    if matches!(ty, Type::Variant(row) if literal_variant_base(row) == Some(LiteralBase::Bool)) {
        return "Bool".to_owned();
    }
    display_inferred_type(ty).render()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EqualityCompatibility {
    Comparable,
    Mismatched,
    Unknown,
}

impl EqualityCompatibility {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Mismatched, _) | (_, Self::Mismatched) => Self::Mismatched,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Comparable, Self::Comparable) => Self::Comparable,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EqualityBaseKind {
    Bool,
    Text,
    Number,
}

fn equality_base_kind(ty: &Type) -> Option<EqualityBaseKind> {
    match ty {
        Type::Named(name) if name == "Bool" => Some(EqualityBaseKind::Bool),
        Type::Named(name) if name == "Text" => Some(EqualityBaseKind::Text),
        Type::Named(name) if name == "Int" || name == "Float" => Some(EqualityBaseKind::Number),
        Type::Variant(row) => match literal_variant_base(row) {
            Some(LiteralBase::Bool) => Some(EqualityBaseKind::Bool),
            Some(LiteralBase::Text) => Some(EqualityBaseKind::Text),
            Some(LiteralBase::Number) => Some(EqualityBaseKind::Number),
            None => None,
        },
        Type::Deferred
        | Type::Named(_)
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Recursive(_)
        | Type::Apply { .. }
        | Type::Function { .. }
        | Type::Optional(_)
        | Type::Nullable(_)
        | Type::Tuple(_)
        | Type::Record(_)
        | Type::SlotRecord { .. } => None,
    }
}

fn equality_sequence_compatibility(
    left: &[Type],
    right: &[Type],
    mut compare: impl FnMut(&Type, &Type) -> EqualityCompatibility,
) -> EqualityCompatibility {
    if left.len() != right.len() {
        return EqualityCompatibility::Mismatched;
    }

    left.iter()
        .zip(right)
        .fold(EqualityCompatibility::Comparable, |compatibility, pair| {
            compatibility.and(compare(pair.0, pair.1))
        })
}

fn is_array_constructor(ty: &Type) -> bool {
    matches!(ty, Type::Named(name) if name == "Array")
}

fn row_field_type<'r>(row: &'r Row, field: &str) -> Option<&'r Type> {
    row.entries.iter().find_map(|entry| match entry {
        RowEntry::Field { name, ty } if name == field => Some(ty),
        RowEntry::Field { .. } | RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
    })
}

fn is_resolved_operator_operand(ty: &Type) -> bool {
    is_resolved_value_type(ty) && !type_has_open_row(ty)
}

/// Whether a resolved left operand of `??` might still be empty at runtime.
/// Optional/Nullable wrappers, and the empty types themselves, can be empty.
/// Everything else (including `Result`) cannot.
fn coalesce_left_can_be_empty(ty: &Type) -> bool {
    let (empties, core) = peel_empty_values(ty);
    if !empties.is_empty() {
        return true;
    }
    matches!(
        core,
        Type::Named(name) if name == "Null" || name == "Undefined"
    )
}

fn type_has_open_row(ty: &Type) -> bool {
    match ty {
        Type::Apply { callee, args } => {
            type_has_open_row(callee) || args.iter().any(type_has_open_row)
        }
        Type::Function { params, result, .. } => {
            params.iter().any(type_has_open_row) || type_has_open_row(result)
        }
        Type::Optional(inner) | Type::Nullable(inner) => type_has_open_row(inner),
        Type::Tuple(items) => items.iter().any(type_has_open_row),
        Type::Variant(row) if literal_variant_base(row).is_some() => false,
        Type::Record(row) | Type::Variant(row) => {
            row.tail == RowTail::Open
                || row.entries.iter().any(|entry| match entry {
                    RowEntry::Field { ty, .. } => type_has_open_row(ty),
                    RowEntry::Tag { payload, .. } => payload.iter().any(type_has_open_row),
                    RowEntry::Literal { .. } => false,
                })
        }
        Type::SlotRecord { data, slots } => [data, slots].into_iter().any(|row| {
            row.tail == RowTail::Open
                || row.entries.iter().any(|entry| match entry {
                    RowEntry::Field { ty, .. } => type_has_open_row(ty),
                    RowEntry::Tag { payload, .. } => payload.iter().any(type_has_open_row),
                    RowEntry::Literal { .. } => false,
                })
        }),
        Type::Deferred
        | Type::Named(_)
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Recursive(_) => false,
    }
}

fn operator_operand_note(operator: &str) -> &'static str {
    match operator {
        "+" => "`+` accepts two numbers or two Text values",
        "-" | "*" | "/" | "%" | "^" => "this operator accepts numeric operands",
        "<" | "<=" | ">" | ">=" => "this operator accepts numeric operands",
        "==" | "!=" => "both operands must have compatible types",
        "&&" | "||" | "!" => "this operator accepts Bool operands",
        _ => "use operands supported by this operator",
    }
}

fn binary_operand_is_text(ty: &Type) -> bool {
    matches!(named_type_name(ty), Some("Text"))
        || matches!(ty, Type::Variant(row) if literal_variant_base(row) == Some(LiteralBase::Text))
}

fn binary_operand_is_numeric(ty: &Type) -> bool {
    numeric_type_name(ty).is_some()
        || matches!(ty, Type::Variant(row) if literal_variant_base(row) == Some(LiteralBase::Number))
}

fn binary_operand_is_float(ty: &Type) -> bool {
    if numeric_type_name(ty) == Some("Float") {
        return true;
    }
    matches!(ty, Type::Variant(row)
        if literal_variant_base(row) == Some(LiteralBase::Number)
            && row.entries.iter().any(|entry| matches!(entry, RowEntry::Literal { value: Literal::Number(number) } if is_float_literal_text(number))))
}

fn fold_binary_literals(operator: &str, left: &Literal, right: &Literal) -> Option<Literal> {
    match (operator, left, right) {
        ("&&", Literal::Bool(left), Literal::Bool(right)) => Some(Literal::Bool(*left && *right)),
        ("||", Literal::Bool(left), Literal::Bool(right)) => Some(Literal::Bool(*left || *right)),
        ("+", Literal::String(left), Literal::String(right)) => {
            let mut text = decode_string_literal(left);
            text.push_str(&decode_string_literal(right));
            Some(Literal::String(quote_string_literal(&text)))
        }
        ("+" | "-" | "*" | "/", Literal::Number(left), Literal::Number(right)) => {
            fold_number_arithmetic(operator, left, right)
        }
        ("==" | "!=", Literal::Bool(left), Literal::Bool(right)) => {
            Some(Literal::Bool(compare_equal(operator, left == right)))
        }
        ("==" | "!=", Literal::String(left), Literal::String(right)) => {
            let equal = decode_string_literal(left) == decode_string_literal(right);
            Some(Literal::Bool(compare_equal(operator, equal)))
        }
        ("==" | "!=" | "<" | "<=" | ">" | ">=", Literal::Number(left), Literal::Number(right)) => {
            fold_number_comparison(operator, left, right).map(Literal::Bool)
        }
        _ => None,
    }
}

fn fold_unary_literal(operator: &str, value: &Literal) -> Option<Literal> {
    match (operator, value) {
        ("!", Literal::Bool(value)) => Some(Literal::Bool(!value)),
        ("-", Literal::Number(value)) => fold_number_negation(value),
        _ => None,
    }
}

fn fold_number_arithmetic(operator: &str, left: &str, right: &str) -> Option<Literal> {
    match (parse_fold_number(left)?, parse_fold_number(right)?) {
        (FoldNumber::Int(left), FoldNumber::Int(right)) => {
            fold_int_arithmetic(operator, left, right)
                .map(|value| Literal::Number(value.to_string()))
        }
        (FoldNumber::Float(left), FoldNumber::Float(right)) => {
            fold_float_arithmetic(operator, left, right)
                .and_then(format_float_literal)
                .map(Literal::Number)
        }
        (FoldNumber::Int(_), FoldNumber::Float(_)) | (FoldNumber::Float(_), FoldNumber::Int(_)) => {
            None
        }
    }
}

fn fold_int_arithmetic(operator: &str, left: i64, right: i64) -> Option<i64> {
    if operator == "/" && right == 0 {
        return None;
    }

    match operator {
        "+" => left.checked_add(right),
        "-" => left.checked_sub(right),
        "*" => left.checked_mul(right),
        "/" => left.checked_div(right),
        _ => None,
    }
}

fn fold_float_arithmetic(operator: &str, left: f64, right: f64) -> Option<f64> {
    if operator == "/" && is_float_zero(right) {
        return None;
    }

    match operator {
        "+" => Some(left + right),
        "-" => Some(left - right),
        "*" => Some(left * right),
        "/" => Some(left / right),
        _ => None,
    }
}

fn fold_number_negation(value: &str) -> Option<Literal> {
    match parse_fold_number(value)? {
        FoldNumber::Int(value) => value
            .checked_neg()
            .map(|value| Literal::Number(value.to_string())),
        FoldNumber::Float(value) => format_float_literal(-value).map(Literal::Number),
    }
}

fn fold_number_comparison(operator: &str, left: &str, right: &str) -> Option<bool> {
    match (parse_fold_number(left)?, parse_fold_number(right)?) {
        (FoldNumber::Int(left), FoldNumber::Int(right)) => {
            Some(compare_ordering(operator, left.cmp(&right)))
        }
        (FoldNumber::Float(left), FoldNumber::Float(right)) => {
            numeric_ordering(left, right).map(|ordering| compare_ordering(operator, ordering))
        }
        (FoldNumber::Int(_), FoldNumber::Float(_)) | (FoldNumber::Float(_), FoldNumber::Int(_)) => {
            None
        }
    }
}

fn compare_ordering(operator: &str, ordering: Ordering) -> bool {
    match operator {
        "==" => compare_equal(operator, ordering == Ordering::Equal),
        "!=" => compare_equal(operator, ordering == Ordering::Equal),
        "<" => ordering == Ordering::Less,
        "<=" => ordering != Ordering::Greater,
        ">" => ordering == Ordering::Greater,
        ">=" => ordering != Ordering::Less,
        _ => false,
    }
}

fn compare_equal(operator: &str, equal: bool) -> bool {
    if operator == "==" { equal } else { !equal }
}

fn parse_fold_number(text: &str) -> Option<FoldNumber> {
    let normalized = text.replace('_', "");
    if is_float_literal_text(text) {
        normalized.parse::<f64>().ok().map(FoldNumber::Float)
    } else {
        normalized.parse::<i64>().ok().map(FoldNumber::Int)
    }
}

fn format_float_literal(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }

    let mut text = value.to_string();
    if !is_float_literal_text(&text) {
        text.push_str(".0");
    }
    if text.parse::<f64>().ok()?.to_bits() == value.to_bits() {
        Some(text)
    } else {
        None
    }
}

fn numeric_ordering(left: f64, right: f64) -> Option<Ordering> {
    left.partial_cmp(&right)
}

fn is_float_zero(value: f64) -> bool {
    value.to_bits() << 1 == 0
}

pub(super) fn is_float_literal_text(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
}

fn quote_string_literal(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}
