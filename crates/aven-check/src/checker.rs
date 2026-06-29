use std::collections::{HashMap, HashSet, hash_map::Entry};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, InterpolationSegment, Item, Literal,
    MatchArm, MergedItem, Module, Param, RecordEntry, Signature, collect_declarations,
    merged_items, pattern_bindings, walk_expr_children,
};

use crate::BUILTIN_TYPES;
use crate::InferredType;
use crate::comptime::{self, Evaluation};
use crate::env::{
    LocalTypeScopes, LocalValueType, TypeEnv, free_metas_in_local_values,
    free_row_vars_in_local_values,
};
use crate::host_comptime::{ComptimeArg, ComptimeError, HostComptimeFnSpec, HostGlobals};
use crate::lower::{
    DeclaredAnnotation, DeclaredAnnotationSource, TypeLowering, binding_for_declaration,
    declared_annotation_for_declaration,
};
use crate::ty::{
    LiteralBase, Row, RowEntry, RowKind, RowTail, Type, TypeScheme, display_inferred_type,
    free_metas, generalize, has_only_meta_unknowns, is_concrete_type, is_meta_type, is_null_value,
    is_resolved_value_type, is_undefined_value, literal_base, literal_variant_base, map_type,
    mismatched_literal_kind, named_builtin, named_type_mismatch, named_type_name,
    numeric_type_name, render_literal_value, type_contains_deferred,
};
use crate::unify::Unifier;

pub(crate) struct Checker<'a> {
    known_types: HashSet<String>,
    type_definitions: HashMap<String, Type>,
    value_types: HashMap<String, Option<TypeScheme>>,
    comptime_bindings: HashSet<String>,
    comptime_artifacts: HashMap<String, bool>,
    comptime_specializations: HashMap<comptime::SpecializationKey, comptime::EvaluationResult>,
    local_types: LocalTypeScopes,
    local_comptime_values: Vec<HashMap<String, comptime::ComptimeValue>>,
    local_comptime_params: Vec<HashSet<String>>,
    bindings: HashMap<String, Option<&'a Binding>>,
    annotations: HashMap<String, &'a Expr>,
    memo: HashMap<String, TypeScheme>,
    in_progress: HashSet<String>,
    unifier: Unifier,
    /// Host/library globals seeded into the top-level value environment. They
    /// are checked through the same `value_types` paths as user declarations,
    /// which shadow them.
    globals: Vec<(String, Type)>,
    host_comptime_fns: HashMap<String, HostComptimeFnSpec>,
    report_unbound_names: bool,
    report_unresolved_bindings: bool,
    reported_unbound_name_spans: HashSet<Span>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) inferred_types: Vec<InferredType>,
}

#[derive(Clone)]
struct DiagnosticSnapshot {
    diagnostics_len: usize,
    reported_unbound_name_spans: HashSet<Span>,
}

#[derive(Debug, Clone)]
struct MatchArmBodyType {
    span: Span,
    ty: Type,
}

#[derive(Debug, Clone)]
struct MatchArmTypeConflict {
    earlier_ty: Type,
    diverging_ty: Type,
    diverging_span: Span,
}

#[derive(Debug, Clone)]
struct RuntimeMatchArmTypeConflict {
    earlier: String,
    diverging: String,
    diverging_span: Span,
}

