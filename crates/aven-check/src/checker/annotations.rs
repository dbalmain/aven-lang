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
        self.diagnostics.extend(evaluation.diagnostics);

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

    pub(super) fn normalize_with_visited(&self, ty: &Type, visited: HashSet<String>) -> Type {
        match ty {
            Type::Named(name) => {
                let Some(definition) = self.type_definitions.get(name) else {
                    return Type::Named(name.clone());
                };

                // Recursive definitions stay nominal: expanding them is never
                // idempotent (each pass unfolds one more level), so match and
                // boundary machinery unfold them lazily instead.
                if visited.contains(name)
                    || self.type_references_name(definition, name, &mut HashSet::new())
                {
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
            ExprKind::Call { callee, .. } => {
                match self.try_lower_comptime_annotation(annotation) {
                    Some(ty) => ty,
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

    pub(super) fn lookup_comptime_reified_type_expr(&self, expr: &Expr) -> Option<Type> {
        match &ungroup_expr(expr).kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                self.lookup_comptime_reified_type(name).cloned()
            }
            _ => None,
        }
    }

    pub(super) fn lookup_comptime_reified_type(&self, name: &str) -> Option<&Type> {
        match self.lookup_comptime_value(name)? {
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

fn call_callee_name(callee: &Expr) -> Option<&str> {
    match &ungroup_expr(callee).kind {
        ExprKind::Name(name) => Some(name.as_str()),
        _ => None,
    }
}
