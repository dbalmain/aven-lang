use super::*;

impl<'a> Checker<'a> {
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

    pub(super) fn try_lower_comptime_annotation(&mut self, annotation: &Expr) -> Option<Type> {
        let evaluation = comptime::evaluate_type_position(self, annotation);
        self.finish_comptime_annotation_evaluation(evaluation)
    }

    pub(super) fn try_lower_comptime_annotation_for_eager_validation(
        &mut self,
        annotation: &Expr,
    ) -> Option<Type> {
        let evaluation = comptime::evaluate_type_position_for_eager_validation(self, annotation);
        self.finish_comptime_annotation_evaluation(evaluation)
    }

    fn finish_comptime_annotation_evaluation(
        &mut self,
        evaluation: comptime::EvaluationResult,
    ) -> Option<Type> {
        self.extend_unique_diagnostics(evaluation.diagnostics);

        match evaluation.evaluation {
            Evaluation::Evaluated(value) => value.reify_type_position().into_reified_type(),
            Evaluation::Deferred => Some(Type::Deferred),
            Evaluation::Unsupported => None,
        }
    }

    pub(super) fn reflection_subject_is_unresolved(&self, ty: &Type) -> bool {
        match ty {
            Type::Deferred | Type::Variable(_) | Type::Meta(_) => true,
            Type::Named(name) => !BUILTIN_TYPES.contains(&name.as_str()),
            Type::Recursive(_) => false,
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

    pub(super) fn type_admits_undefined(&self, ty: &Type) -> bool {
        matches!(self.normalize(ty), Type::Optional(_))
    }

    pub(super) fn strip_optional(&self, ty: &Type) -> Type {
        match self.normalize(ty) {
            Type::Optional(inner) => *inner,
            ty => ty,
        }
    }

    pub(super) fn strip_nullable(&self, ty: &Type) -> Type {
        match self.normalize(ty) {
            Type::Optional(inner) => Type::Optional(Box::new(self.strip_nullable(&inner))),
            Type::Nullable(inner) => *inner,
            ty => ty,
        }
    }

    pub(crate) fn normalize(&self, ty: &Type) -> Type {
        self.normalize_with_visited(ty, HashSet::new())
    }

    /// Unfold exactly one recursive reference when a consumer needs its outer
    /// constructor. Back-edge references inside the cloned head remain atomic.
    pub(super) fn unfold_recursive_type_once(&self, ty: &Type) -> Type {
        crate::unfold_recursive_type_once(ty, &self.recursive_type_unfoldings)
    }

    /// Normalize aliases and unfold recursive references at a structural
    /// demand. Each id unfolds at most once, so a back edge stays a reference.
    pub(super) fn normalize_for_demand(&self, ty: &Type) -> Type {
        self.unfold_recursive_type_for_demand(&self.normalize(ty), &mut HashSet::new())
    }

    fn unfold_recursive_type_for_demand(
        &self,
        ty: &Type,
        visited: &mut HashSet<RecursiveTypeId>,
    ) -> Type {
        match ty {
            Type::Recursive(id) => {
                if !visited.insert(*id) {
                    return ty.clone();
                }
                let unfolded = self
                    .recursive_type_unfoldings
                    .get(id)
                    .map(|head| self.normalize(head))
                    .unwrap_or_else(|| ty.clone());
                visited.remove(id);
                unfolded
            }
            Type::Apply { callee, args } => Type::Apply {
                callee: Box::new(self.unfold_recursive_type_for_demand(callee, visited)),
                args: args
                    .iter()
                    .map(|arg| self.unfold_recursive_type_for_demand(arg, visited))
                    .collect(),
            },
            Type::Function {
                params,
                result,
                required,
            } => Type::Function {
                params: params
                    .iter()
                    .map(|param| self.unfold_recursive_type_for_demand(param, visited))
                    .collect(),
                result: Box::new(self.unfold_recursive_type_for_demand(result, visited)),
                required: *required,
            },
            Type::Optional(inner) => Type::Optional(Box::new(
                self.unfold_recursive_type_for_demand(inner, visited),
            )),
            Type::Nullable(inner) => Type::Nullable(Box::new(
                self.unfold_recursive_type_for_demand(inner, visited),
            )),
            Type::Tuple(items) => Type::Tuple(
                items
                    .iter()
                    .map(|item| self.unfold_recursive_type_for_demand(item, visited))
                    .collect(),
            ),
            Type::Record(row) => Type::Record(self.unfold_recursive_row_for_demand(row, visited)),
            Type::Variant(row) => Type::Variant(self.unfold_recursive_row_for_demand(row, visited)),
            Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => ty.clone(),
        }
    }

    fn unfold_recursive_row_for_demand(
        &self,
        row: &Row,
        visited: &mut HashSet<RecursiveTypeId>,
    ) -> Row {
        Row {
            entries: row
                .entries
                .iter()
                .map(|entry| match entry {
                    RowEntry::Field { name, ty } => RowEntry::Field {
                        name: name.clone(),
                        ty: self.unfold_recursive_type_for_demand(ty, visited),
                    },
                    RowEntry::Tag { name, payload } => RowEntry::Tag {
                        name: name.clone(),
                        payload: payload
                            .iter()
                            .map(|ty| self.unfold_recursive_type_for_demand(ty, visited))
                            .collect(),
                    },
                    RowEntry::Literal { value } => RowEntry::Literal {
                        value: value.clone(),
                    },
                })
                .collect(),
            tail: row.tail,
        }
    }

    pub(super) fn normalize_with_visited(&self, ty: &Type, visited: HashSet<String>) -> Type {
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
            Type::Recursive(id) => Type::Recursive(*id),
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

    pub(super) fn normalize_types(&self, types: &[Type], visited: &HashSet<String>) -> Vec<Type> {
        types
            .iter()
            .map(|ty| self.normalize_with_visited(ty, visited.clone()))
            .collect()
    }

    pub(super) fn normalize_optional(&self, inner: &Type, visited: HashSet<String>) -> Type {
        match self.normalize_with_visited(inner, visited) {
            Type::Optional(inner) => Type::Optional(inner),
            inner => Type::Optional(Box::new(inner)),
        }
    }

    pub(super) fn normalize_nullable(&self, inner: &Type, visited: HashSet<String>) -> Type {
        match self.normalize_with_visited(inner, visited) {
            Type::Optional(inner) => Type::Optional(Box::new(Type::Nullable(inner))),
            Type::Nullable(inner) => Type::Nullable(inner),
            inner => Type::Nullable(Box::new(inner)),
        }
    }

    pub(super) fn normalize_row(&self, row: &Row, visited: &HashSet<String>) -> Row {
        Row {
            entries: row
                .entries
                .iter()
                .map(|entry| self.normalize_row_entry(entry, visited))
                .collect(),
            tail: row.tail,
        }
    }

    pub(super) fn normalize_row_entry(
        &self,
        entry: &RowEntry,
        visited: &HashSet<String>,
    ) -> RowEntry {
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

    pub(super) fn fork_annotation_checker(&self) -> Checker<'a> {
        let mut checker =
            Checker::with_type_definitions(self.known_types.clone(), self.type_definitions.clone());
        checker.comptime_bindings = self.comptime_bindings.clone();
        checker.comptime_artifacts = self.comptime_artifacts.clone();
        checker.comptime_specializations = self.comptime_specializations.clone();
        checker.comptime_specialization_calls = self.comptime_specialization_calls.clone();
        checker.comptime_specialization_stack = self.comptime_specialization_stack.clone();
        checker.comptime_specialization_active = self.comptime_specialization_active.clone();
        checker.recursive_type_unfoldings = self.recursive_type_unfoldings.clone();
        checker.recursive_type_comparisons = self.recursive_type_comparisons.clone();
        checker.module_identity = self.module_identity.clone();
        checker
            .unifier
            .set_recursive_type_unfoldings(self.recursive_type_unfoldings.clone());
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
                    return ty;
                }
                if let Some(ty) = self.try_lower_zero_argument_type(name, annotation.span) {
                    return ty;
                }

                self.check_type_name(name, annotation.span);
                Type::Named(name.clone())
            }
            ExprKind::Name(name) => self
                .lookup_comptime_reified_type(name)
                .unwrap_or_else(|| Type::Variable(name.clone())),
            ExprKind::Group(inner) => self.lower_annotation(inner),
            ExprKind::Index { callee, args } => self
                .lower_comptime_type_index(callee, args)
                .unwrap_or_else(|| {
                    if !is_collection_type_sugar(callee, args) {
                        self.report_bracket_type_application(annotation.span);
                    }
                    self.lower_type_application(callee, args)
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
            ExprKind::FieldAccess {
                receiver,
                field,
                field_span,
                ..
            } => {
                if let Some(specifier) = self.static_import_specifier_for_receiver(receiver) {
                    if let Some(ty) = self.imports.type_export(&specifier, field) {
                        return ty.clone();
                    }
                    self.report_unknown_module_type(field, *field_span);
                    return Type::Deferred;
                }
                self.lower_deferred_annotation(annotation);
                Type::Deferred
            }
            ExprKind::Set(entries) => self.lower_row_entries(entries, RowKind::Variant),
            ExprKind::Binary { operator, .. } if operator == "|" => {
                self.lower_union_annotation(annotation)
            }
            ExprKind::Literal(Literal::Bool(_) | Literal::Number(_) | Literal::String(_))
            | ExprKind::Tag(_) => self.lower_singleton_variant_annotation(annotation),
            ExprKind::Call { callee, args } => {
                if matches!(&callee.kind, ExprKind::Tag(_)) {
                    return self.lower_singleton_variant_annotation(annotation);
                }

                if self.is_type_application_callee(callee)
                    && !self.is_uppercase_comptime_function_callee(callee)
                {
                    return self.lower_type_application(callee, args);
                }

                self.check_unevaluable_lowercase_comptime_arguments(callee, args);
                match self.try_lower_comptime_annotation(annotation) {
                    Some(ty) => {
                        if self.is_uppercase_comptime_function_callee(callee) {
                            // Specialization validates the outer comptime
                            // parameter bounds. This walk supplies only nested
                            // value/name diagnostics that its evaluator cannot
                            // collect from unsupported forms.
                            for arg in args {
                                let bound_comptime_name = match &ungroup_expr(arg).kind {
                                    ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                                        self.lookup_comptime_value(name).is_some()
                                    }
                                    _ => false,
                                };
                                if !bound_comptime_name {
                                    self.check_value_expr(arg);
                                }
                            }
                        }
                        ty
                    }
                    None => {
                        // Imported names applied in type position must expand; silent
                        // Deferred would accept anything (the pre-fix module bug).
                        if let Some(name) = call_callee_name(callee)
                            && self.is_imported_name(name)
                        {
                            self.report_unexpandable_imported_application(name, annotation.span);
                        }
                        self.lower_deferred_annotation(annotation);
                        Type::Deferred
                    }
                }
            }
            ExprKind::Missing => Type::Deferred,
            ExprKind::Literal(_)
            | ExprKind::Interpolation(_)
            | ExprKind::Undefined
            | ExprKind::Null
            | ExprKind::Array(_)
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

    fn check_unevaluable_lowercase_comptime_arguments(&mut self, callee: &Expr, args: &[Expr]) {
        let Some((params, _)) = self.comptime_param_function(callee) else {
            return;
        };
        if self.is_uppercase_comptime_function_callee(callee) {
            return;
        }

        let bindings = self.current_comptime_value_bindings();
        for (param, arg) in params.iter().zip(args) {
            if !param.comptime
                || self.expr_references_unresolved_comptime_param(arg)
                || self
                    .evaluate_comptime_param_argument(arg, &bindings)
                    .is_some()
            {
                continue;
            }

            let diagnostics_start = self.diagnostics.len();
            self.check_value_expr(arg);
            if self.diagnostics.len() == diagnostics_start && self.is_runtime_computation_call(arg)
            {
                let function = call_callee_name(callee).unwrap_or("comptime function");
                self.push_unique_diagnostic(comptime::comptime_argument_not_known(
                    arg.span, function,
                ));
            }
        }
    }

    fn lower_type_application(&mut self, callee: &Expr, args: &[Expr]) -> Type {
        Type::Apply {
            callee: Box::new(self.lower_annotation(callee)),
            args: self.lower_annotations(args),
        }
    }

    fn is_type_application_callee(&self, callee: &Expr) -> bool {
        let Some(name) = call_callee_name(callee) else {
            return false;
        };

        BUILTIN_TYPES.contains(&name)
            || self.known_types.contains(name)
            || name.chars().next().is_some_and(char::is_uppercase)
    }

    pub(super) fn is_uppercase_comptime_function_callee(&self, callee: &Expr) -> bool {
        let Some(name) = call_callee_name(callee) else {
            return false;
        };

        name.chars().next().is_some_and(char::is_uppercase)
            && self.lookup_comptime_function_export(name).is_some()
    }

    fn static_import_specifier_for_receiver(&self, receiver: &Expr) -> Option<String> {
        let ExprKind::Name(name) = &ungroup_expr(receiver).kind else {
            return None;
        };
        let binding = self.bindings.get(name).and_then(|binding| *binding)?;
        aven_parser::static_import_specifier(&binding.value)
    }

    pub(super) fn lower_union_annotation(&mut self, annotation: &Expr) -> Type {
        let entries = union_annotation_entries(annotation);
        self.lower_row_entries(&entries, RowKind::Variant)
    }

    pub(super) fn lower_singleton_variant_annotation(&mut self, annotation: &Expr) -> Type {
        let entry = RecordEntry::Element(annotation.clone());
        self.lower_row_entries(std::slice::from_ref(&entry), RowKind::Variant)
    }

    pub(super) fn lower_annotations(&mut self, items: &[Expr]) -> Vec<Type> {
        items
            .iter()
            .map(|item| self.lower_annotation(item))
            .collect()
    }

    pub(super) fn lower_comptime_type_index(
        &mut self,
        callee: &Expr,
        args: &[Expr],
    ) -> Option<Type> {
        let subject = self.lookup_comptime_reified_type_expr(callee)?;
        let [arg] = args else {
            return Some(Type::Deferred);
        };

        match self.unfold_recursive_type_once(&self.normalize(&subject)) {
            Type::Tuple(items) => Some(self.infer_tuple_index(&items, arg)),
            Type::Record(row) if row.tail == RowTail::Closed => {
                let Some(label) = self.comptime_known_label(arg) else {
                    return Some(Type::Deferred);
                };

                Some(
                    row_field_type(&row, &label)
                        .cloned()
                        .unwrap_or(Type::Deferred),
                )
            }
            Type::Record(_) => Some(Type::Deferred),
            _ => None,
        }
    }

    pub(super) fn lookup_comptime_reified_type_expr(&self, expr: &Expr) -> Option<Type> {
        match &ungroup_expr(expr).kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => self
                .lookup_comptime_reified_type(name)
                .or_else(|| self.type_definitions.get(name).cloned()),
            _ => None,
        }
    }

    /// Resolve a local comptime binding in type position. Value bindings
    /// (`Literal`, `Bool`, `LabelSet`) reify to their type-position form so a
    /// value parameter such as `n = 3` can appear as a field type `size: n`.
    pub(super) fn lookup_comptime_reified_type(&self, name: &str) -> Option<Type> {
        match self
            .lookup_comptime_value(name)?
            .clone()
            .reify_type_position()
        {
            comptime::ComptimeValue::ReifiedType(ty) => Some(ty),
            comptime::ComptimeValue::LabelSet(_)
            | comptime::ComptimeValue::Literal(_)
            | comptime::ComptimeValue::Bool(_) => None,
        }
    }

    pub(super) fn lower_deferred_annotation(&mut self, annotation: &Expr) {
        walk_expr_children(annotation, &mut |child| {
            self.lower_annotation(child);
        });
    }
}

pub(super) fn call_callee_name(callee: &Expr) -> Option<&str> {
    match &ungroup_expr(callee).kind {
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => Some(name.as_str()),
        _ => None,
    }
}

fn is_collection_type_sugar(callee: &Expr, args: &[Expr]) -> bool {
    matches!(
        &ungroup_expr(callee).kind,
        ExprKind::ComptimeName(name) if matches!(name.as_str(), "Array" | "Set")
    ) && args.len() == 1
        && callee.span.start >= args[0].span.end
}