enum MatchArmCombination {
    Joined(Type),
    Conflict(MatchArmTypeConflict),
}

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

    fn collect_top_level_environment(&mut self, module: &'a Module) {
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

    fn build_value_types(&mut self, module: &Module) {
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

    fn build_comptime_artifacts(&mut self, module: &Module) {
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

    fn check_declaration(&mut self, module: &Module, declaration: &Declaration) {
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

    fn check_value_expr_without_unbound_names(&mut self, expr: &Expr) {
        let previous_unbound = self.report_unbound_names;
        let previous_unresolved = self.report_unresolved_bindings;
        self.report_unbound_names = false;
        self.report_unresolved_bindings = false;
        self.check_value_expr(expr);
        self.report_unbound_names = previous_unbound;
        self.report_unresolved_bindings = previous_unresolved;
    }

    fn push_local_comptime_param_scope(&mut self, params: &[Param]) {
        self.local_comptime_params.push(
            params
                .iter()
                .filter(|param| param.comptime)
                .map(|param| param.name.clone())
                .collect(),
        );
    }

    fn check_items(&mut self, items: &[Item]) {
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

    fn check_local_binding(&mut self, binding: &Binding, signature: Option<&Signature>) {
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
            let scheme = generalize(resolved, &env_metas, &env_row_vars);
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

    fn check_runtime_binding_liftability(&mut self, value: &Expr) {
        let mut visiting = HashSet::new();
        if self.runtime_rhs_is_artifact(value, &mut visiting) {
            self.report_non_liftable_into_runtime(value.span);
        }
    }

    fn check_comptime_binding_evaluation_support(&mut self, value: &Expr) {
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

    fn is_unshadowed_record_selection_builtin_call(&self, value: &Expr) -> bool {
        let ExprKind::Call { callee, .. } = &ungroup_expr(value).kind else {
            return false;
        };
        let Some(name) = expr_name(callee) else {
            return false;
        };

        comptime::RecordSelectionKind::from_name(name).is_some()
            && !self.record_selection_builtin_is_shadowed(&TypeEnv::new(), name)
    }

    fn comptime_binding_is_artifact(&mut self, name: &str, visiting: &mut HashSet<String>) -> bool {
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

    fn runtime_rhs_is_artifact(&mut self, value: &Expr, visiting: &mut HashSet<String>) -> bool {
        match &value.kind {
            ExprKind::Group(inner) => self.runtime_rhs_is_artifact(inner, visiting),
            ExprKind::ComptimeName(name) => self.comptime_reference_is_artifact(name, visiting),
            ExprKind::Name(_) => false,
            _ if Self::literal_or_tag_value_shape(value) => false,
            _ => self.rhs_is_non_liftable_artifact(value, visiting),
        }
    }

    fn comptime_reference_is_artifact(
        &mut self,
        name: &str,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if self.comptime_bindings.contains(name) {
            return self.comptime_binding_is_artifact(name, visiting);
        }

        self.known_types.contains(name)
    }

    fn rhs_is_non_liftable_artifact(
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

    fn lower_clean_normalized_type(&self, value: &Expr) -> Option<Type> {
        let mut checker = self.fork_annotation_checker();
        let lowering = checker.lower_annotation_with_diagnostics(value);
        lowering
            .diagnostics
            .is_empty()
            .then(|| checker.normalize(&lowering.ty))
    }

    fn expr_contains_runtime_value_reference(&self, value: &Expr) -> bool {
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

    fn expr_contains_unknown_comptime_reference(
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

    fn literal_or_tag_value_shape(value: &Expr) -> bool {
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

    fn row_literal_or_tag_value_shape(entries: &[RecordEntry]) -> bool {
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

    fn is_runtime_value_reference(&self, name: &str) -> bool {
        self.local_types.get(name).is_some()
            || (self.bindings.contains_key(name) && !self.comptime_bindings.contains(name))
    }

    fn record_local_value_type(&mut self, name_span: Span, value_type: &LocalValueType) {
        match value_type {
            LocalValueType::Known(ty) => self.record_inferred_type(name_span, ty.clone()),
            LocalValueType::Scheme(scheme) => {
                self.record_synthesized_type(name_span, &scheme.ty);
            }
            LocalValueType::Unknown => {}
        }
    }

    fn record_inferred_type(&mut self, name_span: Span, ty: Type) {
        if type_contains_deferred(&ty) {
            return;
        }

        self.inferred_types.push(InferredType { name_span, ty });
    }

    fn record_synthesized_type(&mut self, name_span: Span, ty: &Type) {
        self.record_inferred_type(name_span, display_inferred_type(ty));
    }

    fn top_level_binding_final_type(&mut self, name: &str) -> Option<Type> {
        let scheme = self.memo.get(name).cloned()?;
        let ty = self.unifier.instantiate_scheme(&scheme);
        Some(self.normalize(&self.resolve_and_default(&ty)))
    }

    fn report_unresolved_runtime_binding_if_stuck(
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

    fn binding_value_had_prior_diagnostic(
        &self,
        binding: &Binding,
        diagnostics_start: usize,
    ) -> bool {
        self.diagnostics[..diagnostics_start]
            .iter()
            .filter_map(|diagnostic| diagnostic.labels.first())
            .any(|label| binding.value.span.contains(label.span))
    }

    fn binding_value_is_bare_placeholder(value: &Expr) -> bool {
        match &ungroup_expr(value).kind {
            ExprKind::Name(name) if name_is_placeholder(name) => true,
            ExprKind::Missing => true,
            _ => false,
        }
    }

    fn binding_value_is_overload_selection_pending(&self, value: &Expr) -> bool {
        match &ungroup_expr(value).kind {
            ExprKind::Name(name) => self.name_is_deferred_overload(name),
            ExprKind::Call { callee, .. } => {
                self.binding_value_is_overload_selection_pending(callee)
            }
            _ => false,
        }
    }

    fn name_is_deferred_overload(&self, name: &str) -> bool {
        self.bindings
            .get(name)
            .is_some_and(|binding| binding.is_none())
            && self
                .value_types
                .get(name)
                .is_some_and(|scheme| scheme.is_none())
    }

    fn binding_value_is_host_comptime_runtime_arg_deferral(&self, value: &Expr) -> bool {
        let ExprKind::Call { callee, args } = &ungroup_expr(value).kind else {
            return false;
        };
        let Some(name) = expr_name(callee) else {
            return false;
        };
        let env = self.local_types.inference_env();
        let Some(spec) = self.host_comptime_fn(&env, name) else {
            return false;
        };

        let bindings = self.current_comptime_value_bindings();
        spec.comptime_params.iter().any(|index| {
            let Some(arg) = args.get(*index) else {
                return false;
            };

            self.evaluate_comptime_param_argument(arg, &bindings)
                .is_none()
        })
    }

    fn binding_value_is_open_record_rest_match_unknown(&mut self, value: &Expr) -> bool {
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

    fn infer_record_subject_row_for_exemption(&mut self, subject: &Expr) -> Option<Row> {
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

    fn record_expr_type(&mut self, span: Span, ty: &Type) {
        if span.is_empty() {
            return;
        }

        let ty = self.normalize(&self.resolve_and_default(ty));
        if is_resolved_value_type(&ty) {
            self.record_synthesized_type(span, &ty);
        }
    }

    fn diagnostic_snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            diagnostics_len: self.diagnostics.len(),
            reported_unbound_name_spans: self.reported_unbound_name_spans.clone(),
        }
    }

    fn restore_diagnostic_snapshot(&mut self, snapshot: DiagnosticSnapshot) {
        self.diagnostics.truncate(snapshot.diagnostics_len);
        self.reported_unbound_name_spans = snapshot.reported_unbound_name_spans;
    }

    fn push_type_mismatch_diagnostic(&mut self, diagnostic: Diagnostic) {
        let primary_span = diagnostic.labels.first().map(|label| label.span);
        if primary_span.is_some_and(|span| {
            self.diagnostics.iter().any(|existing| {
                existing.code.as_deref() == Some(codes::ty::MISMATCH)
                    && existing.labels.first().map(|label| label.span) == Some(span)
            })
        }) {
            return;
        }

        self.diagnostics.push(diagnostic);
    }

    fn deduplicate_diagnostics_since(&mut self, start: usize) {
        let mut index = start;
        while index < self.diagnostics.len() {
            let code = self.diagnostics[index].code.as_deref();
            let primary_span = self.diagnostics[index]
                .labels
                .first()
                .map(|label| label.span);
            let duplicate = code.is_some()
                && primary_span.is_some()
                && self.diagnostics[..index].iter().any(|existing| {
                    existing.code.as_deref() == code
                        && existing.labels.first().map(|label| label.span) == primary_span
                });
            if duplicate {
                self.diagnostics.remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn check_value_expr(&mut self, expr: &Expr) {
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

    fn check_name_reference(&mut self, name: &str, span: Span) {
        let env = self.local_types.inference_env();
        self.infer_name_reference(&env, name, span);
    }

    /// Check a call expression in statement position. When the callee resolves
    /// to a concretely-known function type (e.g. a host global), surface
    /// argument/arity errors through the existing arity/mismatch machinery
    /// rather than letting inference silently defer them. A non-concrete callee
    /// (unknown/free name) keeps today's permissive behaviour.
    fn check_value_call(&mut self, callee: &Expr, args: &[Expr]) {
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
    fn check_value_field_access(&mut self, receiver: &Expr, field: &str, span: Span) {
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

    fn check_lambda_value_expr(
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

    fn check_value_exprs(&mut self, items: &[Expr]) {
        for item in items {
            self.check_value_expr(item);
        }
    }

    fn check_value_record_entries(&mut self, entries: &[RecordEntry]) {
        self.report_value_record_markers(entries);
        self.report_redundant_undefined_record_fields(entries);
        self.walk_value_record_values(entries);
    }

    fn report_value_record_markers(&mut self, entries: &[RecordEntry]) {
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

    fn report_redundant_undefined_record_fields(&mut self, entries: &[RecordEntry]) {
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

    fn walk_value_record_values(&mut self, entries: &[RecordEntry]) {
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

    fn check_match_arms(&mut self, subject: &Expr, arms: &[MatchArm], expected: Option<&Type>) {
        self.check_value_expr(subject);
        let env = self.local_types.inference_env();
        let inferred_subject = self.infer(&env, subject);
        let resolved_subject = self.normalize(&self.resolve_and_default(&inferred_subject));
        self.check_match_exhaustiveness(subject, arms, &resolved_subject);
        let subject_type = is_concrete_type(&resolved_subject).then_some(resolved_subject);

        let mut body_types = Vec::new();
        for arm in arms {
            self.local_types.push();
            if pattern_contains_or_pattern(&arm.pattern) {
                self.report_unsupported_or_patterns(&arm.pattern);
            } else {
                for (name, ty) in pattern_local_types(&arm.pattern, subject_type.as_ref()) {
                    self.local_types.define(&name, ty);
                }
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

    fn infer_match_arm_body_type_for_check(&mut self, env: &TypeEnv, body: &Expr) -> Type {
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let inferred_types_len = self.inferred_types.len();
        let ty = self.infer(env, body);
        self.restore_diagnostic_snapshot(diagnostic_snapshot);
        self.inferred_types.truncate(inferred_types_len);
        ty
    }

    fn report_incompatible_match_arm_results(
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

    fn runtime_match_arm_type_conflict(
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

    fn report_incompatible_match_arm_type_conflict(
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

    fn expr_references_unresolved_comptime_param(&self, expr: &Expr) -> bool {
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

    fn unresolved_comptime_param_is_in_scope(&self, name: &str) -> bool {
        self.lookup_comptime_value(name).is_none()
            && self
                .local_comptime_params
                .iter()
                .rev()
                .any(|scope| scope.contains(name))
    }

    fn check_match_exhaustiveness(
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
            .any(|arm| arm.guards.is_empty() && is_catch_all_pattern(&arm.pattern));
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
                    .filter_map(|arm| variant_pattern_tag(&arm.pattern))
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
                    .filter_map(|arm| {
                        literal_pattern_value(&arm.pattern).map(|(literal, _)| literal)
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

    fn lower_normalized_annotation(&mut self, annotation: &Expr) -> Type {
        let ty = self.lower_annotation(annotation);
        self.normalize(&ty)
    }

    fn check_value_against(&mut self, expected: &Type, value: &Expr) {
        match (&value.kind, expected) {
            (ExprKind::Group(inner), _) => self.check_value_against(expected, inner),
            (ExprKind::Block(items), _) => self.check_block_against(expected, items),
            (
                ExprKind::Lambda {
                    params,
                    return_annotation,
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
                body,
                expected_params,
                expected_result,
            ),
            (ExprKind::Name(name) | ExprKind::ComptimeName(name), _) => {
                let env = self.local_types.inference_env();
                let actual = self.infer_name_reference(&env, name, value.span);
                if !type_contains_deferred(&actual) {
                    self.check_type_against_type(expected, &actual, value.span);
                }
            }
            (_, Type::Optional(inner)) => {
                if !is_undefined_value(value) {
                    self.check_value_against(inner, value);
                }
            }
            (_, Type::Nullable(inner)) => {
                if !is_null_value(value) {
                    self.check_value_against(inner, value);
                }
            }
            (ExprKind::Literal(literal), Type::Named(name)) => {
                if let Some(found) = mismatched_literal_kind(name, literal) {
                    self.report_type_mismatch(name, found, value.span);
                }
            }
            (
                ExprKind::Literal(literal @ (Literal::Number(_) | Literal::String(_))),
                Type::Variant(row),
            ) => {
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
                ExprKind::Array(elements),
                Type::Apply {
                    callee,
                    args: element_types,
                },
            ) if matches!(callee.as_ref(), Type::Named(name) if name == "Array")
                && element_types.len() == 1 =>
            {
                self.check_collection_elements(&element_types[0], elements);
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
                if let Some(elements) = literal_set_elements(entries) {
                    self.check_collection_elements(&element_types[0], elements);
                } else {
                    self.walk_value_record_values(entries);
                }
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

    fn check_block_against(&mut self, expected: &Type, items: &[Item]) {
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

    fn check_lambda_against_function(
        &mut self,
        lambda_span: Span,
        params: &[Param],
        return_annotation: Option<&Expr>,
        body: &Expr,
        expected_params: &[Type],
        expected_result: &Type,
    ) {
        if params.len() != expected_params.len() {
            // The expected type is a function-type annotation, which has no
            // defaults: required == total.
            self.report_function_arity_mismatch(
                expected_params.len(),
                expected_params.len(),
                params.len(),
                lambda_span,
            );
            self.check_lambda_value_expr(params, return_annotation, body);
            return;
        }

        let mut param_types = Vec::new();
        for (param, expected) in params.iter().zip(expected_params) {
            let actual = param
                .annotation
                .as_ref()
                .map(|annotation| {
                    let actual = self.lower_normalized_annotation(annotation);
                    // Function parameters are contravariant. A lambda's
                    // explicit parameter annotation is the actual accepted type,
                    // so compare it in the same swapped direction as
                    // Function-vs-Function comparison.
                    self.check_type_against_type(&actual, expected, annotation.span);
                    actual
                })
                .unwrap_or_else(|| expected.clone());
            param_types.push(actual);
        }

        let body_expected = if let Some(annotation) = return_annotation {
            let actual = self.lower_normalized_annotation(annotation);
            self.check_type_against_type(expected_result, &actual, annotation.span);
            actual
        } else {
            expected_result.clone()
        };

        self.local_types.push();
        self.push_local_comptime_param_scope(params);
        for (param, ty) in params.iter().zip(param_types) {
            self.record_inferred_type(param.name_span, ty.clone());
            self.local_types
                .define(&param.name, LocalValueType::Known(ty));
        }
        self.check_value_against(&body_expected, body);
        self.local_comptime_params.pop();
        self.local_types.pop();
    }

    fn check_collection_elements<'b>(
        &mut self,
        element_type: &Type,
        elements: impl IntoIterator<Item = &'b Expr>,
    ) {
        for element in elements {
            self.check_value_against(element_type, element);
        }
    }

    fn check_call_arg_against_param(&mut self, expected: &Type, arg: &Expr) -> bool {
        let diagnostics_start = self.diagnostics.len();
        self.check_value_against(expected, arg);
        let reported_diagnostic = self.diagnostics.len() > diagnostics_start;
        self.deduplicate_diagnostics_since(diagnostics_start);
        !reported_diagnostic
    }

    fn check_type_against_type(&mut self, expected: &Type, actual: &Type, span: Span) {
        if expected == actual {
            return;
        }

        match (expected, actual) {
            (Type::Optional(expected_inner), Type::Optional(actual_inner))
            | (Type::Nullable(expected_inner), Type::Nullable(actual_inner)) => {
                self.check_type_against_type(expected_inner, actual_inner, span);
            }
            (Type::Optional(_), Type::Named(name)) if name == "Undefined" => {}
            (Type::Nullable(_), Type::Named(name)) if name == "Null" => {}
            (Type::Optional(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Nullable(inner), _) => self.check_type_against_type(inner, actual, span),
            (Type::Named(expected), Type::Named(actual))
                if named_type_mismatch(expected, actual) =>
            {
                self.report_type_mismatch_between_types(expected, actual, span);
            }
            (Type::Named(expected), actual @ (Type::Optional(_) | Type::Nullable(_))) => {
                let inner = match actual {
                    Type::Optional(inner) | Type::Nullable(inner) => inner,
                    _ => unreachable!("actual is constrained by the outer pattern"),
                };
                if let Type::Named(actual_name) = inner.as_ref()
                    && (named_type_mismatch(expected, actual_name) || expected == actual_name)
                {
                    self.report_type_mismatch_between_types(expected, &actual.render(), span);
                }
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
            (Type::Record(expected), Type::Record(actual)) => {
                let (Some(expected), Some(actual)) =
                    (literal_record_type(expected), literal_record_type(actual))
                else {
                    return;
                };
                if actual.open
                    || actual
                        .fields
                        .iter()
                        .any(|field| self.type_admits_undefined(field.ty))
                {
                    return;
                }

                let actual_fields: Vec<_> = actual
                    .fields
                    .iter()
                    .map(|field| (field.name, span, FieldValue::Type(field.ty)))
                    .collect();
                self.compare_record(&expected, &actual_fields, ExtraFields::Allow, span);
            }
            (Type::Variant(expected), Type::Variant(actual)) => {
                self.check_variant_type_against_type(expected, actual, span);
            }
            _ => {}
        }
    }

    fn check_variant_type_against_type(&mut self, expected: &Row, actual: &Row, span: Span) {
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

    fn resolve_variant_row(&self, row: &Row) -> Row {
        let Type::Variant(row) = self.unifier.resolve(&Type::Variant(row.clone())) else {
            unreachable!("variant resolution preserves the outer type")
        };
        row
    }

    fn check_named_type_against_variant(&mut self, expected: &Row, actual: &str, span: Span) {
        let expected = self.resolve_variant_row(expected);
        let Some(literals) = literal_variant_members(&expected) else {
            return;
        };

        let rendered_expected = Type::Variant(expected.clone()).render();
        if literal_union_accepts_base_type(&literals, actual) {
            self.report_wide_value_into_literal_union(&rendered_expected, actual, span);
        } else {
            self.report_type_mismatch_between_types(&rendered_expected, actual, span);
        }
    }

    fn check_variant_type_against_named(&mut self, expected: &str, actual: &Row, span: Span) {
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

    fn check_literal_value_against_variant(&mut self, row: &Row, literal: &Literal, span: Span) {
        let row = self.resolve_variant_row(row);
        let Some(literals) = literal_variant_members(&row) else {
            self.report_type_mismatch(
                &Type::Variant(row).render(),
                literal_kind_name(literal),
                span,
            );
            return;
        };

        if let Some(expected_base) = literal_variant_base(&row)
            && literal_base(literal) != Some(expected_base)
        {
            self.report_type_mismatch(
                &Type::Variant(row).render(),
                literal_kind_name(literal),
                span,
            );
            return;
        }

        if row.tail != RowTail::Closed || literals.contains(&literal) {
            return;
        }

        self.report_literal_not_in_union(literal, &literals, span);
    }

    fn check_record_value_against(
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
            self.compare_record(&expected, &actual_fields, ExtraFields::Reject, actual.span);
            return;
        }

        let env = self.local_types.inference_env();
        let actual = self.infer_record_entries(&env, value_entries);
        if !type_contains_deferred(&actual) {
            self.check_type_against_type(&Type::Record(row.clone()), &actual, value_span);
        }
        self.walk_value_record_values(value_entries);
    }

    fn check_literal_record_shorthands(&mut self, record: &ValueRecordShape<'_>) {
        for field in &record.fields {
            if field.value.is_none() {
                self.check_name_reference(field.name, field.name_span);
            }
        }
    }

    fn check_variant_value_against(
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

    fn compare_record(
        &mut self,
        expected: &ExpectedRecordShape<'_>,
        actual: &[(&str, Span, FieldValue<'_>)],
        extra_fields: ExtraFields,
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
                None => self.report_missing_field(field.name, record_span),
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

    pub(crate) fn lower_declared_annotation(
        &mut self,
        source: DeclaredAnnotationSource<'_>,
    ) -> DeclaredAnnotation {
        let lowering = self.lower_annotation_with_diagnostics(source.annotation);

        DeclaredAnnotation {
            name: source.name.to_owned(),
            declaration_span: source.declaration_span,
            annotation_span: source.annotation.span,
            ty: lowering.ty,
            diagnostics: lowering.diagnostics,
        }
    }

    pub(crate) fn lower_annotation_with_diagnostics(&mut self, annotation: &Expr) -> TypeLowering {
        let start = self.diagnostics.len();
        let ty = self.lower_annotation(annotation);
        let diagnostics = self.diagnostics[start..].to_vec();

        TypeLowering { ty, diagnostics }
    }

    fn try_lower_comptime_annotation(&mut self, annotation: &Expr) -> Option<Type> {
        let evaluation = comptime::evaluate_type_position(self, annotation);
        self.diagnostics.extend(evaluation.diagnostics);

        match evaluation.evaluation {
            Evaluation::Evaluated(value) => value.reify_type_position().into_reified_type(),
            Evaluation::Deferred => Some(Type::Deferred),
            Evaluation::Unsupported => None,
        }
    }

    fn reflection_subject_is_unresolved(&self, ty: &Type) -> bool {
        match ty {
            Type::Deferred | Type::Variable(_) | Type::Meta(_) => true,
            Type::Named(name) => !BUILTIN_TYPES.contains(&name.as_str()),
            Type::Apply { callee, args } => {
                self.reflection_subject_is_unresolved(callee)
                    || args
                        .iter()
                        .any(|arg| self.reflection_subject_is_unresolved(arg))
            }
            Type::Function { params, result, .. } => {
                params
                    .iter()
                    .any(|param| self.reflection_subject_is_unresolved(param))
                    || self.reflection_subject_is_unresolved(result)
            }
            Type::Optional(inner) | Type::Nullable(inner) => {
                self.reflection_subject_is_unresolved(inner)
            }
            Type::Tuple(items) => items
                .iter()
                .any(|item| self.reflection_subject_is_unresolved(item)),
            Type::Record(row) => row.tail != RowTail::Closed,
            Type::Variant(row) => {
                row.tail != RowTail::Closed
                    || row.entries.iter().any(|entry| match entry {
                        RowEntry::Tag { payload, .. } => payload
                            .iter()
                            .any(|ty| self.reflection_subject_is_unresolved(ty)),
                        RowEntry::Field { ty, .. } => self.reflection_subject_is_unresolved(ty),
                        RowEntry::Literal { .. } => false,
                    })
            }
        }
    }

    fn type_admits_undefined(&self, ty: &Type) -> bool {
        matches!(self.normalize(ty), Type::Optional(_))
    }

    fn strip_optional(&self, ty: &Type) -> Type {
        match self.normalize(ty) {
            Type::Optional(inner) => *inner,
            ty => ty,
        }
    }

    fn strip_nullable(&self, ty: &Type) -> Type {
        match self.normalize(ty) {
            Type::Optional(inner) => Type::Optional(Box::new(self.strip_nullable(&inner))),
            Type::Nullable(inner) => *inner,
            ty => ty,
        }
    }

    pub(crate) fn normalize(&self, ty: &Type) -> Type {
        self.normalize_with_visited(ty, HashSet::new())
    }

    fn normalize_with_visited(&self, ty: &Type, visited: HashSet<String>) -> Type {
        match ty {
            Type::Named(name) => {
                let Some(definition) = self.type_definitions.get(name) else {
                    return Type::Named(name.clone());
                };

                if visited.contains(name) {
                    return Type::Named(name.clone());
                }

                let mut next_visited = visited;
                next_visited.insert(name.clone());
                self.normalize_with_visited(definition, next_visited)
            }
            Type::Deferred => Type::Deferred,
            Type::Variable(name) => Type::Variable(name.clone()),
            Type::Meta(id) => Type::Meta(*id),
            Type::Apply { callee, args } => Type::Apply {
                callee: Box::new(self.normalize_with_visited(callee, visited.clone())),
                args: self.normalize_types(args, &visited),
            },
            Type::Function {
                params,
                result,
                required,
            } => Type::Function {
                params: self.normalize_types(params, &visited),
                result: Box::new(self.normalize_with_visited(result, visited)),
                required: *required,
            },
            Type::Optional(inner) => self.normalize_optional(inner, visited),
            Type::Nullable(inner) => self.normalize_nullable(inner, visited),
            Type::Tuple(items) => Type::Tuple(self.normalize_types(items, &visited)),
            Type::Record(row) => Type::Record(self.normalize_row(row, &visited)),
            Type::Variant(row) => Type::Variant(self.normalize_row(row, &visited)),
        }
    }

    fn normalize_types(&self, types: &[Type], visited: &HashSet<String>) -> Vec<Type> {
        types
            .iter()
            .map(|ty| self.normalize_with_visited(ty, visited.clone()))
            .collect()
    }

    fn normalize_optional(&self, inner: &Type, visited: HashSet<String>) -> Type {
        match self.normalize_with_visited(inner, visited) {
            Type::Optional(inner) => Type::Optional(inner),
            inner => Type::Optional(Box::new(inner)),
        }
    }

    fn normalize_nullable(&self, inner: &Type, visited: HashSet<String>) -> Type {
        match self.normalize_with_visited(inner, visited) {
            Type::Optional(inner) => Type::Optional(Box::new(Type::Nullable(inner))),
            Type::Nullable(inner) => Type::Nullable(inner),
            inner => Type::Nullable(Box::new(inner)),
        }
    }

    fn normalize_row(&self, row: &Row, visited: &HashSet<String>) -> Row {
        Row {
            entries: row
                .entries
                .iter()
                .map(|entry| self.normalize_row_entry(entry, visited))
                .collect(),
            tail: row.tail,
        }
    }

    fn normalize_row_entry(&self, entry: &RowEntry, visited: &HashSet<String>) -> RowEntry {
        match entry {
            RowEntry::Field { name, ty } => RowEntry::Field {
                name: name.clone(),
                ty: self.normalize_with_visited(ty, visited.clone()),
            },
            RowEntry::Tag { name, payload } => RowEntry::Tag {
                name: name.clone(),
                payload: self.normalize_types(payload, visited),
            },
            RowEntry::Literal { value } => RowEntry::Literal {
                value: value.clone(),
            },
        }
    }

    fn fork_annotation_checker(&self) -> Checker<'a> {
        let mut checker =
            Checker::with_type_definitions(self.known_types.clone(), self.type_definitions.clone());
        checker.comptime_bindings = self.comptime_bindings.clone();
        checker.comptime_artifacts = self.comptime_artifacts.clone();
        checker.comptime_specializations = self.comptime_specializations.clone();
        checker.local_comptime_values = self.local_comptime_values.clone();
        checker.local_comptime_params = self.local_comptime_params.clone();
        checker.bindings = self.bindings.clone();
        checker.annotations = self.annotations.clone();
        checker
    }

    pub(crate) fn lower_annotation(&mut self, annotation: &Expr) -> Type {
        match &annotation.kind {
            ExprKind::ComptimeName(name) => {
                if let Some(ty) = self.lookup_comptime_reified_type(name) {
                    return ty.clone();
                }

                self.check_type_name(name, annotation.span);
                Type::Named(name.clone())
            }
            ExprKind::Name(name) => self
                .lookup_comptime_reified_type(name)
                .cloned()
                .unwrap_or_else(|| Type::Variable(name.clone())),
            ExprKind::Group(inner) => self.lower_annotation(inner),
            ExprKind::Index { callee, args } => self
                .lower_comptime_type_index(callee, args)
                .unwrap_or_else(|| Type::Apply {
                    callee: Box::new(self.lower_annotation(callee)),
                    args: self.lower_annotations(args),
                }),
            ExprKind::Optional(inner) => Type::Optional(Box::new(self.lower_annotation(inner))),
            ExprKind::Nullable(inner) => Type::Nullable(Box::new(self.lower_annotation(inner))),
            ExprKind::NonNull(inner) => {
                let inner = self.lower_annotation(inner);
                self.strip_nullable(&inner)
            }
            ExprKind::Unary {
                operator, value, ..
            } if operator == "!" => {
                let inner = self.lower_annotation(value);
                self.strip_optional(&inner)
            }
            ExprKind::Arrow { params, result } => {
                // A function-type annotation has no defaults: all params are
                // required. Standalone function-type default syntax is deferred.
                let lowered = self.lower_annotations(params);
                let required = lowered.len();
                Type::Function {
                    params: lowered,
                    result: Box::new(self.lower_annotation(result)),
                    required,
                }
            }
            ExprKind::Tuple(items) => Type::Tuple(self.lower_annotations(items)),
            ExprKind::Record(entries) => self.lower_row_entries(entries, RowKind::Record),
            ExprKind::Set(entries) => self.lower_row_entries(entries, RowKind::Variant),
            ExprKind::Binary { operator, .. } if operator == "|" => {
                self.lower_union_annotation(annotation)
            }
            ExprKind::Literal(Literal::Number(_) | Literal::String(_)) | ExprKind::Tag(_) => {
                self.lower_singleton_variant_annotation(annotation)
            }
            ExprKind::Literal(Literal::Bool(_)) => named_builtin("Bool"),
            ExprKind::Call { .. } => self
                .try_lower_comptime_annotation(annotation)
                .unwrap_or_else(|| {
                    self.lower_deferred_annotation(annotation);
                    Type::Deferred
                }),
            ExprKind::Missing => Type::Deferred,
            ExprKind::Literal(_)
            | ExprKind::Interpolation(_)
            | ExprKind::Undefined
            | ExprKind::Null
            | ExprKind::Array(_)
            | ExprKind::FieldAccess { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Propagate { .. }
            | ExprKind::Match { .. }
            | ExprKind::Lambda { .. }
            | ExprKind::Block(_) => {
                self.lower_deferred_annotation(annotation);
                Type::Deferred
            }
        }
    }

    fn lower_union_annotation(&mut self, annotation: &Expr) -> Type {
        let entries = union_annotation_entries(annotation);
        self.lower_row_entries(&entries, RowKind::Variant)
    }

    fn lower_singleton_variant_annotation(&mut self, annotation: &Expr) -> Type {
        let entry = RecordEntry::Element(annotation.clone());
        self.lower_row_entries(std::slice::from_ref(&entry), RowKind::Variant)
    }

    fn lower_annotations(&mut self, items: &[Expr]) -> Vec<Type> {
        items
            .iter()
            .map(|item| self.lower_annotation(item))
            .collect()
    }

    fn lower_comptime_type_index(&mut self, callee: &Expr, args: &[Expr]) -> Option<Type> {
        let subject = self.lookup_comptime_reified_type_expr(callee)?;
        let [arg] = args else {
            return Some(Type::Deferred);
        };

        let subject = self.normalize(&subject);
        let Type::Record(row) = subject else {
            return Some(Type::Deferred);
        };
        if row.tail != RowTail::Closed {
            return Some(Type::Deferred);
        }

        let Some(label) = self.comptime_known_label(arg) else {
            return Some(Type::Deferred);
        };

        Some(
            row_field_type(&row, &label)
                .cloned()
                .unwrap_or(Type::Deferred),
        )
    }

    fn lookup_comptime_reified_type_expr(&self, expr: &Expr) -> Option<Type> {
        match &ungroup_expr(expr).kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                self.lookup_comptime_reified_type(name).cloned()
            }
            _ => None,
        }
    }

    fn lookup_comptime_reified_type(&self, name: &str) -> Option<&Type> {
        match self.lookup_comptime_value(name)? {
            comptime::ComptimeValue::ReifiedType(ty) => Some(ty),
            comptime::ComptimeValue::LabelSet(_)
            | comptime::ComptimeValue::Literal(_)
            | comptime::ComptimeValue::Bool(_) => None,
        }
    }

    fn lower_deferred_annotation(&mut self, annotation: &Expr) {
        walk_expr_children(annotation, &mut |child| {
            self.lower_annotation(child);
        });
    }

    fn lower_row_entries(&mut self, entries: &[RecordEntry], kind: RowKind) -> Type {
        let row = match self.fold_row_entries(entries, kind, RowFoldMode::Annotation) {
            Ok(row) => row,
            Err(()) => return Type::Deferred,
        };

        match kind {
            RowKind::Record => Type::Record(row),
            RowKind::Variant => Type::Variant(row),
        }
    }

    fn infer_record_entries(&mut self, env: &TypeEnv, entries: &[RecordEntry]) -> Type {
        let row = match self.fold_row_entries(entries, RowKind::Record, RowFoldMode::Value { env })
        {
            Ok(row) => row,
            Err(()) => return Type::Deferred,
        };

        Type::Record(row)
    }

    fn fold_row_entries(
        &mut self,
        entries: &[RecordEntry],
        kind: RowKind,
        mode: RowFoldMode<'_>,
    ) -> Result<Row, ()> {
        let mut row = Row {
            entries: Vec::new(),
            tail: RowTail::Closed,
        };

        for (index, entry) in entries.iter().enumerate() {
            if self.fold_row_entry(entry, kind, mode, &mut row).is_err() {
                for remaining in &entries[index + 1..] {
                    self.fold_deferred_row_entry(remaining, kind, mode);
                }
                return Err(());
            }
        }

        Ok(row)
    }

    fn fold_row_entry(
        &mut self,
        entry: &RecordEntry,
        kind: RowKind,
        mode: RowFoldMode<'_>,
        row: &mut Row,
    ) -> Result<(), ()> {
        match entry {
            RecordEntry::Field {
                name,
                value,
                overwrite,
                span,
                ..
            } => {
                if kind != RowKind::Record {
                    return Err(());
                }
                if matches!(mode, RowFoldMode::Value { .. }) && is_undefined_value(value) {
                    return Ok(());
                }

                let ty = self.fold_field_type(value, mode);

                let entry = RowEntry::Field {
                    name: name.clone(),
                    ty,
                };

                if *overwrite {
                    if let Some(index) = row_entry_index(&row.entries, name) {
                        row.entries[index] = entry;
                    } else if row.tail == RowTail::Closed {
                        self.report_replace_absent_field(name, *span);
                        return Err(());
                    } else {
                        row.entries.push(entry);
                    }
                    Ok(())
                } else if row_entry_index(&row.entries, name).is_some() {
                    self.report_duplicate_row_label(
                        name,
                        *span,
                        match mode {
                            RowFoldMode::Annotation => DuplicateRowLabelContext::RecordAdd,
                            RowFoldMode::Value { .. } => DuplicateRowLabelContext::RecordValueAdd,
                        },
                    );
                    Err(())
                } else {
                    row.entries.push(entry);
                    Ok(())
                }
            }
            RecordEntry::FieldComputed { key, value, span } => {
                if kind == RowKind::Record
                    && matches!(mode, RowFoldMode::Value { .. })
                    && is_undefined_value(value)
                {
                    self.fold_expression(key, mode);
                    return Ok(());
                }

                let Some(label) = self.comptime_known_label(key) else {
                    self.fold_deferred_row_entry(entry, kind, mode);
                    return Err(());
                };

                let ty = self.fold_field_type(value, mode);
                if kind != RowKind::Record {
                    return Err(());
                }

                let entry = RowEntry::Field {
                    name: label.clone(),
                    ty,
                };

                if row_entry_index(&row.entries, &label).is_some() {
                    self.report_duplicate_row_label(
                        &label,
                        *span,
                        match mode {
                            RowFoldMode::Annotation => DuplicateRowLabelContext::RecordAdd,
                            RowFoldMode::Value { .. } => DuplicateRowLabelContext::RecordValueAdd,
                        },
                    );
                    Err(())
                } else {
                    row.entries.push(entry);
                    Ok(())
                }
            }
            RecordEntry::Shorthand { .. } => Err(()),
            RecordEntry::Delete { name, span, .. } => {
                if row.tail != RowTail::Closed {
                    return Err(());
                }

                if let Some(labels) = self
                    .lookup_comptime_value(name)
                    .and_then(comptime_value_label_set)
                {
                    let mut missing = false;
                    for label in &labels {
                        if row_entry_index(&row.entries, label).is_none() {
                            self.report_delete_absent_field(label, *span);
                            missing = true;
                        }
                    }
                    if missing {
                        return Err(());
                    }

                    for label in labels {
                        if let Some(index) = row_entry_index(&row.entries, &label) {
                            row.entries.remove(index);
                        }
                    }
                    return Ok(());
                }

                self.delete_closed_row_label(row, name, *span)
            }
            RecordEntry::DeleteComputed { key, span } => {
                if row.tail != RowTail::Closed {
                    self.fold_deferred_row_entry(entry, kind, mode);
                    return Err(());
                }

                let Some(label) = self.comptime_known_label(key) else {
                    self.fold_deferred_row_entry(entry, kind, mode);
                    return Err(());
                };

                self.delete_closed_row_label(row, &label, *span)
            }
            RecordEntry::Rename { from, to, span, .. } => {
                if row.tail != RowTail::Closed {
                    return Err(());
                }

                let Some(index) = row_entry_index(&row.entries, from) else {
                    self.report_rename_absent_field(from, *span);
                    return Err(());
                };

                if row_entry_index(&row.entries, to).is_some() {
                    self.report_rename_target_present(from, to, *span);
                    return Err(());
                }

                row.entries[index] = relabel_row_entry(&row.entries[index], to);
                Ok(())
            }
            RecordEntry::Spread {
                value,
                overwrite,
                span,
            } => {
                let Some(source) = self.fold_spread_source(value, kind, mode) else {
                    return Err(());
                };

                self.merge_source_row(row, source, *overwrite, *span, kind)
            }
            RecordEntry::Open { .. } => {
                if matches!(mode, RowFoldMode::Value { .. }) {
                    Err(())
                } else {
                    row.tail = RowTail::Open;
                    Ok(())
                }
            }
            RecordEntry::Iteration { .. } => self.fold_iteration_entry(entry, kind, mode, row),
            RecordEntry::Element(value) => match kind {
                RowKind::Record => self.fold_record_element(value, mode, row),
                RowKind::Variant => {
                    let Some(entry) = self.lower_variant_tag(value) else {
                        return Err(());
                    };

                    self.check_homogeneous_variant_entry(&row.entries, &entry, value.span)?;

                    let label = row_entry_label(&entry);
                    if row_entry_index(&row.entries, label).is_some() {
                        self.report_duplicate_row_label(
                            label,
                            value.span,
                            DuplicateRowLabelContext::VariantAdd,
                        );
                        Err(())
                    } else {
                        row.entries.push(entry);
                        Ok(())
                    }
                }
            },
        }
    }

    fn delete_closed_row_label(
        &mut self,
        row: &mut Row,
        label: &str,
        span: Span,
    ) -> Result<(), ()> {
        let Some(index) = row_entry_index(&row.entries, label) else {
            self.report_delete_absent_field(label, span);
            return Err(());
        };

        row.entries.remove(index);
        Ok(())
    }

    fn fold_iteration_entry(
        &mut self,
        entry: &RecordEntry,
        kind: RowKind,
        mode: RowFoldMode<'_>,
        row: &mut Row,
    ) -> Result<(), ()> {
        let RecordEntry::Iteration {
            source,
            binder,
            guard,
            body,
            ..
        } = entry
        else {
            unreachable!("fold_iteration_entry called for non-iteration entry");
        };

        if kind != RowKind::Record {
            self.fold_expression(source, mode);
            if let Some(guard) = guard {
                self.fold_expression(guard, mode);
            }
            for entry in body {
                self.fold_deferred_row_entry(entry, kind, mode);
            }
            return Err(());
        }

        let Some(labels) = self.comptime_known_label_set_for_mode(source, mode) else {
            self.fold_expression(source, mode);
            if matches!(mode, RowFoldMode::Annotation) {
                if let Some(guard) = guard {
                    self.fold_expression(guard, mode);
                }
                for entry in body {
                    self.fold_deferred_row_entry(entry, kind, mode);
                }
            }
            return Err(());
        };

        for label in labels {
            let literal = label_literal(&label);
            let mut scope = HashMap::new();
            scope.insert(
                binder.clone(),
                comptime::ComptimeValue::Literal(literal.clone()),
            );
            self.local_comptime_values.push(scope);

            let result = match mode {
                RowFoldMode::Annotation => self.fold_unrolled_iteration_body(
                    body,
                    guard.as_ref(),
                    kind,
                    RowFoldMode::Annotation,
                    row,
                ),
                RowFoldMode::Value { env } => {
                    let mut body_env = env.clone();
                    body_env.insert(binder.clone(), LocalValueType::Known(literal_type(literal)));
                    self.fold_unrolled_iteration_body(
                        body,
                        guard.as_ref(),
                        kind,
                        RowFoldMode::Value { env: &body_env },
                        row,
                    )
                }
            };

            self.local_comptime_values.pop();
            result?;
        }

        Ok(())
    }

    fn fold_unrolled_iteration_body(
        &mut self,
        body: &[RecordEntry],
        guard: Option<&Expr>,
        kind: RowKind,
        mode: RowFoldMode<'_>,
        row: &mut Row,
    ) -> Result<(), ()> {
        if let Some(guard) = guard {
            match comptime::evaluate_runtime_value(guard, &self.current_comptime_value_bindings())
                .evaluation
            {
                Evaluation::Evaluated(comptime::ComptimeValue::Bool(true)) => {}
                Evaluation::Evaluated(comptime::ComptimeValue::Bool(false)) => return Ok(()),
                Evaluation::Evaluated(_) | Evaluation::Deferred | Evaluation::Unsupported => {
                    self.fold_expression(guard, mode);
                    for body_entry in body {
                        self.fold_deferred_row_entry(body_entry, kind, mode);
                    }
                    return Err(());
                }
            }
        }

        for (index, body_entry) in body.iter().enumerate() {
            if self.fold_row_entry(body_entry, kind, mode, row).is_err() {
                for remaining in &body[index + 1..] {
                    self.fold_deferred_row_entry(remaining, kind, mode);
                }
                return Err(());
            }
        }

        Ok(())
    }

    fn fold_record_element(
        &mut self,
        value: &Expr,
        mode: RowFoldMode<'_>,
        row: &mut Row,
    ) -> Result<(), ()> {
        let ExprKind::Tuple(items) = &ungroup_expr(value).kind else {
            self.fold_expression(value, mode);
            return Err(());
        };
        let [key, field_value] = items.as_slice() else {
            self.fold_expression(value, mode);
            return Err(());
        };

        let Some(label) = self.comptime_known_label(key) else {
            self.fold_expression(value, mode);
            return Err(());
        };

        let entry = RowEntry::Field {
            name: label.clone(),
            ty: self.fold_field_type(field_value, mode),
        };

        if row_entry_index(&row.entries, &label).is_some() {
            self.report_duplicate_row_label(
                &label,
                value.span,
                match mode {
                    RowFoldMode::Annotation => DuplicateRowLabelContext::RecordAdd,
                    RowFoldMode::Value { .. } => DuplicateRowLabelContext::RecordValueAdd,
                },
            );
            Err(())
        } else {
            row.entries.push(entry);
            Ok(())
        }
    }

    fn fold_field_type(&mut self, value: &Expr, mode: RowFoldMode<'_>) -> Type {
        match mode {
            RowFoldMode::Annotation => self.lower_annotation(value),
            RowFoldMode::Value { env } => self.infer(env, value),
        }
    }

    fn fold_expression(&mut self, value: &Expr, mode: RowFoldMode<'_>) -> Type {
        match mode {
            RowFoldMode::Annotation => self.lower_annotation(value),
            RowFoldMode::Value { env } => self.infer(env, value),
        }
    }

    fn fold_spread_source(
        &mut self,
        value: &Expr,
        kind: RowKind,
        mode: RowFoldMode<'_>,
    ) -> Option<RowSource> {
        match mode {
            RowFoldMode::Annotation => {
                let ty = self.lower_annotation(value);
                self.annotation_row_source(&ty, kind)
            }
            RowFoldMode::Value { env } => {
                if kind != RowKind::Record {
                    return None;
                }
                let ty = self.infer(env, value);
                self.value_record_source(&ty)
            }
        }
    }

    fn fold_deferred_row_entry(
        &mut self,
        entry: &RecordEntry,
        kind: RowKind,
        mode: RowFoldMode<'_>,
    ) {
        match entry {
            RecordEntry::Field { value, .. }
            | RecordEntry::Spread { value, .. }
            | RecordEntry::DeleteComputed { key: value, .. } => {
                self.fold_expression(value, mode);
            }
            RecordEntry::FieldComputed { key, value, .. } => {
                self.fold_expression(key, mode);
                self.fold_expression(value, mode);
            }
            RecordEntry::Element(value) => match kind {
                RowKind::Record => {
                    self.fold_expression(value, mode);
                }
                RowKind::Variant => {
                    if matches!(mode, RowFoldMode::Annotation) {
                        self.lower_variant_tag(value);
                    } else {
                        self.fold_expression(value, mode);
                    }
                }
            },
            RecordEntry::Iteration {
                source,
                guard,
                body,
                ..
            } => {
                self.fold_expression(source, mode);
                if let Some(guard) = guard {
                    self.fold_expression(guard, mode);
                }
                for entry in body {
                    self.fold_deferred_row_entry(entry, kind, mode);
                }
            }
            RecordEntry::Shorthand { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Open { .. } => {}
        }
    }

    fn annotation_row_source(&self, ty: &Type, kind: RowKind) -> Option<RowSource> {
        match (self.normalize(ty), kind) {
            (Type::Record(row), RowKind::Record) | (Type::Variant(row), RowKind::Variant) => {
                Some(RowSource::from_row(row))
            }
            (Type::Variable(_), _) => Some(RowSource::Open(Row {
                entries: Vec::new(),
                tail: RowTail::Open,
            })),
            _ => None,
        }
    }

    fn value_record_source(&self, ty: &Type) -> Option<RowSource> {
        let resolved = self.unifier.resolve(ty);
        match self.normalize(&resolved) {
            Type::Record(row) => Some(RowSource::from_row(row)),
            Type::Deferred
            | Type::Named(_)
            | Type::Variable(_)
            | Type::Meta(_)
            | Type::Apply { .. }
            | Type::Function { .. }
            | Type::Optional(_)
            | Type::Nullable(_)
            | Type::Tuple(_)
            | Type::Variant(_) => None,
        }
    }

    fn merge_source_row(
        &mut self,
        row: &mut Row,
        source: RowSource,
        overwrite: bool,
        span: Span,
        kind: RowKind,
    ) -> Result<(), ()> {
        let (source, source_is_open) = match source {
            RowSource::Closed(row) => (row, false),
            RowSource::Open(row) => (row, true),
        };

        for entry in source.entries {
            if kind == RowKind::Variant {
                self.check_homogeneous_variant_entry(&row.entries, &entry, span)?;
            }

            let label = row_entry_label(&entry).to_owned();
            if let Some(index) = row_entry_index(&row.entries, &label) {
                if kind == RowKind::Record
                    && self.merge_optional_record_patch_field(&row.entries[index], &entry, span)
                {
                    continue;
                }

                if !overwrite {
                    self.report_duplicate_row_label(&label, span, DuplicateRowLabelContext::Spread);
                    return Err(());
                }
                row.entries[index] = entry;
            } else {
                row.entries.push(entry);
            }
        }

        if source_is_open {
            row.tail = RowTail::Open;
        }

        Ok(())
    }

    fn merge_optional_record_patch_field(
        &mut self,
        base: &RowEntry,
        incoming: &RowEntry,
        span: Span,
    ) -> bool {
        let (
            RowEntry::Field { ty: base_ty, .. },
            RowEntry::Field {
                ty: incoming_ty, ..
            },
        ) = (base, incoming)
        else {
            return false;
        };

        let Type::Optional(inner) = self.normalize(incoming_ty) else {
            return false;
        };

        self.check_type_against_type(base_ty, &inner, span);
        true
    }

    fn check_homogeneous_variant_entry(
        &mut self,
        existing_entries: &[RowEntry],
        incoming: &RowEntry,
        span: Span,
    ) -> Result<(), ()> {
        let Some(existing) = variant_entry_kind(existing_entries) else {
            return Ok(());
        };
        let Some(incoming) = row_entry_variant_kind(incoming) else {
            return Ok(());
        };
        if existing == incoming {
            return Ok(());
        }

        self.report_mixed_variant_entries(incoming, span);
        Err(())
    }

    fn lower_variant_tag(&mut self, tag: &Expr) -> Option<RowEntry> {
        match &tag.kind {
            ExprKind::Tag(name) => Some(RowEntry::Tag {
                name: name.clone(),
                payload: Vec::new(),
            }),
            ExprKind::Literal(literal @ (Literal::Number(_) | Literal::String(_))) => {
                Some(RowEntry::Literal {
                    value: literal.clone(),
                })
            }
            ExprKind::Name(name) => {
                self.report_lowercase_variant_tag(name, tag.span);
                Some(RowEntry::Tag {
                    name: name.clone(),
                    payload: Vec::new(),
                })
            }
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::Tag(name) => Some(RowEntry::Tag {
                    name: name.clone(),
                    payload: self.lower_annotations(args),
                }),
                ExprKind::Name(name) => {
                    self.report_lowercase_variant_tag(name, callee.span);
                    Some(RowEntry::Tag {
                        name: name.clone(),
                        payload: self.lower_annotations(args),
                    })
                }
                _ => {
                    self.lower_deferred_annotation(tag);
                    None
                }
            },
            _ => {
                self.lower_deferred_annotation(tag);
                None
            }
        }
    }

    fn check_type_name(&mut self, name: &str, span: Span) {
        if self.known_types.contains(name) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error(format!("unknown type name `{name}`"))
                .with_code(codes::ty::UNKNOWN_NAME)
                .with_label(Label::primary(span, "type name not found"))
                .with_note("define the type, import it, or use a lowercase type variable for a generic type"),
        );
    }

    fn report_lowercase_variant_tag(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("variant tag `{name}` must be an uppercase `@`-tag"))
                .with_code(codes::ty::LOWERCASE_VARIANT_TAG)
                .with_label(Label::primary(span, "lowercase variant tag"))
                .with_note("variant tags use uppercase names, for example `@Ok` or `@Err`"),
        );
    }

    fn report_mixed_variant_entries(&mut self, incoming: VariantEntryKind, span: Span) {
        let label = match incoming {
            VariantEntryKind::Tag => "this tag member is mixed with literal members",
            VariantEntryKind::Literal => "this literal member is mixed with tag members",
        };

        self.diagnostics.push(
            Diagnostic::error("variant rows cannot mix tags and literal members")
                .with_code(codes::ty::MIXED_VARIANT_ENTRIES)
                .with_label(Label::primary(span, label))
                .with_note("use either variant tags or literal values in one row for now"),
        );
    }

    fn report_non_liftable_into_runtime(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("runtime binding cannot hold a non-liftable comptime artifact")
                .with_code(codes::comptime::NON_LIFTABLE_INTO_RUNTIME)
                .with_label(Label::primary(
                    span,
                    "this is a non-liftable comptime artifact",
                ))
                .with_note(
                    "types are compile-time artifacts; bind them with a capitalized name, or compute a runtime value here",
                ),
        );
    }

    fn report_comptime_evaluation_unsupported(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(
                "comptime evaluation is not supported yet, so this comptime binding's value cannot be computed",
            )
            .with_code(codes::comptime::EVALUATION_UNSUPPORTED)
            .with_label(Label::primary(
                span,
                "this comptime binding needs evaluation",
            ))
            .with_note(
                "the comptime evaluator is planned for Milestone 14; write a literal type or value here, or move the computation to a lowercase runtime binding if the result is a runtime value",
            ),
        );
    }

    fn report_unresolved_binding(&mut self, name_span: Span) {
        self.diagnostics.push(
            Diagnostic::error("cannot determine a type for this binding")
                .with_code(codes::ty::UNRESOLVED_BINDING)
                .with_label(Label::primary(
                    name_span,
                    "this binding's type could not be inferred",
                ))
                .with_note("add a type annotation, or change the value so its type resolves"),
        );
    }

    fn report_type_mismatch(&mut self, expected: &str, found: &'static str, span: Span) {
        self.push_type_mismatch_diagnostic(
            Diagnostic::error(format!("expected `{expected}`, found a {found}"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, format!("this is a {found}")))
                .with_note(format!(
                    "change the value to produce `{expected}`, or change the annotation to match the literal"
                )),
        );
    }

    fn report_tuple_arity_mismatch(&mut self, expected: usize, found: usize, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected a {expected}-element tuple, found a {found}-element tuple"
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "tuple length does not match annotation",
            ))
            .with_note("add or remove tuple elements to match the annotation"),
        );
    }

    fn report_function_arity_mismatch(
        &mut self,
        required: usize,
        total: usize,
        found: usize,
        span: Span,
    ) {
        let message = if required == total {
            format!(
                "expected a function with {total} parameter{}, found one with {found}",
                if total == 1 { "" } else { "s" },
            )
        } else {
            format!("expected between {required} and {total} arguments, found {found}")
        };
        self.diagnostics.push(
            Diagnostic::error(message)
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    span,
                    "function parameter count does not match annotation",
                ))
                .with_note("add or remove parameters to match the annotation"),
        );
    }

    fn report_variant_tag_mismatch(&mut self, tag: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("unexpected variant tag `{tag}`"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, "this tag is not in the variant type"))
                .with_note("use a tag listed by the annotation, or change the annotation"),
        );
    }

    fn report_literal_not_in_union(
        &mut self,
        literal: &Literal,
        expected: &[&Literal],
        span: Span,
    ) {
        let literal = render_literal_value(literal);
        let expected = render_literal_union(expected);

        self.diagnostics.push(
            Diagnostic::error(format!("literal {literal} is not one of {expected}"))
                .with_code(codes::ty::LITERAL_NOT_IN_UNION)
                .with_label(Label::primary(
                    span,
                    "this literal is not allowed by the annotation",
                ))
                .with_note(format!(
                    "use one of {expected}, or change the literal-union annotation"
                )),
        );
    }

    fn report_wide_value_into_literal_union(&mut self, expected: &str, actual: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("expected literal union `{expected}`, found `{actual}`"))
                .with_code(codes::ty::WIDE_VALUE_INTO_LITERAL_UNION)
                .with_label(Label::primary(
                    span,
                    format!("this value has the wider `{actual}` type"),
                ))
                .with_note(
                    "a bound value may be any value of its base type; use a fresh member literal here, or keep the narrower literal-union type on the value",
                ),
        );
    }

    fn report_variant_entry_kind_mismatch(&mut self, expected: &Type, actual: &Type, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected `{}`, found `{}`",
                expected.render(),
                actual.render()
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "variant row member kinds do not match",
            ))
            .with_note("use tag variants with tag variants, or literal unions with literal unions"),
        );
    }

    fn report_variant_payload_arity_mismatch(
        &mut self,
        tag: &str,
        expected: usize,
        found: usize,
        span: Span,
    ) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "expected variant tag `{tag}` with {expected} payload value{}, found {found}",
                if expected == 1 { "" } else { "s" },
            ))
            .with_code(codes::ty::MISMATCH)
            .with_label(Label::primary(
                span,
                "variant payload count does not match annotation",
            ))
            .with_note("add or remove payload values to match the variant annotation"),
        );
    }

    fn report_open_variant_not_assignable(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("open variant may contain tags not allowed by the annotation")
                .with_code(codes::ty::OPEN_VARIANT_NOT_ASSIGNABLE)
                .with_label(Label::primary(span, "this value has an open variant type"))
                .with_note(
                    "make the annotation open with `..`, or close the value's variant type before assigning it",
                ),
        );
    }

    fn report_open_variant_non_exhaustive(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("non-exhaustive match on an open variant")
                .with_code(codes::ty::NON_EXHAUSTIVE_MATCH)
                .with_label(Label::primary(
                    span,
                    "this subject may contain tags beyond those listed",
                ))
                .with_note("add a default arm such as `_ => ...`"),
        );
    }

    fn report_unreachable_literal_match_arms(&mut self, row: &Row, arms: &[MatchArm]) {
        let Some(members) = literal_variant_members(row) else {
            return;
        };

        for arm in arms {
            let Some((literal, span)) = literal_pattern_value(&arm.pattern) else {
                continue;
            };
            if !members.contains(&literal) {
                self.report_unreachable_literal_match_arm(literal, span);
            }
        }
    }

    fn report_unreachable_literal_match_arm(&mut self, literal: &Literal, span: Span) {
        let literal = render_literal_value(literal);
        self.diagnostics.push(
            Diagnostic::error(format!("unreachable match arm for literal {literal}"))
                .with_code(codes::ty::UNREACHABLE_MATCH_ARM)
                .with_label(Label::primary(
                    span,
                    "this literal pattern cannot match the subject",
                ))
                .with_note(format!(
                    "literal {literal} is not a possible value of the subject"
                )),
        );
    }

    fn report_missing_variant_match_tags(&mut self, missing: &[&str], span: Span) {
        let tags = missing
            .iter()
            .map(|tag| format!("`{tag}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let message = if missing.len() == 1 {
            format!("non-exhaustive match; missing tag {tags}")
        } else {
            format!("non-exhaustive match; missing tags {tags}")
        };

        self.diagnostics.push(
            Diagnostic::error(message)
                .with_code(codes::ty::NON_EXHAUSTIVE_MATCH)
                .with_label(Label::primary(
                    span,
                    "this subject has variant tags without matching arms",
                ))
                .with_note("add the missing arm(s), or add `_ => ...` as a default"),
        );
    }

    fn report_missing_literal_match_members(&mut self, missing: &[&Literal], span: Span) {
        let literals = missing
            .iter()
            .map(|literal| render_literal_value(literal))
            .collect::<Vec<_>>()
            .join(", ");
        let message = if missing.len() == 1 {
            format!("non-exhaustive match; missing literal {literals}")
        } else {
            format!("non-exhaustive match; missing literals {literals}")
        };

        self.diagnostics.push(
            Diagnostic::error(message)
                .with_code(codes::ty::NON_EXHAUSTIVE_MATCH)
                .with_label(Label::primary(
                    span,
                    "this subject has literal values without matching arms",
                ))
                .with_note("add the missing arm(s), or add `_ => ...` as a default"),
        );
    }

    fn report_missing_empty_match_values(&mut self, missing: &[EmptyValue], span: Span) {
        let values = missing
            .iter()
            .map(|value| value.render())
            .collect::<Vec<_>>()
            .join(", ");
        let message = format!("non-exhaustive match; missing {values}");

        self.diagnostics.push(
            Diagnostic::error(message)
                .with_code(codes::ty::NON_EXHAUSTIVE_MATCH)
                .with_label(Label::primary(
                    span,
                    "this subject has empty values without matching arms",
                ))
                .with_note("add the missing arm(s), or add `_ => ...` as a default"),
        );
    }

    fn report_type_mismatch_between_types(&mut self, expected: &str, actual: &str, span: Span) {
        self.push_type_mismatch_diagnostic(
            Diagnostic::error(format!("expected `{expected}`, found `{actual}`"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    span,
                    format!("this value has type `{actual}`"),
                ))
                .with_note(format!(
                    "change the value to produce `{expected}`, or change the annotation to `{actual}`"
                )),
        );
    }

    fn report_redundant_undefined_field(
        &mut self,
        span: Span,
        delete_suggestion: impl Into<String>,
    ) {
        let delete_suggestion = delete_suggestion.into();
        self.diagnostics.push(
            Diagnostic::error("redundant `undefined` field value")
                .with_code(codes::record::REDUNDANT_UNDEFINED)
                .with_label(Label::primary(
                    span,
                    "this field is explicitly `undefined`",
                ))
                .with_note(format!(
                    "omit the field (it defaults to `undefined`), or use {delete_suggestion} to delete it from a spread"
                )),
        );
    }

    fn report_missing_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("missing field `{name}`"))
                .with_code(codes::ty::MISSING_FIELD)
                .with_label(Label::primary(
                    span,
                    "this record is missing a required field",
                ))
                .with_note(format!(
                    "add `{name}: ...`, or make the field type optional with `?T`"
                )),
        );
    }

    fn report_unexpected_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("unexpected field `{name}`"))
                .with_code(codes::ty::UNEXPECTED_FIELD)
                .with_label(Label::primary(span, "this field is not in the record type"))
                .with_note(
                    "remove the field, or open the record type with `..` to allow extra fields",
                ),
        );
    }

    fn report_duplicate_row_label(
        &mut self,
        name: &str,
        span: Span,
        context: DuplicateRowLabelContext,
    ) {
        let (label, note) = match context {
            DuplicateRowLabelContext::RecordAdd => (
                "this label is already present in the accumulated row",
                format!(
                    "use `{name} :: ...` to replace the existing label, or remove one `{name}` entry"
                ),
            ),
            DuplicateRowLabelContext::RecordValueAdd => (
                "this label is already present in the accumulated row",
                format!(
                    "use `{name} := ...` to replace the existing label, or remove one `{name}` entry"
                ),
            ),
            DuplicateRowLabelContext::VariantAdd => (
                "this label is already present in the accumulated row",
                "use `:..` with a replacement variant source, or remove one of the colliding tags"
                    .to_owned(),
            ),
            DuplicateRowLabelContext::Spread => (
                "this spread collides with a label already in the accumulated row",
                "use `:..` to overwrite-merge, or remove one of the colliding labels".to_owned(),
            ),
        };

        self.diagnostics.push(
            Diagnostic::error(format!("duplicate row label `{name}`"))
                .with_code(codes::ty::DUPLICATE_SPREAD_LABEL)
                .with_label(Label::primary(span, label))
                .with_note(note),
        );
    }

    fn report_replace_absent_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("cannot replace missing label `{name}`"))
                .with_code(codes::ty::REPLACE_ABSENT_FIELD)
                .with_label(Label::primary(
                    span,
                    "this replacement has no existing label to replace",
                ))
                .with_note(format!(
                    "use `{name}: ...` to add the label, or spread a closed row containing `{name}` first"
                )),
        );
    }

    fn report_delete_absent_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("cannot delete missing label `{name}`"))
                .with_code(codes::ty::DELETE_ABSENT_FIELD)
                .with_label(Label::primary(
                    span,
                    "this delete has no existing label to remove",
                ))
                .with_note(format!(
                    "spread or add `{name}` before deleting it, or remove this delete"
                )),
        );
    }

    fn report_rename_absent_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("cannot rename missing label `{name}`"))
                .with_code(codes::ty::RENAME_ABSENT_FIELD)
                .with_label(Label::primary(
                    span,
                    "this rename has no existing label to rename",
                ))
                .with_note(format!(
                    "spread or add `{name}` before renaming it, or remove this rename"
                )),
        );
    }

    fn report_rename_target_present(&mut self, from: &str, to: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("cannot rename `{from}` to existing label `{to}`"))
                .with_code(codes::ty::RENAME_ABSENT_FIELD)
                .with_label(Label::primary(
                    span,
                    "the rename target is already present in the accumulated row",
                ))
                .with_note(format!(
                    "delete or rename the existing `{to}` label before renaming `{from}`"
                )),
        );
    }

    fn report_unsupported_or_patterns(&mut self, pattern: &Expr) {
        let mut spans = Vec::new();
        collect_or_pattern_spans(pattern, &mut spans);

        for span in spans {
            if self.diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some(codes::ty::UNSUPPORTED_PATTERN)
                    && diagnostic.labels.first().map(|label| label.span) == Some(span)
            }) {
                continue;
            }

            self.diagnostics.push(
                Diagnostic::error("or-patterns are not supported yet")
                    .with_code(codes::ty::UNSUPPORTED_PATTERN)
                    .with_label(Label::primary(
                        span,
                        "`|` in pattern position is reserved for or-patterns",
                    ))
                    .with_note(
                        "or-patterns are planned, but this checker slice does not lower them yet",
                    ),
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DuplicateRowLabelContext {
    RecordAdd,
    RecordValueAdd,
    VariantAdd,
    Spread,
}

#[derive(Debug)]
enum RowSource {
    Closed(Row),
    Open(Row),
}

impl RowSource {
    fn from_row(row: Row) -> Self {
        if row.tail == RowTail::Closed {
            Self::Closed(row)
        } else {
            Self::Open(row)
        }
    }
}

#[derive(Clone, Copy)]
enum RowFoldMode<'a> {
    Annotation,
    Value { env: &'a TypeEnv },
}

#[derive(Clone, Copy)]
enum SetUnionPart<'a> {
    Operand(&'a Expr),
    Element(&'a Expr),
}

impl<'a> SetUnionPart<'a> {
    fn expr(self) -> &'a Expr {
        match self {
            Self::Operand(expr) | Self::Element(expr) => expr,
        }
    }

    fn promotes_singleton(self) -> bool {
        matches!(self, Self::Element(_))
    }
}

#[derive(Debug, Clone, Copy)]
enum LabelReflection {
    KeysOf,
    TagsOf,
}

impl LabelReflection {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "keysOf" => Some(Self::KeysOf),
            "tagsOf" => Some(Self::TagsOf),
            _ => None,
        }
    }

    fn evaluate(
        self,
        subject: &Type,
        arg_span: Span,
        subject_is_unresolved: bool,
    ) -> comptime::EvaluationResult {
        match self {
            Self::KeysOf => comptime::evaluate_keys_of(subject, arg_span, subject_is_unresolved),
            Self::TagsOf => comptime::evaluate_tags_of(subject, arg_span, subject_is_unresolved),
        }
    }
}

struct ComptimeArgument {
    value: comptime::ComptimeValue,
    label_set_members: Option<Vec<LabelSetMember>>,
}

struct LabelSetMember {
    label: String,
    literal: Literal,
    span: Span,
}

#[derive(Debug)]
struct ExpectedRecordShape<'a> {
    fields: Vec<ExpectedRecordField<'a>>,
    open: bool,
}

#[derive(Debug)]
struct ExpectedRecordField<'a> {
    name: &'a str,
    ty: &'a Type,
}

#[derive(Debug, Clone, Copy)]
enum FieldValue<'a> {
    Value(Option<&'a Expr>),
    Type(&'a Type),
}

#[derive(Debug, Clone, Copy)]
enum ExtraFields {
    Reject,
    Allow,
}

#[derive(Debug)]
struct ValueRecordShape<'a> {
    fields: Vec<ValueRecordField<'a>>,
    span: Span,
}

#[derive(Debug)]
struct ValueRecordField<'a> {
    name: &'a str,
    name_span: Span,
    value: Option<&'a Expr>,
}

