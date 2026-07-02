use super::*;

impl<'a> Checker<'a> {
    pub(crate) fn with_type_definitions(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
    ) -> Self {
        Self {
            known_types,
            type_definitions,
            value_types: HashMap::new(),
            comptime_bindings: HashSet::new(),
            comptime_artifacts: HashMap::new(),
            comptime_specializations: HashMap::new(),
            local_types: LocalTypeScopes::default(),
            local_comptime_values: Vec::new(),
            local_comptime_params: Vec::new(),
            bindings: HashMap::new(),
            annotations: HashMap::new(),
            memo: HashMap::new(),
            in_progress: HashSet::new(),
            unifier: Unifier::default(),
            globals: Vec::new(),
            host_comptime_fns: HashMap::new(),
            report_unbound_names: true,
            report_unresolved_bindings: true,
            reported_unbound_name_spans: HashSet::new(),
            propagation_contexts: Vec::new(),
            diagnostics: Vec::new(),
            inferred_types: Vec::new(),
        }
    }

    pub(crate) fn with_module_environment(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
    ) -> Self {
        let mut checker = Self::with_type_definitions(known_types, type_definitions);
        checker.collect_top_level_environment(module);
        checker
    }

    #[cfg(test)]
    pub(crate) fn with_module(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
    ) -> Self {
        Self::with_module_and_host_globals(
            known_types,
            type_definitions,
            module,
            &HostGlobals::default(),
        )
    }

    pub(crate) fn with_module_and_host_globals(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
        globals: &HostGlobals,
    ) -> Self {
        let mut checker = Self::with_module_environment(known_types, type_definitions, module);
        checker.globals = globals.types.clone();
        checker.host_comptime_fns = globals
            .comptime_fns
            .iter()
            .map(|(name, spec)| (name.clone(), spec.clone()))
            .collect();
        checker.build_value_types(module);
        checker.build_comptime_artifacts(module);
        checker
    }

    pub(super) fn collect_top_level_environment(&mut self, module: &'a Module) {
        for declaration in collect_declarations(module) {
            if let Some(source) = declared_annotation_for_declaration(module, &declaration) {
                self.annotations
                    .insert(declaration.name.clone(), source.annotation);
            }

            if declaration.phase == DeclarationPhase::Comptime {
                self.comptime_bindings.insert(declaration.name.clone());
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

    pub(super) fn build_value_types(&mut self, module: &Module) {
        // Seed host/library globals into `value_types` before inferring user
        // declarations, but only for names no user declaration claims, so user
        // top-level declarations shadow them (runtime-prelude scoping). Seeding
        // up front lets a user binding that references a global (e.g.
        // `x = logger.info`) resolve it through the existing read paths during
        // inference.
        let declared: HashSet<_> = collect_declarations(module)
            .into_iter()
            .map(|declaration| declaration.name)
            .collect();
        let mut types: HashMap<String, Option<TypeScheme>> = self
            .globals
            .iter()
            .filter(|(name, _)| !declared.contains(name))
            .map(|(name, ty)| {
                (
                    name.clone(),
                    Some(scheme_from_global(ty, &mut self.unifier)),
                )
            })
            .collect();
        self.value_types = types.clone();

        for declaration in collect_declarations(module) {
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
            } else if let Some(inferred) = self.infer_top_level_without_unbound_names(&name)
                && !type_contains_deferred(&inferred.ty)
            {
                types.insert(name.clone(), Some(inferred));
            }
        }

        self.value_types = types;
    }

    pub(super) fn build_comptime_artifacts(&mut self, module: &Module) {
        let names: Vec<_> = collect_declarations(module)
            .into_iter()
            .filter(|declaration| declaration.phase == DeclarationPhase::Comptime)
            .map(|declaration| declaration.name)
            .collect();

        for name in names {
            let mut visiting = HashSet::new();
            self.comptime_binding_is_artifact(&name, &mut visiting);
        }
    }

    pub(crate) fn check_module(&mut self, module: &Module) {
        // Top-level declared annotations go through declarations so inline and
        // adjacent signature+binding forms share one lookup path.
        for declaration in collect_declarations(module) {
            self.check_declaration(module, &declaration);
        }

        for (index, item) in module.items.iter().enumerate() {
            if let Item::Expr(expr) = item {
                if index + 1 != module.items.len() {
                    self.report_unused_result_if_dropped(&TypeEnv::new(), expr);
                }
                self.check_value_expr(expr);
            }
        }
    }

    pub(super) fn check_declaration(&mut self, module: &Module, declaration: &Declaration) {
        let binding = binding_for_declaration(module, declaration);
        let mut checked_value = false;
        let declared_annotation = declared_annotation_for_declaration(module, declaration);
        let has_declared_annotation = declared_annotation.is_some();

        if declaration.phase == DeclarationPhase::Runtime
            && let Some(binding) = binding
        {
            self.check_runtime_binding_liftability(&binding.value);
        }

        if declaration.phase == DeclarationPhase::Comptime
            && let Some(binding) = binding
        {
            self.check_comptime_binding_evaluation_support(&binding.value);
        }

        if let Some(source) = declared_annotation {
            let declared_type = self.lower_annotation(source.annotation);
            let expected_type = self.normalize(&declared_type);
            self.record_inferred_type(declaration.name_span, expected_type.clone());

            if let Some(binding) = binding {
                self.check_value_against(&expected_type, &binding.value);
                checked_value = true;
            }
        } else if let Some(Some(scheme)) = self.value_types.get(&declaration.name).cloned() {
            self.record_synthesized_type(declaration.name_span, &scheme.ty);
        }

        if !checked_value && let Some(binding) = binding {
            let diagnostics_start = self.diagnostics.len();
            if declaration.phase == DeclarationPhase::Comptime {
                self.check_value_expr_without_unbound_names(&binding.value);
            } else {
                self.check_value_expr(&binding.value);
                if !has_declared_annotation
                    && let Some(ty) = self.top_level_binding_final_type(&declaration.name)
                {
                    self.report_unresolved_runtime_binding_if_stuck(
                        binding,
                        &ty,
                        diagnostics_start,
                    );
                }
            }
        }
    }

    pub(super) fn check_value_expr_without_unbound_names(&mut self, expr: &Expr) {
        let previous_unbound = self.report_unbound_names;
        let previous_unresolved = self.report_unresolved_bindings;
        self.report_unbound_names = false;
        self.report_unresolved_bindings = false;
        self.check_value_expr(expr);
        self.report_unbound_names = previous_unbound;
        self.report_unresolved_bindings = previous_unresolved;
    }

    pub(super) fn push_local_comptime_param_scope(&mut self, params: &[Param]) {
        self.local_comptime_params.push(
            params
                .iter()
                .filter(|param| param.comptime)
                .map(|param| param.name.clone())
                .collect(),
        );
    }

    pub(super) fn check_items(&mut self, items: &[Item]) {
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
                MergedItem::Expr(expr) => {
                    if !is_final_expr_item(items, expr) {
                        let env = self.local_types.inference_env();
                        self.report_unused_result_if_dropped(&env, expr);
                    }
                    self.check_value_expr(expr);
                }
            }
        }

        self.local_types.pop();
    }

