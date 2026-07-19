use super::*;

impl<'a> Checker<'a> {
    pub(crate) fn with_type_definitions(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
    ) -> Self {
        Self {
            known_types,
            type_definitions,
            named_family_aliases: HashMap::new(),
            named_families: HashMap::new(),
            builtin_methods: BuiltinMethodEnvironment::default(),
            local_builtin_methods: Vec::new(),
            trusted_builtin_method_source: false,
            zero_argument_type_bindings: HashSet::new(),
            prelowered_type_bindings: HashMap::new(),
            prelowered_type_module: comptime::ComptimeModuleIdentity::specifier("host"),
            transparent_alias_cycles: HashSet::new(),
            value_types: HashMap::new(),
            comptime_bindings: HashSet::new(),
            comptime_artifacts: HashMap::new(),
            comptime_specializations: HashMap::new(),
            comptime_specialization_calls: Vec::new(),
            comptime_specialization_stack: Vec::new(),
            comptime_specialization_active: HashMap::new(),
            recursive_type_unfoldings: HashMap::new(),
            recursive_type_comparisons: HashSet::new(),
            module_identity: comptime::ComptimeModuleIdentity::Current,
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
            requirement_self_scopes: Vec::new(),
            provider_owner_scopes: Vec::new(),
            method_obligations: Vec::new(),
            next_method_obligation_id: 0,
            method_assumption_scopes: Vec::new(),
            slot_reifications: HashMap::new(),
            direct_slot_inits: HashMap::new(),
            primitive_family_coercions: HashMap::new(),
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
            comptime::ComptimeModuleIdentity::Current,
        )
    }

    pub(crate) fn with_module_and_host_globals_and_imports(
        known_types: HashSet<String>,
        type_definitions: HashMap<String, Type>,
        module: &'a Module,
        globals: &HostGlobals,
        imports: &ModuleImports,
        module_identity: comptime::ComptimeModuleIdentity,
    ) -> Self {
        let mut checker = Self::with_module_environment(known_types, type_definitions, module);
        checker.module_identity = module_identity;
        checker.globals = globals.types.clone();
        checker.imports = imports.clone();
        checker.builtin_methods = imports.builtin_methods.clone();
        checker.trusted_builtin_method_source = imports.trusted_builtin_method_source;
        checker.seed_imported_named_families(module);
        checker.recursive_type_unfoldings = imports.recursive_type_unfoldings.clone();
        checker
            .unifier
            .set_recursive_type_unfoldings(checker.recursive_type_unfoldings.clone());
        checker.host_comptime_fns = globals
            .comptime_fns
            .iter()
            .map(|(name, spec)| (name.clone(), spec.clone()))
            .collect();
        checker.prelowered_type_bindings = globals.type_definitions.iter().cloned().collect();
        checker.prelowered_type_module = globals.type_definition_module.clone();
        checker.lower_prelowered_type_definitions();

        checker.prepare_named_families(module);

        let mut reserved_names: HashSet<_> = BUILTIN_TYPES
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        reserved_names.extend(
            globals
                .type_definitions
                .iter()
                .map(|(name, _)| name.clone()),
        );
        checker.lower_module_type_definitions(module, &reserved_names);
        checker.canonicalize_named_family_aliases(module);
        checker.lower_named_family_methods(module);
        checker.lower_builtin_method_attachments(module);
        checker.build_statics(globals);
        checker.collect_top_level_pattern_bindings(module);
        checker.build_value_types(module);
        checker.build_comptime_artifacts(module);
        checker
    }

    fn seed_imported_named_families(&mut self, module: &Module) {
        for item in &module.items {
            let Item::PatternBinding(binding) = item else {
                continue;
            };
            let Some(specifier) = aven_parser::static_import_specifier(&binding.value) else {
                continue;
            };
            let ExprKind::Record(entries) = &ungroup_expr(&binding.pattern).kind else {
                continue;
            };
            for entry in entries {
                let (source, target) = match entry {
                    RecordEntry::Shorthand { name, .. } => (name, name),
                    RecordEntry::Rename { from, to, .. } => (from, to),
                    _ => continue,
                };
                let Some(family) = self
                    .imports
                    .named_family_export(&specifier, source)
                    .cloned()
                else {
                    continue;
                };
                self.named_family_aliases
                    .insert(target.clone(), family.owner.clone());
                self.named_family_aliases
                    .insert(family.owner.clone(), family.owner.clone());
                self.named_families
                    .entry(family.owner.clone())
                    .or_insert(family);
            }
        }
    }

    fn lower_module_type_definitions(&mut self, module: &Module, reserved_names: &HashSet<String>) {
        self.zero_argument_type_bindings =
            crate::lower::type_definition_names(module, &self.known_types, reserved_names);
        let cyclic = crate::lower::cyclic_aliases(module, &self.zero_argument_type_bindings);
        self.transparent_alias_cycles = cyclic.names;
        self.diagnostics.extend(cyclic.diagnostics);

        for declaration in collect_declarations(module) {
            if !self.zero_argument_type_bindings.contains(&declaration.name) {
                continue;
            }
            self.lower_zero_argument_type_definition(&declaration.name, declaration.span);
        }
    }