#[derive(Debug)]
struct VariantTagShape<'a> {
    name: &'a str,
    payload: &'a [Type],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariantEntryKind {
    Tag,
    Literal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyValue {
    Undefined,
    Null,
}

impl EmptyValue {
    fn render(self) -> &'static str {
        match self {
            EmptyValue::Undefined => "`undefined`",
            EmptyValue::Null => "`null`",
        }
    }
}

fn is_non_liftable_artifact_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named(_)
            | Type::Variable(_)
            | Type::Apply { .. }
            | Type::Function { .. }
            | Type::Optional(_)
            | Type::Nullable(_)
            | Type::Tuple(_)
            | Type::Record(_)
            | Type::Variant(_)
    )
}

fn row_entry_label(entry: &RowEntry) -> &str {
    match entry {
        RowEntry::Field { name, .. } | RowEntry::Tag { name, .. } => name,
        RowEntry::Literal { value } => render_literal_value(value),
    }
}

fn row_entry_index(entries: &[RowEntry], label: &str) -> Option<usize> {
    entries
        .iter()
        .position(|entry| row_entry_label(entry) == label)
}

fn relabel_row_entry(entry: &RowEntry, label: &str) -> RowEntry {
    match entry {
        RowEntry::Field { ty, .. } => RowEntry::Field {
            name: label.to_owned(),
            ty: ty.clone(),
        },
        RowEntry::Tag { payload, .. } => RowEntry::Tag {
            name: label.to_owned(),
            payload: payload.clone(),
        },
        RowEntry::Literal { value } => RowEntry::Literal {
            value: value.clone(),
        },
    }
}

