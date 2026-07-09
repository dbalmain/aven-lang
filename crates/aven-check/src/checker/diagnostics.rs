use super::*;

impl<'a> Checker<'a> {
    pub(super) fn diagnostic_snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            diagnostics_len: self.diagnostics.len(),
            reported_unbound_name_spans: self.reported_unbound_name_spans.clone(),
            reported_import_spans: self.reported_import_spans.clone(),
            propagation_context_site_counts: self
                .propagation_contexts
                .iter()
                .map(|context| context.sites.len())
                .collect(),
        }
    }

    pub(super) fn restore_diagnostic_snapshot(&mut self, snapshot: DiagnosticSnapshot) {
        self.diagnostics.truncate(snapshot.diagnostics_len);
        self.reported_unbound_name_spans = snapshot.reported_unbound_name_spans;
        self.reported_import_spans = snapshot.reported_import_spans;
        for (context, site_count) in self
            .propagation_contexts
            .iter_mut()
            .zip(&snapshot.propagation_context_site_counts)
        {
            context.sites.truncate(*site_count);
        }
        self.propagation_contexts
            .truncate(snapshot.propagation_context_site_counts.len());
    }

    pub(super) fn push_type_mismatch_diagnostic(&mut self, diagnostic: Diagnostic) {
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

    pub(super) fn push_unique_diagnostic(&mut self, diagnostic: Diagnostic) {
        let code = diagnostic.code.as_deref();
        let primary_span = diagnostic.labels.first().map(|label| label.span);
        if code.is_some()
            && primary_span.is_some()
            && self.diagnostics.iter().any(|existing| {
                existing.code.as_deref() == code
                    && existing.labels.first().map(|label| label.span) == primary_span
            })
        {
            return;
        }

        self.diagnostics.push(diagnostic);
    }

    pub(super) fn deduplicate_diagnostics_since(&mut self, start: usize) {
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

    pub(super) fn report_dynamic_import(&mut self, span: Span) {
        if !self.report_import_once(span) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error("dynamic import is not supported yet")
                .with_code(codes::module::DYNAMIC_IMPORT)
                .with_label(Label::primary(
                    span,
                    "import specifier must be a static string literal",
                ))
                .with_note(
                    "Milestone Z1+Z2 only supports local relative imports with string literals",
                )
                .with_note("dynamic import is deferred to Milestone Z"),
        );
    }

    pub(super) fn report_unsupported_import_root(&mut self, specifier: &str, span: Span) {
        if !self.report_import_once(span) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::error(format!("unsupported import specifier `{specifier}`"))
                .with_code(codes::module::UNSUPPORTED_ROOT)
                .with_label(Label::primary(
                    span,
                    "this import root is not supported in this milestone",
                ))
                .with_note("use a local relative specifier beginning with `./` or `../`")
                .with_note("`$/`, `~/`, `//`, standard libraries, and packages are deferred to Milestone Z"),
        );
    }

    /// A static relative import checked without an injected imports map (the
    /// LSP's single-file context, or a bare `check_module` embedding). The
    /// specifier itself is fine, so this is a warning, not an error — the same
    /// file passes `aven check`, which loads the module graph.
    pub(super) fn report_unresolved_import(&mut self, specifier: &str, span: Span) {
        if !self.report_import_once(span) {
            return;
        }

        self.diagnostics.push(
            Diagnostic::warning(format!("import `{specifier}` is not resolved here"))
                .with_code(codes::module::UNRESOLVED_IMPORT)
                .with_label(Label::primary(
                    span,
                    "this context checks one file at a time, so the module's contents are unknown",
                ))
                .with_note("`aven check` and `aven run` resolve local relative imports")
                .with_note("editor cross-file import support arrives later in Milestone Z"),
        );
    }

    fn report_import_once(&mut self, span: Span) -> bool {
        self.reported_import_spans.insert(span)
    }

    pub(super) fn check_type_name(&mut self, name: &str, span: Span) {
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

    pub(super) fn report_lowercase_variant_tag(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("variant tag `{name}` must be an uppercase `@`-tag"))
                .with_code(codes::ty::LOWERCASE_VARIANT_TAG)
                .with_label(Label::primary(span, "lowercase variant tag"))
                .with_note("variant tags use uppercase names, for example `@Ok` or `@Err`"),
        );
    }

    pub(super) fn report_mixed_variant_entries(&mut self, incoming: VariantEntryKind, span: Span) {
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

    pub(super) fn report_non_liftable_into_runtime(&mut self, span: Span) {
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

    pub(super) fn report_comptime_evaluation_unsupported(&mut self, span: Span) {
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

    pub(super) fn report_unresolved_binding(&mut self, name_span: Span) {
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

    pub(super) fn report_spread_shape_unknown(&mut self, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error("record fields must be statically known for a block spread")
                .with_code(codes::ty::SPREAD_SHAPE_UNKNOWN)
                .with_label(Label::primary(
                    span,
                    "the record's fields must be statically known to open it into scope",
                ))
                .with_note(
                    "use a static import, a closed record literal or transform, or bind a closed record type before spreading it",
                ),
        );
    }

    pub(super) fn report_unsupported_uppercase_pattern_binder(&mut self, name: &str, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "extracting type `{name}` from a module is not supported yet"
            ))
            .with_code(codes::ty::UPPERCASE_PATTERN_BINDER_UNSUPPORTED)
            .with_label(Label::primary(
                span,
                "uppercase pattern binder would extract a type",
            ))
            .with_note("extracting types from modules is not supported yet; this is deferred to Milestone Z"),
        );
    }

    pub(super) fn report_duplicate_local_from_spread(&mut self, name: &str, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!("duplicate local binding `{name}`"))
                .with_code(codes::name::DUPLICATE_LOCAL)
                .with_label(Label::primary(
                    span,
                    format!("spread would introduce `{name}` here"),
                ))
                .with_note("remove or rename the field before spreading, or use `:..` inside a block to replace intentionally"),
        );
    }

    pub(super) fn report_duplicate_declaration_from_spread(&mut self, name: &str, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!("duplicate declaration `{name}`"))
                .with_code(codes::name::DUPLICATE_DECLARATION)
                .with_label(Label::primary(
                    span,
                    format!("spread would declare `{name}` here"),
                ))
                .with_note(
                    "remove or rename the field before spreading so top-level declarations stay unique",
                ),
        );
    }

    pub(super) fn report_propagate_needs_result(&mut self, span: Span) {
        self.push_unique_diagnostic(
            Diagnostic::error("function body uses `?^` but does not return a `Result`")
                .with_code(codes::ty::PROPAGATE_NEEDS_RESULT)
                .with_label(Label::primary(
                    span,
                    "this is the function's result, but `?^` requires it to be a Result",
                ))
                .with_note(
                    "wrap the final expression in `@Ok(...)`, or handle the errors instead of propagating them",
                ),
        );
    }

    pub(super) fn report_propagate_not_result_if_concrete(&mut self, ty: &Type, span: Span) {
        if result_type_args(ty).is_some() || !is_resolved_value_type(ty) {
            return;
        }

        self.push_unique_diagnostic(
            Diagnostic::error("propagation operator requires a `Result` value")
                .with_code(codes::ty::PROPAGATE_NOT_RESULT)
                .with_label(Label::primary(span, "this value is not a `Result`"))
                .with_note("`?^`/`?!` operate on `Result[ok, err]` values"),
        );
    }

    pub(super) fn report_type_mismatch(&mut self, expected: &str, found: &'static str, span: Span) {
        self.push_type_mismatch_diagnostic(
            Diagnostic::error(format!("expected `{expected}`, found a {found}"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, format!("this is a {found}")))
                .with_note(format!(
                    "change the value to produce `{expected}`, or change the annotation to match the literal"
                )),
        );
    }

    pub(super) fn report_tuple_arity_mismatch(
        &mut self,
        expected: usize,
        found: usize,
        span: Span,
    ) {
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

    pub(super) fn report_tuple_index_not_comptime(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("tuple index must be known at compile time")
                .with_code(codes::ty::TUPLE_INDEX_NOT_COMPTIME)
                .with_label(Label::primary(
                    span,
                    "this index is not a compile-time integer",
                ))
                .with_note(
                    "tuple indices must be known at compile time; convert to an array for runtime indexing, or use a comptime index",
                ),
        );
    }

    pub(super) fn report_tuple_index_out_of_range(
        &mut self,
        span: Span,
        index: usize,
        arity: usize,
    ) {
        self.diagnostics.push(
            Diagnostic::error(format!("tuple index `{index}` is out of range"))
                .with_code(codes::ty::TUPLE_INDEX_OUT_OF_RANGE)
                .with_label(Label::primary(
                    span,
                    format!(
                        "this tuple has {arity} element{}",
                        if arity == 1 { "" } else { "s" }
                    ),
                ))
                .with_note(
                    "use an in-range compile-time index, or convert the tuple to an array for runtime indexing",
                ),
        );
    }

    pub(super) fn report_function_arity_mismatch(
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

    pub(super) fn report_variant_tag_mismatch(&mut self, tag: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("unexpected variant tag `{tag}`"))
                .with_code(codes::ty::MISMATCH)
                .with_label(Label::primary(span, "this tag is not in the variant type"))
                .with_note("use a tag listed by the annotation, or change the annotation"),
        );
    }

    pub(super) fn report_literal_not_in_union(
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

    pub(super) fn report_wide_value_into_literal_union(
        &mut self,
        expected: &str,
        actual: &str,
        span: Span,
    ) {
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

    pub(super) fn report_variant_entry_kind_mismatch(
        &mut self,
        expected: &Type,
        actual: &Type,
        span: Span,
    ) {
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

    pub(super) fn report_variant_payload_arity_mismatch(
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

    pub(super) fn report_open_variant_not_assignable(&mut self, span: Span) {
        self.diagnostics.push(
            Diagnostic::error("open variant may contain tags not allowed by the annotation")
                .with_code(codes::ty::OPEN_VARIANT_NOT_ASSIGNABLE)
                .with_label(Label::primary(span, "this value has an open variant type"))
                .with_note(
                    "make the annotation open with `..`, or close the value's variant type before assigning it",
                ),
        );
    }

    pub(super) fn report_open_variant_non_exhaustive(&mut self, span: Span) {
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

    pub(super) fn report_unreachable_literal_match_arms(&mut self, row: &Row, arms: &[MatchArm]) {
        let Some(members) = literal_variant_members(row) else {
            return;
        };

        for arm in arms {
            for (literal, span) in arm_covered_literals(&arm.pattern) {
                if !members.contains(&literal) {
                    self.report_unreachable_literal_match_arm(literal, span);
                }
            }
        }
    }

    pub(super) fn report_unreachable_literal_match_arm(&mut self, literal: &Literal, span: Span) {
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

    pub(super) fn report_missing_variant_match_tags(&mut self, missing: &[&str], span: Span) {
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

    pub(super) fn report_missing_literal_match_members(
        &mut self,
        missing: &[&Literal],
        span: Span,
    ) {
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

    pub(super) fn report_missing_empty_match_values(&mut self, missing: &[EmptyValue], span: Span) {
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

    pub(super) fn report_type_mismatch_between_types(
        &mut self,
        expected: &str,
        actual: &str,
        span: Span,
    ) {
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

    pub(super) fn report_redundant_undefined_field(
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

    pub(super) fn report_missing_field(&mut self, name: &str, span: Span) {
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

    pub(super) fn report_unexpected_field(&mut self, name: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("unexpected field `{name}`"))
                .with_code(codes::ty::UNEXPECTED_FIELD)
                .with_label(Label::primary(span, "this field is not in the record type"))
                .with_note(
                    "remove the field, or open the record type with `..` to allow extra fields",
                ),
        );
    }

    pub(super) fn report_duplicate_row_label(
        &mut self,
        name: &str,
        span: Span,
        context: DuplicateRowLabelContext,
    ) {
        if self.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some(codes::ty::DUPLICATE_SPREAD_LABEL)
                && diagnostic.labels.first().map(|label| label.span) == Some(span)
        }) {
            return;
        }

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

    pub(super) fn report_replace_absent_field(&mut self, name: &str, span: Span) {
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

    pub(super) fn report_delete_absent_field(&mut self, name: &str, span: Span) {
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

    pub(super) fn report_rename_absent_field(&mut self, name: &str, span: Span) {
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

    pub(super) fn report_rename_target_present(&mut self, from: &str, to: &str, span: Span) {
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

    pub(super) fn report_or_pattern_binding_mismatch(
        &mut self,
        mismatch: &OrPatternBindingMismatch,
    ) {
        let names = mismatch
            .names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let note = if mismatch.names.len() == 1 {
            format!("binder {names} must be bound by every alternative")
        } else {
            format!("binders {names} must be bound by every alternative")
        };

        self.diagnostics.push(
            Diagnostic::error("or-pattern alternatives bind different names")
                .with_code(codes::ty::OR_PATTERN_BINDING_MISMATCH)
                .with_label(Label::primary(
                    mismatch.span,
                    "this alternative binds a different set of names",
                ))
                .with_note(note),
        );
    }
}
