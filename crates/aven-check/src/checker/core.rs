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
            comptime_specializations_in_progress: HashSet::new(),
            local_types: LocalTypeScopes::default(),
            local_comptime_values: Vec::new(),
            local_comptime_params: Vec::new(),
            bindings: HashMap::new(),
            annotations: HashMap::new(),
            memo: HashMap::new(),
            in_progress: HashSet::new(),
            unifier: Unifier::default(),
            globals: Vec::new(),
            statics: HashMap::new(),
            host_comptime_fns: HashMap::new(),
            imports: ModuleImports::default(),
            report_unbound_names: true,
            report_unresolved_bindings: true,
            reported_unbound_name_spans: HashSet::new(),
            reported_import_spans: HashSet::new(),
            propagation_contexts: Vec::new(),
            rigid_type_var_scopes: Vec::new(),
            inline_lambda_type_var_scopes: Vec::new(),
            pattern_bindings: HashMap::new(),
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

    #[cfg(test)]
    pub(crate) fn with_module_and_host_globals(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
        globals: &HostGlobals,
    ) -> Self {
        Self::with_module_and_host_globals_and_imports(
            known_types,
            type_definitions,
            module,
            globals,
            &ModuleImports::default(),
        )
    }

    pub(crate) fn with_module_and_host_globals_and_imports(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
        globals: &HostGlobals,
        imports: &ModuleImports,
    ) -> Self {
        let mut checker = Self::with_module_environment(known_types, type_definitions, module);
        checker.globals = globals.types.clone();
        checker.imports = imports.clone();
        checker.host_comptime_fns = globals
            .comptime_fns
            .iter()
            .map(|(name, spec)| (name.clone(), spec.clone()))
            .collect();
        checker.build_statics(globals);
        checker.collect_top_level_pattern_bindings(module);
        checker.build_value_types(module);
        checker.build_comptime_artifacts(module);
        checker
    }

    /// Assemble the statics table (compiler builtins + host-registered) into
    /// generalized schemes so each `Type.static` use instantiates fresh.
    fn build_statics(&mut self, globals: &HostGlobals) {
        for (type_name, members) in builtin_type_statics()
            .into_iter()
            .chain(globals.statics.iter().cloned())
        {
            let entry = self.statics.entry(type_name).or_default();
            for (member, ty) in members {
                let scheme = scheme_from_global(&ty, &mut self.unifier);
                entry.insert(member, scheme);
            }
        }
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

        self.collect_top_level_pattern_bindings(module);
    }

    fn collect_top_level_pattern_bindings(&mut self, module: &'a Module) {
        for item in &module.items {
            if let Item::PatternBinding(binding) = item {
                for site in pattern_bindings(&binding.pattern) {
                    self.pattern_bindings.insert(site.name.to_owned(), binding);
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
        let declarations = collect_declarations(module);
        let declared: HashSet<_> = declarations
            .iter()
            .map(|declaration| declaration.name.clone())
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

        for declaration in &declarations {
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
        }

        self.value_types = types.clone();
        self.insert_top_level_spread_types(module, &mut types);
        self.value_types = types.clone();

        for declaration in declarations {
            let name = declaration.name.clone();

            if binding_for_declaration(module, &declaration)
                .is_some_and(|binding| self.is_uppercase_comptime_function(&name, &binding.value))
            {
                continue;
            }

            if binding_for_declaration(module, &declaration).is_none()
                && !self.pattern_bindings.contains_key(&name)
            {
                continue;
            }

            if let Some(annotation) = self.clean_declared_annotation(&name) {
                // Polymorphic annotations (`Type::Variable`) need quantification
                // so each use site instantiates fresh metas, matching host
                // globals and unannotated generalized exports.
                types.insert(
                    name.clone(),
                    Some(scheme_from_global(&annotation, &mut self.unifier)),
                );
            } else if let Some(inferred) = self.infer_top_level_without_unbound_names(&name)
                && !type_contains_deferred(&inferred.ty)
            {
                types.insert(name.clone(), Some(inferred));
            }
        }

        self.value_types = types;
    }

    fn insert_top_level_spread_types(
        &mut self,
        module: &Module,
        types: &mut HashMap<String, Option<TypeScheme>>,
    ) {
        for item in &module.items {
            let Item::SpreadBinding(binding) = item else {
                continue;
            };

            if binding.overwrite {
                continue;
            }

            let Some(row) = self.closed_spread_row(binding, &TypeEnv::new(), false) else {
                continue;
            };

            for entry in row.entries {
                let RowEntry::Field { name, ty } = entry else {
                    continue;
                };

                if let Entry::Vacant(entry) = types.entry(name) {
                    entry.insert(Some(scheme_from_global(&ty, &mut self.unifier)));
                }
            }
        }
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

        let mut top_level_spread_names = HashSet::new();
        for (index, item) in module.items.iter().enumerate() {
            match item {
                Item::PatternBinding(binding) => {
                    self.check_top_level_pattern_binding(binding);
                }
                Item::SpreadBinding(binding) => {
                    self.check_top_level_spread_binding(binding, &mut top_level_spread_names);
                }
                Item::Expr(expr) => {
                    if index + 1 != module.items.len() {
                        self.report_unused_result_if_dropped(&TypeEnv::new(), expr);
                    }
                    self.check_value_expr(expr);
                    if index + 1 == module.items.len() {
                        let diagnostics_start = self.diagnostics.len();
                        let _ = self.infer(&TypeEnv::new(), expr);
                        self.deduplicate_diagnostics_since(diagnostics_start);
                    }
                }
                Item::Binding(_) | Item::Signature(_) => {}
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
            if self.is_uppercase_comptime_function(&declaration.name, &binding.value) {
                self.report_redundant_comptime_markers(&binding.value);
            } else if is_import_call(&binding.value) {
                // Uppercase names are reserved for types; an import binds a
                // module record, never a type.
                self.report_uppercase_module_binding(&declaration.name, declaration.name_span);
            } else if !has_declared_annotation
                && let Some(name) =
                    crate::lower::bare_lowercase_unknown_name(&binding.value, &self.known_types)
            {
                self.report_runtime_name_alias(&declaration.name, name, binding.value.span);
            } else {
                self.check_comptime_binding_evaluation_support(&binding.value);
            }
        }

        if let Some(source) = declared_annotation {
            let declared_type = self.lower_annotation(source.annotation);
            let expected_type = self.normalize(&declared_type);
            self.record_inferred_type(declaration.name_span, declared_type);

            if let Some(binding) = binding {
                self.check_value_against_declared_type(&expected_type, &binding.value);
                checked_value = true;
            }
        } else if let Some(Some(scheme)) = self.value_types.get(&declaration.name).cloned() {
            self.record_synthesized_type(declaration.name_span, &scheme.ty);
        }

        if !checked_value && let Some(binding) = binding {
            let diagnostics_start = self.diagnostics.len();
            if declaration.phase == DeclarationPhase::Comptime
                && !self.is_uppercase_comptime_function(&declaration.name, &binding.value)
            {
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
                self.deduplicate_diagnostics_since(diagnostics_start);
            }
        }
    }

    pub(super) fn is_uppercase_comptime_function(&self, name: &str, value: &Expr) -> bool {
        name.chars().next().is_some_and(char::is_uppercase) && lambda_parts(value).is_some()
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

        let signature_type = signature.map(|signature| {
            let declared = self.lower_annotation(&signature.annotation);
            let normalized = self.normalize(&declared);
            (declared, normalized)
        });
        let binding_type = binding.annotation.as_ref().map(|annotation| {
            let declared = self.lower_annotation(annotation);
            let normalized = self.normalize(&declared);
            (declared, normalized)
        });
        let declared_type = signature_type.as_ref().or(binding_type.as_ref());

        if let Some((_, expected)) = declared_type {
            self.check_value_against_declared_type(expected, &binding.value);
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
                .map(|(_, normalized)| normalized.clone())
                .map(LocalValueType::Known)
                .unwrap_or(LocalValueType::Unknown)
        };

        if let Some((declared, _)) = declared_type {
            self.record_inferred_type(binding.name_span, declared.clone());
        } else {
            self.record_local_value_type(binding.name_span, &inferred_type);
        }
        self.local_types.define(&binding.name, inferred_type);
    }

    pub(super) fn check_local_pattern_binding(&mut self, binding: &PatternBinding) {
        self.check_runtime_binding_liftability(&binding.value);
        self.define_pattern_binding(&binding.pattern, &binding.value, false);
    }

    pub(super) fn check_top_level_pattern_binding(&mut self, binding: &PatternBinding) {
        self.check_runtime_binding_liftability(&binding.value);
        self.check_pattern_binding(&binding.pattern, &binding.value, &TypeEnv::new());
    }

    pub(super) fn check_local_spread_binding(&mut self, binding: &SpreadBinding) {
        self.check_runtime_binding_liftability(&binding.value);
        self.check_value_expr(&binding.value);
        let env = self.local_types.inference_env();
        let Some(row) = self.closed_spread_row(binding, &env, true) else {
            return;
        };

        for entry in row.entries {
            let RowEntry::Field { name, ty } = entry else {
                continue;
            };
            if !binding.overwrite && self.local_binding_name_is_visible(&name) {
                self.report_duplicate_local_from_spread(&name, binding.span);
                continue;
            }
            self.local_types.define(&name, LocalValueType::Known(ty));
        }
    }

    pub(super) fn check_top_level_spread_binding(
        &mut self,
        binding: &SpreadBinding,
        top_level_spread_names: &mut HashSet<String>,
    ) {
        if binding.overwrite {
            return;
        }
        self.check_runtime_binding_liftability(&binding.value);
        self.check_value_expr(&binding.value);
        let Some(row) = self.closed_spread_row(binding, &TypeEnv::new(), true) else {
            return;
        };

        for entry in row.entries {
            let RowEntry::Field { name, .. } = entry else {
                continue;
            };
            if self.bindings.contains_key(&name)
                || self.pattern_bindings.contains_key(&name)
                || top_level_spread_names.contains(&name)
            {
                self.report_duplicate_declaration_from_spread(&name, binding.span);
            }
            top_level_spread_names.insert(name);
        }
    }

    fn define_pattern_binding(&mut self, pattern: &Expr, value: &Expr, top_level: bool) {
        let env = self.local_types.inference_env();
        let local_types = self.check_pattern_binding(pattern, value, &env);
        let binding_sites = pattern_bindings(pattern);
        for (name, ty) in local_types {
            for site in binding_sites.iter().filter(|site| site.name == name) {
                self.record_local_value_type(site.span, &ty);
            }
            if !top_level {
                self.local_types.define(&name, ty);
            }
        }
    }

    fn check_pattern_binding(
        &mut self,
        pattern: &Expr,
        value: &Expr,
        env: &TypeEnv,
    ) -> Vec<(String, LocalValueType)> {
        self.check_value_expr(value);
        let inferred = self.infer(env, value);
        let resolved = self.normalize(&self.resolve_and_default(&inferred));
        self.report_unsupported_uppercase_pattern_binders(pattern, value);
        self.report_missing_record_pattern_fields(pattern, &resolved);
        pattern_local_types(&self.type_definitions, pattern, Some(&resolved))
    }

    pub(super) fn closed_spread_row(
        &mut self,
        binding: &SpreadBinding,
        env: &TypeEnv,
        report: bool,
    ) -> Option<Row> {
        let inferred = self.infer(env, &binding.value);
        let resolved = self.normalize(&self.resolve_and_default(&inferred));
        let Type::Record(row) = resolved else {
            if report {
                self.report_spread_shape_unknown(binding.span);
            }
            return None;
        };
        if row.tail != RowTail::Closed {
            if report {
                self.report_spread_shape_unknown(binding.span);
            }
            return None;
        }
        Some(row)
    }

    fn local_binding_name_is_visible(&self, name: &str) -> bool {
        self.local_types.get(name).is_some() || self.value_types.contains_key(name)
    }

    fn report_unsupported_uppercase_pattern_binders(&mut self, pattern: &Expr, value: &Expr) {
        // Allowed uppercase binders are those that extract a matching type
        // export field. For `{ User -> Alias }`, the export key is `User` but
        // the binder name is `Alias`.
        let imported = aven_parser::static_import_specifier(value)
            .map(|specifier| type_export_pattern_binders(pattern, &specifier, &self.imports))
            .unwrap_or_default();
        for site in pattern_bindings(pattern) {
            if is_comptime_identifier_name(site.name) && !imported.contains(site.name) {
                self.report_unsupported_uppercase_pattern_binder(site.name, site.span);
            }
        }
    }

    fn report_missing_record_pattern_fields(&mut self, pattern: &Expr, subject: &Type) {
        let Type::Record(row) = subject else {
            return;
        };
        self.report_missing_record_pattern_fields_in_expr(pattern, row);
    }

    fn report_missing_record_pattern_fields_in_expr(&mut self, pattern: &Expr, row: &Row) {
        let ExprKind::Record(entries) = &ungroup_expr(pattern).kind else {
            return;
        };

        for entry in entries {
            match entry {
                RecordEntry::Field {
                    name,
                    name_span,
                    value,
                    ..
                } => {
                    if let Some(field_ty) = row_field_type(row, name) {
                        if let Type::Record(nested) = self.normalize(field_ty) {
                            self.report_missing_record_pattern_fields_in_expr(value, &nested);
                        }
                    } else {
                        self.report_missing_field(name, *name_span);
                    }
                }
                RecordEntry::Shorthand {
                    name, name_span, ..
                } => {
                    if row_field_type(row, name).is_none() {
                        self.report_missing_field(name, *name_span);
                    }
                }
                RecordEntry::Rename {
                    from, from_span, ..
                } => {
                    if row_field_type(row, from).is_none() {
                        self.report_missing_field(from, *from_span);
                    }
                }
                RecordEntry::Spread { .. }
                | RecordEntry::Delete { .. }
                | RecordEntry::FieldComputed { .. }
                | RecordEntry::DeleteComputed { .. }
                | RecordEntry::Iteration { .. }
                | RecordEntry::Open { .. }
                | RecordEntry::Element(_) => {}
            }
        }
    }

    pub(super) fn check_runtime_binding_liftability(&mut self, value: &Expr) {
        let mut visiting = HashSet::new();
        if self.runtime_rhs_is_artifact(value, &mut visiting) {
            self.report_non_liftable_into_runtime(value.span);
        }
    }

    pub(super) fn check_comptime_binding_evaluation_support(&mut self, value: &Expr) {
        if !comptime_rhs_needs_evaluation(value)
            && !self.is_uppercase_comptime_function_application(value)
        {
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

    fn is_uppercase_comptime_function_application(&self, value: &Expr) -> bool {
        let ExprKind::Call { callee, .. } = &ungroup_expr(value).kind else {
            return false;
        };

        self.is_uppercase_comptime_function_callee(callee)
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
            _ if Self::literal_operation_value_shape(value) => false,
            // `pick`/`omit` are runtime builtins (they also reify in type
            // position). A direct call is a value computation even when its
            // subject is a type record — not a pure type artifact.
            _ if self.is_unshadowed_record_selection_builtin_call(value) => false,
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
            ExprKind::Tuple(items) => items.iter().all(Self::literal_or_tag_value_shape),
            ExprKind::Array(entries) | ExprKind::Record(entries) | ExprKind::Set(entries) => {
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

    pub(super) fn literal_operation_value_shape(value: &Expr) -> bool {
        match &ungroup_expr(value).kind {
            ExprKind::Unary {
                operator, value, ..
            } if matches!(operator.as_str(), "-" | "!") => {
                Self::literal_or_tag_value_shape(value)
                    || Self::literal_operation_value_shape(value)
            }
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            } if matches!(
                operator.as_str(),
                "+" | "-" | "*" | "/" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||"
            ) =>
            {
                (Self::literal_or_tag_value_shape(left)
                    || Self::literal_operation_value_shape(left))
                    && (Self::literal_or_tag_value_shape(right)
                        || Self::literal_operation_value_shape(right))
            }
            _ => false,
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

        let ty = self.display_named_definitions(ty);
        self.inferred_types.push(InferredType { name_span, ty });
    }

    pub(super) fn record_synthesized_type(&mut self, name_span: Span, ty: &Type) {
        self.record_inferred_type(name_span, display_inferred_type(ty));
    }

    pub(super) fn display_named_definitions(&self, ty: Type) -> Type {
        let mut names = self.type_definitions.keys().cloned().collect::<Vec<_>>();
        names.sort();
        if names.is_empty() {
            return ty;
        }

        map_type(&ty, &mut |node| {
            names.iter().find_map(|name| {
                let definition = self.type_definitions.get(name)?;
                if matches!(definition, Type::Named(_))
                    || !self.type_references_name(node, name, &mut HashSet::new())
                {
                    return None;
                }
                (self.normalize(definition).render() == node.render())
                    .then(|| Type::Named(name.clone()))
            })
        })
    }

    pub(super) fn type_references_name(
        &self,
        ty: &Type,
        name: &str,
        visiting: &mut HashSet<String>,
    ) -> bool {
        match ty {
            Type::Named(candidate) if candidate == name => true,
            Type::Named(candidate) => {
                visiting.insert(candidate.clone())
                    && self
                        .type_definitions
                        .get(candidate)
                        .is_some_and(|definition| {
                            self.type_references_name(definition, name, visiting)
                        })
            }
            Type::Apply { callee, args } => {
                self.type_references_name(callee, name, visiting)
                    || args
                        .iter()
                        .any(|arg| self.type_references_name(arg, name, visiting))
            }
            Type::Function { params, result, .. } => {
                params
                    .iter()
                    .any(|param| self.type_references_name(param, name, visiting))
                    || self.type_references_name(result, name, visiting)
            }
            Type::Optional(inner) | Type::Nullable(inner) => {
                self.type_references_name(inner, name, visiting)
            }
            Type::Tuple(items) => items
                .iter()
                .any(|item| self.type_references_name(item, name, visiting)),
            Type::Record(row) | Type::Variant(row) => row.entries.iter().any(|entry| match entry {
                RowEntry::Field { ty, .. } => self.type_references_name(ty, name, visiting),
                RowEntry::Tag { payload, .. } => payload
                    .iter()
                    .any(|ty| self.type_references_name(ty, name, visiting)),
                RowEntry::Literal { .. } => false,
            }),
            Type::Deferred | Type::Variable(_) | Type::Meta(_) => false,
        }
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
        spec.comptime_params.iter().any(|param| {
            let Some(arg) = args.get(param.index()) else {
                return false;
            };

            match param {
                HostComptimeParam::Value(_) => self
                    .evaluate_comptime_runtime_argument(arg, &bindings)
                    .is_none(),
                HostComptimeParam::TypeOf(_) => true,
            }
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
