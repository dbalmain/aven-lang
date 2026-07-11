use super::*;

impl comptime::EvalContext for Checker<'_> {
    fn lower_comptime_type(
        &mut self,
        expr: &Expr,
        bindings: &HashMap<String, comptime::ComptimeValue>,
    ) -> comptime::LoweredType {
        let start = self.diagnostics.len();
        self.local_comptime_values.push(bindings.clone());
        let ty = self.lower_annotation(expr);
        self.local_comptime_values.pop();
        let diagnostics = self.diagnostics.split_off(start);

        comptime::LoweredType {
            ty: self.normalize(&ty),
            diagnostics,
        }
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
}
