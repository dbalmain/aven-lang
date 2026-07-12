use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, hash_map::Entry};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Binding, Declaration, DeclarationPhase, Expr, ExprKind, InterpolationSegment, Item, Literal,
    MatchArm, MergedItem, Module, Param, PatternBinding, PropagationMode, RecordEntry, Signature,
    SpreadBinding, collect_declarations, decode_string_literal, is_comptime_identifier_name,
    lambda_parts, merged_items, pattern_bindings, walk_expr_children,
};

use crate::BUILTIN_TYPES;
use crate::comptime::{self, Evaluation};
use crate::env::{
    LocalTypeScopes, LocalValueType, TypeEnv, free_metas_in_local_values,
    free_row_vars_in_local_values,
};
use crate::host_comptime::{
    ComptimeArg, ComptimeError, HostComptimeFnSpec, HostComptimeParam, HostGlobals, HostStatics,
};
use crate::lower::{
    DeclaredAnnotation, DeclaredAnnotationSource, TypeLowering, binding_for_declaration,
    declared_annotation_for_declaration,
};
use crate::ty::{
    LiteralBase, Row, RowEntry, RowKind, RowMergeSource, RowTail, Type, TypeScheme,
    builtin_collection_method_type, display_inferred_type, generalize, is_concrete_type,
    is_meta_type, is_null_value, is_resolved_value_type, is_text_type, is_undefined_value,
    literal_base, literal_variant_base, map_type, mismatched_literal_kind, named_builtin,
    named_type_mismatch, named_type_name, numeric_type_name, open_literal_variant_base,
    render_literal_value, type_contains_deferred, type_contains_variable, type_is_uninhabited,
};
use crate::unify::Unifier;
use crate::{InferredType, ModuleImports};

mod annotations;
mod comptime_context;
mod core;
mod diagnostics;
mod inference;
mod match_checking;
mod rows;
mod type_checking;
mod value;

pub(crate) struct Checker<'a> {
    known_types: HashSet<String>,
    type_definitions: HashMap<String, Type>,
    value_types: HashMap<String, Option<TypeScheme>>,
    comptime_bindings: HashSet<String>,
    comptime_artifacts: HashMap<String, bool>,
    comptime_specializations: HashMap<comptime::SpecializationKey, comptime::EvaluationResult>,
    comptime_specializations_in_progress: HashSet<comptime::SpecializationKey>,
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
    /// Statics carried by named types, keyed `type name -> static name ->
    /// scheme`. Field access on an unshadowed static-carrying type name resolves
    /// through this table (`Map.from`, `Json.encode`).
    statics: HashMap<String, HashMap<String, TypeScheme>>,
    host_comptime_fns: HashMap<String, HostComptimeFnSpec>,
    pub(crate) imports: ModuleImports,
    report_unbound_names: bool,
    report_unresolved_bindings: bool,
    reported_unbound_name_spans: HashSet<Span>,
    reported_import_spans: HashSet<Span>,
    propagation_contexts: Vec<PropagationContext>,
    pattern_bindings: HashMap<String, &'a PatternBinding>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) inferred_types: Vec<InferredType>,
}

#[derive(Clone)]
struct DiagnosticSnapshot {
    diagnostics_len: usize,
    reported_unbound_name_spans: HashSet<Span>,
    reported_import_spans: HashSet<Span>,
    propagation_context_site_counts: Vec<usize>,
}

#[derive(Debug, Default)]
struct PropagationContext {
    sites: Vec<PropagationSite>,
}

#[derive(Debug, Clone)]
struct PropagationSite {
    span: Span,
    error_ty: Type,
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

#[derive(Debug, Clone)]
struct PatternLocalTypes {
    bindings: Vec<(String, LocalValueType)>,
    mismatches: Vec<OrPatternBindingMismatch>,
}

#[derive(Debug, Clone)]
struct OrPatternBindingMismatch {
    span: Span,
    names: Vec<String>,
}

enum MatchArmCombination {
    Joined(Type),
    Conflict(MatchArmTypeConflict),
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
    let mut seen = HashSet::new();

