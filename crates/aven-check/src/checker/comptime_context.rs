use super::*;

impl comptime::EvalContext for Checker<'_> {
    fn lower_comptime_type(
        &mut self,
        expr: &Expr,
        bindings: &HashMap<String, comptime::ComptimeValue>,
        captured_types: &HashMap<String, Type>,
        _in_function_body: bool,
    ) -> comptime::LoweredType {
        let start = self.diagnostics.len();
        let mut visible_type_definitions = self.type_definitions.clone();
        visible_type_definitions.extend(captured_types.clone());
        let mut visible_known_types = self.known_types.clone();
        visible_known_types.extend(captured_types.keys().cloned());
        let saved_type_definitions =
            std::mem::replace(&mut self.type_definitions, visible_type_definitions);
        let saved_known_types = std::mem::replace(&mut self.known_types, visible_known_types);
        self.local_comptime_values.push(bindings.clone());
        let ty = self.lower_annotation(expr);
        self.local_comptime_values.pop();
        self.known_types = saved_known_types;
        self.type_definitions = saved_type_definitions;
        let diagnostics = self.diagnostics.split_off(start);

        comptime::LoweredType {
            ty: self.normalize(&ty),
            diagnostics,
        }
    }

    fn runtime_binding_reference(&self, name: &str, span: Span) -> Option<Diagnostic> {
        (self.bindings.contains_key(name) && !self.comptime_bindings.contains(name)).then(|| {
            Diagnostic::error(format!(
                "runtime binding `{name}` cannot be used while specializing a comptime function"
            ))
            .with_code(codes::comptime::EVALUATION_UNSUPPORTED)
            .with_label(Label::primary(
                span,
                "this reference is not known at compile time",
            ))
            .with_note("comptime function bodies may capture only comptime-known module bindings")
        })
    }

    fn lookup_comptime_function(&self, name: &str) -> Option<comptime::ComptimeFunction> {
        self.lookup_comptime_function_export(name)
    }

    fn cached_specialization(
        &self,
        key: &comptime::SpecializationKey,
    ) -> Option<comptime::EvaluationResult> {
        self.comptime_specializations.get(key).cloned()
    }

    fn cache_specialization(
        &mut self,
        key: comptime::SpecializationKey,
        result: comptime::EvaluationResult,
    ) {
        self.comptime_specializations.insert(key, result);
    }

    fn specialization_is_in_progress(&self, key: &comptime::SpecializationKey) -> bool {
        self.comptime_specializations_in_progress.contains(key)
    }

    fn begin_specialization(&mut self, key: comptime::SpecializationKey) {
        self.comptime_specializations_in_progress.insert(key);
    }

    fn end_specialization(&mut self, key: &comptime::SpecializationKey) {
        self.comptime_specializations_in_progress.remove(key);
    }

    fn infer_value_type(&mut self, expr: &Expr) -> Type {
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred = self.infer(&TypeEnv::new(), expr);
        let ty = self.normalize(&self.resolve_and_default(&inferred));
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        display_inferred_type(&ty)
    }

    fn type_is_unresolved(&self, ty: &Type) -> bool {
        self.reflection_subject_is_unresolved(ty)
    }

    fn type_fits_boundary(&mut self, expected: &Type, actual: &Type) -> bool {
        self.type_fits_boundary_without_reporting(expected, actual)
    }
}
