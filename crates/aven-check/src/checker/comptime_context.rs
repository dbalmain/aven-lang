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

    fn specialization_is_active(&self, key: &comptime::SpecializationKey) -> bool {
        self.comptime_specialization_active.contains_key(key)
    }

    fn active_specialization_reference(
        &mut self,
        key: &comptime::SpecializationKey,
    ) -> Option<RecursiveTypeId> {
        let &target_position = self.comptime_specialization_active.get(key)?;
        let target_index = self.comptime_specialization_stack[target_position].index;
        let current_key = self.comptime_specialization_calls.last()?;
        let &current_position = self.comptime_specialization_active.get(current_key)?;
        let current = &mut self.comptime_specialization_stack[current_position];
        current.lowlink = current.lowlink.min(target_index);
        if current.key == *key {
            current.self_edge = true;
        }
        Some(self.comptime_specialization_stack[target_position].id)
    }

    fn begin_specialization(
        &mut self,
        key: comptime::SpecializationKey,
        id: RecursiveTypeId,
        call_span: Span,
    ) -> Result<(), Diagnostic> {
        if self.comptime_specialization_calls.len() >= comptime::DEFAULT_EVALUATION_FUEL {
            return Err(comptime::evaluation_limit(call_span));
        }
        let index = self.comptime_specialization_stack.len();
        self.comptime_specialization_active
            .insert(key.clone(), index);
        self.comptime_specialization_calls.push(key.clone());
        self.comptime_specialization_stack
            .push(SpecializationFrame {
                key,
                id,
                index,
                lowlink: index,
                self_edge: false,
                call_span,
                result: None,
            });
        Ok(())
    }

    fn finish_specialization(
        &mut self,
        key: &comptime::SpecializationKey,
        result: comptime::EvaluationResult,
    ) -> comptime::EvaluationResult {
        let Some(position) = self.comptime_specialization_active.get(key).copied() else {
            return result;
        };
        self.comptime_specialization_stack[position].result = Some(result.clone());
        if self.comptime_specialization_calls.pop().as_ref() != Some(key) {
            return result;
        }

        let index = self.comptime_specialization_stack[position].index;
        let lowlink = self.comptime_specialization_stack[position].lowlink;
        if lowlink != index {
            if let Some(parent_key) = self.comptime_specialization_calls.last()
                && let Some(parent_position) =
                    self.comptime_specialization_active.get(parent_key).copied()
            {
                let parent = &mut self.comptime_specialization_stack[parent_position];
                parent.lowlink = parent.lowlink.min(lowlink);
            }
            return comptime::EvaluationResult {
                evaluation: Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(
                    Type::Recursive(self.comptime_specialization_stack[position].id),
                )),
                diagnostics: result.diagnostics,
            };
        }

        let component = self.comptime_specialization_stack.split_off(position);
        for frame in &component {
            self.comptime_specialization_active.remove(&frame.key);
        }
        let recursive = component.len() > 1 || component.iter().any(|frame| frame.self_edge);
        if !recursive {
            if !matches!(result.evaluation, Evaluation::Unsupported) {
                self.comptime_specializations
                    .insert(key.clone(), result.clone());
            }
            return result;
        }

        self.finish_recursive_component(component, key, result)
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

    fn unfold_recursive_type_once(&self, ty: &Type) -> Type {
        Checker::unfold_recursive_type_once(self, ty)
    }

    fn type_fits_boundary(&mut self, expected: &Type, actual: &Type) -> bool {
        self.type_fits_boundary_without_reporting(expected, actual)
    }
}

