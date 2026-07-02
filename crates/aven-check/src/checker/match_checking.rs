use super::*;

impl<'a> Checker<'a> {
    pub(super) fn check_match_arms(
        &mut self,
        subject: &Expr,
        arms: &[MatchArm],
        expected: Option<&Type>,
    ) {
        self.check_value_expr(subject);
        let env = self.local_types.inference_env();
        let inferred_subject = self.infer(&env, subject);
        let resolved_subject = self.normalize(&self.resolve_and_default(&inferred_subject));
        self.check_match_exhaustiveness(subject, arms, &resolved_subject);
        let subject_type = is_resolved_value_type(&resolved_subject).then_some(resolved_subject);

        let mut body_types = Vec::new();
        for arm in arms {
            self.local_types.push();
            let local_types = checked_pattern_local_types(&arm.pattern, subject_type.as_ref());
            for mismatch in &local_types.mismatches {
                self.report_or_pattern_binding_mismatch(mismatch);
            }
            for (name, ty) in local_types.bindings {
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
                let env = self.local_types.inference_env();
                let ty = self.infer_match_arm_body_type_for_check(&env, &arm.body);
                body_types.push(MatchArmBodyType {
                    span: arm.body.span,
                    ty,
                });
            }
            self.local_types.pop();
        }

        if expected.is_none() {
            self.report_incompatible_match_arm_results(subject, arms, &body_types);
        }
    }

    pub(super) fn infer_match_arm_body_type_for_check(
        &mut self,
        env: &TypeEnv,
        body: &Expr,
    ) -> Type {
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred_types_len = self.inferred_types.len();
        let ty = self.infer(env, body);
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        self.inferred_types.truncate(inferred_types_len);
        ty
    }

    pub(super) fn report_incompatible_match_arm_results(
        &mut self,
        subject: &Expr,
        arms: &[MatchArm],
        body_types: &[MatchArmBodyType],
    ) {
        if body_types.len() < 2
            || self.comptime_selected_match_arm(subject, arms).is_some()
            || self.expr_references_unresolved_comptime_param(subject)
        {
            return;
        }

        let snapshot = self.unifier.snapshot();
        let conflict = match self.combine_match_arm_body_types(body_types) {
            MatchArmCombination::Joined(_) => None,
            MatchArmCombination::Conflict(conflict) => {
                self.runtime_match_arm_type_conflict(&conflict)
            }
        };
        self.unifier.restore(snapshot);

        if let Some(conflict) = conflict {
            self.report_incompatible_match_arm_type_conflict(conflict);
        }
    }

    pub(super) fn runtime_match_arm_type_conflict(
        &self,
        conflict: &MatchArmTypeConflict,
    ) -> Option<RuntimeMatchArmTypeConflict> {
        let earlier = self.resolved_match_result_type(&conflict.earlier_ty);
        let diverging = self.resolved_match_result_type(&conflict.diverging_ty);
        if !is_resolved_value_type(&earlier) || !is_resolved_value_type(&diverging) {
            return None;
        }

        Some(RuntimeMatchArmTypeConflict {
            earlier: display_inferred_type(&earlier).render(),
            diverging: display_inferred_type(&diverging).render(),
            diverging_span: conflict.diverging_span,
        })
    }

    pub(super) fn report_incompatible_match_arm_type_conflict(
        &mut self,
        conflict: RuntimeMatchArmTypeConflict,
    ) {
        self.diagnostics.push(
            Diagnostic::error("match arms produce incompatible types")
                .with_code(codes::ty::INCOMPATIBLE_MATCH_ARMS)
                .with_label(Label::primary(
                    conflict.diverging_span,
                    format!("this arm produces `{}`", conflict.diverging),
                ))
                .with_note(format!(
                    "earlier arms produce `{}`; make all arms produce the same type, or annotate the match result type",
                    conflict.earlier
                )),
        );
    }