fn literal_record_type(row: &Row) -> Option<ExpectedRecordShape<'_>> {
    let mut fields = Vec::new();

    for entry in &row.entries {
        match entry {
            RowEntry::Field { name, ty } => fields.push(ExpectedRecordField { name, ty }),
            RowEntry::Tag { .. } | RowEntry::Literal { .. } => return None,
        }
    }

    Some(ExpectedRecordShape {
        fields,
        open: row.tail == RowTail::Open,
    })
}

fn literal_record_value(entries: &[RecordEntry], span: Span) -> Option<ValueRecordShape<'_>> {
    let mut fields = Vec::new();

    for entry in entries {
        match entry {
            RecordEntry::Field {
                name,
                name_span,
                value,
                overwrite: false,
                ..
            } => fields.push(ValueRecordField {
                name,
                name_span: *name_span,
                value: Some(value),
            }),
            RecordEntry::Shorthand {
                name, name_span, ..
            } => fields.push(ValueRecordField {
                name,
                name_span: *name_span,
                value: None,
            }),
            RecordEntry::Field {
                overwrite: true, ..
            }
            | RecordEntry::FieldComputed { .. }
            | RecordEntry::Spread { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::DeleteComputed { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Iteration { .. }
            | RecordEntry::Open { .. }
            | RecordEntry::Element(_) => return None,
        }
    }

    Some(ValueRecordShape { fields, span })
}

fn literal_set_elements(entries: &[RecordEntry]) -> Option<Vec<&Expr>> {
    entries
        .iter()
        .map(|entry| match entry {
            RecordEntry::Element(value) => Some(value),
            RecordEntry::Field { .. }
            | RecordEntry::FieldComputed { .. }
            | RecordEntry::Shorthand { .. }
            | RecordEntry::Spread { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::DeleteComputed { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Iteration { .. }
            | RecordEntry::Open { .. } => None,
        })
        .collect()
}

fn union_annotation_entries(expr: &Expr) -> Vec<RecordEntry> {
    let mut entries = Vec::new();
    collect_union_annotation_entries(expr, &mut entries);
    entries
}

fn collect_union_annotation_entries(expr: &Expr, entries: &mut Vec<RecordEntry>) {
    match &ungroup_expr(expr).kind {
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } if operator == "|" => {
            collect_union_annotation_entries(left, entries);
            collect_union_annotation_entries(right, entries);
        }
        ExprKind::Set(set_entries) => entries.extend(set_entries.iter().cloned()),
        _ => entries.push(RecordEntry::Element(expr.clone())),
    }
}

fn value_set_union_parts(expr: &Expr) -> Option<Vec<SetUnionPart<'_>>> {
    let mut parts = Vec::new();
    collect_value_set_union_parts(expr, &mut parts)?;
    Some(parts)
}

fn collect_value_set_union_parts<'a>(
    expr: &'a Expr,
    parts: &mut Vec<SetUnionPart<'a>>,
) -> Option<()> {
    match &ungroup_expr(expr).kind {
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } if operator == "|" => {
            collect_value_set_union_parts(left, parts)?;
            collect_value_set_union_parts(right, parts)
        }
        ExprKind::Set(entries) => {
            let elements = literal_set_elements(entries)?;
            parts.extend(elements.into_iter().map(SetUnionPart::Element));
            Some(())
        }
        _ => {
            parts.push(SetUnionPart::Operand(expr));
            Some(())
        }
    }
}