impl Checker<'_> {
    pub(super) fn lower_zero_argument_type_definition(&mut self, name: &str, span: Span) {
        let Some(function) = self.zero_argument_type_export(name) else {
            return;
        };
        let result = comptime::evaluate_zero_argument_type_binding(self, function, span);
        self.extend_unique_diagnostics(result.diagnostics);

        let ty = match result.evaluation {
            Evaluation::Evaluated(value) => value.reify_type_position().into_reified_type(),
            Evaluation::Deferred | Evaluation::Unsupported => Some(Type::Deferred),
        };
        if let Some(ty) = ty {
            self.type_definitions.insert(name.to_owned(), ty);
        }
    }

    fn zero_argument_type_export(&self, name: &str) -> Option<comptime::ComptimeExport> {
        if !self.zero_argument_type_bindings.contains(name) {
            return None;
        }
        let binding = self.bindings.get(name).and_then(|binding| *binding)?;
        Some(
            comptime::ComptimeExport::from_type_binding(
                name,
                &binding.value,
                self.type_definitions.clone(),
                self.local_comptime_function_definitions(),
            )
            .with_module_identity(self.module_identity.clone()),
        )
    }

    pub(super) fn try_lower_zero_argument_type(&mut self, name: &str, span: Span) -> Option<Type> {
        let function = self.zero_argument_type_export(name)?;
        let result = comptime::evaluate_zero_argument_type_binding(self, function, span);
        self.extend_unique_diagnostics(result.diagnostics);
        match result.evaluation {
            Evaluation::Evaluated(value) => value.reify_type_position().into_reified_type(),
            Evaluation::Deferred => Some(Type::Deferred),
            Evaluation::Unsupported => None,
        }
    }

    pub(super) fn lower_prelowered_type_definitions(&mut self) {
        let names = self
            .prelowered_type_bindings
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for name in names {
            let result = self.evaluate_prelowered_type_definition(&name, Span::point(0));
            self.extend_unique_diagnostics(result.diagnostics);
            if let Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(ty)) =
                result.evaluation
            {
                self.type_definitions.insert(name, ty);
            }
        }
        self.prelowered_type_bindings.clear();
    }

    fn evaluate_prelowered_type_definition(
        &mut self,
        name: &str,
        span: Span,
    ) -> comptime::EvaluationResult {
        let origin =
            comptime::ComptimeOrigin::new(self.prelowered_type_module.clone(), name.to_owned());
        let key = comptime::SpecializationKey::zero_argument(origin);
        if let Some(result) = self.comptime_specializations.get(&key) {
            return result.clone();
        }
        if let Some(id) =
            <Self as comptime::EvalContext>::active_specialization_reference(self, &key)
        {
            return comptime::EvaluationResult {
                evaluation: Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(
                    Type::Recursive(id),
                )),
                diagnostics: Vec::new(),
            };
        }

        let id = comptime::intern_recursive_type(&key, &[]);
        if let Err(diagnostic) =
            <Self as comptime::EvalContext>::begin_specialization(self, key.clone(), id, span)
        {
            return comptime::EvaluationResult {
                evaluation: Evaluation::Deferred,
                diagnostics: vec![diagnostic],
            };
        }

        let Some(head) = self.prelowered_type_bindings.get(name).cloned() else {
            return <Self as comptime::EvalContext>::finish_specialization(
                self,
                &key,
                comptime::EvaluationResult {
                    evaluation: Evaluation::Unsupported,
                    diagnostics: Vec::new(),
                },
            );
        };
        let (head, diagnostics) = self.resolve_prelowered_type_head(&head, span);
        <Self as comptime::EvalContext>::finish_specialization(
            self,
            &key,
            comptime::EvaluationResult {
                evaluation: Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(head)),
                diagnostics,
            },
        )
    }

    fn resolve_prelowered_type_head(&mut self, ty: &Type, span: Span) -> (Type, Vec<Diagnostic>) {
        match ty {
            Type::Named(name) if self.prelowered_type_bindings.contains_key(name) => {
                let result = self.evaluate_prelowered_type_definition(name, span);
                let ty = match result.evaluation {
                    Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(ty)) => ty,
                    Evaluation::Evaluated(value) => value
                        .reify_type_position()
                        .into_reified_type()
                        .unwrap_or_else(|| Type::Named(name.clone())),
                    Evaluation::Deferred | Evaluation::Unsupported => Type::Named(name.clone()),
                };
                (ty, result.diagnostics)
            }
            Type::Apply { callee, args } => {
                let (callee, mut diagnostics) = self.resolve_prelowered_type_head(callee, span);
                let mut resolved_args = Vec::with_capacity(args.len());
                for arg in args {
                    let (arg, nested) = self.resolve_prelowered_type_head(arg, span);
                    resolved_args.push(arg);
                    diagnostics.extend(nested);
                }
                (
                    Type::Apply {
                        callee: Box::new(callee),
                        args: resolved_args,
                    },
                    diagnostics,
                )
            }
            Type::Function {
                params,
                result,
                required,
            } => {
                let mut diagnostics = Vec::new();
                let mut resolved_params = Vec::with_capacity(params.len());
                for param in params {
                    let (param, nested) = self.resolve_prelowered_type_head(param, span);
                    resolved_params.push(param);
                    diagnostics.extend(nested);
                }
                let (result, nested) = self.resolve_prelowered_type_head(result, span);
                diagnostics.extend(nested);
                (
                    Type::Function {
                        params: resolved_params,
                        result: Box::new(result),
                        required: *required,
                    },
                    diagnostics,
                )
            }
            Type::Optional(inner) => {
                let (inner, diagnostics) = self.resolve_prelowered_type_head(inner, span);
                (Type::Optional(Box::new(inner)), diagnostics)
            }
            Type::Nullable(inner) => {
                let (inner, diagnostics) = self.resolve_prelowered_type_head(inner, span);
                (Type::Nullable(Box::new(inner)), diagnostics)
            }
            Type::Tuple(items) => {
                let mut diagnostics = Vec::new();
                let mut resolved = Vec::with_capacity(items.len());
                for item in items {
                    let (item, nested) = self.resolve_prelowered_type_head(item, span);
                    resolved.push(item);
                    diagnostics.extend(nested);
                }
                (Type::Tuple(resolved), diagnostics)
            }
            Type::Record(row) | Type::Variant(row) => {
                let mut diagnostics = Vec::new();
                let mut entries = Vec::with_capacity(row.entries.len());
                for entry in &row.entries {
                    entries.push(match entry {
                        RowEntry::Field { name, ty } => {
                            let (ty, nested) = self.resolve_prelowered_type_head(ty, span);
                            diagnostics.extend(nested);
                            RowEntry::Field {
                                name: name.clone(),
                                ty,
                            }
                        }
                        RowEntry::Tag { name, payload } => {
                            let mut resolved = Vec::with_capacity(payload.len());
                            for ty in payload {
                                let (ty, nested) = self.resolve_prelowered_type_head(ty, span);
                                resolved.push(ty);
                                diagnostics.extend(nested);
                            }
                            RowEntry::Tag {
                                name: name.clone(),
                                payload: resolved,
                            }
                        }
                        RowEntry::Literal { value } => RowEntry::Literal {
                            value: value.clone(),
                        },
                    });
                }
                let resolved = Row {
                    entries,
                    tail: row.tail,
                };
                if matches!(ty, Type::Record(_)) {
                    (Type::Record(resolved), diagnostics)
                } else {
                    (Type::Variant(resolved), diagnostics)
                }
            }
            Type::Deferred
            | Type::Named(_)
            | Type::Variable(_)
            | Type::Meta(_)
            | Type::Recursive(_) => (ty.clone(), Vec::new()),
        }
    }

    pub(super) fn extend_unique_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        for diagnostic in diagnostics {
            self.push_unique_diagnostic(diagnostic);
        }
    }

    fn finish_recursive_component(
        &mut self,
        component: Vec<SpecializationFrame>,
        requested: &comptime::SpecializationKey,
        fallback: comptime::EvaluationResult,
    ) -> comptime::EvaluationResult {
        let mut heads = HashMap::new();
        for frame in &component {
            let Some(comptime::EvaluationResult {
                evaluation: Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(head)),
                ..
            }) = frame.result.as_ref()
            else {
                for frame in component {
                    if let Some(result) = frame.result {
                        self.comptime_specializations.insert(frame.key, result);
                    }
                }
                return fallback;
            };
            heads.insert(frame.id, head.clone());
        }

        let component_ids: HashSet<_> = heads.keys().copied().collect();
        let mut productive = HashSet::new();
        loop {
            let added = heads
                .iter()
                .filter_map(|(id, head)| {
                    (!productive.contains(id)
                        && crate::productivity::is_productive(head, &mut |node| {
                            let Type::Recursive(reference) = node else {
                                return None;
                            };
                            component_ids
                                .contains(reference)
                                .then(|| productive.contains(reference))
                        }))
                    .then_some(*id)
                })
                .collect::<Vec<_>>();
            if added.is_empty() {
                break;
            }
            productive.extend(added);
        }

        let transparent_alias_component = component.iter().all(|frame| {
            self.transparent_alias_cycles
                .contains(&frame.key.origin.definition)
        });

        if !transparent_alias_component {
            for (id, head) in &heads {
                if productive.contains(id) {
                    self.recursive_type_unfoldings.insert(*id, head.clone());
                    self.unifier.insert_recursive_type(*id, head.clone());
                }
            }
        }

        let unproductive = component_ids
            .difference(&productive)
            .copied()
            .collect::<HashSet<_>>();

        for frame in component {
            let mut diagnostics = frame
                .result
                .map_or_else(Vec::new, |result| result.diagnostics);
            if !productive.contains(&frame.id) && !transparent_alias_component {
                let forcing = heads
                    .get(&frame.id)
                    .and_then(|head| crate::productivity::forcing_step(head, &unproductive))
                    .unwrap_or_else(|| "strict recursion".to_owned());
                diagnostics.push(
                    Diagnostic::error(format!(
                        "recursive type `{}` has no finite value",
                        frame.key.origin.definition
                    ))
                    .with_code(codes::ty::UNPRODUCTIVE_RECURSION)
                    .with_label(Label::primary(
                        frame.call_span,
                        "unproductive recursive type declared here",
                    ))
                    .with_note(format!(
                        "every value of `{}` requires another recursive value via {forcing}",
                        frame.key.origin.definition,
                    )),
                );
            }
            self.comptime_specializations.insert(
                frame.key,
                comptime::EvaluationResult {
                    evaluation: Evaluation::Evaluated(comptime::ComptimeValue::ReifiedType(
                        Type::Recursive(frame.id),
                    )),
                    diagnostics,
                },
            );
        }

        self.comptime_specializations
            .get(requested)
            .cloned()
            .unwrap_or(fallback)
    }
}