    fn prepare_named_families(&mut self, module: &Module) {
        let providers = collect_declarations(module)
            .into_iter()
            .filter(|declaration| declaration.phase == DeclarationPhase::Comptime)
            .filter_map(|declaration| {
                let binding = binding_for_declaration(module, &declaration)?;
                is_named_family_provider(&binding.value)
                    .then_some((declaration.name, binding.value.clone()))
            })
            .collect::<Vec<_>>();

        for (name, value) in &providers {
            let owner = self.named_family_owner_key(name);
            self.named_family_aliases
                .insert(name.clone(), owner.clone());
            self.named_family_aliases.insert(owner.clone(), owner);
            let provisional = if let Some((base, _)) = aven_parser::primitive_family_parts(value) {
                self.lower_annotation(base)
            } else {
                Type::Record(Row {
                    entries: Vec::new(),
                    tail: RowTail::Closed,
                })
            };
            self.type_definitions.insert(name.clone(), provisional);
        }

        for (name, value) in providers {
            let owner = self
                .named_family_aliases
                .get(&name)
                .cloned()
                .expect("prepared providers have canonical owners");
            if let Some((base_expr, _)) = aven_parser::primitive_family_parts(&value) {
                let lowered_base = self.lower_annotation(base_expr);
                let base = self.normalize(&lowered_base);
                let supported = primitive_family_base_is_supported(&base);
                if !supported {
                    let (message, note) = if matches!(&base, Type::Named(name) if self.named_family_aliases.contains_key(name))
                    {
                        (
                            "a primitive-base family cannot use another named family as its base",
                            "family-on-family bases are deferred; use the original concrete builtin base",
                        )
                    } else if matches!(base, Type::Apply { .. }) {
                        (
                            "named primitive-base families require a concrete builtin container base",
                            "use a fully applied Array, Map, or Set type with no open type variables",
                        )
                    } else {
                        (
                            "named primitive-base families require a concrete scalar builtin base",
                            "the supported bases in this slice are Int, Float, Text, and Bool",
                        )
                    };
                    self.diagnostics.push(
                        Diagnostic::error(message)
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(base_expr.span, "unsupported family base"))
                            .with_note(note),
                    );
                }
                let family_owner = Type::Named(owner.clone());
                let methods = if supported {
                    super::method_sets::effective_base_methods(self, &base)
                } else {
                    Vec::new()
                }
                .into_iter()
                .map(|(member, method)| {
                    (member, lift_inherited_method(method, &base, &family_owner))
                })
                .collect();
                self.type_definitions.insert(name.clone(), base.clone());
                self.named_families.insert(
                    owner.clone(),
                    NamedFamilyType {
                        owner,
                        data: Row {
                            entries: Vec::new(),
                            tail: RowTail::Closed,
                        },
                        defaulted_fields: HashSet::new(),
                        primitive_base: Some(base),
                        methods,
                    },
                );
                continue;
            }

            let ExprKind::Record(entries) = &ungroup_expr(&value).kind else {
                continue;
            };
            let mut data_entries = Vec::new();
            let mut labels = HashSet::new();
            let mut defaulted_fields = HashSet::new();
            for entry in entries {
                let (field, span, annotation) = match entry {
                    RecordEntry::Field {
                        name,
                        name_span,
                        value,
                        overwrite: false,
                        ..
                    } => (name, *name_span, value),
                    RecordEntry::FieldDefault {
                        name,
                        name_span,
                        annotation,
                        ..
                    } => {
                        defaulted_fields.insert(name.clone());
                        (name, *name_span, annotation)
                    }
                    _ => continue,
                };
                if !labels.insert(field.clone()) {
                    self.report_duplicate_row_label(
                        field,
                        span,
                        DuplicateRowLabelContext::RecordAdd,
                    );
                    continue;
                }
                data_entries.push(RowEntry::Field {
                    name: field.clone(),
                    ty: self.lower_annotation(annotation),
                });
            }
            let data = Row {
                entries: data_entries,
                tail: RowTail::Closed,
            };
            self.type_definitions
                .insert(name.clone(), Type::Record(data.clone()));
            self.named_families.insert(
                owner.clone(),
                NamedFamilyType {
                    owner,
                    data,
                    defaulted_fields,
                    primitive_base: None,
                    methods: HashMap::new(),
                },
            );
        }
    }

    fn named_family_owner_key(&self, name: &str) -> String {
        let module = match &self.module_identity {
            comptime::ComptimeModuleIdentity::Current => "current".to_owned(),
            comptime::ComptimeModuleIdentity::Path(path) => format!("path:{}", path.display()),
            comptime::ComptimeModuleIdentity::Specifier(specifier) => {
                format!("specifier:{specifier}")
            }
        };
        format!("\0aven.named-family:{module}\0{name}")
    }

    fn canonicalize_named_family_aliases(&mut self, module: &Module) {
        loop {
            let mut changed = false;
            for declaration in collect_declarations(module) {
                if declaration.phase != DeclarationPhase::Comptime
                    || self.named_family_aliases.contains_key(&declaration.name)
                {
                    continue;
                }
                let Some(binding) = binding_for_declaration(module, &declaration) else {
                    continue;
                };
                let (ExprKind::Name(target) | ExprKind::ComptimeName(target)) =
                    &ungroup_expr(&binding.value).kind
                else {
                    continue;
                };
                let Some(owner) = self.named_family_aliases.get(target).cloned() else {
                    continue;
                };
                self.named_family_aliases
                    .insert(declaration.name.clone(), owner.clone());
                self.type_definitions
                    .insert(declaration.name.clone(), Type::Named(owner));
                changed = true;
            }
            if !changed {
                break;
            }
        }
    }

    fn lower_named_family_methods(&mut self, module: &Module) {
        let providers = collect_declarations(module)
            .into_iter()
            .filter_map(|declaration| {
                let owner = self.named_family_aliases.get(&declaration.name)?;
                let binding = binding_for_declaration(module, &declaration)?;
                is_named_family_provider(&binding.value).then_some((
                    declaration.name,
                    owner.clone(),
                    binding.value.clone(),
                ))
            })
            .collect::<Vec<_>>();

        for (source_name, owner, value) in providers {
            self.provider_owner_scopes.push(source_name);
            let Some(entries) = named_family_provider_entries(&value) else {
                self.provider_owner_scopes.pop();
                continue;
            };
            let data_labels = self
                .named_families
                .get(&owner)
                .map(|family| {
                    family
                        .data
                        .entries
                        .iter()
                        .filter_map(|entry| match entry {
                            RowEntry::Field { name, .. } => Some(name.clone()),
                            RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
                        })
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            let mut methods = self
                .named_families
                .get(&owner)
                .map(|family| family.methods.clone())
                .unwrap_or_default();
            let mut local_names = HashSet::new();
            for entry in entries {
                let RecordEntry::Method {
                    name,
                    name_span,
                    value,
                    ..
                } = entry
                else {
                    continue;
                };
                let ExprKind::Lambda {
                    params,
                    return_annotation: Some(result),
                    requirements,
                    ..
                } = &ungroup_expr(value).kind
                else {
                    continue;
                };
                if name.chars().next().is_some_and(char::is_uppercase) {
                    self.diagnostics.push(
                        Diagnostic::error("method names must be lowercase or operator tokens")
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(*name_span, "uppercase method name")),
                    );
                    continue;
                }
                if data_labels.contains(name) {
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "method `{name}` conflicts with a data field of the same name"
                        ))
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(*name_span, "duplicate member name")),
                    );
                    continue;
                }
                if !local_names.insert(name.clone()) {
                    self.diagnostics.push(
                        Diagnostic::error(format!("duplicate method `{name}`"))
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(*name_span, "method already declared")),
                    );
                    continue;
                }
                self.rigid_type_var_scopes.push(HashSet::new());
                let mut param_types = Vec::with_capacity(params.len());
                let mut complete = true;
                for param in params {
                    let Some(annotation) = &param.annotation else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "provider method parameters must be explicitly typed",
                            )
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(param.name_span, "missing parameter type")),
                        );
                        complete = false;
                        continue;
                    };
                    let lowered = self.lower_annotation(annotation);
                    param_types.push(self.normalize(&lowered));
                }
                if !complete {
                    self.rigid_type_var_scopes.pop();
                    continue;
                }
                let lowered_result = self.lower_annotation(result);
                let result = self.normalize(&lowered_result);
                let constraints = self
                    .requirement_predicates(requirements)
                    .into_iter()
                    .map(|predicate| MethodConstraint {
                        candidate: predicate.candidate,
                        member: predicate.member,
                        params: predicate.params,
                        result: predicate.result,
                    })
                    .collect::<Vec<_>>();
                let variables = named_method_variables(&param_types, &result, &constraints);
                self.rigid_type_var_scopes.pop();
                let mut signature = NamedMethodType {
                    params: param_types,
                    result,
                    constraints,
                    variables,
                    origin: NamedMethodOrigin::Declared,
                };
                if let Some(inherited) = methods
                    .get(name)
                    .filter(|method| matches!(method.origin, NamedMethodOrigin::Inherited { .. }))
                {
                    let (base_owner, base_member) = match &inherited.origin {
                        NamedMethodOrigin::Inherited {
                            base_owner,
                            base_member,
                            ..
                        } => (base_owner.clone(), base_member.clone()),
                        NamedMethodOrigin::Declared | NamedMethodOrigin::Override { .. } => {
                            unreachable!("filtered inherited method")
                        }
                    };
                    if named_method_schemes_alpha_equivalent(inherited, &signature) {
                        signature.origin = NamedMethodOrigin::Override {
                            base_owner,
                            base_member,
                        };
                    } else {
                        self.diagnostics.push(
                            Diagnostic::error(format!(
                                "`{name}` collides with inherited `{}.{base_member}`",
                                base_owner.render(),
                            ))
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(
                                *name_span,
                                "override signature does not match",
                            ))
                            .with_note(format!(
                                "the inherited family signature is `{}`",
                                render_named_method_signature(inherited)
                            ))
                            .with_note(format!(
                                "the declaration has `{}`",
                                render_named_method_signature(&signature)
                            ))
                            .with_note(
                                "an override must keep the inherited signature; rename this method or fix it",
                            ),
                        );
                    }
                }
                methods.insert(name.clone(), signature);
            }
            self.provider_owner_scopes.pop();
            if let Some(family) = self.named_families.get_mut(&owner) {
                family.methods = methods;
            }
        }
    }

    fn lower_builtin_method_attachments(&mut self, module: &Module) {
        for item in &module.items {
            let Item::MethodAttachment(attachment) = item else {
                continue;
            };
            if !self.trusted_builtin_method_source {
                self.diagnostics.push(
                    Diagnostic::error(
                        "only trusted ambient modules may attach methods to builtin types",
                    )
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(
                        attachment.owner.span,
                        "untrusted builtin method attachment",
                    ))
                    .with_note(
                        "builtin method sets are sealed after the host's ambient roots are checked",
                    ),
                );
                continue;
            }

            let Some((owner, owner_variables)) =
                self.lower_builtin_owner_pattern(&attachment.owner)
            else {
                continue;
            };
            let owner_variable_set = owner_variables.iter().cloned().collect::<HashSet<_>>();
            self.rigid_type_var_scopes.push(owner_variable_set);
            for member in &attachment.members {
                let RecordEntry::Method {
                    name,
                    name_span,
                    value,
                    ..
                } = member
                else {
                    continue;
                };
                let ExprKind::Lambda {
                    params,
                    return_annotation: Some(result),
                    requirements,
                    ..
                } = &ungroup_expr(value).kind
                else {
                    continue;
                };

                let mut complete = true;
                let params = params
                    .iter()
                    .filter_map(|param| {
                        let Some(annotation) = &param.annotation else {
                            self.diagnostics.push(
                                Diagnostic::error(
                                    "builtin method parameters must be explicitly typed",
                                )
                                .with_code(codes::ty::MISMATCH)
                                .with_label(Label::primary(
                                    param.name_span,
                                    "missing parameter type",
                                )),
                            );
                            complete = false;
                            return None;
                        };
                        let lowered = self.lower_annotation(annotation);
                        Some(self.normalize(&lowered))
                    })
                    .collect::<Vec<_>>();
                if !complete {
                    continue;
                }
                let lowered_result = self.lower_annotation(result);
                let result = self.normalize(&lowered_result);
                if let Some(variables) = self.rigid_type_var_scopes.last_mut() {
                    for ty in params.iter().chain(std::iter::once(&result)) {
                        variables.extend(type_variable_names(ty));
                    }
                }
                let constraints = self
                    .requirement_predicates(requirements)
                    .into_iter()
                    .map(|predicate| MethodConstraint {
                        candidate: predicate.candidate,
                        member: predicate.member,
                        params: predicate.params,
                        result: predicate.result,
                    })
                    .collect::<Vec<_>>();
                let entry = BuiltinMethodType {
                    owner: owner.clone(),
                    owner_variables: owner_variables.clone(),
                    member: name.clone(),
                    params,
                    result,
                    constraints,
                    owner_span: attachment.owner.span,
                    member_span: *name_span,
                };
                if self.builtin_method_collides(&entry) {
                    continue;
                }
                self.builtin_methods.extend([entry.clone()]);
                self.local_builtin_methods.push(entry);
            }
            self.rigid_type_var_scopes.pop();
        }
    }

    fn lower_builtin_owner_pattern(&mut self, owner: &Expr) -> Option<(Type, Vec<String>)> {
        let lowered_owner = self.lower_annotation(owner);
        let lowered = self.normalize(&lowered_owner);
        let (head, args) = match &lowered {
            Type::Named(head) => (head.as_str(), &[][..]),
            Type::Apply { callee, args } => {
                let Type::Named(head) = callee.as_ref() else {
                    self.report_invalid_builtin_owner_pattern(owner.span);
                    return None;
                };
                (head.as_str(), args.as_slice())
            }
            _ => {
                self.report_invalid_builtin_owner_pattern(owner.span);
                return None;
            }
        };
        let Some(arity) = builtin_owner_arity(head) else {
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "`{head}` is not a compiler-known builtin method owner"
                ))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(owner.span, "unsupported builtin owner")),
            );
            return None;
        };
        if args.len() != arity {
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "builtin owner `{head}` expects {arity} type argument(s)"
                ))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    owner.span,
                    format!("found {} argument(s)", args.len()),
                )),
            );
            return None;
        }
        let mut variables = type_variable_names(&lowered)
            .into_iter()
            .collect::<Vec<_>>();
        variables.sort();
        Some((lowered, variables))
    }

    fn report_invalid_builtin_owner_pattern(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("builtin method attachments require a builtin owner pattern")
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, "invalid owner pattern"))
                .with_note(
                    "write a pattern such as `Array(a)`, `Array(Int)`, or `Array(Array(a))`",
                ),
        );
    }

    fn builtin_method_collides(&mut self, candidate: &BuiltinMethodType) -> bool {
        let collision = self
            .builtin_methods
            .methods()
            .iter()
            .find(|entry| {
                entry.member == candidate.member
                    && builtin_owner_patterns_overlap(&entry.owner, &candidate.owner)
            })
            .cloned();
        if let Some(existing) = collision {
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "builtin method `{}` has overlapping owner patterns",
                    candidate.member
                ))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    candidate.member_span,
                    "colliding method declaration",
                ))
                .with_label(Label::primary(existing.member_span, "previous declaration")),
            );
            return true;
        }

        if intrinsic_builtin_method_collides(&candidate.owner, &candidate.member) {
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "builtin method `{}` conflicts with an intrinsic method",
                    candidate.member
                ))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(
                    candidate.member_span,
                    "intrinsic member already exists",
                )),
            );
            return true;
        }
        false
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
            if name.chars().next().is_some_and(char::is_uppercase)
                && binding_for_declaration(module, &declaration).is_some_and(|binding| {
                    is_method_requirement_row(&binding.value)
                        || aven_parser::is_named_method_provider(&binding.value)
                })
            {
                continue;
            }

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
                    Some(self.declared_method_scheme(&name, &annotation)),
                );
            } else if self.zero_argument_type_bindings.contains(&name)
                && let Some(definition) = self.type_definitions.get(&name).cloned()
                && !matches!(definition, Type::Deferred)
                && (matches!(definition, Type::Recursive(_))
                    || binding_for_declaration(module, &declaration).is_some_and(|binding| {
                        matches!(&ungroup_expr(&binding.value).kind, ExprKind::Call { .. })
                    }))
            {
                // Recursive identities and call-produced type bindings have
                // already gone through specialization. Reuse that result
                // instead of recursively inferring the RHS again. Plain value
                // records and unsupported RHS forms keep ordinary inference.
                let scheme = scheme_from_global(&definition, &mut self.unifier);
                self.memo.insert(name.clone(), scheme.clone());
                types.insert(name.clone(), Some(scheme));
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
                Item::MethodAttachment(attachment) => {
                    self.check_builtin_method_attachment(attachment);
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
        // A body-bearing method record defines a named-family *provider* only
        // when it declares a type (uppercase name with a canonical owner). A
        // lowercase binding with method bodies is a direct slot-record
        // initializer, checked against its declared slot-record annotation.
        if let Some(binding) = binding
            && is_named_family_provider(&binding.value)
            && let Some(owner) = self.named_family_aliases.get(&declaration.name).cloned()
        {
            self.check_named_family_declaration(&owner, &binding.value);
            return;
        }
        if declaration
            .name
            .chars()
            .next()
            .is_some_and(char::is_uppercase)
            && let Some(binding) = binding
            && is_method_requirement_row(&binding.value)
        {
            self.validate_named_requirement(&binding.value);
            return;
        }
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
            self.record_scheme_type(declaration.name_span, &scheme);
        }

        if !checked_value && let Some(binding) = binding {
            let diagnostics_start = self.diagnostics.len();
            if declaration.phase == DeclarationPhase::Comptime
                && !self.is_uppercase_comptime_function(&declaration.name, &binding.value)
            {
                self.check_value_expr_without_unbound_names(&binding.value);
            } else {
                if self.is_uppercase_comptime_function(&declaration.name, &binding.value) {
                    let Some((params, _)) = lambda_parts(&binding.value) else {
                        unreachable!("uppercase comptime functions are lambdas")
                    };
                    let mut comptime_params = params.to_vec();
                    for param in &mut comptime_params {
                        param.comptime = true;
                    }
                    self.push_local_comptime_param_scope(&comptime_params);
                    self.check_value_expr(&binding.value);
                    self.local_comptime_params.pop();
                } else {
                    self.check_value_expr(&binding.value);
                }
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

    fn check_named_family_declaration(&mut self, owner: &str, value: &Expr) {
        let Some(entries) = named_family_provider_entries(value) else {
            return;
        };
        let family = self.named_families.get(owner).cloned();
        let primitive = family
            .as_ref()
            .and_then(|family| family.primitive_base.as_ref())
            .is_some();
        for entry in entries {
            match entry {
                RecordEntry::Method {
                    name,
                    name_span,
                    value,
                    ..
                } => match &ungroup_expr(value).kind {
                    ExprKind::Arrow { .. } => self.diagnostics.push(
                        Diagnostic::error(format!(
                            "instance-carried method slot `{name}` is not supported"
                        ))
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(
                            *name_span,
                            "method signature has no shared implementation",
                        ))
                        .with_note("add `=> body` to declare a type-carried provider method"),
                    ),
                    ExprKind::Lambda {
                        params,
                        requirements,
                        body,
                        ..
                    } => {
                        let Some(signature) = family
                            .as_ref()
                            .and_then(|family| family.methods.get(name))
                            .cloned()
                        else {
                            continue;
                        };
                        self.rigid_type_var_scopes
                            .push(signature.variables.iter().cloned().collect());
                        let assumptions = self.requirement_predicates(requirements);
                        self.push_method_assumptions(assumptions);
                        self.local_types.push();
                        self.local_types.define(
                            aven_parser::METHOD_RECEIVER_NAME,
                            LocalValueType::Known(Type::Named(owner.to_owned())),
                        );
                        for (param, ty) in params.iter().zip(&signature.params) {
                            self.local_types
                                .define(&param.name, LocalValueType::Known(ty.clone()));
                            self.record_inferred_type(param.name_span, ty.clone());
                        }
                        let marker = self.method_obligation_marker();
                        self.check_value_against(&signature.result, body);
                        self.finish_non_generalizing_lambda_obligations(marker);
                        self.local_types.pop();
                        self.pop_method_assumptions();
                        self.rigid_type_var_scopes.pop();
                    }
                    _ => {}
                },
                RecordEntry::FieldDefault {
                    name,
                    annotation,
                    default,
                    ..
                } => {
                    if primitive {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "primitive-base family declarations contain only methods",
                            )
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(
                                annotation.span,
                                "data fields are not supported on a primitive payload",
                            )),
                        );
                        continue;
                    }
                    let lowered = self.lower_annotation(annotation);
                    let ty = self.normalize(&lowered);
                    self.check_value_against(&ty, default);
                    let _ = name;
                }
                RecordEntry::Field {
                    overwrite: false,
                    name_span,
                    ..
                } if primitive => self.diagnostics.push(
                    Diagnostic::error("primitive-base family declarations contain only methods")
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(
                            *name_span,
                            "data fields are not supported on a primitive payload",
                        )),
                ),
                RecordEntry::Field {
                    overwrite: false, ..
                } => {}
                RecordEntry::Field {
                    name, name_span, ..
                } => self.diagnostics.push(
                    Diagnostic::error(format!(
                        "per-instance override `{name}` is not supported in a provider"
                    ))
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(*name_span, "override member")),
                ),
                RecordEntry::Spread { span, .. }
                | RecordEntry::Delete { span, .. }
                | RecordEntry::DeleteComputed { span, .. }
                | RecordEntry::Rename { span, .. }
                | RecordEntry::Iteration { span, .. }
                | RecordEntry::Open { span }
                | RecordEntry::FieldComputed { span, .. }
                | RecordEntry::Shorthand { span, .. } => self.diagnostics.push(
                    Diagnostic::error("method-bearing record declarations must be closed")
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(
                            *span,
                            "transform, spread, or open-row entry is not supported here",
                        ))
                        .with_note("write a closed list of data fields and body-bearing methods"),
                ),
                RecordEntry::Element(expr) => self.diagnostics.push(
                    Diagnostic::error(
                        "method-bearing record declarations cannot contain value entries",
                    )
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(expr.span, "unsupported provider entry")),
                ),
            }
        }
    }

    fn check_builtin_method_attachment(&mut self, attachment: &aven_parser::MethodAttachment) {
        if !self.trusted_builtin_method_source {
            return;
        }
        let Some(entry_owner) = self
            .local_builtin_methods
            .iter()
            .find(|entry| entry.owner_span == attachment.owner.span)
            .map(|entry| entry.owner.clone())
        else {
            return;
        };

        for member in &attachment.members {
            let RecordEntry::Method {
                name,
                name_span,
                value,
                ..
            } = member
            else {
                self.diagnostics.push(
                    Diagnostic::error("builtin method attachments contain only method members")
                        .with_code(codes::ty::MISMATCH)
                        .with_label(Label::primary(
                            builtin_attachment_member_span(member),
                            "unsupported attachment member",
                        )),
                );
                continue;
            };
            let ExprKind::Lambda {
                params,
                requirements,
                body,
                ..
            } = &ungroup_expr(value).kind
            else {
                self.diagnostics.push(
                    Diagnostic::error(format!(
                        "builtin method slot `{name}` requires a source implementation"
                    ))
                    .with_code(codes::ty::MISMATCH)
                    .with_label(Label::primary(*name_span, "method signature has no body")),
                );
                continue;
            };
            let Some(signature) = self
                .local_builtin_methods
                .iter()
                .find(|entry| entry.member_span == *name_span)
                .cloned()
            else {
                continue;
            };

            let mut variables = type_variable_names(&entry_owner);
            for ty in signature
                .params
                .iter()
                .chain(std::iter::once(&signature.result))
            {
                variables.extend(type_variable_names(ty));
            }
            self.rigid_type_var_scopes.push(variables);
            let assumptions = self.requirement_predicates(requirements);
            self.push_method_assumptions(assumptions);
            self.local_types.push();
            self.local_types.define(
                aven_parser::METHOD_RECEIVER_NAME,
                LocalValueType::Known(entry_owner.clone()),
            );
            for (param, ty) in params.iter().zip(&signature.params) {
                self.local_types
                    .define(&param.name, LocalValueType::Known(ty.clone()));
                self.record_inferred_type(param.name_span, ty.clone());
            }
            let marker = self.method_obligation_marker();
            self.check_value_against(&signature.result, body);
            self.finish_non_generalizing_lambda_obligations(marker);
            self.local_types.pop();
            self.pop_method_assumptions();
            self.rigid_type_var_scopes.pop();
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
                MergedItem::MethodAttachment(attachment) => {
                    self.diagnostics.push(
                        Diagnostic::error("builtin method attachments are top-level declarations")
                            .with_code(codes::ty::MISMATCH)
                            .with_label(Label::primary(attachment.span, "nested attachment")),
                    );
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
            let obligation_marker = self.method_obligation_marker();
            let inferred = self.infer(&env, &binding.value);
            let resolved = self.resolve_and_default(&inferred);
            let env_metas = self.local_types.free_metas(|ty| self.unifier.resolve(ty));
            let env_row_vars = self
                .local_types
                .free_row_vars(|ty| self.unifier.resolve(ty));
            let scheme = self.generalize_method_obligations(
                resolved,
                &env_metas,
                &env_row_vars,
                obligation_marker,
                Some(&binding.name),
            );
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
        pattern_local_types(self.pattern_type_context(), pattern, Some(&resolved))
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
                | RecordEntry::Method { .. }
                | RecordEntry::FieldDefault { .. }
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

        if self
            .try_lower_comptime_annotation_for_eager_validation(value)
            .is_some()
        {
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
            | ExprKind::PrimitiveFamily { .. }
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
                | RecordEntry::Method { .. }
                | RecordEntry::FieldDefault { .. }
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
                self.record_scheme_type(name_span, scheme);
            }
            LocalValueType::Unknown => {}
        }
    }

    pub(super) fn record_inferred_type(&mut self, name_span: Span, ty: Type) {
        if type_contains_deferred(&ty) {
            return;
        }

        self.inferred_types.push(InferredType {
            name_span,
            ty,
            qualified: None,
        });
    }

    pub(super) fn record_synthesized_type(&mut self, name_span: Span, ty: &Type) {
        self.record_inferred_type(name_span, display_inferred_type(ty));
    }

    pub(super) fn record_scheme_type(&mut self, name_span: Span, scheme: &TypeScheme) {
        if type_contains_deferred(&scheme.ty) {
            return;
        }
        self.inferred_types.push(InferredType {
            name_span,
            ty: display_inferred_type(&scheme.ty),
            qualified: (!scheme.predicates.is_empty()).then(|| render_type_scheme(scheme)),
        });
    }

    pub(super) fn top_level_binding_final_type(&mut self, name: &str) -> Option<Type> {
        let scheme = self.memo.get(name).cloned()?;
        let (ty, _) = self.unifier.instantiate_scheme(&scheme);
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

fn builtin_owner_arity(name: &str) -> Option<usize> {
    match name {
        "Array" | "Set" => Some(1),
        "Map" | "Result" => Some(2),
        "Bool" | "Float" | "Int" | "Text" => Some(0),
        _ => None,
    }
}

fn builtin_attachment_member_span(member: &RecordEntry) -> Span {
    match member {
        RecordEntry::Field { span, .. }
        | RecordEntry::FieldComputed { span, .. }
        | RecordEntry::Method { span, .. }
        | RecordEntry::FieldDefault { span, .. }
        | RecordEntry::Shorthand { span, .. }
        | RecordEntry::Spread { span, .. }
        | RecordEntry::Delete { span, .. }
        | RecordEntry::DeleteComputed { span, .. }
        | RecordEntry::Rename { span, .. }
        | RecordEntry::Iteration { span, .. }
        | RecordEntry::Open { span } => *span,
        RecordEntry::Element(expr) => expr.span,
    }
}

fn builtin_owner_patterns_overlap(left: &Type, right: &Type) -> bool {
    match (left, right) {
        (Type::Variable(_), _) | (_, Type::Variable(_)) => true,
        (Type::Named(left), Type::Named(right)) => left == right,
        (
            Type::Apply {
                callee: left_callee,
                args: left_args,
            },
            Type::Apply {
                callee: right_callee,
                args: right_args,
            },
        ) => {
            left_args.len() == right_args.len()
                && builtin_owner_patterns_overlap(left_callee, right_callee)
                && left_args
                    .iter()
                    .zip(right_args)
                    .all(|(left, right)| builtin_owner_patterns_overlap(left, right))
        }
        (Type::Optional(left), Type::Optional(right))
        | (Type::Nullable(left), Type::Nullable(right)) => {
            builtin_owner_patterns_overlap(left, right)
        }
        (Type::Tuple(left), Type::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| builtin_owner_patterns_overlap(left, right))
        }
        _ => left == right,
    }
}

fn intrinsic_builtin_method_collides(owner: &Type, member: &str) -> bool {
    let head = match owner {
        Type::Named(name) => name.as_str(),
        Type::Apply { callee, .. } => match callee.as_ref() {
            Type::Named(name) => name.as_str(),
            _ => return false,
        },
        _ => return false,
    };
    match head {
        "Array" => {
            crate::ty::ARRAY_METHOD_NAMES.contains(&member)
                || (member == "joinWith"
                    && builtin_owner_patterns_overlap(
                        owner,
                        &Type::Apply {
                            callee: Box::new(Type::Named("Array".to_owned())),
                            args: vec![Type::Named("Text".to_owned())],
                        },
                    ))
        }
        "Map" => crate::ty::MAP_METHOD_NAMES.contains(&member),
        "Set" => crate::ty::SET_METHOD_NAMES.contains(&member),
        "Text" => crate::ty::TEXT_METHOD_NAMES.contains(&member),
        "Bool" | "Float" | "Int" => {
            super::method_sets::builtin_method_signature(owner, member).is_some()
        }
        _ => false,
    }
}

fn is_named_family_provider(value: &Expr) -> bool {
    aven_parser::is_named_method_provider(value) || aven_parser::is_primitive_family_provider(value)
}

fn primitive_family_base_is_supported(base: &Type) -> bool {
    match base {
        Type::Named(name) => matches!(name.as_str(), "Int" | "Float" | "Text" | "Bool"),
        Type::Apply { callee, args } => {
            let Type::Named(name) = callee.as_ref() else {
                return false;
            };
            let expected_arity = match name.as_str() {
                "Array" | "Set" => 1,
                "Map" => 2,
                _ => return false,
            };
            args.len() == expected_arity && is_concrete_type(base)
        }
        _ => false,
    }
}

fn named_family_provider_entries(value: &Expr) -> Option<&[RecordEntry]> {
    if let Some((_, members)) = aven_parser::primitive_family_parts(value) {
        return Some(members);
    }
    let ExprKind::Record(entries) = &ungroup_expr(value).kind else {
        return None;
    };
    aven_parser::is_named_method_provider(value).then_some(entries)
}

fn lift_inherited_method(
    mut method: NamedMethodType,
    base: &Type,
    family: &Type,
) -> NamedMethodType {
    let lifted_params = method.params.iter().map(|param| param == base).collect();
    let lifted_result = method.result == *base;
    method.params = method
        .params
        .into_iter()
        .map(|param| {
            if param == *base {
                family.clone()
            } else {
                param
            }
        })
        .collect();
    method.result = if method.result == *base {
        family.clone()
    } else {
        method.result
    };
    for constraint in &mut method.constraints {
        root_lift_type(&mut constraint.candidate, base, family);
        for param in &mut constraint.params {
            root_lift_type(param, base, family);
        }
        root_lift_type(&mut constraint.result, base, family);
    }
    let base_member = match method.origin {
        NamedMethodOrigin::Inherited { base_member, .. } => base_member,
        NamedMethodOrigin::Declared | NamedMethodOrigin::Override { .. } => {
            unreachable!("effective base methods are inherited entries")
        }
    };
    method.origin = NamedMethodOrigin::Inherited {
        base_owner: base.clone(),
        base_member,
        lifted_params,
        lifted_result,
    };
    method
}

fn root_lift_type(ty: &mut Type, base: &Type, family: &Type) {
    if ty == base {
        *ty = family.clone();
    }
}

fn named_method_variables(
    params: &[Type],
    result: &Type,
    constraints: &[MethodConstraint],
) -> Vec<String> {
    let mut variables = HashSet::new();
    for ty in params.iter().chain(std::iter::once(result)) {
        variables.extend(type_variable_names(ty));
    }
    for constraint in constraints {
        variables.extend(type_variable_names(&constraint.candidate));
        for ty in constraint
            .params
            .iter()
            .chain(std::iter::once(&constraint.result))
        {
            variables.extend(type_variable_names(ty));
        }
    }
    let mut variables = variables.into_iter().collect::<Vec<_>>();
    variables.sort();
    variables
}

fn named_method_schemes_alpha_equivalent(
    inherited: &NamedMethodType,
    declared: &NamedMethodType,
) -> bool {
    if inherited.params.len() != declared.params.len()
        || inherited.variables.len() != declared.variables.len()
        || inherited.constraints.len() != declared.constraints.len()
    {
        return false;
    }
    let mut forward = HashMap::new();
    let mut reverse = HashMap::new();
    if !inherited
        .params
        .iter()
        .zip(&declared.params)
        .all(|(left, right)| alpha_equivalent_type(left, right, &mut forward, &mut reverse))
        || !alpha_equivalent_type(
            &inherited.result,
            &declared.result,
            &mut forward,
            &mut reverse,
        )
    {
        return false;
    }

    let mut unmatched = declared.constraints.iter().collect::<Vec<_>>();
    for constraint in &inherited.constraints {
        let Some((index, next_forward, next_reverse)) =
            unmatched.iter().enumerate().find_map(|(index, candidate)| {
                let mut candidate_forward = forward.clone();
                let mut candidate_reverse = reverse.clone();
                alpha_equivalent_constraint(
                    constraint,
                    candidate,
                    &mut candidate_forward,
                    &mut candidate_reverse,
                )
                .then_some((index, candidate_forward, candidate_reverse))
            })
        else {
            return false;
        };
        unmatched.remove(index);
        forward = next_forward;
        reverse = next_reverse;
    }
    true
}

fn alpha_equivalent_constraint(
    left: &MethodConstraint,
    right: &MethodConstraint,
    forward: &mut HashMap<String, String>,
    reverse: &mut HashMap<String, String>,
) -> bool {
    left.member == right.member
        && left.params.len() == right.params.len()
        && alpha_equivalent_type(&left.candidate, &right.candidate, forward, reverse)
        && left
            .params
            .iter()
            .zip(&right.params)
            .all(|(left, right)| alpha_equivalent_type(left, right, forward, reverse))
        && alpha_equivalent_type(&left.result, &right.result, forward, reverse)
}

fn alpha_equivalent_type(
    left: &Type,
    right: &Type,
    forward: &mut HashMap<String, String>,
    reverse: &mut HashMap<String, String>,
) -> bool {
    match (left, right) {
        (Type::Variable(left), Type::Variable(right)) => {
            if let Some(bound) = forward.get(left) {
                return bound == right;
            }
            if reverse.contains_key(right) {
                return false;
            }
            forward.insert(left.clone(), right.clone());
            reverse.insert(right.clone(), left.clone());
            true
        }
        (
            Type::Apply {
                callee: left_callee,
                args: left_args,
            },
            Type::Apply {
                callee: right_callee,
                args: right_args,
            },
        ) => {
            left_args.len() == right_args.len()
                && alpha_equivalent_type(left_callee, right_callee, forward, reverse)
                && left_args
                    .iter()
                    .zip(right_args)
                    .all(|(left, right)| alpha_equivalent_type(left, right, forward, reverse))
        }
        (
            Type::Function {
                params: left_params,
                result: left_result,
                required: left_required,
            },
            Type::Function {
                params: right_params,
                result: right_result,
                required: right_required,
            },
        ) => {
            left_required == right_required
                && left_params.len() == right_params.len()
                && left_params
                    .iter()
                    .zip(right_params)
                    .all(|(left, right)| alpha_equivalent_type(left, right, forward, reverse))
                && alpha_equivalent_type(left_result, right_result, forward, reverse)
        }
        (Type::Optional(left), Type::Optional(right))
        | (Type::Nullable(left), Type::Nullable(right)) => {
            alpha_equivalent_type(left, right, forward, reverse)
        }
        (Type::Tuple(left), Type::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| alpha_equivalent_type(left, right, forward, reverse))
        }
        (Type::Record(left), Type::Record(right)) | (Type::Variant(left), Type::Variant(right)) => {
            alpha_equivalent_row(left, right, forward, reverse)
        }
        (
            Type::SlotRecord {
                data: left_data,
                slots: left_slots,
            },
            Type::SlotRecord {
                data: right_data,
                slots: right_slots,
            },
        ) => {
            alpha_equivalent_row(left_data, right_data, forward, reverse)
                && alpha_equivalent_row(left_slots, right_slots, forward, reverse)
        }
        _ => left == right,
    }
}

