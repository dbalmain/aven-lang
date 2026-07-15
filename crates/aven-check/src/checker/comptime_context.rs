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

    fn type_fits_boundary(&mut self, expected: &Type, actual: &Type) -> bool {
        self.type_fits_boundary_without_reporting(expected, actual)
    }
}

impl Checker<'_> {
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

        for (id, head) in &heads {
            self.recursive_type_unfoldings.insert(*id, head.clone());
            self.unifier.insert_recursive_type(*id, head.clone());
        }

        for frame in component {
            let mut diagnostics = frame
                .result
                .map_or_else(Vec::new, |result| result.diagnostics);
            if !productive.contains(&frame.id) {
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
                        "every value of `{}` requires another recursive value via strict recursion",
                        frame.key.origin.definition
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