fn pattern_local_types(pattern: &Expr, expected: Option<&Type>) -> Vec<(String, LocalValueType)> {
    if pattern_contains_or_pattern(pattern) {
        return Vec::new();
    }

    let mut known = HashMap::new();
    if let Some(expected) = expected {
        collect_known_pattern_types(pattern, expected, &mut known);
    }

    pattern_bindings(pattern)
        .into_iter()
        .map(|binding| {
            let ty = known
                .get(binding.name)
                .cloned()
                .map(LocalValueType::Known)
                .unwrap_or(LocalValueType::Unknown);
            (binding.name.to_owned(), ty)
        })
        .collect()
}

fn collect_record_pattern_rest_binders(pattern: &Expr, binders: &mut Vec<String>) {
    let ExprKind::Record(entries) = &ungroup_expr(pattern).kind else {
        return;
    };

    for entry in entries {
        if let RecordEntry::Spread { value, .. } = entry
            && let ExprKind::Name(name) = &value.kind
            && !name_is_placeholder(name)
        {
            binders.push(name.clone());
        }
    }
}

fn expr_references_name(expr: &Expr, name: &str) -> bool {
    if let ExprKind::Name(current) = &expr.kind
        && current == name
    {
        return true;
    }

    let mut found = false;
    walk_expr_children(expr, &mut |child| {
        if !found && expr_references_name(child, name) {
            found = true;
        }
    });
    found
}