    pub(super) fn check_local_binding(&mut self, binding: &Binding, signature: Option<&Signature>) {
        self.check_runtime_binding_liftability(&binding.value);

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
            let env_metas = self.local_types.free_metas(|ty| self.unifier.resolve(ty));
            let env_row_vars = self
                .local_types
                .free_row_vars(|ty| self.unifier.resolve(ty));
            let scheme = self.generalize_with_row_merges(resolved, &env_metas, &env_row_vars);
            let diagnostics_start = self.diagnostics.len();
            self.check_value_expr(&binding.value);
            self.report_unresolved_runtime_binding_if_stuck(binding, &scheme.ty, diagnostics_start);
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

        self.record_local_value_type(binding.name_span, &inferred_type);
        self.local_types.define(&binding.name, inferred_type);
    }

    pub(super) fn check_runtime_binding_liftability(&mut self, value: &Expr) {
        let mut visiting = HashSet::new();
        if self.runtime_rhs_is_artifact(value, &mut visiting) {
            self.report_non_liftable_into_runtime(value.span);
        }
    }

    pub(super) fn check_comptime_binding_evaluation_support(&mut self, value: &Expr) {
        if !comptime_rhs_needs_evaluation(value) {
            return;
        }

        if self.try_lower_comptime_annotation(value).is_some() {
            return;
        }

        if self.is_unshadowed_record_selection_builtin_call(value) {
            return;
        }

        let lowering = self.lower_annotation_with_diagnostics(value);
        if lowering.diagnostics.is_empty() {
            self.report_comptime_evaluation_unsupported(value.span);
        }
    }

    pub(super) fn is_unshadowed_record_selection_builtin_call(&self, value: &Expr) -> bool {
        let ExprKind::Call { callee, .. } = &ungroup_expr(value).kind else {
            return false;
        };
        let Some(name) = expr_name(callee) else {
            return false;
        };

        comptime::RecordSelectionKind::from_name(name).is_some()
            && !self.record_selection_builtin_is_shadowed(&TypeEnv::new(), name)
    }