    pub(super) fn expr_references_unresolved_comptime_param(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                self.unresolved_comptime_param_is_in_scope(name)
            }
            _ => {
                let mut found = false;
                walk_expr_children(expr, &mut |child| {
                    if !found && self.expr_references_unresolved_comptime_param(child) {
                        found = true;
                    }
                });
                found
            }
        }
    }

    pub(super) fn unresolved_comptime_param_is_in_scope(&self, name: &str) -> bool {
        self.lookup_comptime_value(name).is_none()
            && self
                .local_comptime_params
                .iter()
                .rev()
                .any(|scope| scope.contains(name))
    }

    pub(super) fn check_match_exhaustiveness(
        &mut self,
        subject: &Expr,
        arms: &[MatchArm],
        subject_type: &Type,
    ) {
        let subject_type = self.normalize(subject_type);
        if type_contains_deferred(&subject_type) {
            return;
        }
        let (empty_values, payload_type) = peel_empty_values(&subject_type);
        if !empty_values.is_empty() {
            let missing = empty_values
                .iter()
                .copied()
                .filter(|value| !empty_value_is_covered(arms, *value))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                self.report_missing_empty_match_values(&missing, subject.span);
            }
        }

        let Type::Variant(row) = payload_type else {
            return;
        };

        let entry_kind = if row
            .entries
            .iter()
            .all(|entry| matches!(entry, RowEntry::Tag { .. }))
        {
            VariantEntryKind::Tag
        } else if row
            .entries
            .iter()
            .all(|entry| matches!(entry, RowEntry::Literal { .. }))
        {
            VariantEntryKind::Literal
        } else {
            return;
        };

        if entry_kind == VariantEntryKind::Literal && row.tail == RowTail::Closed {
            self.report_unreachable_literal_match_arms(row, arms);
        }

        let has_default = arms
            .iter()
            .any(|arm| arm.guards.is_empty() && pattern_has_catch_all_alternative(&arm.pattern));
        if has_default {
            return;
        }

        if matches!(row.tail, RowTail::Open | RowTail::Var(_)) {
            self.report_open_variant_non_exhaustive(subject.span);
            return;
        }

        match entry_kind {
            VariantEntryKind::Tag => {
                let covered: HashSet<_> = arms
                    .iter()
                    .filter(|arm| arm.guards.is_empty())
                    .flat_map(|arm| arm_covered_tags(&arm.pattern))
                    .collect();
                let mut seen = HashSet::new();
                let missing: Vec<_> = row
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        RowEntry::Tag { name, .. }
                            if !covered.contains(name.as_str()) && seen.insert(name.as_str()) =>
                        {
                            Some(name.as_str())
                        }
                        RowEntry::Tag { .. }
                        | RowEntry::Field { .. }
                        | RowEntry::Literal { .. } => None,
                    })
                    .collect();

                if !missing.is_empty() {
                    self.report_missing_variant_match_tags(&missing, subject.span);
                }
            }
            VariantEntryKind::Literal => {
                let covered: Vec<_> = arms
                    .iter()
                    .filter(|arm| arm.guards.is_empty())
                    .flat_map(|arm| {
                        arm_covered_literals(&arm.pattern)
                            .into_iter()
                            .map(|(literal, _)| literal)
                    })
                    .collect();
                let mut missing = Vec::new();
                for entry in &row.entries {
                    let RowEntry::Literal { value } = entry else {
                        continue;
                    };
                    if !covered.contains(&value) && !missing.contains(&value) {
                        missing.push(value);
                    }
                }

                if !missing.is_empty() {
                    self.report_missing_literal_match_members(&missing, subject.span);
                }
            }
        }
    }
    pub(super) fn infer_match(&mut self, env: &TypeEnv, subject: &Expr, arms: &[MatchArm]) -> Type {
        if arms.is_empty() {
            return Type::Deferred;
        }

        if let Some(arm) = self.comptime_selected_match_arm(subject, arms) {
            let inferred_subject = self.infer(env, subject);
            let subject_type = self.resolve_if_concrete(&inferred_subject);
            let mut arm_env = env.clone();
            for (name, ty) in pattern_local_types(&arm.pattern, subject_type.as_ref()) {
                arm_env.insert(name, ty);
            }

            return self.infer(&arm_env, &arm.body);
        }

        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred_subject = self.infer(env, subject);
        let subject_type = self.resolve_if_concrete(&inferred_subject);
        let mut body_types = Vec::new();

        for arm in arms {
            let mut arm_env = env.clone();
            for (name, ty) in pattern_local_types(&arm.pattern, subject_type.as_ref()) {
                arm_env.insert(name, ty);
            }

            body_types.push(MatchArmBodyType {
                span: arm.body.span,
                ty: self.infer(&arm_env, &arm.body),
            });
        }

        match self.combine_match_arm_body_types(&body_types) {
            MatchArmCombination::Joined(result_type) => result_type,
            MatchArmCombination::Conflict(_) => {
                self.unifier.restore(snapshot);
                self.restore_diagnostic_snapshot(diagnostic_snapshot);
                Type::Deferred
            }
        }
    }

    pub(super) fn comptime_selected_match_arm<'b>(
        &self,
        subject: &Expr,
        arms: &'b [MatchArm],
    ) -> Option<&'b MatchArm> {
        let bindings = self.current_comptime_value_bindings();
        let Evaluation::Evaluated(value) =
            comptime::evaluate_runtime_value(subject, &bindings).evaluation
        else {
            return None;
        };

        match value {
            comptime::ComptimeValue::Literal(_) | comptime::ComptimeValue::Bool(_) => arms
                .iter()
                .find(|arm| pattern_matches_comptime_value(&arm.pattern, &value)),
            comptime::ComptimeValue::ReifiedType(_) | comptime::ComptimeValue::LabelSet(_) => None,
        }
    }

    pub(super) fn combine_match_arm_body_types(
        &mut self,
        body_types: &[MatchArmBodyType],
    ) -> MatchArmCombination {
        if let Some(result) = self.union_match_variant_arm_body_types(body_types) {
            return result;
        }

        self.unify_match_arm_body_types(body_types)
    }

    pub(super) fn union_match_variant_arm_body_types(
        &mut self,
        body_types: &[MatchArmBodyType],
    ) -> Option<MatchArmCombination> {
        let mut entries = Vec::new();
        let mut open = false;
        let mut kind = None;
        let mut literal_kind = None;

        for body_type in body_types {
            let Type::Variant(row) = self.unifier.resolve(&body_type.ty) else {
                return None;
            };

            let prior = Type::Variant(Row {
                entries: entries.clone(),
                tail: if open { RowTail::Open } else { RowTail::Closed },
            });
            let mut arm_kind = None;
            for entry in &row.entries {
                let Some(entry_kind) = row_entry_variant_kind(entry) else {
                    return Some(MatchArmCombination::Conflict(
                        self.match_arm_type_conflict(prior, body_type),
                    ));
                };
                if arm_kind.is_some_and(|existing| existing != entry_kind) {
                    return Some(MatchArmCombination::Conflict(
                        self.match_arm_type_conflict(prior, body_type),
                    ));
                }
                arm_kind = Some(entry_kind);
            }

            if let (Some(existing), Some(incoming)) = (kind, arm_kind)
                && existing != incoming
            {
                return Some(MatchArmCombination::Conflict(
                    self.match_arm_type_conflict(prior, body_type),
                ));
            }
            kind = kind.or(arm_kind);

            if arm_kind == Some(VariantEntryKind::Literal) {
                let Some(incoming) = literal_variant_base(&row) else {
                    return Some(MatchArmCombination::Conflict(
                        self.match_arm_type_conflict(prior, body_type),
                    ));
                };
                if literal_kind.is_some_and(|existing| existing != incoming) {
                    return Some(MatchArmCombination::Conflict(
                        self.match_arm_type_conflict(prior, body_type),
                    ));
                }
                literal_kind = Some(incoming);
            }

            for entry in row.entries {
                match entry {
                    RowEntry::Tag { name, payload } => {
                        let Some(index) = row_entry_index(&entries, &name) else {
                            entries.push(RowEntry::Tag { name, payload });
                            continue;
                        };

                        let RowEntry::Tag {
                            payload: existing, ..
                        } = &entries[index]
                        else {
                            return Some(MatchArmCombination::Conflict(
                                self.match_arm_type_conflict(prior, body_type),
                            ));
                        };
                        if existing.len() != payload.len() {
                            return Some(MatchArmCombination::Conflict(
                                self.match_arm_type_conflict(prior, body_type),
                            ));
                        }

                        for (expected, actual) in existing.iter().zip(&payload) {
                            if self.unifier.unify(expected, actual).is_err() {
                                return Some(MatchArmCombination::Conflict(
                                    self.match_arm_type_conflict(prior, body_type),
                                ));
                            }
                        }
                    }
                    RowEntry::Literal { value } => {
                        let label = render_literal_value(&value);
                        if row_entry_index(&entries, label).is_none() {
                            entries.push(RowEntry::Literal { value });
                        }
                    }
                    RowEntry::Field { .. } => {
                        return Some(MatchArmCombination::Conflict(
                            self.match_arm_type_conflict(prior, body_type),
                        ));
                    }
                }
            }

            if row.tail != RowTail::Closed {
                open = true;
            }
        }

        let result = Type::Variant(Row {
            entries,
            tail: if open { RowTail::Open } else { RowTail::Closed },
        });
        Some(MatchArmCombination::Joined(self.unifier.resolve(&result)))
    }

    pub(super) fn unify_match_arm_body_types(
        &mut self,
        body_types: &[MatchArmBodyType],
    ) -> MatchArmCombination {
        let result_type = self.unifier.fresh();
        let mut earlier_type = None;

        for body_type in body_types {
            if self.unifier.unify(&result_type, &body_type.ty).is_err() {
                let earlier_ty = earlier_type.unwrap_or_else(|| result_type.clone());
                return MatchArmCombination::Conflict(MatchArmTypeConflict {
                    earlier_ty: self.resolved_match_result_type(&earlier_ty),
                    diverging_ty: self.resolved_match_result_type(&body_type.ty),
                    diverging_span: body_type.span,
                });
            }

            earlier_type = Some(self.resolved_match_result_type(&result_type));
        }

        MatchArmCombination::Joined(result_type)
    }

    pub(super) fn match_arm_type_conflict(
        &self,
        earlier_ty: Type,
        diverging: &MatchArmBodyType,
    ) -> MatchArmTypeConflict {
        MatchArmTypeConflict {
            earlier_ty: self.resolved_match_result_type(&earlier_ty),
            diverging_ty: self.resolved_match_result_type(&diverging.ty),
            diverging_span: diverging.span,
        }
    }

    pub(super) fn resolved_match_result_type(&self, ty: &Type) -> Type {
        self.normalize(&self.resolve_and_default(ty))
    }
}
