use std::collections::{HashMap, HashSet};

use aven_core::Span;
use aven_parser::{Expr, ExprKind, Literal};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// A type expression that is valid to keep for a later comptime/type phase
    /// but is not part of the core lowered type grammar yet.
    Deferred,
    Named(String),
    Variable(String),
    /// A unification variable used only during value inference. It never appears
    /// in a lowered annotation or checked output; published schemes quantify any
    /// metas that remain after inference.
    Meta(u32),
    Apply {
        callee: Box<Type>,
        args: Vec<Type>,
    },
    Function {
        params: Vec<Type>,
        result: Box<Type>,
        /// Number of leading required params. `params[required..]` are the
        /// optional (defaulted) trailing params. Invariant: `required <=
        /// params.len()`.
        required: usize,
    },
    Optional(Box<Type>),
    Nullable(Box<Type>),
    Tuple(Vec<Type>),
    Record(Row),
    Variant(Row),
}

impl Type {
    pub fn render(&self) -> String {
        render_type(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordField {
    pub name: String,
    pub ty: Type,
}

pub const MAP_METHOD_NAMES: &[&str] = &[
    "get", "set", "delete", "has", "keys", "values", "entries", "size", "merge",
];

pub const ARRAY_METHOD_NAMES: &[&str] = &["has", "push"];

pub const SET_METHOD_NAMES: &[&str] = &["has"];

/// Roc-aligned `Str` helpers (camelCase). No `length`/`len` — grapheme
/// ambiguity; Roc omits it on purpose.
pub const TEXT_METHOD_NAMES: &[&str] = &[
    "isEmpty",
    "contains",
    "startsWith",
    "endsWith",
    "trim",
    "trimStart",
    "trimEnd",
    "toLower",
    "toUpper",
    "replaceEach",
    "replaceFirst",
    "dropPrefix",
    "dropSuffix",
    "repeat",
    "splitOn",
    "toInt",
    "toFloat",
];

pub const RESULT_METHOD_NAMES: &[&str] = &[
    "mapErr", "orElse", "map", "andThen", "unwrapOr", "isOk", "isErr",
];

pub fn record_fields(ty: &Type) -> Option<Vec<RecordField>> {
    let mut ty = ty;
    while let Type::Optional(inner) | Type::Nullable(inner) = ty {
        ty = inner;
    }

    match ty {
        Type::Record(row) => Some(
            row.entries
                .iter()
                .filter_map(|entry| match entry {
                    RowEntry::Field { name, ty } => Some(RecordField {
                        name: name.clone(),
                        ty: ty.clone(),
                    }),
                    RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
                })
                .collect(),
        ),
        Type::Apply { .. } | Type::Variant(_) | Type::Named(_) => builtin_collection_fields(ty),
        Type::Deferred
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Function { .. }
        | Type::Optional(_)
        | Type::Nullable(_)
        | Type::Tuple(_) => None,
    }
}

pub fn builtin_collection_method_type(receiver: &Type, name: &str) -> Option<Type> {
    if let Some((key, value)) = map_type_args(receiver) {
        return match name {
            "get" => Some(function(
                vec![key.clone()],
                Type::Optional(Box::new(value.clone())),
            )),
            "set" => Some(function(
                vec![key.clone(), value.clone()],
                map_apply(key.clone(), value.clone()),
            )),
            "delete" => Some(function(
                vec![key.clone()],
                map_apply(key.clone(), value.clone()),
            )),
            "has" => Some(function(vec![key.clone()], named_builtin("Bool"))),
            "keys" => Some(function(Vec::new(), array_apply(key.clone()))),
            "values" => Some(function(Vec::new(), array_apply(value.clone()))),
            "entries" => Some(function(
                Vec::new(),
                array_apply(Type::Tuple(vec![key.clone(), value.clone()])),
            )),
            "size" => Some(function(Vec::new(), named_builtin("Int"))),
            "merge" => Some(function(
                vec![map_apply(key.clone(), value.clone())],
                map_apply(key.clone(), value.clone()),
            )),
            _ => None,
        };
    }

    if let Some(element) = array_type_arg(receiver) {
        return match name {
            "has" => Some(function(vec![element.clone()], named_builtin("Bool"))),
            "push" => Some(function(
                vec![element.clone()],
                array_apply(element.clone()),
            )),
            // Roc `Str.join_with` with the list as receiver (`parts.joinWith(", ")`).
            "joinWith" if is_text_type(element) => {
                Some(function(vec![named_builtin("Text")], named_builtin("Text")))
            }
            _ => None,
        };
    }

    if let Some(element) = set_type_arg(receiver) {
        return match name {
            "has" => Some(function(vec![element.clone()], named_builtin("Bool"))),
            _ => None,
        };
    }

    if is_text_type(receiver) {
        return text_method_type(name);
    }

    if let Type::Named(type_name) = receiver {
        return temporal_method_type(type_name, name);
    }

    let (ok, error) = result_type_args(receiver)?;
    let output_ok = Type::Variable("result_ok".to_owned());
    let output_error = Type::Variable("result_error".to_owned());
    match name {
        "mapErr" => Some(function(
            vec![function(vec![error], output_error.clone())],
            build::result(ok, output_error),
        )),
        "orElse" => Some(function(
            vec![function(
                vec![error],
                build::result(output_ok.clone(), output_error.clone()),
            )],
            build::result(output_ok, output_error),
        )),
        "map" => Some(function(
            vec![function(vec![ok], output_ok.clone())],
            build::result(output_ok, error),
        )),
        "andThen" => Some(function(
            vec![function(
                vec![ok],
                build::result(output_ok.clone(), error.clone()),
            )],
            build::result(output_ok, error),
        )),
        "unwrapOr" => Some(function(vec![ok.clone()], ok)),
        "isOk" | "isErr" => Some(function(Vec::new(), named_builtin("Bool"))),
        _ => None,
    }
}

fn text_method_type(name: &str) -> Option<Type> {
    let text = named_builtin("Text");
    let bool_ty = named_builtin("Bool");
    match name {
        "isEmpty" => Some(function(Vec::new(), bool_ty)),
        "contains" | "startsWith" | "endsWith" => Some(function(vec![text.clone()], bool_ty)),
        "trim" | "trimStart" | "trimEnd" | "toLower" | "toUpper" => {
            Some(function(Vec::new(), text))
        }
        "replaceEach" | "replaceFirst" => Some(function(vec![text.clone(), text.clone()], text)),
        "dropPrefix" | "dropSuffix" => Some(function(vec![text.clone()], text)),
        "repeat" => Some(function(vec![named_builtin("Int")], text)),
        "splitOn" => Some(function(vec![text], array_apply(named_builtin("Text")))),
        "toInt" => Some(function(
            Vec::new(),
            Type::Optional(Box::new(named_builtin("Int"))),
        )),
        "toFloat" => Some(function(
            Vec::new(),
            Type::Optional(Box::new(named_builtin("Float"))),
        )),
        _ => None,
    }
}

fn temporal_method_type(type_name: &str, name: &str) -> Option<Type> {
    let named = |name| named_builtin(name);
    match (type_name, name) {
        ("Date", "format")
        | ("Time", "format")
        | ("DateTime", "format")
        | ("Instant", "format")
        | ("Duration", "format") => Some(function(Vec::new(), named("Text"))),
        ("Date", "plusDays") => Some(function(vec![named("Int")], named("Date"))),
        ("DateTime", "instant") => Some(function(vec![named("Int")], named("Instant"))),
        ("Instant", "dateTime") => Some(function(vec![named("Int")], named("DateTime"))),
        ("Instant", "plus") | ("Instant", "minus") => {
            Some(function(vec![named("Duration")], named("Instant")))
        }
        ("Instant", "since") => Some(function(vec![named("Instant")], named("Duration"))),
        ("Duration", "plus") => Some(function(vec![named("Duration")], named("Duration"))),
        _ => None,
    }
}

fn builtin_collection_fields(receiver: &Type) -> Option<Vec<RecordField>> {
    let names = if map_type_args(receiver).is_some() {
        MAP_METHOD_NAMES
    } else if array_type_arg(receiver).is_some() {
        ARRAY_METHOD_NAMES
    } else if set_type_arg(receiver).is_some() {
        SET_METHOD_NAMES
    } else if is_text_type(receiver) {
        TEXT_METHOD_NAMES
    } else if result_type_args(receiver).is_some() {
        RESULT_METHOD_NAMES
    } else {
        return None;
    };
    let mut fields = names
        .iter()
        .map(|name| RecordField {
            name: (*name).to_owned(),
            ty: builtin_collection_method_type(receiver, name)
                .expect("builtin method names have method types"),
        })
        .collect::<Vec<_>>();

    // `joinWith` is Array(Text)-only; keep it out of the generic array table so
    // Array(Int) does not advertise a method it cannot type.
    if let Some(element) = array_type_arg(receiver)
        && is_text_type(element)
        && let Some(ty) = builtin_collection_method_type(receiver, "joinWith")
    {
        fields.push(RecordField {
            name: "joinWith".to_owned(),
            ty,
        });
    }

    Some(fields)
}

fn map_type_args(ty: &Type) -> Option<(&Type, &Type)> {
    let Type::Apply { callee, args } = ty else {
        return None;
    };
    if !matches!(callee.as_ref(), Type::Named(name) if name == "Map") {
        return None;
    }
    let [key, value] = args.as_slice() else {
        return None;
    };
    Some((key, value))
}

fn array_type_arg(ty: &Type) -> Option<&Type> {
    let Type::Apply { callee, args } = ty else {
        return None;
    };
    if !matches!(callee.as_ref(), Type::Named(name) if name == "Array") {
        return None;
    }
    let [element] = args.as_slice() else {
        return None;
    };
    Some(element)
}

fn set_type_arg(ty: &Type) -> Option<&Type> {
    let Type::Apply { callee, args } = ty else {
        return None;
    };
    if !matches!(callee.as_ref(), Type::Named(name) if name == "Set") {
        return None;
    }
    let [element] = args.as_slice() else {
        return None;
    };
    Some(element)
}

fn result_type_args(ty: &Type) -> Option<(Type, Type)> {
    if let Type::Apply { callee, args } = ty
        && let [ok, error] = args.as_slice()
        && matches!(callee.as_ref(), Type::Named(name) if name == "Result")
    {
        return Some((ok.clone(), error.clone()));
    }

    let Type::Variant(row) = ty else {
        return None;
    };
    let [RowEntry::Tag { name, payload }] = row.entries.as_slice() else {
        return None;
    };
    let [payload] = payload.as_slice() else {
        return None;
    };
    if row.tail != RowTail::Closed {
        return None;
    }

    match name.as_str() {
        "Ok" => Some((payload.clone(), Type::Variable("result_error".to_owned()))),
        "Err" => Some((Type::Variable("result_ok".to_owned()), payload.clone())),
        _ => None,
    }
}

/// Whether `ty` has no inhabitants, so no runtime value can ever have it. The
/// base case is the empty closed variant `@{}`; a tag whose payload is
/// uninhabited, and hence a closed variant all of whose tags are uninhabited,
/// are uninhabited too. Exhaustiveness uses this so a `Result(a, @{})` match
/// need not cover `@Err` — an error that can never be constructed. Conservative
/// elsewhere (records, open rows, named types count as inhabited): the only
/// risk is asking for an arm that is technically unreachable, never dropping a
/// reachable one.
pub(crate) fn type_is_uninhabited(ty: &Type) -> bool {
    let Type::Variant(row) = ty else {
        return false;
    };
    if row.tail != RowTail::Closed {
        return false;
    }
    row.entries.iter().all(|entry| match entry {
        RowEntry::Tag { payload, .. } => payload.iter().any(type_is_uninhabited),
        RowEntry::Field { .. } | RowEntry::Literal { .. } => false,
    })
}

fn map_apply(key: Type, value: Type) -> Type {
    Type::Apply {
        callee: Box::new(named_builtin("Map")),
        args: vec![key, value],
    }
}

fn array_apply(element: Type) -> Type {
    Type::Apply {
        callee: Box::new(named_builtin("Array")),
        args: vec![element],
    }
}

fn function(params: Vec<Type>, result: Type) -> Type {
    let required = params.len();
    Type::Function {
        params,
        result: Box::new(result),
        required,
    }
}

pub fn variant_tags(ty: &Type) -> Option<Vec<String>> {
    let mut ty = ty;
    while let Type::Optional(inner) | Type::Nullable(inner) = ty {
        ty = inner;
    }

    let Type::Variant(row) = ty else {
        return None;
    };

    Some(
        row.entries
            .iter()
            .filter_map(|entry| match entry {
                RowEntry::Tag { name, .. } => Some(name.clone()),
                RowEntry::Field { .. } | RowEntry::Literal { .. } => None,
            })
            .collect(),
    )
}

/// Whether `ty` types as `Text`: the base named type or a string-literal
/// variant row (its base kind is `Text`).
pub fn is_text_type(ty: &Type) -> bool {
    match ty {
        Type::Named(name) => name == "Text",
        Type::Variant(row) => literal_variant_base(row) == Some(LiteralBase::Text),
        _ => false,
    }
}

/// The literal members of a closed literal-union type, in order.
/// `None` for any type that is not a closed all-`Literal` variant row.
pub fn literal_union_members(ty: &Type) -> Option<Vec<String>> {
    let mut ty = ty;
    while let Type::Optional(inner) | Type::Nullable(inner) = ty {
        ty = inner;
    }

    let Type::Variant(row) = ty else {
        return None;
    };
    if row.tail != RowTail::Closed {
        return None;
    }

    row.entries
        .iter()
        .map(|entry| match entry {
            RowEntry::Literal { value } => Some(render_literal_value(value).to_owned()),
            RowEntry::Field { .. } | RowEntry::Tag { .. } => None,
        })
        .collect()
}

pub fn function_signature(ty: &Type) -> Option<(Vec<Type>, Type)> {
    let mut ty = ty;
    while let Type::Optional(inner) | Type::Nullable(inner) = ty {
        ty = inner;
    }

    let Type::Function { params, result, .. } = ty else {
        return None;
    };

    Some((params.clone(), result.as_ref().clone()))
}

/// The required-arity of a function type (peeling `?`/`?`-style wrappers like
/// [`function_signature`]). `None` for non-function types.
pub fn function_required_arity(ty: &Type) -> Option<usize> {
    let mut ty = ty;
    while let Type::Optional(inner) | Type::Nullable(inner) = ty {
        ty = inner;
    }

    match ty {
        Type::Function { required, .. } => Some(*required),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeScheme {
    pub(crate) vars: Vec<u32>,
    pub(crate) row_vars: Vec<u32>,
    pub(crate) row_merges: Vec<RowMergeConstraint>,
    pub(crate) ty: Type,
}

impl TypeScheme {
    pub(crate) fn mono(ty: Type) -> Self {
        Self {
            vars: Vec::new(),
            row_vars: Vec::new(),
            row_merges: Vec::new(),
            ty,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowMergeConstraint {
    pub(crate) result: u32,
    pub(crate) sources: Vec<RowMergeSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowMergeSource {
    pub(crate) row: Row,
    pub(crate) overwrite: bool,
    pub(crate) span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Row {
    pub entries: Vec<RowEntry>,
    pub tail: RowTail,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowEntry {
    Field { name: String, ty: Type },
    Tag { name: String, payload: Vec<Type> },
    Literal { value: Literal },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowTail {
    Closed,
    Open,
    Var(u32),
}

impl RowTail {
    fn render(self) -> String {
        match self {
            RowTail::Closed => String::new(),
            RowTail::Open | RowTail::Var(_) => "..".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Record,
    Variant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiteralBase {
    Bool,
    Text,
    Number,
}

impl LiteralBase {
    pub(crate) fn matches_named(self, name: &str) -> bool {
        match self {
            Self::Bool => name == "Bool",
            Self::Text => name == "Text",
            Self::Number => matches!(name, "Int" | "Float"),
        }
    }
}

pub fn render_type(ty: &Type) -> String {
    TypeRenderer::default().render_type(ty)
}

#[derive(Debug, Default)]
struct TypeRenderer {
    metas: HashMap<u32, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TypePrecedence {
    Arrow,
    Prefix,
    Postfix,
}

impl TypeRenderer {
    fn render_type(&mut self, ty: &Type) -> String {
        self.render_type_with_precedence(ty, TypePrecedence::Arrow)
    }

    fn render_type_with_precedence(&mut self, ty: &Type, parent: TypePrecedence) -> String {
        match ty {
            // `?` is intentionally honest: the checker accepted a syntactic
            // type shape but deferred its real meaning to a later phase.
            Type::Deferred => "?".to_owned(),
            Type::Named(name) | Type::Variable(name) => name.clone(),
            Type::Meta(id) => self.render_meta(*id),
            Type::Apply { callee, args } => {
                let rendered_callee =
                    self.render_type_with_precedence(callee, TypePrecedence::Postfix);
                let rendered_args = args
                    .iter()
                    .map(|arg| self.render_type(arg))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{rendered_callee}({rendered_args})")
            }
            Type::Function {
                params,
                result,
                required,
            } => {
                // Only an all-required single param uses the bare form; an
                // optional param needs both its ` = _` marker and parens so
                // `Int = _ -> Unit` cannot be misread.
                let rendered_params = if params.len() == 1 && *required == 1 {
                    self.render_function_param(&params[0])
                } else {
                    format!(
                        "({})",
                        params
                            .iter()
                            .enumerate()
                            .map(|(index, param)| {
                                let rendered = self.render_type(param);
                                if index < *required {
                                    rendered
                                } else {
                                    format!("{rendered} = _")
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let rendered_result = self.render_type(result);
                let rendered = format!("{rendered_params} -> {rendered_result}");
                if parent > TypePrecedence::Arrow {
                    format!("({rendered})")
                } else {
                    rendered
                }
            }
            Type::Optional(inner) => {
                let rendered_inner =
                    self.render_type_with_precedence(inner, TypePrecedence::Prefix);
                let rendered = format!("?{rendered_inner}");
                if parent > TypePrecedence::Prefix {
                    format!("({rendered})")
                } else {
                    rendered
                }
            }
            Type::Nullable(inner) => {
                let rendered_inner = match inner.as_ref() {
                    Type::Function { .. } => {
                        format!("({})", self.render_type(inner))
                    }
                    _ => self.render_type_with_precedence(inner, TypePrecedence::Postfix),
                };
                format!("{rendered_inner}?")
            }
            Type::Tuple(items) => format!(
                "({})",
                items
                    .iter()
                    .map(|item| self.render_type(item))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Type::Record(row) => self.render_record_row(row),
            Type::Variant(row) => {
                let (rendered, is_multi_part_union) = self.render_variant_row(row);
                if is_multi_part_union && parent > TypePrecedence::Arrow {
                    format!("({rendered})")
                } else {
                    rendered
                }
            }
        }
    }

    fn render_function_param(&mut self, param: &Type) -> String {
        match param {
            Type::Function { .. } => format!("({})", self.render_type(param)),
            _ => self.render_type(param),
        }
    }

    fn render_record_row(&mut self, row: &Row) -> String {
        let mut parts = row
            .entries
            .iter()
            .map(|entry| self.render_row_entry(entry))
            .collect::<Vec<_>>();

        if row.tail != RowTail::Closed {
            parts.push(row.tail.render());
        }

        if parts.is_empty() {
            "{}".to_owned()
        } else {
            format!("{{ {} }}", parts.join(", "))
        }
    }

    fn render_variant_row(&mut self, row: &Row) -> (String, bool) {
        if row.entries.is_empty() {
            return if row.tail == RowTail::Closed {
                ("@{}".to_owned(), false)
            } else {
                ("@{ .. }".to_owned(), false)
            };
        }

        let mut parts = row
            .entries
            .iter()
            .map(|entry| self.render_row_entry(entry))
            .collect::<Vec<_>>();

        if row.tail != RowTail::Closed {
            parts.push(row.tail.render());
        }

        let is_multi_part_union = parts.len() >= 2;
        (parts.join(" | "), is_multi_part_union)
    }

    fn render_row_entry(&mut self, entry: &RowEntry) -> String {
        match entry {
            RowEntry::Field { name, ty } => format!("{name}: {}", self.render_type(ty)),
            RowEntry::Tag { name, payload } if payload.is_empty() => format!("@{name}"),
            RowEntry::Tag { name, payload } => format!(
                "@{name}({})",
                payload
                    .iter()
                    .map(|ty| self.render_type(ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            RowEntry::Literal { value } => render_literal_value(value).to_owned(),
        }
    }

    fn render_meta(&mut self, id: u32) -> String {
        if let Some(name) = self.metas.get(&id) {
            return name.clone();
        }

        let name = meta_name(self.metas.len());
        self.metas.insert(id, name.clone());
        name
    }
}

fn meta_name(index: usize) -> String {
    const NAMES: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";
    NAMES
        .get(index)
        .map(|byte| char::from(*byte).to_string())
        .unwrap_or_else(|| "_".to_owned())
}

/// Rebuild a type, letting `leaf` replace any node (used for substitution and
/// instantiation). Returning `None` keeps the node and recurses structurally.
pub(crate) fn map_type(ty: &Type, leaf: &mut impl FnMut(&Type) -> Option<Type>) -> Type {
    map_type_with_rows(ty, leaf, &mut |_| None)
}

/// Rebuild a type while allowing a row tail to expand into a complete row.
pub(crate) fn map_type_with_rows(
    ty: &Type,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
    tail: &mut impl FnMut(RowTail) -> Option<Row>,
) -> Type {
    if let Some(replaced) = leaf(ty) {
        return replaced;
    }
    match ty {
        Type::Apply { callee, args } => Type::Apply {
            callee: Box::new(map_type_with_rows(callee, leaf, tail)),
            args: args
                .iter()
                .map(|arg| map_type_with_rows(arg, leaf, tail))
                .collect(),
        },
        Type::Function {
            params,
            result,
            required,
        } => Type::Function {
            params: params
                .iter()
                .map(|param| map_type_with_rows(param, leaf, tail))
                .collect(),
            result: Box::new(map_type_with_rows(result, leaf, tail)),
            required: *required,
        },
        Type::Optional(inner) => Type::Optional(Box::new(map_type_with_rows(inner, leaf, tail))),
        Type::Nullable(inner) => Type::Nullable(Box::new(map_type_with_rows(inner, leaf, tail))),
        Type::Tuple(items) => Type::Tuple(
            items
                .iter()
                .map(|item| map_type_with_rows(item, leaf, tail))
                .collect(),
        ),
        Type::Record(row) => Type::Record(map_row(row, leaf, tail)),
        Type::Variant(row) => Type::Variant(map_row(row, leaf, tail)),
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => ty.clone(),
    }
}

fn map_row(
    row: &Row,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
    map_tail: &mut impl FnMut(RowTail) -> Option<Row>,
) -> Row {
    let mut entries: Vec<_> = row
        .entries
        .iter()
        .map(|entry| map_row_entry(entry, leaf, map_tail))
        .collect();
    let tail = if let Some(replacement) = map_tail(row.tail) {
        let replacement = map_row(&replacement, leaf, map_tail);
        entries.extend(replacement.entries);
        replacement.tail
    } else {
        row.tail
    };

    Row { entries, tail }
}

fn map_row_entry(
    entry: &RowEntry,
    leaf: &mut impl FnMut(&Type) -> Option<Type>,
    tail: &mut impl FnMut(RowTail) -> Option<Row>,
) -> RowEntry {
    match entry {
        RowEntry::Field { name, ty } => RowEntry::Field {
            name: name.clone(),
            ty: map_type_with_rows(ty, leaf, tail),
        },
        RowEntry::Tag { name, payload } => RowEntry::Tag {
            name: name.clone(),
            payload: payload
                .iter()
                .map(|ty| map_type_with_rows(ty, leaf, tail))
                .collect(),
        },
        RowEntry::Literal { value } => RowEntry::Literal {
            value: value.clone(),
        },
    }
}

/// Visit every nested type in pre-order (used by the structural predicates).
fn visit_type(ty: &Type, visit: &mut impl FnMut(&Type)) {
    visit_type_with_rows(ty, visit, &mut |_| {});
}

fn visit_type_with_rows(
    ty: &Type,
    visit: &mut impl FnMut(&Type),
    visit_tail: &mut impl FnMut(RowTail),
) {
    visit(ty);
    match ty {
        Type::Apply { callee, args } => {
            visit_type_with_rows(callee, visit, visit_tail);
            args.iter()
                .for_each(|arg| visit_type_with_rows(arg, visit, visit_tail));
        }
        Type::Function { params, result, .. } => {
            params
                .iter()
                .for_each(|param| visit_type_with_rows(param, visit, visit_tail));
            visit_type_with_rows(result, visit, visit_tail);
        }
        Type::Optional(inner) => visit_type_with_rows(inner, visit, visit_tail),
        Type::Nullable(inner) => visit_type_with_rows(inner, visit, visit_tail),
        Type::Tuple(items) => items
            .iter()
            .for_each(|item| visit_type_with_rows(item, visit, visit_tail)),
        Type::Record(row) | Type::Variant(row) => {
            row.entries
                .iter()
                .for_each(|entry| visit_row_entry(entry, visit, visit_tail));
            visit_tail(row.tail);
        }
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => {}
    }
}

fn visit_row_entry(
    entry: &RowEntry,
    visit: &mut impl FnMut(&Type),
    visit_tail: &mut impl FnMut(RowTail),
) {
    match entry {
        RowEntry::Field { ty, .. } => visit_type_with_rows(ty, visit, visit_tail),
        RowEntry::Tag { payload, .. } => payload
            .iter()
            .for_each(|ty| visit_type_with_rows(ty, visit, visit_tail)),
        RowEntry::Literal { .. } => {}
    }
}

pub(crate) fn free_metas(ty: &Type) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut metas = Vec::new();
    visit_type(ty, &mut |node| {
        if let Type::Meta(id) = node
            && seen.insert(*id)
        {
            metas.push(*id);
        }
    });
    metas
}

pub(crate) fn free_row_vars(ty: &Type) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut row_vars = Vec::new();
    visit_type_with_rows(ty, &mut |_| {}, &mut |tail| {
        if let RowTail::Var(id) = tail
            && seen.insert(id)
        {
            row_vars.push(id);
        }
    });
    row_vars
}

pub(crate) fn generalize(resolved: Type, env_metas: &[u32], env_row_vars: &[u32]) -> TypeScheme {
    let env_metas: HashSet<_> = env_metas.iter().copied().collect();
    let env_row_vars: HashSet<_> = env_row_vars.iter().copied().collect();
    let vars = free_metas(&resolved)
        .into_iter()
        .filter(|id| !env_metas.contains(id))
        .collect();
    let row_vars = free_row_vars(&resolved)
        .into_iter()
        .filter(|id| !env_row_vars.contains(id))
        .collect();
    TypeScheme {
        vars,
        row_vars,
        row_merges: Vec::new(),
        ty: resolved,
    }
}

pub(crate) fn type_contains_meta(ty: &Type, id: u32) -> bool {
    let mut found = false;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Meta(candidate) if *candidate == id) {
            found = true;
        }
    });
    found
}

pub(crate) fn is_concrete_type(ty: &Type) -> bool {
    let mut concrete_types = true;
    let mut concrete_rows = true;
    visit_type_with_rows(
        ty,
        &mut |node| {
            if matches!(node, Type::Deferred | Type::Variable(_) | Type::Meta(_)) {
                concrete_types = false;
            }
        },
        &mut |tail| {
            if matches!(tail, RowTail::Var(_)) {
                concrete_rows = false;
            }
        },
    );
    concrete_types && concrete_rows
}

pub fn type_contains_deferred(ty: &Type) -> bool {
    let mut found = false;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Deferred) {
            found = true;
        }
    });
    found
}

pub(crate) fn type_contains_variable(ty: &Type) -> bool {
    let mut found = false;
    visit_type(ty, &mut |node| {
        if matches!(node, Type::Variable(_)) {
            found = true;
        }
    });
    found
}

/// Free `Type::Variable` names in `ty` (annotation binders / skolems).
pub(crate) fn type_variable_names(ty: &Type) -> HashSet<String> {
    let mut names = HashSet::new();
    visit_type(ty, &mut |node| {
        if let Type::Variable(name) = node {
            names.insert(name.clone());
        }
    });
    names
}

pub(crate) fn named_builtin(name: &str) -> Type {
    Type::Named(name.to_owned())
}

pub(crate) fn literal_variant_base(row: &Row) -> Option<LiteralBase> {
    let mut base = None;

    for entry in &row.entries {
        let incoming = match entry {
            RowEntry::Literal { value } => literal_base(value)?,
            RowEntry::Field { .. } | RowEntry::Tag { .. } => return None,
        };

        match base {
            None => base = Some(incoming),
            Some(existing) if existing == incoming => {}
            Some(_) => return None,
        }
    }

    base
}

pub(crate) fn literal_base(literal: &Literal) -> Option<LiteralBase> {
    match literal {
        Literal::Bool(_) => Some(LiteralBase::Bool),
        Literal::String(_) => Some(LiteralBase::Text),
        Literal::Number(_) => Some(LiteralBase::Number),
        Literal::Regex(_) => None,
    }
}

pub(crate) fn open_literal_variant_base(row: &Row) -> Option<LiteralBase> {
    if row.tail == RowTail::Closed {
        return None;
    }

    literal_variant_base(row)
}

pub(crate) fn is_resolved_value_type(ty: &Type) -> bool {
    match ty {
        Type::Deferred | Type::Variable(_) | Type::Meta(_) => false,
        Type::Named(_) => true,
        Type::Apply { callee, args } => {
            is_resolved_value_type(callee) && args.iter().all(is_resolved_value_type)
        }
        Type::Function { params, result, .. } => {
            params.iter().all(is_resolved_value_type) && is_resolved_value_type(result)
        }
        Type::Optional(inner) | Type::Nullable(inner) => is_resolved_value_type(inner),
        Type::Tuple(items) => items.iter().all(is_resolved_value_type),
        Type::Record(row) => {
            !matches!(row.tail, RowTail::Var(_))
                && row.entries.iter().all(|entry| match entry {
                    RowEntry::Field { ty, .. } => is_resolved_value_type(ty),
                    RowEntry::Tag { .. } | RowEntry::Literal { .. } => false,
                })
        }
        Type::Variant(row) if literal_variant_base(row).is_some() => true,
        Type::Variant(row) => {
            !matches!(row.tail, RowTail::Var(_))
                && row.entries.iter().all(|entry| match entry {
                    RowEntry::Tag { payload, .. } => payload.iter().all(is_resolved_value_type),
                    RowEntry::Field { .. } | RowEntry::Literal { .. } => false,
                })
        }
    }
}

pub(crate) fn display_inferred_type(ty: &Type) -> Type {
    map_type(ty, &mut |node| {
        if let Type::Variant(row) = node
            && literal_variant_base(row).is_some()
        {
            let mut displayed = row.clone();
            displayed.tail = RowTail::Closed;
            return Some(Type::Variant(displayed));
        }

        None
    })
}

/// Public type builders so hosts and tests can spell Aven types in Rust
/// without reaching into row internals. These wrap the same `Type`/`Row`
/// representation the checker uses; `check_module_with_globals` consumes the
/// `Type`s they produce.
pub mod build {
    use super::{Row, RowEntry, RowTail, Type};

    /// A named type such as `Text` or a user/host-defined type name.
    pub fn named(name: &str) -> Type {
        Type::Named(name.to_owned())
    }

    /// A named type variable, used by generic host/global signatures.
    pub fn var(name: &str) -> Type {
        Type::Variable(name.to_owned())
    }

    pub fn text() -> Type {
        named("Text")
    }

    /// A closed literal-union type `"a" | "b" | ...` (text singletons).
    pub fn text_literals(values: &[&str]) -> Type {
        Type::Variant(Row {
            entries: values
                .iter()
                .map(|value| RowEntry::Literal {
                    value: aven_parser::Literal::String(format!("{value:?}")),
                })
                .collect(),
            tail: RowTail::Closed,
        })
    }

    pub fn int() -> Type {
        named("Int")
    }

    pub fn float() -> Type {
        named("Float")
    }

    pub fn bool() -> Type {
        named("Bool")
    }

    pub fn unit() -> Type {
        named("Unit")
    }

    /// A function type `(params...) -> result` where every param is required.
    pub fn function(params: Vec<Type>, result: Type) -> Type {
        let required = params.len();
        Type::Function {
            params,
            result: Box::new(result),
            required,
        }
    }

    /// A function type with required leading params followed by optional
    /// (defaulted) trailing params, e.g. `function_opt(vec![text()],
    /// vec![open_record(vec![])], unit())` for one required `Text` and one
    /// optional fields record.
    pub fn function_opt(required: Vec<Type>, optional: Vec<Type>, result: Type) -> Type {
        let required_arity = required.len();
        Type::Function {
            params: required.into_iter().chain(optional).collect(),
            result: Box::new(result),
            required: required_arity,
        }
    }

    /// A closed record `{ field: ty, ... }`.
    pub fn record(fields: Vec<(&str, Type)>) -> Type {
        Type::Record(record_row(fields, RowTail::Closed))
    }

    /// A closed variant `@{ Tag(payload...), ... }` built from `(tag, payload)`
    /// pairs. Mirrors [`record`] for the variant side so hosts spell tagged
    /// unions (e.g. closed error types) without hand-rolling rows.
    pub fn variant(tags: Vec<(&str, Vec<Type>)>) -> Type {
        Type::Variant(Row {
            entries: tags
                .into_iter()
                .map(|(name, payload)| RowEntry::Tag {
                    name: name.to_owned(),
                    payload,
                })
                .collect(),
            tail: RowTail::Closed,
        })
    }

    /// An applied type `Callee(args...)` where `callee` is a named type
    /// constructor (e.g. `Boxed(value)`, `Result(ok, err)`). The constructor is
    /// opaque to unification: `Apply` unifies structurally by callee + args.
    pub fn apply(name: &str, args: Vec<Type>) -> Type {
        Type::Apply {
            callee: Box::new(named(name)),
            args,
        }
    }

    /// The applied `Result(ok, err)` type. This is the surface representation of
    /// `Result` in this codebase (`Apply { Result, [ok, err] }`); the runtime
    /// inhabits it with `@Ok(ok)` / `@Err(err)` tag values.
    pub fn result(ok: Type, err: Type) -> Type {
        Type::Apply {
            callee: Box::new(named("Result")),
            args: vec![ok, err],
        }
    }

    /// The applied `Map(key, value)` type.
    pub fn map(key: Type, value: Type) -> Type {
        Type::Apply {
            callee: Box::new(named("Map")),
            args: vec![key, value],
        }
    }

    /// The collection type `Array elem` (`Apply Named("Array") [elem]`).
    pub fn array(element: Type) -> Type {
        Type::Apply {
            callee: Box::new(Type::Named("Array".to_owned())),
            args: vec![element],
        }
    }

    /// The closed empty record `{}`.
    pub fn empty_record() -> Type {
        record(vec![])
    }

    /// An open record `{ field: ty, ..., .. }` that admits extra fields.
    pub fn open_record(fields: Vec<(&str, Type)>) -> Type {
        Type::Record(record_row(fields, RowTail::Open))
    }

    /// An optional type `?ty` (may be absent / `undefined`).
    pub fn optional(ty: Type) -> Type {
        Type::Optional(Box::new(ty))
    }

    /// A nullable type `ty?` (may be `null`).
    pub fn nullable(ty: Type) -> Type {
        Type::Nullable(Box::new(ty))
    }

    fn record_row(fields: Vec<(&str, Type)>, tail: RowTail) -> Row {
        Row {
            entries: fields
                .into_iter()
                .map(|(name, ty)| RowEntry::Field {
                    name: name.to_owned(),
                    ty,
                })
                .collect(),
            tail,
        }
    }
}

pub(crate) fn render_literal_value(literal: &Literal) -> &str {
    match literal {
        Literal::Bool(true) => "true",
        Literal::Bool(false) => "false",
        Literal::Number(value) | Literal::String(value) | Literal::Regex(value) => value,
    }
}

pub(crate) fn named_type_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) => Some(name),
        Type::Deferred
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Apply { .. }
        | Type::Function { .. }
        | Type::Optional(_)
        | Type::Nullable(_)
        | Type::Tuple(_)
        | Type::Record(_)
        | Type::Variant(_) => None,
    }
}

pub(crate) fn numeric_type_name(ty: &Type) -> Option<&'static str> {
    match named_type_name(ty) {
        Some("Int") => Some("Int"),
        Some("Float") => Some("Float"),
        _ => None,
    }
}

pub(crate) fn is_meta_type(ty: &Type) -> bool {
    matches!(ty, Type::Meta(_))
}

pub(crate) fn mismatched_literal_kind(expected: &str, literal: &Literal) -> Option<&'static str> {
    match (expected, literal) {
        ("Bool", Literal::Bool(_)) => None,
        ("Text", Literal::String(_)) | ("Int" | "Float", Literal::Number(_)) => None,
        ("Int" | "Float" | "Bool" | "Null" | "Undefined" | "Unit", Literal::String(_)) => {
            Some("text literal")
        }
        ("Text" | "Bool" | "Null" | "Undefined" | "Unit", Literal::Number(_)) => {
            Some("number literal")
        }
        ("Text" | "Int" | "Float" | "Null" | "Undefined" | "Unit", Literal::Bool(_)) => {
            Some("bool literal")
        }
        // Core scalars that accept the literal are handled above. Any other
        // named expectation rejects the literal (including host nominals like
        // `Data` / `Instant`). Callers should only surface this when `expected`
        // is a known type name — unknown names already report
        // `type.unknown-name` and stay unconstrained.
        _ => Some(match literal {
            Literal::Bool(_) => "bool literal",
            Literal::String(_) => "text literal",
            Literal::Number(_) => "number literal",
            Literal::Regex(_) => "regex literal",
        }),
    }
}

/// Distinct resolved named types never unify. Callers must only invoke this for
/// `Type::Named` pairs (not Deferred/Meta/Variable).
pub(crate) fn named_type_mismatch(expected: &str, actual: &str) -> bool {
    expected != actual
}

pub(crate) fn is_undefined_value(value: &Expr) -> bool {
    matches!(&value.kind, ExprKind::Undefined)
}

pub(crate) fn is_null_value(value: &Expr) -> bool {
    matches!(&value.kind, ExprKind::Null)
}

#[cfg(test)]
mod tests {
    use super::{Type, build};

    #[test]
    fn build_array_round_trips_through_apply() {
        assert_eq!(
            build::array(build::text()),
            Type::Apply {
                callee: Box::new(Type::Named("Array".to_owned())),
                args: vec![build::text()],
            }
        );
    }
}