    pub(super) fn comptime_binding_is_artifact(
        &mut self,
        name: &str,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if let Some(is_artifact) = self.comptime_artifacts.get(name).copied() {
            return is_artifact;
        }

        if !self.comptime_bindings.contains(name) {
            return false;
        }

        if !visiting.insert(name.to_owned()) {
            return false;
        }

        let binding = self.bindings.get(name).and_then(|binding| *binding);
        let is_artifact = binding
            .is_some_and(|binding| self.rhs_is_non_liftable_artifact(&binding.value, visiting));

        visiting.remove(name);
        self.comptime_artifacts.insert(name.to_owned(), is_artifact);
        is_artifact
    }

    pub(super) fn runtime_rhs_is_artifact(
        &mut self,
        value: &Expr,
        visiting: &mut HashSet<String>,
    ) -> bool {
        match &value.kind {
            ExprKind::Group(inner) => self.runtime_rhs_is_artifact(inner, visiting),
            ExprKind::ComptimeName(name) => self.comptime_reference_is_artifact(name, visiting),
            ExprKind::Name(_) => false,
            _ if Self::literal_or_tag_value_shape(value) => false,
            _ => self.rhs_is_non_liftable_artifact(value, visiting),
        }
    }

    pub(super) fn comptime_reference_is_artifact(
        &mut self,
        name: &str,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if self.comptime_bindings.contains(name) {
            return self.comptime_binding_is_artifact(name, visiting);
        }

        self.known_types.contains(name)
    }

    pub(super) fn rhs_is_non_liftable_artifact(
        &mut self,
        value: &Expr,
        visiting: &mut HashSet<String>,
    ) -> bool {
        match &value.kind {
            ExprKind::Group(inner) => {
                return self.rhs_is_non_liftable_artifact(inner, visiting);
            }
            ExprKind::Literal(_) | ExprKind::Tag(_) => {
                return false;
            }
            ExprKind::ComptimeName(name) => {
                return self.comptime_reference_is_artifact(name, visiting);
            }
            ExprKind::Name(name) if self.is_runtime_value_reference(name) => {
                return false;
            }
            _ => {}
        }

        if self.expr_contains_runtime_value_reference(value)
            || self.expr_contains_unknown_comptime_reference(value, visiting)
        {
            return false;
        }

        let Some(ty) = self.lower_clean_normalized_type(value) else {
            return false;
        };

        !type_contains_deferred(&ty) && is_non_liftable_artifact_type(&ty)
    }

    pub(super) fn lower_clean_normalized_type(&self, value: &Expr) -> Option<Type> {
        let mut checker = self.fork_annotation_checker();
        let lowering = checker.lower_annotation_with_diagnostics(value);
        lowering
            .diagnostics
            .is_empty()
            .then(|| checker.normalize(&lowering.ty))
    }

    pub(super) fn expr_contains_runtime_value_reference(&self, value: &Expr) -> bool {
        if let ExprKind::Name(name) = &value.kind
            && self.is_runtime_value_reference(name)
        {
            return true;
        }

        let mut found = false;
        walk_expr_children(value, &mut |child| {
            if !found && self.expr_contains_runtime_value_reference(child) {
                found = true;
            }
        });
        found
    }

    pub(super) fn expr_contains_unknown_comptime_reference(
        &mut self,
        value: &Expr,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if let ExprKind::ComptimeName(name) = &value.kind
            && !self.comptime_reference_is_artifact(name, visiting)
        {
            return true;
        }

        let mut found = false;
        walk_expr_children(value, &mut |child| {
            if !found && self.expr_contains_unknown_comptime_reference(child, visiting) {
                found = true;
            }
        });
        found
    }

