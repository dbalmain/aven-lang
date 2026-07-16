use super::method_sets::builtin_method_signature;
use super::*;

impl<'a> Checker<'a> {
    pub(super) fn declared_method_scheme(&mut self, name: &str, ty: &Type) -> TypeScheme {
        let (scheme, names) = super::scheme_from_global_with_names(ty, &mut self.unifier);
        let requirements = self
            .bindings
            .get(name)
            .and_then(|binding| *binding)
            .and_then(|binding| match &ungroup_expr(&binding.value).kind {
                ExprKind::Lambda { requirements, .. } => Some(requirements.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if requirements.is_empty() {
            return scheme;
        }

        self.inline_lambda_type_var_scopes.push(names);
        let predicates = self.requirement_predicates(&requirements);
        self.inline_lambda_type_var_scopes.pop();
        self.qualify_scheme(scheme, predicates, Some(name), &[])
    }

    pub(super) fn instantiate_scheme_at(&mut self, scheme: &TypeScheme, call_span: Span) -> Type {
        let (ty, mut predicates) = self.unifier.instantiate_scheme(scheme);
        for predicate in &mut predicates {
            predicate.call_span = Some(call_span);
        }
        self.method_obligations.extend(predicates);
        self.simplify_method_obligations(false);
        ty
    }

    pub(super) fn method_obligation_marker(&self) -> usize {
        self.method_obligations.len()
    }

    pub(super) fn take_method_obligations_since(&mut self, marker: usize) -> Vec<MethodPredicate> {
        self.method_obligations.split_off(marker)
    }

    pub(super) fn add_operator_obligation(
        &mut self,
        candidate: Type,
        member: &str,
        param: Type,
        result: Type,
        operator_span: Span,
    ) {
        self.method_obligations.push(MethodPredicate {
            candidate: candidate.clone(),
            member: member.to_owned(),
            params: vec![param],
            result,
            operator_span,
            binding: None,
            call_span: None,
        });
        self.simplify_method_obligations(false);
    }

    pub(super) fn simplify_method_obligations(&mut self, finalizing: bool) {
        let pending = std::mem::take(&mut self.method_obligations);
        for predicate in pending {
            let predicate = self.resolve_method_predicate(&predicate);
            if self.method_predicate_is_entailed(&predicate) {
                continue;
            }

            match &predicate.candidate {
                Type::Meta(_) | Type::Variable(_) => self.method_obligations.push(predicate),
                Type::Deferred if finalizing => {
                    self.report_unresolved_method_receiver(&predicate);
                }
                Type::Deferred => self.method_obligations.push(predicate),
                owner => self.discharge_known_method_predicate(owner, &predicate),
            }
        }
    }

    fn resolve_method_predicate(&self, predicate: &MethodPredicate) -> MethodPredicate {
        let resolved_candidate = self.normalize(&self.resolve_and_default(&predicate.candidate));
        let candidate = widen_literal_method_owner(&resolved_candidate);
        let resolve_signature_type = |ty: &Type| {
            let resolved = self.normalize(&self.resolve_and_default(ty));
            map_type(&resolved, &mut |node| {
                (node == &resolved_candidate).then(|| candidate.clone())
            })
        };
        MethodPredicate {
            candidate: candidate.clone(),
            member: predicate.member.clone(),
            params: predicate
                .params
                .iter()
                .map(resolve_signature_type)
                .collect(),
            result: resolve_signature_type(&predicate.result),
            operator_span: predicate.operator_span,
            binding: predicate.binding.clone(),
            call_span: predicate.call_span,
        }
    }

    fn method_predicate_is_entailed(&mut self, predicate: &MethodPredicate) -> bool {
        let assumptions = self
            .method_assumption_scopes
            .iter()
            .rev()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        assumptions.into_iter().any(|assumption| {
            let assumption = self.resolve_method_predicate(&assumption);
            if assumption.member != predicate.member
                || assumption.candidate != predicate.candidate
                || assumption.params.len() != predicate.params.len()
            {
                return false;
            }

            let snapshot = self.unifier.snapshot();
            let matches = assumption
                .params
                .iter()
                .zip(&predicate.params)
                .all(|(expected, actual)| self.unifier.unify(expected, actual).is_ok())
                && self
                    .unifier
                    .unify(&assumption.result, &predicate.result)
                    .is_ok();
            if !matches {
                self.unifier.restore(snapshot);
            }
            matches
        })
    }

    fn discharge_known_method_predicate(&mut self, owner: &Type, predicate: &MethodPredicate) {
        let Some(actual) = builtin_method_signature(owner, &predicate.member) else {
            self.report_missing_method(owner, predicate);
            return;
        };

        let snapshot = self.unifier.snapshot();
        let matches = actual.params.len() == predicate.params.len()
            && actual
                .params
                .iter()
                .zip(&predicate.params)
                .all(|(actual, expected)| self.unifier.unify(actual, expected).is_ok())
            && self
                .unifier
                .unify(&actual.result, &predicate.result)
                .is_ok();
        if !matches {
            self.unifier.restore(snapshot);
            self.report_method_signature_mismatch(owner, &actual, predicate);
        }
    }

    pub(super) fn requirement_predicates(
        &mut self,
        requirements: &[Requirement],
    ) -> Vec<MethodPredicate> {
        let mut predicates = Vec::new();
        for requirement in requirements {
            let candidate = self.requirement_candidate(&requirement.name);
            let mut visiting = HashSet::new();
            self.collect_requirement_bound(
                &requirement.bound,
                &requirement.name,
                &candidate,
                &mut visiting,
                &mut predicates,
            );
        }
        predicates
    }

    fn requirement_candidate(&mut self, name: &str) -> Type {
        if let Some(candidate) = self
            .inline_lambda_type_var_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
        {
            return candidate.clone();
        }
        if self.is_rigid_type_var(name) {
            return Type::Variable(name.to_owned());
        }

        let candidate = self.unifier.fresh();
        if let Some(scope) = self.inline_lambda_type_var_scopes.last_mut() {
            scope.insert(name.to_owned(), candidate.clone());
        }
        candidate
    }

    fn collect_requirement_bound(
        &mut self,
        bound: &Expr,
        candidate_name: &str,
        candidate: &Type,
        visiting: &mut HashSet<String>,
        predicates: &mut Vec<MethodPredicate>,
    ) {
        match &ungroup_expr(bound).kind {
            ExprKind::Record(entries) => self.collect_requirement_entries(
                entries,
                candidate_name,
                candidate,
                visiting,
                predicates,
            ),
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                if !visiting.insert(name.clone()) {
                    return;
                }
                let value = self
                    .bindings
                    .get(name)
                    .and_then(|binding| *binding)
                    .map(|binding| binding.value.clone());
                if let Some(value) = value {
                    self.collect_requirement_bound(
                        &value,
                        candidate_name,
                        candidate,
                        visiting,
                        predicates,
                    );
                } else {
                    self.report_unknown_requirement(name, bound.span);
                }
                visiting.remove(name);
            }
            _ => self.report_invalid_requirement_bound(bound.span),
        }
    }

    fn collect_requirement_entries(
        &mut self,
        entries: &[RecordEntry],
        candidate_name: &str,
        candidate: &Type,
        visiting: &mut HashSet<String>,
        predicates: &mut Vec<MethodPredicate>,
    ) {
        if !matches!(entries.last(), Some(RecordEntry::Open { .. })) {
            self.report_requirement_needs_open(entries);
        }

        for entry in entries {
            match entry {
                RecordEntry::Field {
                    name,
                    name_span,
                    value,
                    ..
                } => {
                    let ExprKind::Arrow { params, result } = &ungroup_expr(value).kind else {
                        self.report_invalid_requirement_member(name, value.span);
                        continue;
                    };
                    self.requirement_self_scopes.push(candidate.clone());
                    let params = self.lower_annotations(params);
                    let result = self.lower_annotation(result);
                    self.requirement_self_scopes.pop();
                    let replace_candidate = |ty: &Type| {
                        map_type(ty, &mut |node| match node {
                            Type::Variable(name) if name == candidate_name => {
                                Some(candidate.clone())
                            }
                            _ => None,
                        })
                    };
                    predicates.push(MethodPredicate {
                        candidate: candidate.clone(),
                        member: name.clone(),
                        params: params.iter().map(replace_candidate).collect(),
                        result: replace_candidate(&result),
                        operator_span: *name_span,
                        binding: None,
                        call_span: None,
                    });
                }
                RecordEntry::Spread { value, .. } => self.collect_requirement_bound(
                    value,
                    candidate_name,
                    candidate,
                    visiting,
                    predicates,
                ),
                RecordEntry::Open { .. } => {}
                _ => self.report_invalid_requirement_member("this entry", record_entry_span(entry)),
            }
        }
    }

    pub(super) fn push_method_assumptions(&mut self, assumptions: Vec<MethodPredicate>) {
        self.method_assumption_scopes.push(assumptions);
        self.simplify_method_obligations(false);
    }

    pub(super) fn pop_method_assumptions(&mut self) -> Vec<MethodPredicate> {
        self.method_assumption_scopes.pop().unwrap_or_default()
    }

    pub(super) fn finalize_lambda_requirements(
        &mut self,
        marker: usize,
        requirements: &[Requirement],
        assumptions: Vec<MethodPredicate>,
    ) {
        self.simplify_method_obligations(true);
        let residual = self.take_method_obligations_since(marker);
        if requirements.is_empty() {
            self.method_obligations.extend(residual);
        } else {
            for predicate in residual {
                self.report_missing_requirement(&predicate);
            }
        }
        self.method_obligations.extend(assumptions);
    }

    pub(super) fn finish_checked_lambda_obligations(&mut self, marker: usize) {
        self.simplify_method_obligations(false);
        let _ = self.take_method_obligations_since(marker);
    }

    pub(super) fn finish_non_generalizing_lambda_obligations(&mut self, marker: usize) {
        self.simplify_method_obligations(true);
        for predicate in self.take_method_obligations_since(marker) {
            self.report_missing_requirement(&predicate);
        }
    }

    pub(super) fn checking_embedded_std_before_constraint_migration(&self) -> bool {
        matches!(
            &self.module_identity,
            comptime::ComptimeModuleIdentity::Specifier(specifier)
                if specifier.starts_with("std/")
        )
    }

    pub(super) fn qualify_scheme(
        &mut self,
        mut scheme: TypeScheme,
        mut predicates: Vec<MethodPredicate>,
        binding: Option<&str>,
        env_metas: &[u32],
    ) -> TypeScheme {
        for predicate in &mut predicates {
            *predicate = self.resolve_method_predicate(predicate);
            if predicate.binding.is_none() {
                predicate.binding = binding.map(str::to_owned);
            }
        }

        predicates = deduplicate_predicates(predicates);
        predicates.retain(|predicate| !self.reject_relational_predicate(predicate));

        let env_metas: HashSet<_> = env_metas.iter().copied().collect();
        let mut quantified: HashSet<_> = scheme.vars.iter().copied().collect();
        for predicate in &predicates {
            for id in predicate_free_metas(predicate) {
                if !env_metas.contains(&id) && quantified.insert(id) {
                    scheme.vars.push(id);
                }
            }
        }

        let ordinary = free_metas(&scheme.ty).into_iter().collect::<HashSet<_>>();
        let ambiguous = scheme.vars.iter().copied().find(|id| {
            !ordinary.contains(id)
                && predicates
                    .iter()
                    .any(|predicate| predicate_contains_meta(predicate, *id))
        });
        if let Some(id) = ambiguous {
            self.report_ambiguous_method_scheme(binding, id, &predicates);
            predicates.retain(|predicate| !predicate_contains_meta(predicate, id));
            scheme.vars.retain(|candidate| *candidate != id);
        }

        scheme.predicates = predicates;
        scheme
    }

    pub(super) fn generalize_method_obligations(
        &mut self,
        resolved: Type,
        env_metas: &[u32],
        env_row_vars: &[u32],
        marker: usize,
        binding: Option<&str>,
    ) -> TypeScheme {
        self.simplify_method_obligations(true);
        let predicates = self.take_method_obligations_since(marker);
        let scheme = self.generalize_with_row_merges(resolved, env_metas, env_row_vars);
        self.qualify_scheme(scheme, predicates, binding, env_metas)
    }

    fn reject_relational_predicate(&mut self, predicate: &MethodPredicate) -> bool {
        let candidate_metas = free_metas(&predicate.candidate)
            .into_iter()
            .collect::<HashSet<_>>();
        let candidate_vars = type_variable_names(&predicate.candidate);
        let signature_metas = predicate
            .params
            .iter()
            .flat_map(free_metas)
            .chain(free_metas(&predicate.result))
            .any(|id| !candidate_metas.contains(&id));
        let signature_vars = predicate
            .params
            .iter()
            .flat_map(type_variable_names)
            .chain(type_variable_names(&predicate.result))
            .any(|name| !candidate_vars.contains(&name));
        if signature_metas || signature_vars {
            self.report_relational_requirement(predicate);
            true
        } else {
            false
        }
    }

    pub(super) fn validate_named_requirement(&mut self, value: &Expr) {
        let candidate = Type::Variable("Self".to_owned());
        let mut visiting = HashSet::new();
        let mut predicates = Vec::new();
        self.collect_requirement_bound(
            value,
            "__named_requirement_candidate",
            &candidate,
            &mut visiting,
            &mut predicates,
        );
        for predicate in predicates {
            self.reject_relational_predicate(&predicate);
        }
    }

    fn report_missing_method(&mut self, owner: &Type, predicate: &MethodPredicate) {
        let owner = display_inferred_type(owner).render();
        let required = render_predicate_requirement(predicate);
        let mut diagnostic = Diagnostic::error(format!(
            "`{owner}` does not satisfy `{required}`: method `{}` is missing",
            predicate.member
        ))
        .with_code(codes::ty::INVALID_OPERATOR_OPERANDS)
        .with_label(Label::primary(
            predicate.call_span.unwrap_or(predicate.operator_span),
            format!("`{owner}` has no `{}` method", predicate.member),
        ))
        .with_label(Label::primary(
            predicate.operator_span,
            "operator requirement originated here",
        ))
        .with_note(format!(
            "expected `{}` after substituting `{owner}` for `Self`",
            render_method_signature(&predicate.params, &predicate.result)
        ));
        if let Some(binding) = &predicate.binding {
            diagnostic = diagnostic.with_note(format!(
                "required while instantiating generic binding `{binding}` at this call site"
            ));
        }
        self.push_unique_diagnostic(diagnostic);
    }

    fn report_method_signature_mismatch(
        &mut self,
        owner: &Type,
        actual: &super::method_sets::MethodSignature,
        predicate: &MethodPredicate,
    ) {
        let owner = display_inferred_type(owner).render();
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "`{owner}` does not satisfy `{}`: method `{}` has an incompatible signature",
                render_predicate_requirement(predicate),
                predicate.member
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                predicate.call_span.unwrap_or(predicate.operator_span),
                "the concrete method does not match this requirement",
            ))
            .with_label(Label::primary(
                predicate.operator_span,
                "operator requirement originated here",
            ))
            .with_note(format!(
                "actual: `{}`",
                render_method_signature(&actual.params, &actual.result)
            ))
            .with_note(format!(
                "expected after substituting `{owner}` for `Self`: `{}`",
                render_method_signature(&predicate.params, &predicate.result)
            )),
        );
    }

    fn report_unresolved_method_receiver(&mut self, predicate: &MethodPredicate) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "operator `{}` has an unresolved receiver",
                predicate.member
            ))
            .with_code(codes::ty::UNRESOLVED_BINDING)
            .with_label(Label::primary(
                predicate.operator_span,
                "the left operand's type is still unknown here",
            ))
            .with_note(
                "add a concrete annotation or a method requirement to the surrounding binding",
            ),
        );
    }

    fn report_missing_requirement(&mut self, predicate: &MethodPredicate) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "operator `{}` is not covered by this binding's declared requirements",
                predicate.member
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                predicate.operator_span,
                "this generic operator use needs another requirement line",
            ))
            .with_note(format!(
                "add a requirement such as `t: {}` for the left operand's generic type",
                render_predicate_requirement(predicate)
            )),
        );
    }

    fn report_relational_requirement(&mut self, predicate: &MethodPredicate) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "operator `{}` relates more than one generic type, which is not supported in v0",
                predicate.member
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                predicate.operator_span,
                "this method signature contains a free scheme variable other than its candidate",
            ))
            .with_note(format!(
                "the reserved later form is `Op(t, \"{}\", u) = w`",
                predicate.member
            ))
            .with_note(
                "for v0, pass the operation explicitly as a parameter of type `(t, u) -> w`",
            ),
        );
    }

    fn report_ambiguous_method_scheme(
        &mut self,
        binding: Option<&str>,
        id: u32,
        predicates: &[MethodPredicate],
    ) {
        let predicate = predicates
            .iter()
            .find(|predicate| predicate_contains_meta(predicate, id));
        let span = predicate
            .map(|predicate| predicate.operator_span)
            .unwrap_or(Span::new(0, 0));
        let subject = binding.map_or_else(
            || "inferred binding".to_owned(),
            |binding| format!("binding `{binding}`"),
        );
        self.push_unique_diagnostic(
            Diagnostic::error(format!("{subject} has an ambiguous method requirement"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    span,
                    "this constrained type does not occur in the ordinary function type",
                ))
                .with_note(
                    "every constrained generic type must occur in a parameter or result type",
                ),
        );
    }

    fn report_unknown_requirement(&mut self, name: &str, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!("unknown method requirement `{name}`"))
                .with_code(codes::ty::UNKNOWN_NAME)
                .with_label(Label::primary(span, "requirement row not found"))
                .with_note("define an open method row or write the requirement inline"),
        );
    }

    fn report_invalid_requirement_bound(&mut self, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error("method requirement must name or contain an open method row")
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, "not a method requirement row")),
        );
    }

    fn report_invalid_requirement_member(&mut self, member: &str, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "method requirement member `{member}` must be a method signature"
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "expected a method signature such as `<(Self): Bool`",
            )),
        );
    }

    fn report_requirement_needs_open(&mut self, entries: &[RecordEntry]) {
        let span = entries
            .first()
            .zip(entries.last())
            .map(|(first, last)| record_entry_span(first).merge(record_entry_span(last)))
            .unwrap_or(Span::new(0, 0));
        self.push_unique_diagnostic(
            Diagnostic::error("missing `..` on a method bound")
                .with_code(codes::parse::MISSING_METHOD_BOUND_OPEN)
                .with_label(Label::primary(span, "method bounds must end with `..`"))
                .with_note("insert `..` before the closing `}`"),
        );
    }
}