fn collect_known_pattern_types(pattern: &Expr, expected: &Type, known: &mut HashMap<String, Type>) {
    match (&pattern.kind, expected) {
        (ExprKind::Group(inner), _) => collect_known_pattern_types(inner, expected, known),
        (_, Type::Optional(inner))
            if empty_value_pattern(pattern) != Some(EmptyValue::Undefined) =>
        {
            collect_known_pattern_types(pattern, inner, known);
        }
        (_, Type::Nullable(inner)) if empty_value_pattern(pattern) != Some(EmptyValue::Null) => {
            collect_known_pattern_types(pattern, inner, known);
        }
        (ExprKind::Name(name), _) if name != "_" && is_concrete_type(expected) => {
            known.insert(name.clone(), expected.clone());
        }
        (ExprKind::Call { callee, args }, Type::Variant(entries)) => {
            let ExprKind::Tag(tag) = &callee.kind else {
                return;
            };
            let Some(payload) = literal_variant_payload(entries, tag) else {
                return;
            };
            if payload.len() != args.len() {
                return;
            }
            for (arg, ty) in args.iter().zip(payload) {
                collect_known_pattern_types(arg, ty, known);
            }
        }
        (ExprKind::Record(entries), Type::Record(row)) => {
            collect_known_record_pattern_types(entries, row, known);
        }
        (ExprKind::Tag(_), Type::Variant(_)) => {}
        _ => {}
    }
}

fn collect_known_record_pattern_types(
    entries: &[RecordEntry],
    row: &Row,
    known: &mut HashMap<String, Type>,
) {
    let matched_labels: HashSet<_> = entries.iter().filter_map(record_pattern_label).collect();

    for entry in entries {
        match entry {
            RecordEntry::Field { name, value, .. } => {
                if let Some(field_ty) = row_field_type(row, name) {
                    collect_known_pattern_types(value, field_ty, known);
                }
            }
            RecordEntry::Shorthand { name, .. } => {
                if let Some(field_ty) = row_field_type(row, name)
                    && is_concrete_type(field_ty)
                {
                    known.insert(name.clone(), field_ty.clone());
                }
            }
            RecordEntry::Spread { value, .. } => {
                let ExprKind::Name(name) = &value.kind else {
                    continue;
                };
                if name == "_" || row.tail != RowTail::Closed {
                    continue;
                }

                let residual = Row {
                    entries: row
                        .entries
                        .iter()
                        .filter(|entry| !matched_labels.contains(row_entry_label(entry)))
                        .cloned()
                        .collect(),
                    tail: RowTail::Closed,
                };
                known.insert(name.clone(), Type::Record(residual));
            }
            RecordEntry::Delete { .. }
            | RecordEntry::FieldComputed { .. }
            | RecordEntry::DeleteComputed { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Iteration { .. }
            | RecordEntry::Open { .. }
            | RecordEntry::Element(_) => {}
        }
    }
}

fn record_pattern_label(entry: &RecordEntry) -> Option<&str> {
    match entry {
        RecordEntry::Field { name, .. } | RecordEntry::Shorthand { name, .. } => Some(name),
        RecordEntry::Spread { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::FieldComputed { .. }
        | RecordEntry::DeleteComputed { .. }
        | RecordEntry::Rename { .. }
        | RecordEntry::Iteration { .. }
        | RecordEntry::Open { .. }
        | RecordEntry::Element(_) => None,
    }
}

fn row_field_type<'a>(row: &'a Row, label: &str) -> Option<&'a Type> {
    let index = row_entry_index(&row.entries, label)?;
    match &row.entries[index] {
        RowEntry::Field { ty, .. } => Some(ty),
        RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
    }
}

fn collect_comptime_type_bindings(
    annotation: &Expr,
    actual: &Type,
    bindings: &mut HashMap<String, comptime::ComptimeValue>,
) {
    match (&ungroup_expr(annotation).kind, actual) {
        (ExprKind::Name(name), actual) => {
            bindings.insert(
                name.clone(),
                comptime::ComptimeValue::ReifiedType(actual.clone()),
            );
        }
        (
            ExprKind::Index { callee, args },
            Type::Apply {
                callee: actual_callee,
                args: actual_args,
            },
        ) if args.len() == actual_args.len() => {
            collect_comptime_type_bindings(callee, actual_callee, bindings);
            for (arg, actual_arg) in args.iter().zip(actual_args) {
                collect_comptime_type_bindings(arg, actual_arg, bindings);
            }
        }
        (ExprKind::Nullable(inner), Type::Nullable(actual_inner)) => {
            collect_comptime_type_bindings(inner, actual_inner, bindings);
        }
        (ExprKind::Optional(inner), Type::Optional(actual_inner)) => {
            collect_comptime_type_bindings(inner, actual_inner, bindings);
        }
        (ExprKind::Tuple(items), Type::Tuple(actual_items))
            if items.len() == actual_items.len() =>
        {
            for (item, actual_item) in items.iter().zip(actual_items) {
                collect_comptime_type_bindings(item, actual_item, bindings);
            }
        }
        (
            ExprKind::Arrow { params, result },
            Type::Function {
                params: actual_params,
                result: actual_result,
                ..
            },
        ) if params.len() == actual_params.len() => {
            for (param, actual_param) in params.iter().zip(actual_params) {
                collect_comptime_type_bindings(param, actual_param, bindings);
            }
            collect_comptime_type_bindings(result, actual_result, bindings);
        }
        (ExprKind::Record(entries), Type::Record(row)) => {
            collect_record_comptime_type_bindings(entries, row, bindings);
        }
        (ExprKind::Binary { operator, .. }, Type::Variant(row)) if operator == "|" => {
            let entries = union_annotation_entries(annotation);
            collect_variant_comptime_type_bindings(&entries, row, bindings);
        }
        (ExprKind::Set(entries), Type::Variant(row)) => {
            collect_variant_comptime_type_bindings(entries, row, bindings);
        }
        _ => {}
    }
}

fn collect_record_comptime_type_bindings(
    entries: &[RecordEntry],
    row: &Row,
    bindings: &mut HashMap<String, comptime::ComptimeValue>,
) {
    for entry in entries {
        match entry {
            RecordEntry::Field { name, value, .. } => {
                if let Some(field_ty) = row_field_type(row, name) {
                    collect_comptime_type_bindings(value, field_ty, bindings);
                }
            }
            RecordEntry::Spread { value, .. } => {
                collect_comptime_type_bindings(value, &Type::Record(row.clone()), bindings);
            }
            RecordEntry::Shorthand { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::FieldComputed { .. }
            | RecordEntry::DeleteComputed { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Iteration { .. }
            | RecordEntry::Open { .. }
            | RecordEntry::Element(_) => {}
        }
    }
}

fn collect_variant_comptime_type_bindings(
    entries: &[RecordEntry],
    row: &Row,
    bindings: &mut HashMap<String, comptime::ComptimeValue>,
) {
    for (entry, row_entry) in entries.iter().zip(&row.entries) {
        if let (RecordEntry::Element(expr), RowEntry::Tag { payload, .. }) = (entry, row_entry)
            && let ExprKind::Call { args, .. } = &expr.kind
        {
            for (arg, actual) in args.iter().zip(payload) {
                collect_comptime_type_bindings(arg, actual, bindings);
            }
        }
    }
}

fn comptime_value_label(value: &comptime::ComptimeValue) -> Option<String> {
    let Literal::String(text) = value.as_literal()? else {
        return None;
    };
    string_literal_label(text)
}

fn comptime_value_label_set(value: &comptime::ComptimeValue) -> Option<Vec<String>> {
    match value {
        comptime::ComptimeValue::LabelSet(labels) => Some(labels.clone()),
        comptime::ComptimeValue::ReifiedType(_)
        | comptime::ComptimeValue::Literal(_)
        | comptime::ComptimeValue::Bool(_) => None,
    }
}

fn label_literal(label: &str) -> Literal {
    Literal::String(format!("\"{label}\""))
}

fn literal_type(literal: Literal) -> Type {
    Type::Variant(Row {
        entries: vec![RowEntry::Literal { value: literal }],
        tail: RowTail::Closed,
    })
}

fn literal_union_domain_row(domain: &Type) -> Option<&Row> {
    match domain {
        Type::Variant(row) => Some(row),
        Type::Apply { callee, args }
            if matches!(callee.as_ref(), Type::Named(name) if name == "Set") && args.len() == 1 =>
        {
            match &args[0] {
                Type::Variant(row) => Some(row),
                _ => None,
            }
        }
        _ => None,
    }
}

fn string_literal_label(text: &str) -> Option<String> {
    text.strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .map(str::to_owned)
}

fn literal_variant_payload<'a>(row: &'a Row, tag: &str) -> Option<&'a [Type]> {
    literal_variant_payload_lookup(row, tag).flatten()
}

fn literal_variant_payload_lookup<'a>(row: &'a Row, tag: &str) -> Option<Option<&'a [Type]>> {
    // Like `variant_payload_lookup`, but a closed-row-only view: an open tail
    // means the row is not a literal variant, so callers should defer.
    if row.tail == RowTail::Open {
        return None;
    }

    variant_payload_lookup(row, tag)
}

fn variant_payload_lookup<'a>(row: &'a Row, tag: &str) -> Option<Option<&'a [Type]>> {
    let mut found = None;

    for entry in &row.entries {
        match entry {
            RowEntry::Tag { name, payload } if name == tag => {
                found = Some(payload.as_slice());
            }
            RowEntry::Tag { .. } => {}
            RowEntry::Field { .. } | RowEntry::Literal { .. } => return None,
        }
    }

    Some(found)
}

fn variant_tags(row: &Row) -> Option<Vec<VariantTagShape<'_>>> {
    let mut tags = Vec::new();

    for entry in &row.entries {
        match entry {
            RowEntry::Tag { name, payload } => tags.push(VariantTagShape {
                name,
                payload: payload.as_slice(),
            }),
            RowEntry::Field { .. } | RowEntry::Literal { .. } => return None,
        }
    }

    Some(tags)
}

fn literal_variant_members(row: &Row) -> Option<Vec<&Literal>> {
    let mut literals = Vec::new();

    for entry in &row.entries {
        match entry {
            RowEntry::Literal { value } => literals.push(value),
            RowEntry::Field { .. } | RowEntry::Tag { .. } => return None,
        }
    }

    Some(literals)
}

fn row_has_literal_entries(row: &Row) -> bool {
    row.entries
        .iter()
        .any(|entry| matches!(entry, RowEntry::Literal { .. }))
}

fn variant_entry_kind(entries: &[RowEntry]) -> Option<VariantEntryKind> {
    entries.iter().find_map(row_entry_variant_kind)
}

fn row_entry_variant_kind(entry: &RowEntry) -> Option<VariantEntryKind> {
    match entry {
        RowEntry::Tag { .. } => Some(VariantEntryKind::Tag),
        RowEntry::Literal { .. } => Some(VariantEntryKind::Literal),
        RowEntry::Field { .. } => None,
    }
}

fn literal_union_accepts_base_type(literals: &[&Literal], base: &str) -> bool {
    literals.iter().any(|literal| {
        matches!(
            (literal, base),
            (Literal::String(_), "Text") | (Literal::Number(_), "Int" | "Float")
        )
    })
}

fn literal_kind_name(literal: &Literal) -> &'static str {
    match literal {
        Literal::Bool(_) => "bool literal",
        Literal::String(_) => "text literal",
        Literal::Number(_) => "number literal",
        Literal::Regex(_) => "regex literal",
        Literal::Path(_) => "path literal",
    }
}