    pub(super) fn literal_or_tag_value_shape(value: &Expr) -> bool {
        match &ungroup_expr(value).kind {
            ExprKind::Literal(_) | ExprKind::Tag(_) | ExprKind::Undefined | ExprKind::Null => true,
            ExprKind::Tuple(items) | ExprKind::Array(items) => {
                items.iter().all(Self::literal_or_tag_value_shape)
            }
            ExprKind::Record(entries) | ExprKind::Set(entries) => {
                Self::row_literal_or_tag_value_shape(entries)
            }
            ExprKind::Call { callee, args }
                if matches!(&ungroup_expr(callee).kind, ExprKind::Tag(_)) =>
            {
                args.iter().all(Self::literal_or_tag_value_shape)
            }
            ExprKind::Missing
            | ExprKind::Interpolation(_)
            | ExprKind::Name(_)
            | ExprKind::ComptimeName(_)
            | ExprKind::Group(_)
            | ExprKind::Index { .. }
            | ExprKind::Optional(_)
            | ExprKind::Nullable(_)
            | ExprKind::NonNull(_)
            | ExprKind::Arrow { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::Call { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Propagate { .. }
            | ExprKind::Match { .. }
            | ExprKind::Lambda { .. }
            | ExprKind::Block(_) => false,
        }
    }

    pub(super) fn row_literal_or_tag_value_shape(entries: &[RecordEntry]) -> bool {
        let mut has_value_entry = entries.is_empty();

        for entry in entries {
            match entry {
                RecordEntry::Field { value, .. } | RecordEntry::Element(value) => {
                    if !Self::literal_or_tag_value_shape(value) {
                        return false;
                    }
                    has_value_entry = true;
                }
                RecordEntry::FieldComputed { key, value, .. } => {
                    if !Self::literal_or_tag_value_shape(key)
                        || !Self::literal_or_tag_value_shape(value)
                    {
                        return false;
                    }
                    has_value_entry = true;
                }
                RecordEntry::Shorthand { .. }
                | RecordEntry::Spread { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::DeleteComputed { .. }
                | RecordEntry::Rename { .. }
                | RecordEntry::Iteration { .. }
                | RecordEntry::Open { .. } => {}
            }
        }

        has_value_entry
    }

    pub(super) fn is_runtime_value_reference(&self, name: &str) -> bool {
        self.local_types.get(name).is_some()
            || (self.bindings.contains_key(name) && !self.comptime_bindings.contains(name))
    }

    pub(super) fn record_local_value_type(&mut self, name_span: Span, value_type: &LocalValueType) {
        match value_type {
            LocalValueType::Known(ty) => self.record_inferred_type(name_span, ty.clone()),
            LocalValueType::Scheme(scheme) => {
                self.record_synthesized_type(name_span, &scheme.ty);
            }
            LocalValueType::Unknown => {}
        }
    }

    pub(super) fn record_inferred_type(&mut self, name_span: Span, ty: Type) {
        if type_contains_deferred(&ty) {
            return;
        }

        self.inferred_types.push(InferredType { name_span, ty });
    }

    pub(super) fn record_synthesized_type(&mut self, name_span: Span, ty: &Type) {
        self.record_inferred_type(name_span, display_inferred_type(ty));
    }

    pub(super) fn top_level_binding_final_type(&mut self, name: &str) -> Option<Type> {
        let scheme = self.memo.get(name).cloned()?;
        let ty = self.unifier.instantiate_scheme(&scheme);
        Some(self.normalize(&self.resolve_and_default(&ty)))
    }

    pub(super) fn generalize_with_row_merges(
        &self,
        resolved: Type,
        env_metas: &[u32],
        env_row_vars: &[u32],
    ) -> TypeScheme {
        let mut scheme = generalize(resolved, env_metas, env_row_vars);
        scheme.row_merges = self
            .unifier
            .row_merge_closure(&scheme.row_vars, env_row_vars);
        let mut seen: HashSet<_> = scheme.row_vars.iter().copied().collect();
        for constraint in &scheme.row_merges {
            if seen.insert(constraint.result) {
                scheme.row_vars.push(constraint.result);
            }
            for source in &constraint.sources {
                for id in crate::ty::free_row_vars(&Type::Record(source.row.clone())) {
                    if seen.insert(id) {
                        scheme.row_vars.push(id);
                    }
                }
            }
        }
        scheme
    }

    pub(super) fn report_unresolved_runtime_binding_if_stuck(
        &mut self,
        binding: &Binding,
        ty: &Type,
        diagnostics_start: usize,
    ) {
        if !self.report_unresolved_bindings
            || self.diagnostics.len() != diagnostics_start
            || self.binding_value_had_prior_diagnostic(binding, diagnostics_start)
        {
            return;
        }

        let ty = self.normalize(&self.resolve_and_default(ty));
        if !matches!(ty, Type::Deferred) {
            return;
        }

        if Self::binding_value_is_bare_placeholder(&binding.value)
            || self.binding_value_is_overload_selection_pending(&binding.value)
            || self.binding_value_is_host_comptime_runtime_arg_deferral(&binding.value)
            || self.binding_value_is_open_record_rest_match_unknown(&binding.value)
        {
            return;
        }

        let mut visiting = HashSet::new();
        if self.runtime_rhs_is_artifact(&binding.value, &mut visiting) {
            return;
        }

        self.report_unresolved_binding(binding.name_span);
    }

    pub(super) fn binding_value_had_prior_diagnostic(
        &self,
        binding: &Binding,
        diagnostics_start: usize,
    ) -> bool {
        let mut spans = Vec::new();
        let mut visiting = HashSet::new();
        self.collect_value_diagnostic_spans(&binding.value, &mut visiting, &mut spans);

        self.diagnostics[..diagnostics_start]
            .iter()
            .filter_map(|diagnostic| diagnostic.labels.first())
            .any(|label| spans.iter().any(|span| span.contains(label.span)))
    }

    /// Collect spans a prior diagnostic could plausibly explain this binding's
    /// value through: the value expression itself plus, transitively, the value
    /// spans of any top-level binding this value (or its sub-expressions) reach
    /// by name. The transitive chase is what lets R6 see diagnostics whose
    /// primary label lives on a called helper binding rather than on this
    /// binding's value.
    pub(super) fn collect_value_diagnostic_spans(
        &self,
        value: &Expr,
        visiting: &mut HashSet<String>,
        spans: &mut Vec<Span>,
    ) {
        spans.push(value.span);

        walk_expr_children(value, &mut |child| {
            self.collect_value_diagnostic_spans(child, visiting, spans);
        });

        let value = ungroup_expr(value);
        if let ExprKind::Name(name) = &value.kind
            && visiting.insert(name.clone())
        {
            if let Some(Some(other)) = self.bindings.get(name) {
                self.collect_value_diagnostic_spans(&other.value, visiting, spans);
            }
            visiting.remove(name);
        }
    }

    pub(super) fn binding_value_is_bare_placeholder(value: &Expr) -> bool {
        match &ungroup_expr(value).kind {
            ExprKind::Name(name) if name_is_placeholder(name) => true,
            ExprKind::Missing => true,
            _ => false,
        }
    }

    pub(super) fn binding_value_is_overload_selection_pending(&self, value: &Expr) -> bool {
        match &ungroup_expr(value).kind {
            ExprKind::Name(name) => self.name_is_deferred_overload(name),
            ExprKind::Call { callee, .. } => {
                self.binding_value_is_overload_selection_pending(callee)
            }
            _ => false,
        }
    }

    pub(super) fn name_is_deferred_overload(&self, name: &str) -> bool {
        self.bindings
            .get(name)
            .is_some_and(|binding| binding.is_none())
            && self
                .value_types
                .get(name)
                .is_some_and(|scheme| scheme.is_none())
    }

    pub(super) fn binding_value_is_host_comptime_runtime_arg_deferral(&self, value: &Expr) -> bool {
        let ExprKind::Call { callee, args } = &ungroup_expr(value).kind else {
            return false;
        };
        let env = self.local_types.inference_env();
        let Some(name) = self.comptime_callee_name(&env, callee) else {
            return false;
        };
        let Some(spec) = self.host_comptime_fn(&env, &name) else {
            return false;
        };

        let bindings = self.current_comptime_value_bindings();
        spec.comptime_params.iter().any(|index| {
            let Some(arg) = args.get(*index) else {
                return false;
            };

            self.evaluate_comptime_runtime_argument(arg, &bindings)
                .is_none()
        })
    }

    pub(super) fn binding_value_is_open_record_rest_match_unknown(&mut self, value: &Expr) -> bool {
        let ExprKind::Match { subject, arms, .. } = &ungroup_expr(value).kind else {
            return false;
        };
        let Some(subject_row) = self.infer_record_subject_row_for_exemption(subject) else {
            return false;
        };
        if subject_row.tail == RowTail::Closed {
            return false;
        }

        arms.iter().any(|arm| {
            let mut rest_binders = Vec::new();
            collect_record_pattern_rest_binders(&arm.pattern, &mut rest_binders);
            rest_binders
                .iter()
                .any(|binder| expr_references_name(&arm.body, binder))
        })
    }

    pub(super) fn infer_record_subject_row_for_exemption(&mut self, subject: &Expr) -> Option<Row> {
        let unifier_snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred_types_len = self.inferred_types.len();
        let env = self.local_types.inference_env();
        let inferred = self.infer(&env, subject);
        let resolved = self.normalize(&self.resolve_and_default(&inferred));
        self.unifier.restore(unifier_snapshot);
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        self.inferred_types.truncate(inferred_types_len);

        match resolved {
            Type::Record(row) => Some(row),
            _ => None,
        }
    }

    pub(super) fn record_expr_type(&mut self, span: Span, ty: &Type) {
        if span.is_empty() {
            return;
        }

        let ty = self.normalize(&self.resolve_and_default(ty));
        if is_resolved_value_type(&ty) {
            self.record_synthesized_type(span, &ty);
        }
    }
}
