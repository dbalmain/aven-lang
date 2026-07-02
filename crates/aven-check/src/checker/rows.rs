use super::*;

impl<'a> Checker<'a> {
    pub(super) fn lower_row_entries(&mut self, entries: &[RecordEntry], kind: RowKind) -> Type {
        let row = match self.fold_row_entries(entries, kind, RowFoldMode::Annotation) {
            Ok(row) => row,
            Err(()) => return Type::Deferred,
        };

        match kind {
            RowKind::Record => Type::Record(row),
            RowKind::Variant => Type::Variant(row),
        }
    }

    pub(super) fn infer_record_entries(&mut self, env: &TypeEnv, entries: &[RecordEntry]) -> Type {
        let row = match self.fold_row_entries(entries, RowKind::Record, RowFoldMode::Value { env })
        {
            Ok(row) => row,
            Err(()) => return Type::Deferred,
        };

        Type::Record(row)
    }

    pub(super) fn fold_row_entries(
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

    pub(super) fn fold_row_entry(
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

    pub(super) fn delete_closed_row_label(
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

    pub(super) fn fold_iteration_entry(
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

    pub(super) fn fold_unrolled_iteration_body(
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

    pub(super) fn fold_record_element(
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

    pub(super) fn fold_field_type(&mut self, value: &Expr, mode: RowFoldMode<'_>) -> Type {
        match mode {
            RowFoldMode::Annotation => self.lower_annotation(value),
            RowFoldMode::Value { env } => self.infer(env, value),
        }
    }

    pub(super) fn fold_expression(&mut self, value: &Expr, mode: RowFoldMode<'_>) -> Type {
        match mode {
            RowFoldMode::Annotation => self.lower_annotation(value),
            RowFoldMode::Value { env } => self.infer(env, value),
        }
    }

    pub(super) fn fold_spread_source(
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

    pub(super) fn fold_deferred_row_entry(
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

    pub(super) fn annotation_row_source(&self, ty: &Type, kind: RowKind) -> Option<RowSource> {
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

    pub(super) fn value_record_source(&mut self, ty: &Type) -> Option<RowSource> {
        let resolved = self.unifier.resolve(ty);
        match self.normalize(&resolved) {
            Type::Record(row) => Some(RowSource::from_row(row)),
            Type::Meta(_) => {
                let tail = self.unifier.fresh_row_var();
                let source = Type::Record(Row {
                    entries: Vec::new(),
                    tail: RowTail::Var(tail),
                });
                self.unifier.unify(&resolved, &source).ok()?;
                let Type::Record(row) = source else {
                    unreachable!("source was constructed as a record")
                };
                Some(RowSource::Open(row))
            }
            Type::Deferred
            | Type::Named(_)
            | Type::Variable(_)
            | Type::Apply { .. }
            | Type::Function { .. }
            | Type::Optional(_)
            | Type::Nullable(_)
            | Type::Tuple(_)
            | Type::Variant(_) => None,
        }
    }

    pub(super) fn merge_source_row(
        &mut self,
        row: &mut Row,
        source: RowSource,
        overwrite: bool,
        span: Span,
        kind: RowKind,
    ) -> Result<(), ()> {
        let source = match source {
            RowSource::Closed(row) | RowSource::Open(row) => row,
        };
        let source_tail = source.tail;

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

        row.tail = self.merge_row_tails(row.tail, source_tail, overwrite, span);

        Ok(())
    }

    pub(super) fn merge_optional_record_patch_field(
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

    pub(super) fn merge_row_tails(
        &mut self,
        accumulated: RowTail,
        incoming: RowTail,
        overwrite: bool,
        span: Span,
    ) -> RowTail {
        match (accumulated, incoming) {
            (tail, RowTail::Closed) | (RowTail::Closed, tail) => tail,
            (RowTail::Open, _) | (_, RowTail::Open) => RowTail::Open,
            (RowTail::Var(left), RowTail::Var(right)) if left == right => RowTail::Var(left),
            (RowTail::Var(left), RowTail::Var(right)) => {
                let result = self.unifier.fresh_row_merge(vec![
                    RowMergeSource {
                        row: Row {
                            entries: Vec::new(),
                            tail: RowTail::Var(left),
                        },
                        overwrite: false,
                        span,
                    },
                    RowMergeSource {
                        row: Row {
                            entries: Vec::new(),
                            tail: RowTail::Var(right),
                        },
                        overwrite,
                        span,
                    },
                ]);
                RowTail::Var(result)
            }
        }
    }

    pub(super) fn check_homogeneous_variant_entry(
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

    pub(super) fn lower_variant_tag(&mut self, tag: &Expr) -> Option<RowEntry> {
        match &tag.kind {
            ExprKind::Tag(name) => Some(RowEntry::Tag {
                name: name.clone(),
                payload: Vec::new(),
            }),
            ExprKind::Literal(
                literal @ (Literal::Bool(_) | Literal::Number(_) | Literal::String(_)),
            ) => Some(RowEntry::Literal {
                value: literal.clone(),
            }),
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
}