fn render_literal_union(literals: &[&Literal]) -> String {
    if literals.is_empty() {
        return "an empty literal union".to_owned();
    }

    literals
        .iter()
        .map(|literal| render_literal_value(literal))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn peel_empty_values(ty: &Type) -> (Vec<EmptyValue>, &Type) {
    let mut values = Vec::new();
    let mut payload = ty;

    loop {
        match payload {
            Type::Optional(inner) => {
                if !values.contains(&EmptyValue::Undefined) {
                    values.push(EmptyValue::Undefined);
                }
                payload = inner;
            }
            Type::Nullable(inner) => {
                if !values.contains(&EmptyValue::Null) {
                    values.push(EmptyValue::Null);
                }
                payload = inner;
            }
            _ => return (values, payload),
        }
    }
}

fn empty_value_is_covered(arms: &[MatchArm], value: EmptyValue) -> bool {
    arms.iter().any(|arm| {
        arm.guards.is_empty()
            && (empty_value_pattern(&arm.pattern) == Some(value)
                || is_underscore_pattern(&arm.pattern))
    })
}

fn empty_value_pattern(pattern: &Expr) -> Option<EmptyValue> {
    match &pattern.kind {
        ExprKind::Group(inner) => empty_value_pattern(inner),
        ExprKind::Undefined => Some(EmptyValue::Undefined),
        ExprKind::Null => Some(EmptyValue::Null),
        _ => None,
    }
}

fn is_underscore_pattern(pattern: &Expr) -> bool {
    match &pattern.kind {
        ExprKind::Group(inner) => is_underscore_pattern(inner),
        ExprKind::Name(name) if name == "_" => true,
        _ => false,
    }
}

fn name_is_placeholder(name: &str) -> bool {
    name == "_"
}

fn builtin_value_name_is_bound(name: &str) -> bool {
    matches!(name, "keysOf" | "tagsOf" | "typeOf" | "pick" | "omit")
}

fn is_catch_all_pattern(pattern: &Expr) -> bool {
    match &pattern.kind {
        ExprKind::Group(inner) => is_catch_all_pattern(inner),
        ExprKind::Name(_) => true,
        _ => false,
    }
}

fn pattern_contains_or_pattern(pattern: &Expr) -> bool {
    let mut spans = Vec::new();
    collect_or_pattern_spans(pattern, &mut spans);
    !spans.is_empty()
}

fn collect_or_pattern_spans(pattern: &Expr, spans: &mut Vec<Span>) {
    match &pattern.kind {
        ExprKind::Group(inner) => collect_or_pattern_spans(inner, spans),
        ExprKind::Binary {
            left,
            operator,
            operator_span,
            right,
        } => {
            if operator == "|" {
                spans.push(*operator_span);
            }
            collect_or_pattern_spans(left, spans);
            collect_or_pattern_spans(right, spans);
        }
        _ => {
            walk_expr_children(pattern, &mut |child| {
                collect_or_pattern_spans(child, spans);
            });
        }
    }
}

fn variant_pattern_tag(pattern: &Expr) -> Option<&str> {
    match &pattern.kind {
        ExprKind::Group(inner) => variant_pattern_tag(inner),
        ExprKind::Tag(tag) => Some(tag),
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Tag(tag) => Some(tag),
            _ => None,
        },
        _ => None,
    }
}

fn scheme_from_global(ty: &Type, unifier: &mut Unifier) -> TypeScheme {
    let mut metas_by_name = HashMap::new();
    let mut vars = Vec::new();
    let generalized = map_type(ty, &mut |node| {
        let Type::Variable(name) = node else {
            return None;
        };
        let id = match metas_by_name.entry(name.clone()) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let Type::Meta(id) = unifier.fresh() else {
                    unreachable!("fresh types are metavariables");
                };
                entry.insert(id);
                vars.push(id);
                id
            }
        };
        Some(Type::Meta(id))
    });

    if vars.is_empty() {
        TypeScheme::mono(generalized)
    } else {
        TypeScheme {
            vars,
            row_vars: Vec::new(),
            ty: generalized,
        }
    }
}

fn applied_type_constructor_mismatch(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Named(expected), Type::Named(actual)) => expected != actual,
        (Type::Deferred | Type::Variable(_) | Type::Meta(_), _)
        | (_, Type::Deferred | Type::Variable(_) | Type::Meta(_)) => false,
        _ => expected != actual,
    }
}

fn reportable_type_shape(ty: &Type) -> bool {
    !matches!(ty, Type::Deferred | Type::Variable(_) | Type::Meta(_))
}

fn literal_pattern_value(pattern: &Expr) -> Option<(&Literal, Span)> {
    match &pattern.kind {
        ExprKind::Group(inner) => literal_pattern_value(inner),
        ExprKind::Literal(literal @ (Literal::Number(_) | Literal::String(_))) => {
            Some((literal, pattern.span))
        }
        _ => None,
    }
}

fn pattern_matches_comptime_value(pattern: &Expr, value: &comptime::ComptimeValue) -> bool {
    match (&ungroup_expr(pattern).kind, value) {
        (ExprKind::Name(_), _) => true,
        (ExprKind::Literal(Literal::Bool(pattern)), comptime::ComptimeValue::Bool(value)) => {
            pattern == value
        }
        (ExprKind::Literal(pattern), comptime::ComptimeValue::Literal(value)) => pattern == value,
        _ => false,
    }
}