    for entry in entries {
        match entry {
            RecordEntry::Field {
                name,
                name_span,
                value,
                overwrite: false,
                ..
            } => {
                if !seen.insert(name) {
                    return None;
                }
                fields.push(ValueRecordField {
                    name,
                    name_span: *name_span,
                    value: Some(value),
                });
            }
            RecordEntry::Shorthand {
                name, name_span, ..
            } => {
                if !seen.insert(name) {
                    return None;
                }
                fields.push(ValueRecordField {
                    name,
                    name_span: *name_span,
                    value: None,
                });
            }
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

fn pattern_local_types(
    type_definitions: &HashMap<String, Type>,
    pattern: &Expr,
    expected: Option<&Type>,
) -> Vec<(String, LocalValueType)> {
    checked_pattern_local_types(type_definitions, pattern, expected).bindings
}

fn checked_pattern_local_types(
    type_definitions: &HashMap<String, Type>,
    pattern: &Expr,
    expected: Option<&Type>,
) -> PatternLocalTypes {
    let mut mismatches = Vec::new();
    collect_or_pattern_binding_mismatches(pattern, &mut mismatches);

    PatternLocalTypes {
        bindings: merged_pattern_local_types(type_definitions, pattern, expected),
        mismatches,
    }
}

fn merged_pattern_local_types(
    type_definitions: &HashMap<String, Type>,
    pattern: &Expr,
    expected: Option<&Type>,
) -> Vec<(String, LocalValueType)> {
    let alternatives = flatten_or_alternatives(pattern);
    if alternatives.len() == 1 {
        return single_pattern_local_types(type_definitions, pattern, expected);
    }

    let alternative_types = alternatives
        .iter()
        .map(|alternative| single_pattern_local_type_map(type_definitions, alternative, expected))
        .collect::<Vec<_>>();
    let names = alternative_types
        .iter()
        .flat_map(|types| types.keys().cloned())
        .collect::<BTreeSet<_>>();

    names
        .into_iter()
        .map(|name| {
            let ty = merged_or_pattern_local_type(&name, &alternative_types);
            (name, ty)
        })
        .collect()
}

fn single_pattern_local_type_map(
    type_definitions: &HashMap<String, Type>,
    pattern: &Expr,
    expected: Option<&Type>,
) -> HashMap<String, LocalValueType> {
    single_pattern_local_types(type_definitions, pattern, expected)
        .into_iter()
        .collect()
}

fn merged_or_pattern_local_type(
    name: &str,
    alternative_types: &[HashMap<String, LocalValueType>],
) -> LocalValueType {
    let Some(first) = alternative_types.first().and_then(|types| types.get(name)) else {
        return LocalValueType::Unknown;
    };

    if alternative_types
        .iter()
        .all(|types| types.get(name) == Some(first))
        && matches!(first, LocalValueType::Known(_))
    {
        first.clone()
    } else {
        LocalValueType::Unknown
    }
}

fn single_pattern_local_types(
    type_definitions: &HashMap<String, Type>,
    pattern: &Expr,
    expected: Option<&Type>,
) -> Vec<(String, LocalValueType)> {
    let mut known = HashMap::new();
    if let Some(expected) = expected {
        collect_known_pattern_types(type_definitions, pattern, expected, &mut known);
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

fn collect_or_pattern_binding_mismatches(
    pattern: &Expr,
    mismatches: &mut Vec<OrPatternBindingMismatch>,
) {
    match &ungroup_expr(pattern).kind {
        ExprKind::Binary { operator, .. } if operator == "|" => {
            let alternatives = flatten_or_alternatives(pattern);
            collect_flat_or_pattern_binding_mismatches(&alternatives, mismatches);
            for alternative in alternatives {
                collect_or_pattern_binding_mismatches(alternative, mismatches);
            }
        }
        _ => {
            walk_expr_children(pattern, &mut |child| {
                collect_or_pattern_binding_mismatches(child, mismatches);
            });
        }
    }
}

fn collect_flat_or_pattern_binding_mismatches(
    alternatives: &[&Expr],
    mismatches: &mut Vec<OrPatternBindingMismatch>,
) {
    let Some((first, rest)) = alternatives.split_first() else {
        return;
    };

    let expected = pattern_binding_names(first);
    for alternative in rest {
        let actual = pattern_binding_names(alternative);
        if actual == expected {
            continue;
        }

        mismatches.push(OrPatternBindingMismatch {
            span: alternative.span,
            names: expected.symmetric_difference(&actual).cloned().collect(),
        });
    }
}

fn pattern_binding_names(pattern: &Expr) -> BTreeSet<String> {
    pattern_bindings(pattern)
        .into_iter()
        .map(|binding| binding.name.to_owned())
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

fn collect_known_pattern_types(
    type_definitions: &HashMap<String, Type>,
    pattern: &Expr,
    expected: &Type,
    known: &mut HashMap<String, Type>,
) {
    match (&pattern.kind, expected) {
        (ExprKind::Group(inner), _) => {
            collect_known_pattern_types(type_definitions, inner, expected, known);
        }
        (_, Type::Optional(inner))
            if empty_value_pattern(pattern) != Some(EmptyValue::Undefined) =>
        {
            collect_known_pattern_types(type_definitions, pattern, inner, known);
        }
        (_, Type::Nullable(inner)) if empty_value_pattern(pattern) != Some(EmptyValue::Null) => {
            collect_known_pattern_types(type_definitions, pattern, inner, known);
        }
        (ExprKind::Name(name), _)
            if name != "_"
                && (is_resolved_value_type(expected)
                    || matches!(expected, Type::Meta(_))
                    || type_contains_variable(expected)) =>
        {
            known.insert(name.clone(), expected.clone());
        }
        (
            ExprKind::Binary {
                left,
                operator,
                right,
                ..
            },
            _,
        ) if operator == "|" => {
            collect_known_pattern_types(type_definitions, left, expected, known);
            collect_known_pattern_types(type_definitions, right, expected, known);
        }
        (ExprKind::Call { callee, args }, _) => {
            let ExprKind::Tag(tag) = &callee.kind else {
                return;
            };
            let Some(row) = subject_variant_row(expected, type_definitions) else {
                return;
            };
            let Some(payload) = literal_variant_payload(&row, tag) else {
                return;
            };
            if payload.len() != args.len() {
                return;
            }
            for (arg, ty) in args.iter().zip(payload) {
                collect_known_pattern_types(type_definitions, arg, ty, known);
            }
        }
        (ExprKind::Record(entries), Type::Record(row)) => {
            collect_known_record_pattern_types(type_definitions, entries, row, known);
        }
        (ExprKind::Tag(_), _) if subject_variant_row(expected, type_definitions).is_some() => {}
        _ => {}
    }
}

fn collect_known_record_pattern_types(
    type_definitions: &HashMap<String, Type>,
    entries: &[RecordEntry],
    row: &Row,
    known: &mut HashMap<String, Type>,
) {
    let matched_labels: HashSet<_> = entries.iter().filter_map(record_pattern_label).collect();

    for entry in entries {
        match entry {
            RecordEntry::Field { name, value, .. } => {
                if let Some(field_ty) = row_field_type(row, name) {
                    collect_known_pattern_types(type_definitions, value, field_ty, known);
                }
            }
            RecordEntry::Shorthand { name, .. } => {
                if let Some(field_ty) = row_field_type(row, name)
                    && (is_concrete_type(field_ty) || type_contains_variable(field_ty))
                {
                    known.insert(name.clone(), field_ty.clone());
                }
            }
            RecordEntry::Rename { from, to, .. } => {
                if let Some(field_ty) = row_field_type(row, from)
                    && (is_concrete_type(field_ty) || type_contains_variable(field_ty))
                {
                    known.insert(to.clone(), field_ty.clone());
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
            | RecordEntry::Iteration { .. }
            | RecordEntry::Open { .. }
            | RecordEntry::Element(_) => {}
        }
    }
}

fn record_pattern_label(entry: &RecordEntry) -> Option<&str> {
    match entry {
        RecordEntry::Field { name, .. } | RecordEntry::Shorthand { name, .. } => Some(name),
        RecordEntry::Rename { from, .. } => Some(from),
        RecordEntry::Spread { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::FieldComputed { .. }
        | RecordEntry::DeleteComputed { .. }
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

fn local_value_type_as_type(value: LocalValueType) -> Option<Type> {
    match value {
        LocalValueType::Known(ty) => Some(ty),
        LocalValueType::Scheme(scheme) => Some(scheme.ty),
        LocalValueType::Unknown => None,
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

fn subject_variant_row<'a>(
    ty: &'a Type,
    type_definitions: &'a HashMap<String, Type>,
) -> Option<Cow<'a, Row>> {
    if let Type::Variant(row) = ty {
        return Some(Cow::Borrowed(row));
    }

    if let Type::Named(name) = ty
        && let Some(Type::Variant(row)) = type_definitions.get(name)
    {
        return Some(Cow::Borrowed(row));
    }

    if matches!(ty, Type::Named(name) if name == "Bool") {
        return Some(Cow::Owned(Row {
            entries: vec![
                RowEntry::Literal {
                    value: Literal::Bool(true),
                },
                RowEntry::Literal {
                    value: Literal::Bool(false),
                },
            ],
            tail: RowTail::Closed,
        }));
    }

    let (ok_ty, err_ty) = result_type_args(ty)?;
    Some(Cow::Owned(Row {
        entries: vec![
            RowEntry::Tag {
                name: "Ok".to_owned(),
                payload: vec![ok_ty.clone()],
            },
            RowEntry::Tag {
                name: "Err".to_owned(),
                payload: vec![err_ty.clone()],
            },
        ],
        tail: RowTail::Closed,
    }))
}

pub(crate) fn string_literal_label(text: &str) -> Option<String> {
    text.strip_prefix('"')?.strip_suffix('"')?;
    Some(decode_string_literal(text))
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
            (Literal::Bool(_), "Bool")
                | (Literal::String(_), "Text")
                | (Literal::Number(_), "Int" | "Float")
        )
    })
}

fn literal_kind_name(literal: &Literal) -> &'static str {
    match literal {
        Literal::Bool(_) => "bool literal",
        Literal::String(_) => "text literal",
        Literal::Number(_) => "number literal",
        Literal::Regex(_) => "regex literal",
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

/// Re-apply a peeled empty-value stack (outermost first) to a type, the inverse
/// of [`peel_empty_values`].
fn rewrap_empty_values(mut ty: Type, empties: &[EmptyValue]) -> Type {
    for empty in empties.iter().rev() {
        ty = match empty {
            EmptyValue::Undefined => Type::Optional(Box::new(ty)),
            EmptyValue::Null => Type::Nullable(Box::new(ty)),
        };
    }
    ty
}

/// A compact source-like rendering of a receiver expression for diagnostics
/// (`headers[0]`, `user?.profile`). Returns `None` for shapes not worth naming,
/// so the caller can fall back to a generic phrasing.
fn describe_receiver_expr(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Group(inner) => describe_receiver_expr(inner),
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => Some(name.clone()),
        ExprKind::FieldAccess {
            receiver,
            field,
            null_safe,
            ..
        } => {
            let receiver = describe_receiver_expr(receiver)?;
            let operator = if *null_safe { "?." } else { "." };
            Some(format!("{receiver}{operator}{field}"))
        }
        ExprKind::Index { callee, args } => {
            let callee = describe_receiver_expr(callee)?;
            let args = args
                .iter()
                .map(describe_index_arg)
                .collect::<Option<Vec<_>>>()?
                .join(", ");
            Some(format!("{callee}[{args}]"))
        }
        _ => None,
    }
}

fn describe_index_arg(expr: &Expr) -> Option<String> {
    match &ungroup_expr(expr).kind {
        ExprKind::Literal(Literal::Number(number)) => Some(number.clone()),
        ExprKind::Literal(Literal::String(text)) => Some(format!("\"{text}\"")),
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => Some(name.clone()),
        _ => None,
    }
}

fn render_empty_values(empties: &[EmptyValue]) -> String {
    empties
        .iter()
        .map(|empty| empty.render())
        .collect::<Vec<_>>()
        .join(" or ")
}

fn empty_value_is_covered(arms: &[MatchArm], value: EmptyValue) -> bool {
    arms.iter().any(|arm| {
        arm.guards.is_empty()
            && (arm_covered_empty_values(&arm.pattern).contains(&value)
                || pattern_has_underscore_alternative(&arm.pattern))
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
    name == "Map" || crate::COMPTIME_BUILTIN_FUNCTIONS.contains(&name)
}

fn is_map_receiver_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Apply { callee, args }
            if args.len() == 2 && matches!(callee.as_ref(), Type::Named(name) if name == "Map")
    )
}

fn is_catch_all_pattern(pattern: &Expr) -> bool {
    match &pattern.kind {
        ExprKind::Group(inner) => is_catch_all_pattern(inner),
        ExprKind::Name(_) => true,
        _ => false,
    }
}

fn pattern_has_catch_all_alternative(pattern: &Expr) -> bool {
    flatten_or_alternatives(pattern)
        .into_iter()
        .any(is_catch_all_pattern)
}

fn pattern_has_underscore_alternative(pattern: &Expr) -> bool {
    flatten_or_alternatives(pattern)
        .into_iter()
        .any(is_underscore_pattern)
}

fn flatten_or_alternatives(pattern: &Expr) -> Vec<&Expr> {
    let mut alternatives = Vec::new();
    collect_or_alternatives(pattern, &mut alternatives);
    alternatives
}

fn collect_or_alternatives<'a>(pattern: &'a Expr, alternatives: &mut Vec<&'a Expr>) {
    match &ungroup_expr(pattern).kind {
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } if operator == "|" => {
            collect_or_alternatives(left, alternatives);
            collect_or_alternatives(right, alternatives);
        }
        _ => alternatives.push(pattern),
    }
}

fn arm_covered_empty_values(pattern: &Expr) -> Vec<EmptyValue> {
    flatten_or_alternatives(pattern)
        .into_iter()
        .filter_map(empty_value_pattern)
        .collect()
}

fn arm_covered_tags(pattern: &Expr) -> Vec<&str> {
    flatten_or_alternatives(pattern)
        .into_iter()
        .filter_map(variant_pattern_tag)
        .collect()
}

fn pure_tag_pattern(pattern: &Expr) -> Option<(&str, usize)> {
    match &pattern.kind {
        ExprKind::Group(inner) => pure_tag_pattern(inner),
        ExprKind::Tag(tag) => Some((tag, 0)),
        ExprKind::Call { callee, args } => match &callee.kind {
            ExprKind::Tag(tag) => Some((tag, args.len())),
            _ => None,
        },
        _ => None,
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

/// Statics carried by compiler-builtin types. `Map`'s `empty`/`from` are the
/// only ones today; host-registered types (e.g. `Json`) supply theirs through
/// [`HostGlobals::statics`].
pub(crate) fn builtin_type_statics() -> HostStatics {
    vec![("Map".to_owned(), map_statics())]
}

fn map_statics() -> Vec<(String, Type)> {
    let key = Type::Variable("k".to_owned());
    let value = Type::Variable("v".to_owned());
    let map_type = Type::Apply {
        callee: Box::new(Type::Named("Map".to_owned())),
        args: vec![key.clone(), value.clone()],
    };
    let entry_type = Type::Tuple(vec![key.clone(), value.clone()]);
    let entries_type = Type::Apply {
        callee: Box::new(Type::Named("Array".to_owned())),
        args: vec![entry_type],
    };

    vec![
        (
            "empty".to_owned(),
            function_type(Vec::new(), map_type.clone()),
        ),
        (
            "from".to_owned(),
            function_type(vec![entries_type], map_type),
        ),
    ]
}

fn function_type(params: Vec<Type>, result: Type) -> Type {
    let required = params.len();
    Type::Function {
        params,
        result: Box::new(result),
        required,
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
            row_merges: Vec::new(),
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
        ExprKind::Literal(
            literal @ (Literal::Bool(_) | Literal::Number(_) | Literal::String(_)),
        ) => Some((literal, pattern.span)),
        _ => None,
    }
}

fn arm_covered_literals(pattern: &Expr) -> Vec<(&Literal, Span)> {
    flatten_or_alternatives(pattern)
        .into_iter()
        .filter_map(literal_pattern_value)
        .collect()
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

    match &value.kind {
        ExprKind::Call { callee, .. } => !matches!(
            &ungroup_expr(callee).kind,
            ExprKind::Name(name) | ExprKind::ComptimeName(name)
                if name.chars().next().is_some_and(char::is_uppercase)
        ),
        ExprKind::Binary { .. }
        | ExprKind::Unary { .. }
        | ExprKind::FieldAccess { .. }
        | ExprKind::Propagate { .. }
        | ExprKind::Match { .. }
        | ExprKind::Block(_)
        | ExprKind::Lambda { .. }
        | ExprKind::Interpolation(_) => true,
        _ => false,
    }
}

fn expr_name(expr: &Expr) -> Option<&str> {
    match &ungroup_expr(expr).kind {
        ExprKind::Name(name) => Some(name),
        _ => None,
    }
}

/// A tuple index must be a comptime-known non-negative integer literal. Floats,
/// negatives, and runtime expressions are rejected (the caller diagnoses them).
fn comptime_known_tuple_index(expr: &Expr) -> Option<usize> {
    let ExprKind::Literal(Literal::Number(number)) = &ungroup_expr(expr).kind else {
        return None;
    };
    number.parse::<usize>().ok()
}

fn ungroup_expr(mut expr: &Expr) -> &Expr {
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    expr
}

/// An `import(...)` call with any specifier — the shape whose value is a
/// module record, never a type.
pub(crate) fn is_import_call(expr: &Expr) -> bool {
    matches!(
        &ungroup_expr(expr).kind,
        ExprKind::Call { callee, .. }
            if matches!(&ungroup_expr(callee).kind, ExprKind::Name(name) if name == "import")
    )
}

fn is_final_expr_item(items: &[Item], expr: &Expr) -> bool {
    matches!(items.last(), Some(Item::Expr(final_expr)) if std::ptr::eq(final_expr, expr))
}

fn is_result_type(ty: &Type) -> bool {
    result_type_args(ty).is_some()
}

fn result_type_args(ty: &Type) -> Option<(&Type, &Type)> {
    let Type::Apply { callee, args } = ty else {
        return None;
    };
    if args.len() == 2 && matches!(callee.as_ref(), Type::Named(name) if name == "Result") {
        Some((&args[0], &args[1]))
    } else {
        None
    }
}

fn result_type(ok_ty: Type, error_ty: Type) -> Type {
    Type::Apply {
        callee: Box::new(Type::Named("Result".to_owned())),
        args: vec![ok_ty, error_ty],
    }
}

fn empty_variant_type() -> Type {
    Type::Variant(Row {
        entries: Vec::new(),
        tail: RowTail::Closed,
    })
}

fn final_value_expr(expr: &Expr) -> Option<&Expr> {
    let expr = ungroup_expr(expr);
    match &expr.kind {
        ExprKind::Block(items) => match items.last() {
            Some(Item::Expr(final_expr)) => final_value_expr(final_expr),
            _ => None,
        },
        _ => Some(expr),
    }
}

fn final_result_span(expr: &Expr) -> Span {
    final_value_expr(expr).map_or(expr.span, |final_expr| final_expr.span)
}

/// Binder names in `pattern` that extract a type or uppercase comptime-function
/// export from the static import of `specifier`. Handles rename (`{ User ->
/// Alias }` binds `Alias` from export field `User`).
fn type_export_pattern_binders(
    pattern: &Expr,
    specifier: &str,
    imports: &ModuleImports,
) -> HashSet<String> {
    let ExprKind::Record(entries) = &ungroup_expr(pattern).kind else {
        return HashSet::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            RecordEntry::Shorthand { name, .. }
                if imports.type_export(specifier, name).is_some()
                    || imports.comptime_export(specifier, name).is_some() =>
            {
                Some(name.clone())
            }
            RecordEntry::Rename { from, to, .. }
                if imports.type_export(specifier, from).is_some()
                    || imports.comptime_export(specifier, from).is_some() =>
            {
                Some(to.clone())
            }
            _ => None,
        })
        .collect()
}

/// Export-field name that a record-pattern binder extracts. Shorthand `{ pair }`
/// maps binder `pair` → export `pair`; rename `{ pair -> p }` maps `p` → `pair`.
pub(crate) fn import_pattern_source_for_binder<'a>(
    pattern: &'a Expr,
    binder: &str,
) -> Option<&'a str> {
    let ExprKind::Record(entries) = &ungroup_expr(pattern).kind else {
        return None;
    };
    entries.iter().find_map(|entry| match entry {
        RecordEntry::Shorthand { name, .. } if name == binder => Some(name.as_str()),
        RecordEntry::Rename { from, to, .. } if to == binder => Some(from.as_str()),
        _ => None,
    })
}

fn result_constructor_tag(callee: &Expr) -> Option<&str> {
    let ExprKind::Tag(tag) = &ungroup_expr(callee).kind else {
        return None;
    };
    matches!(tag.as_str(), "Ok" | "Err").then_some(tag)
}

fn result_constructor_type(tag: &str, args: &[Expr]) -> Type {
    Type::Variant(Row {
        entries: vec![RowEntry::Tag {
            name: tag.to_owned(),
            payload: vec![Type::Deferred; args.len()],
        }],
        tail: RowTail::Closed,
    })
}

fn single_tag_payload_type(ty: &Type, tag: &str) -> Option<Type> {
    let Type::Variant(row) = ty else {
        return None;
    };

    row.entries.iter().find_map(|entry| {
        let RowEntry::Tag { name, payload } = entry else {
            return None;
        };
        (name == tag && payload.len() == 1).then(|| payload[0].clone())
    })
}