pub(crate) fn is_method_requirement_row(expr: &Expr) -> bool {
    let ExprKind::Record(entries) = &ungroup_expr(expr).kind else {
        return false;
    };
    entries.iter().all(|entry| {
        matches!(
            entry,
            RecordEntry::Field {
                value: Expr {
                    kind: ExprKind::Arrow { .. },
                    ..
                },
                ..
            } | RecordEntry::Spread { .. }
                | RecordEntry::Open { .. }
        )
    }) && entries.iter().any(|entry| {
        matches!(
            entry,
            RecordEntry::Field {
                value: Expr {
                    kind: ExprKind::Arrow { .. },
                    ..
                },
                ..
            } | RecordEntry::Spread { .. }
        )
    })
}

fn deduplicate_predicates(predicates: Vec<MethodPredicate>) -> Vec<MethodPredicate> {
    let mut deduplicated = Vec::new();
    for predicate in predicates {
        let duplicate = deduplicated.iter().any(|existing: &MethodPredicate| {
            existing.candidate == predicate.candidate
                && existing.member == predicate.member
                && existing.params == predicate.params
                && existing.result == predicate.result
        });
        if !duplicate {
            deduplicated.push(predicate);
        }
    }
    deduplicated
}

fn predicate_free_metas(predicate: &MethodPredicate) -> impl Iterator<Item = u32> + '_ {
    free_metas(&predicate.candidate)
        .into_iter()
        .chain(predicate.params.iter().flat_map(free_metas))
        .chain(free_metas(&predicate.result))
}