fn alpha_equivalent_row(
    left: &Row,
    right: &Row,
    forward: &mut HashMap<String, String>,
    reverse: &mut HashMap<String, String>,
) -> bool {
    left.tail == right.tail
        && left.entries.len() == right.entries.len()
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| match (left, right) {
                (
                    RowEntry::Field {
                        name: left_name,
                        ty: left,
                    },
                    RowEntry::Field {
                        name: right_name,
                        ty: right,
                    },
                ) => {
                    left_name == right_name && alpha_equivalent_type(left, right, forward, reverse)
                }
                (
                    RowEntry::Tag {
                        name: left_name,
                        payload: left,
                    },
                    RowEntry::Tag {
                        name: right_name,
                        payload: right,
                    },
                ) => {
                    left_name == right_name
                        && left.len() == right.len()
                        && left.iter().zip(right).all(|(left, right)| {
                            alpha_equivalent_type(left, right, forward, reverse)
                        })
                }
                (RowEntry::Literal { value: left }, RowEntry::Literal { value: right }) => {
                    left == right
                }
                _ => false,
            })
}

fn render_named_method_signature(method: &NamedMethodType) -> String {
    let params = method
        .params
        .iter()
        .map(Type::render)
        .collect::<Vec<_>>()
        .join(", ");
    format!("({params}): {}", method.result.render())
}