pub(crate) fn comptime_rhs_needs_evaluation(value: &Expr) -> bool {
    let mut value = value;
    while let ExprKind::Group(inner) = &value.kind {
        value = inner;
    }

    matches!(
        &value.kind,
        ExprKind::Call { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::Propagate { .. }
            | ExprKind::Match { .. }
            | ExprKind::Block(_)
            | ExprKind::Lambda { .. }
            | ExprKind::Interpolation(_)
    )
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
        let ty = self.unifier.instantiate_scheme(&scheme);
        self.resolve_if_concrete(&ty)
    }

    #[cfg(test)]
    pub(crate) fn infer_top_level_scheme(&mut self, name: &str) -> Option<TypeScheme> {
        self.infer_top_level(name)
    }

    fn infer_local_value(&mut self, env: &TypeEnv, value: &Expr) -> Option<Type> {
        let ty = self.infer(env, value);
        self.resolve_if_concrete(&ty)
    }

    /// Fully resolve `ty`; keep it only when no metavariable remains, so a
    /// synthesized value type never leaks an unsolved meta into checking.
    fn resolve_if_concrete(&self, ty: &Type) -> Option<Type> {
        let ty = self.normalize(&self.resolve_and_default(ty));
        is_resolved_value_type(&ty).then_some(ty)
    }

    fn resolve_and_default(&self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        self.unifier.default_numerics(&resolved)
    }

    fn infer_top_level(&mut self, name: &str) -> Option<TypeScheme> {
        if let Some(scheme) = self.memo.get(name).cloned() {
            return Some(scheme);
        }
        if self.in_progress.contains(name) {
            return Some(TypeScheme::mono(Type::Deferred));
        }

        let binding = (*self.bindings.get(name)?)?;
        self.in_progress.insert(name.to_owned());

        let scheme = if let Some(annotation) = self.clean_declared_annotation(name) {
            TypeScheme::mono(annotation)
        } else {
            let ty = self.infer(&TypeEnv::new(), &binding.value);
            generalize(self.resolve_and_default(&ty), &[], &[])
        };

        self.in_progress.remove(name);
        self.memo.insert(name.to_owned(), scheme.clone());
        Some(scheme)
    }

    fn infer_top_level_without_unbound_names(&mut self, name: &str) -> Option<TypeScheme> {
        let previous = self.report_unbound_names;
        self.report_unbound_names = false;
        let scheme = self.infer_top_level(name);
        self.report_unbound_names = previous;
        scheme
    }

    fn clean_declared_annotation(&self, name: &str) -> Option<Type> {
        let annotation = *self.annotations.get(name)?;
        let mut checker = self.fork_annotation_checker();
        let lowering = checker.lower_annotation_with_diagnostics(annotation);
        if lowering.diagnostics.is_empty() {
            Some(checker.normalize(&lowering.ty))
        } else {
            None
        }
    }

    fn infer(&mut self, env: &TypeEnv, expr: &Expr) -> Type {
        let ty = match &expr.kind {
            ExprKind::Literal(literal @ (Literal::Number(_) | Literal::String(_))) => {
                self.open_literal_variant(literal)
            }
            ExprKind::Literal(Literal::Bool(_)) => named_builtin("Bool"),
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
            ExprKind::Array(elements) => self.infer_array(env, elements),
            ExprKind::Set(entries) => self.infer_set(env, entries),
            ExprKind::Record(entries) => {
                if let Some(shape) = literal_record_value(entries, expr.span) {
                    let mut fields = Vec::new();
                    for field in &shape.fields {
                        let Some(value) = field.value else {
                            return Type::Deferred;
                        };
                        fields.push(RowEntry::Field {
                            name: field.name.to_owned(),
                            ty: self.infer(env, value),
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
                body,
            } => self.infer_lambda(env, params, return_annotation.as_deref(), body),
            ExprKind::Call { callee, args } => self.infer_call(env, callee, args),
            ExprKind::Index { callee, args } => self.infer_value_index(env, callee, args),
            ExprKind::FieldAccess {
                receiver, field, ..
            } => self.infer_field_access(env, receiver, field),
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            } if operator == "|" => self.infer_set_union(env, expr),
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            } => self.infer_binary(env, left, operator, right),
            ExprKind::Unary {
                operator, value, ..
            } => self.infer_unary(env, operator, value),
            ExprKind::Block(items) => self.infer_block(env, items),
            ExprKind::Match { subject, arms, .. } => self.infer_match(env, subject, arms),
            ExprKind::Interpolation(segments) => self.infer_interpolation(env, segments),
            ExprKind::Propagate { value, .. } => self.infer_propagate(env, value),
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Optional(_)
            | ExprKind::Nullable(_)
            | ExprKind::NonNull(_)
            | ExprKind::Arrow { .. } => Type::Deferred,
        };
        self.record_expr_type(expr.span, &ty);
        ty
    }

    fn open_literal_variant(&mut self, literal: &Literal) -> Type {
        Type::Variant(Row {
            entries: vec![RowEntry::Literal {
                value: literal.clone(),
            }],
            tail: RowTail::Var(self.unifier.fresh_row_var()),
        })
    }

    /// Infer `value?!` / `value?^`. Both unwrap a `Result[ok, err]` to its `ok`
    /// branch, so when the operand resolves to a concrete `Result` the result is
    /// its ok type — this is what lets a phantom-typed `open(path, mode)?!` carry
    /// the resolved handle record forward, so misusing it (a missing method) is a
    /// real field error. A non-`Result` operand (still unknown) stays deferred.
    fn infer_propagate(&mut self, env: &TypeEnv, value: &Expr) -> Type {
        let inferred = self.infer(env, value);
        let resolved = self.resolve_and_default(&inferred);
        match &resolved {
            Type::Apply { callee, args }
                if args.len() == 2
                    && matches!(callee.as_ref(), Type::Named(name) if name == "Result") =>
            {
                args[0].clone()
            }
            _ => Type::Deferred,
        }
    }

    fn infer_interpolation(&mut self, env: &TypeEnv, segments: &[InterpolationSegment]) -> Type {
        for segment in segments {
            if let InterpolationSegment::Expr(expr) = segment {
                self.infer(env, expr);
            }
        }

        named_builtin("Text")
    }

    fn infer_binary(&mut self, env: &TypeEnv, left: &Expr, operator: &str, right: &Expr) -> Type {
        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let left_type = self.infer(env, left);
        let right_type = self.infer(env, right);

        if let Some(result) = self.infer_binary_type(operator, &left_type, &right_type) {
            result
        } else {
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            Type::Deferred
        }
    }

    fn infer_binary_type(&mut self, operator: &str, left: &Type, right: &Type) -> Option<Type> {
        match operator {
            "+" => self
                .infer_numeric_binary_type(left, right)
                .or_else(|| self.infer_same_named_binary_type(left, right, "Text")),
            "-" | "*" | "/" | "%" | "^" => self.infer_numeric_binary_type(left, right),
            "<" | "<=" | ">" | ">=" => self.infer_numeric_comparison_type(left, right),
            "==" | "!=" => self.infer_equality_type(left, right),
            "&&" | "||" => self.infer_same_named_binary_type(left, right, "Bool"),
            _ => None,
        }
    }

    fn infer_numeric_binary_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        let left = self.widen_numeric_operand(left);
        let right = self.widen_numeric_operand(right);

        if is_meta_type(&left)
            && is_meta_type(&right)
            && (self.unifier.is_numeric_meta(&left) || self.unifier.is_numeric_meta(&right))
        {
            self.unifier.unify(&left, &right).ok()?;
            return Some(self.unifier.resolve(&left));
        }

        match (numeric_type_name(&left), numeric_type_name(&right)) {
            (Some("Float"), Some(_)) | (Some(_), Some("Float")) => Some(named_builtin("Float")),
            (Some("Int"), Some("Int")) => Some(named_builtin("Int")),
            (None, Some(right_name)) if is_meta_type(&left) => self
                .unifier
                .unify(&left, &named_builtin(right_name))
                .ok()
                .map(|()| named_builtin(right_name)),
            (Some(left_name), None) if is_meta_type(&right) => self
                .unifier
                .unify(&right, &named_builtin(left_name))
                .ok()
                .map(|()| named_builtin(left_name)),
            _ => None,
        }
    }

    fn infer_numeric_comparison_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        self.infer_numeric_binary_type(left, right)
            .map(|_| named_builtin("Bool"))
    }

    fn infer_same_named_binary_type(
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

    fn widen_numeric_operand(&mut self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        if let Type::Variant(row) = &resolved
            && literal_variant_base(row) == Some(LiteralBase::Number)
        {
            return self.unifier.fresh_numeric();
        }

        resolved
    }

    fn widen_same_named_operand(&mut self, ty: &Type, name: &'static str) -> Type {
        let resolved = self.unifier.resolve(ty);
        if name == "Text"
            && let Type::Variant(row) = &resolved
            && literal_variant_base(row) == Some(LiteralBase::Text)
        {
            return named_builtin("Text");
        }

        resolved
    }

    fn infer_equality_type(&mut self, left: &Type, right: &Type) -> Option<Type> {
        let left = self.unifier.resolve(left);
        let right = self.unifier.resolve(right);

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
            return self
                .unifier
                .unify(&left, &right)
                .ok()
                .map(|()| named_builtin("Bool"));
        }

        None
    }

    fn infer_unary(&mut self, env: &TypeEnv, operator: &str, value: &Expr) -> Type {
        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let value_type = self.infer(env, value);

        let result = match operator {
            "-" => self.infer_numeric_unary_type(&value_type),
            _ => None,
        };

        if let Some(result) = result {
            result
        } else {
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            Type::Deferred
        }
    }

    fn infer_numeric_unary_type(&mut self, value: &Type) -> Option<Type> {
        let value = self.unifier.resolve(value);
        if let Some(name) = numeric_type_name(&value) {
            return Some(named_builtin(name));
        }
        self.unifier.is_numeric_meta(&value).then_some(value)
    }

    fn infer_lambda(
        &mut self,
        env: &TypeEnv,
        params: &[Param],
        return_annotation: Option<&Expr>,
        body: &Expr,
    ) -> Type {
        let mut next_env = env.clone();
        let mut param_types = Vec::new();
        // Defaults are trailing (per D1): the required-arity is the count of
        // leading params without a default.
        let mut required = params.len();

        for (index, param) in params.iter().enumerate() {
            let ty = if let Some(annotation) = &param.annotation {
                self.lower_annotation_for_inference(annotation)
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

        let body_type = self.infer(&next_env, body);
        let result_type = if let Some(annotation) = return_annotation {
            // A body that contradicts its return annotation defers rather than
            // reporting here: inference only synthesizes types, and diagnosing
            // the mismatch is a later return-annotation-checking slice.
            let expected = self.lower_annotation_for_inference(annotation);
            if self.unifier.unify(&body_type, &expected).is_err() {
                Type::Deferred
            } else {
                expected
            }
        } else {
            body_type
        };

        Type::Function {
            params: param_types,
            result: Box::new(result_type),
            required,
        }
    }

    fn infer_field_access(&mut self, env: &TypeEnv, receiver: &Expr, field: &str) -> Type {
        let snapshot = self.unifier.snapshot();
        let diagnostic_snapshot = self.diagnostic_snapshot();
        let receiver_type = self.infer(env, receiver);
        let field_type = self.unifier.fresh();
        let tail = self.unifier.fresh_row_var();
        let required = Type::Record(Row {
            entries: vec![RowEntry::Field {
                name: field.to_owned(),
                ty: field_type.clone(),
            }],
            tail: RowTail::Var(tail),
        });

        if self.unifier.unify(&receiver_type, &required).is_err() {
            self.unifier.restore(snapshot);
            self.restore_diagnostic_snapshot(diagnostic_snapshot);
            Type::Deferred
        } else {
            field_type
        }
    }

    fn infer_call(&mut self, env: &TypeEnv, callee: &Expr, args: &[Expr]) -> Type {
        if let ExprKind::Tag(tag) = &callee.kind {
            return self.infer_variant_constructor(env, tag, args);
        }

        if let Some(result) = self.infer_record_selection_builtin_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_host_comptime_call(env, callee, args) {
            return result;
        }

        if let Some(result) = self.infer_comptime_param_call(env, callee, args) {
            return result;
        }

        let callee_type = self.infer(env, callee);
        let arg_types: Vec<_> = args.iter().map(|arg| self.infer(env, arg)).collect();

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
            return result.as_ref().clone();
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
            result_type
        }
    }

    fn check_call_arg_types_against_params(
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

    fn infer_host_comptime_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let name = expr_name(callee)?;
        let spec = self.host_comptime_fn(env, name)?;
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
            return Some(Type::Deferred);
        }

        let args_match = self.check_call_arg_types_against_params(args, &arg_types, &params);
        if !self.host_comptime_args_match_params(&spec, args, &arg_types, &params) {
            return Some(Type::Deferred);
        }

        let bindings = self.current_comptime_value_bindings();
        let mut comptime_args = Vec::new();
        let mut error_span = callee.span;
        for index in &spec.comptime_params {
            let Some(arg) = args.get(*index) else {
                return Some(Type::Deferred);
            };
            let Some(argument) = self.evaluate_comptime_param_argument(arg, &bindings) else {
                return Some(Type::Deferred);
            };
            error_span = arg.span;
            comptime_args.push(ComptimeArg::from_comptime_value(argument.value));
        }

        if !args_match {
            return Some(Type::Deferred);
        }

        match spec.resolver.resolve(&comptime_args) {
            Ok(ty) => Some(ty),
            Err(error) => {
                self.report_host_comptime_error(error, error_span);
                Some(Type::Deferred)
            }
        }
    }

    fn host_comptime_fn(&self, env: &TypeEnv, name: &str) -> Option<HostComptimeFnSpec> {
        if env.get(name).is_some() || self.bindings.contains_key(name) {
            return None;
        }

        self.host_comptime_fns.get(name).cloned()
    }

    fn host_comptime_args_match_params(
        &mut self,
        spec: &HostComptimeFnSpec,
        args: &[Expr],
        arg_types: &[Type],
        params: &[Type],
    ) -> bool {
        spec.comptime_params.iter().all(|index| {
            if args.get(*index).is_none() {
                return false;
            }
            let Some(actual) = arg_types.get(*index) else {
                return false;
            };
            let Some(expected) = params.get(*index) else {
                return false;
            };

            let snapshot = self.unifier.snapshot();
            let matches = self.unifier.unify(actual, expected).is_ok();
            self.unifier.restore(snapshot);
            matches
        })
    }

    fn report_host_comptime_error(&mut self, error: ComptimeError, span: Span) {
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

    fn infer_record_selection_builtin_call(
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

    fn record_selection_builtin_is_shadowed(&self, env: &TypeEnv, name: &str) -> bool {
        env.get(name).is_some()
            || self.bindings.contains_key(name)
            || self.value_types.contains_key(name)
    }

    fn infer_record_selection_subject(&mut self, env: &TypeEnv, arg: &Expr) -> Type {
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

    fn evaluate_record_selection_labels(
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

    fn infer_comptime_param_call(
        &mut self,
        env: &TypeEnv,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let (params, body) = self.comptime_param_function(callee)?;
        if params.len() != args.len() {
            return Some(Type::Deferred);
        }

        let mut type_bindings = HashMap::new();
        let mut body_env = TypeEnv::new();

        for (param, arg) in params.iter().zip(args).filter(|(param, _)| !param.comptime) {
            let inferred = self.infer(env, arg);
            let actual = self.normalize(&self.resolve_and_default(&inferred));

            if let Some(annotation) = &param.annotation {
                collect_comptime_type_bindings(annotation, &actual, &mut type_bindings);
                let expected = self.lower_annotation_for_inference(annotation);
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
                return Some(Type::Deferred);
            };
            let value = argument.value.clone();

            let domain = param.annotation.as_ref().and_then(|annotation| {
                self.evaluate_comptime_param_domain(annotation, &type_bindings)
            });

            let diagnostics_before_domain_check = self.diagnostics.len();
            if let Some(row) = domain.as_ref().and_then(literal_union_domain_row) {
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
                    comptime::ComptimeValue::ReifiedType(_) | comptime::ComptimeValue::Bool(_) => {}
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
        let result = self.infer(&body_env, body);
        self.local_comptime_values.pop();

        Some(self.resolve_and_default(&result))
    }

    fn evaluate_comptime_param_argument(
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

        let members = self.concrete_label_set_members(arg, bindings)?;
        let labels = members.iter().map(|member| member.label.clone()).collect();
        Some(ComptimeArgument {
            value: comptime::ComptimeValue::LabelSet(labels),
            label_set_members: Some(members),
        })
    }

    fn comptime_param_function(&self, callee: &Expr) -> Option<(&'a [Param], &'a Expr)> {
        let name = expr_name(callee)?;
        let binding = (*self.bindings.get(name)?)?;
        let (params, body) = lambda_parts(&binding.value)?;
        params
            .iter()
            .any(|param| param.comptime)
            .then_some((params, body))
    }

    fn evaluate_comptime_param_domain(
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

    fn infer_value_index(&mut self, env: &TypeEnv, callee: &Expr, args: &[Expr]) -> Type {
        let [arg] = args else {
            return Type::Deferred;
        };

        let callee_type = self.infer(env, callee);
        let callee_type = self.normalize(&self.unifier.resolve(&callee_type));
        let Type::Record(row) = callee_type else {
            return Type::Deferred;
        };
        if row.tail != RowTail::Closed {
            return Type::Deferred;
        }

        let Some(label) = self.comptime_known_label(arg) else {
            return Type::Deferred;
        };

        row_field_type(&row, &label)
            .cloned()
            .unwrap_or(Type::Deferred)
    }

    fn comptime_known_label(&self, expr: &Expr) -> Option<String> {
        match &ungroup_expr(expr).kind {
            ExprKind::Literal(Literal::String(text)) => string_literal_label(text),
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => self
                .lookup_comptime_value(name)
                .and_then(comptime_value_label),
            _ => None,
        }
    }

    fn comptime_known_label_set(&self, expr: &Expr) -> Option<Vec<String>> {
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

    fn comptime_known_label_set_for_mode(
        &self,
        expr: &Expr,
        mode: RowFoldMode<'_>,
    ) -> Option<Vec<String>> {
        self.comptime_known_label_set(expr).or_else(|| match mode {
            RowFoldMode::Annotation => self.comptime_known_reflection_reified_type(expr),
            RowFoldMode::Value { env } => self.comptime_known_reflection_value(expr, env),
        })
    }

    fn comptime_known_reflection_reified_type(&self, expr: &Expr) -> Option<Vec<String>> {
        let ExprKind::Call { callee, args } = &ungroup_expr(expr).kind else {
            return None;
        };
        let reflection = LabelReflection::from_name(expr_name(callee)?)?;

        let [arg] = args.as_slice() else {
            return None;
        };
        let subject = self.lookup_comptime_reified_type_expr(arg)?;
        let subject = self.normalize(&subject);

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

    fn comptime_known_reflection_value(&self, expr: &Expr, env: &TypeEnv) -> Option<Vec<String>> {
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
        let subject = self.normalize(&self.unifier.resolve(subject));

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

    fn lookup_comptime_value(&self, name: &str) -> Option<&comptime::ComptimeValue> {
        self.local_comptime_values
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
    }

    fn current_comptime_value_bindings(&self) -> HashMap<String, comptime::ComptimeValue> {
        let mut bindings = HashMap::new();
        for scope in &self.local_comptime_values {
            bindings.extend(scope.clone());
        }
        bindings
    }

    fn concrete_label_set_members(
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

    fn infer_name_reference(&mut self, env: &TypeEnv, name: &str, span: Span) -> Type {
        if let Some(local) = env.get(name).cloned() {
            return match local {
                LocalValueType::Known(ty) => ty,
                LocalValueType::Scheme(scheme) => self.unifier.instantiate_scheme(&scheme),
                LocalValueType::Unknown => Type::Deferred,
            };
        }

        if let Some(scheme) = self.infer_top_level(name) {
            return self.unifier.instantiate_scheme(&scheme);
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
                Some(scheme) => self.unifier.instantiate_scheme(&scheme),
                None => Type::Deferred,
            };
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

    fn report_unbound_name(&mut self, name: &str, span: Span) {
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

    fn report_unused_result_if_dropped(&mut self, env: &TypeEnv, expr: &Expr) {
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

    fn report_unused_result(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::warning("unused `Result`")
                .with_code(codes::ty::UNUSED_RESULT)
                .with_label(Label::primary(span, "this `Result` is unused"))
                .with_note(
                    "unwrap it with `?!` (panic on `@Err`), propagate it with `?^`, or discard it explicitly with `_ =`.",
                ),
        );
    }

    fn infer_variant_constructor(&mut self, env: &TypeEnv, tag: &str, args: &[Expr]) -> Type {
        let mut payload = Vec::new();

        for arg in args {
            let arg_type = self.infer(env, arg);
            let arg_type = self.unifier.resolve(&arg_type);
            let numeric_metas_only = has_only_meta_unknowns(&arg_type)
                && free_metas(&arg_type)
                    .into_iter()
                    .all(|id| self.unifier.is_numeric_meta(&Type::Meta(id)));
            if !is_resolved_value_type(&arg_type) && !numeric_metas_only {
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

    fn infer_array(&mut self, env: &TypeEnv, elements: &[Expr]) -> Type {
        self.infer_collection(env, elements, "Array")
    }

    fn infer_set(&mut self, env: &TypeEnv, entries: &[RecordEntry]) -> Type {
        let Some(elements) = literal_set_elements(entries) else {
            return Type::Deferred;
        };
        self.infer_collection(env, elements, "Set")
    }

    fn infer_set_union(&mut self, env: &TypeEnv, expr: &Expr) -> Type {
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

    fn infer_set_union_part_type(&mut self, env: &TypeEnv, part: SetUnionPart<'_>) -> Type {
        let ty = self.infer(env, part.expr());
        if part.promotes_singleton() {
            return ty;
        }

        self.set_operand_element_type(&ty).unwrap_or(ty)
    }

    fn set_operand_element_type(&mut self, ty: &Type) -> Option<Type> {
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

    fn infer_collection<'b>(
        &mut self,
        env: &TypeEnv,
        elements: impl IntoIterator<Item = &'b Expr>,
        name: &str,
    ) -> Type {
        let element_type = self.unifier.fresh();
        for element in elements {
            let item_type = self.infer(env, element);
            if self.unifier.unify(&element_type, &item_type).is_err() {
                return Type::Deferred;
            }
        }

        Type::Apply {
            callee: Box::new(Type::Named(name.to_owned())),
            args: vec![element_type],
        }
    }

    fn infer_block(&mut self, env: &TypeEnv, items: &[Item]) -> Type {
        let mut next_env = env.clone();

        for item in merged_items(items) {
            match item {
                MergedItem::Binding { signature, binding } => {
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
                            let scheme = generalize(resolved, &env_metas, &env_row_vars);
                            if type_contains_deferred(&scheme.ty) {
                                LocalValueType::Unknown
                            } else {
                                LocalValueType::Scheme(scheme)
                            }
                        });
                    next_env.insert(binding.name.clone(), local_type);
                }
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

    fn infer_match(&mut self, env: &TypeEnv, subject: &Expr, arms: &[MatchArm]) -> Type {
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

    fn comptime_selected_match_arm<'b>(
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

    fn combine_match_arm_body_types(
        &mut self,
        body_types: &[MatchArmBodyType],
    ) -> MatchArmCombination {
        if let Some(result) = self.union_match_variant_arm_body_types(body_types) {
            return result;
        }

        self.unify_match_arm_body_types(body_types)
    }

    fn union_match_variant_arm_body_types(
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

    fn unify_match_arm_body_types(
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

    fn match_arm_type_conflict(
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

    fn resolved_match_result_type(&self, ty: &Type) -> Type {
        self.normalize(&self.resolve_and_default(ty))
    }

    fn lower_annotation_for_inference(&self, annotation: &Expr) -> Type {
        let mut checker = self.fork_annotation_checker();
        let ty = checker.lower_annotation(annotation);
        checker.normalize(&ty)
    }
}

impl<'a> comptime::EvalContext<'a> for Checker<'a> {
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

    fn lookup_comptime_function(&self, name: &str) -> Option<comptime::ComptimeFunction<'a>> {
        let binding = (*self.bindings.get(name)?)?;
        let (params, body) = lambda_parts(&binding.value)?;

        Some(comptime::ComptimeFunction {
            name: &binding.name,
            params,
            body,
        })
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

fn lambda_parts(expr: &Expr) -> Option<(&[Param], &Expr)> {
    match &ungroup_expr(expr).kind {
        ExprKind::Lambda { params, body, .. } => Some((params, body)),
        _ => None,
    }
}

fn expr_name(expr: &Expr) -> Option<&str> {
    match &ungroup_expr(expr).kind {
        ExprKind::Name(name) => Some(name),
        _ => None,
    }
}

fn ungroup_expr(mut expr: &Expr) -> &Expr {
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    expr
}

fn is_final_expr_item(items: &[Item], expr: &Expr) -> bool {
    matches!(items.last(), Some(Item::Expr(final_expr)) if std::ptr::eq(final_expr, expr))
}

fn is_result_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Apply { callee, args }
            if args.len() == 2
                && matches!(callee.as_ref(), Type::Named(name) if name == "Result")
    )
}