fn predicate_contains_meta(predicate: &MethodPredicate, id: u32) -> bool {
    predicate_free_metas(predicate).any(|candidate| candidate == id)
}

fn record_entry_span(entry: &RecordEntry) -> Span {
    match entry {
        RecordEntry::Field { span, .. }
        | RecordEntry::FieldComputed { span, .. }
        | RecordEntry::Shorthand { span, .. }
        | RecordEntry::Spread { span, .. }
        | RecordEntry::Delete { span, .. }
        | RecordEntry::DeleteComputed { span, .. }
        | RecordEntry::Rename { span, .. }
        | RecordEntry::Iteration { span, .. }
        | RecordEntry::Open { span } => *span,
        RecordEntry::Element(value) => value.span,
    }
}

fn render_predicate_requirement(predicate: &MethodPredicate) -> String {
    let params = predicate
        .params
        .iter()
        .map(|param| render_relative_type(param, &predicate.candidate, true))
        .collect::<Vec<_>>()
        .join(", ");
    let replace_result = !matches!(predicate.member.as_str(), "<" | "<=" | ">" | ">=");
    let result = render_relative_type(&predicate.result, &predicate.candidate, replace_result);
    format!("{{ {}({params}): {result}, .. }}", predicate.member)
}

fn render_relative_type(ty: &Type, candidate: &Type, replace_candidate: bool) -> String {
    map_type(ty, &mut |node| {
        (replace_candidate && node == candidate).then(|| Type::Named("Self".to_owned()))
    })
    .render()
}

fn render_method_signature(params: &[Type], result: &Type) -> String {
    let params = if params.len() == 1 {
        params[0].render()
    } else {
        format!(
            "({})",
            params
                .iter()
                .map(Type::render)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!("{params} -> {}", result.render())
}

fn widen_literal_method_owner(ty: &Type) -> Type {
    let Type::Variant(row) = ty else {
        return ty.clone();
    };
    match literal_variant_base(row) {
        Some(LiteralBase::Bool) => named_builtin("Bool"),
        Some(LiteralBase::Text) => named_builtin("Text"),
        Some(LiteralBase::Number) => {
            let float = row.entries.iter().any(|entry| {
                matches!(
                    entry,
                    RowEntry::Literal {
                        value: Literal::Number(number)
                    } if number.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
                )
            });
            named_builtin(if float { "Float" } else { "Int" })
        }
        None => ty.clone(),
    }
}
