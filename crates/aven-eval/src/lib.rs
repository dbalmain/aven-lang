use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fmt,
    rc::Rc,
};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    Expr, ExprKind, InterpolationSegment, Item, Literal, MatchArm, Module, PropagationMode,
    RecordEntry, decode_string_literal,
};

pub mod logging;

/// The evaluator's control-flow channel. Most failures are ordinary runtime
/// errors ([`Flow::Fail`]); [`Flow::Propagate`] carries an `@Err` value that is
/// early-returning from the enclosing function via `?^`. Both bubble through `?`;
/// `Propagate` is caught only at the closure body and the top-level item loop.
enum Flow {
    /// A real runtime error: one or more diagnostics.
    Fail(Vec<Diagnostic>),
    /// An `@Err` value early-returning from the enclosing function (`?^`).
    Propagate(Value),
}

/// Internal evaluator result. `Ok` is the produced value; `Err` is a [`Flow`].
type Eval<T = Value> = Result<T, Flow>;

pub type NativeFn = Rc<dyn Fn(&[Value]) -> Result<Value, String>>;

#[derive(Clone)]
pub struct Closure {
    params: Vec<ClosureParam>,
    body: Rc<Expr>,
    env: Environment,
}

/// A closure parameter: its binding name plus an optional default expression
/// (trailing-only, enforced by the parser/checker). The default is evaluated in
/// the call environment, in parameter order, only when the argument is omitted.
#[derive(Clone, Debug)]
struct ClosureParam {
    name: String,
    default: Option<Rc<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeType {
    Named(String),
    Optional(Box<Value>),
    Nullable(Box<Value>),
    Array(Box<Value>),
}

impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Closure")
            .field("params", &self.params)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Array(Rc<Vec<Value>>),
    Tuple(Rc<Vec<Value>>),
    Set(Rc<Vec<Value>>),
    Map(Rc<Vec<(Value, Value)>>),
    Record(Rc<Vec<(String, Value)>>),
    Tag {
        name: String,
        payload: Vec<Value>,
    },
    Closure(Closure),
    Native(NativeFn),
    /// A runtime type descriptor. The evaluator keeps this intentionally small:
    /// named types plus the composite shapes JSON decode needs. Record types
    /// remain ordinary `Value::Record` values whose fields are type values.
    Type(RuntimeType),
    Undefined,
    Null,
}

/// Type names bound as `Value::Type` intrinsics. `Array`/`Json` are included so
/// `Array[T]` and dynamic JSON decode targets can evaluate to the minimal
/// composite type values JSON decode needs at runtime.
const TYPE_VALUE_NAMES: [&str; 9] = [
    "Array",
    "Bool",
    "Float",
    "Int",
    "Json",
    "Null",
    "Text",
    "Undefined",
    "Unit",
];

pub const MAP_METHOD_NAMES: &[&str] = &[
    "get", "set", "delete", "has", "keys", "values", "entries", "size", "merge",
];

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => f.debug_tuple("Int").field(value).finish(),
            Self::Float(value) => f.debug_tuple("Float").field(value).finish(),
            Self::Text(value) => f.debug_tuple("Text").field(value).finish(),
            Self::Bool(value) => f.debug_tuple("Bool").field(value).finish(),
            Self::Array(values) => f.debug_tuple("Array").field(values).finish(),
            Self::Tuple(values) => f.debug_tuple("Tuple").field(values).finish(),
            Self::Set(values) => f.debug_tuple("Set").field(values).finish(),
            Self::Map(entries) => f.debug_tuple("Map").field(entries).finish(),
            Self::Record(fields) => f.debug_tuple("Record").field(fields).finish(),
            Self::Tag { name, payload } => f
                .debug_struct("Tag")
                .field("name", name)
                .field("payload", payload)
                .finish(),
            Self::Closure(closure) => f.debug_tuple("Closure").field(closure).finish(),
            Self::Native(_) => f.write_str("Native(<native>)"),
            Self::Type(ty) => f.debug_tuple("Type").field(ty).finish(),
            Self::Undefined => f.write_str("Undefined"),
            Self::Null => f.write_str("Null"),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Tuple(left), Self::Tuple(right)) => left == right,
            (Self::Set(left), Self::Set(right)) => sets_equal(left, right),
            (Self::Map(left), Self::Map(right)) => maps_equal(left, right),
            (Self::Record(left), Self::Record(right)) => records_equal(left, right),
            (
                Self::Tag {
                    name: left_name,
                    payload: left_payload,
                },
                Self::Tag {
                    name: right_name,
                    payload: right_payload,
                },
            ) => left_name == right_name && left_payload == right_payload,
            (Self::Type(left), Self::Type(right)) => left == right,
            (Self::Undefined, Self::Undefined) | (Self::Null, Self::Null) => true,
            (Self::Closure(_), _) | (_, Self::Closure(_)) => false,
            (Self::Native(_), _) | (_, Self::Native(_)) => false,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::Text(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Array(values) => fmt_array(values, f),
            Self::Tuple(values) => fmt_tuple(values, f),
            Self::Set(values) => fmt_set(values, f),
            Self::Map(entries) => fmt_map(entries, f),
            Self::Record(fields) => fmt_record(fields, f),
            Self::Tag { name, payload } => fmt_tag(name, payload, f),
            Self::Closure(_) => write!(f, "<function>"),
            Self::Native(_) => write!(f, "<native>"),
            Self::Type(ty) => write!(f, "{ty}"),
            Self::Undefined => write!(f, "undefined"),
            Self::Null => write!(f, "null"),
        }
    }
}

impl Value {
    pub fn native(function: impl Fn(&[Value]) -> Result<Value, String> + 'static) -> Self {
        Self::Native(Rc::new(function))
    }

    pub fn record(fields: Vec<(String, Value)>) -> Self {
        Self::Record(Rc::new(fields))
    }

    pub fn named_type(name: impl Into<String>) -> Self {
        Self::Type(RuntimeType::Named(name.into()))
    }

    pub fn unit() -> Self {
        Self::Tuple(Rc::new(Vec::new()))
    }

    pub fn is_unit(&self) -> bool {
        matches!(self, Self::Tuple(values) if values.is_empty())
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Text(_) => "Text",
            Self::Bool(_) => "Bool",
            Self::Array(_) => "Array",
            Self::Tuple(_) => "Tuple",
            Self::Set(_) => "Set",
            Self::Map(_) => "Map",
            Self::Record(_) => "Record",
            Self::Tag { .. } => "Tag",
            Self::Closure(_) => "Function",
            Self::Native(_) => "Native",
            Self::Type(_) => "Type",
            Self::Undefined => "Undefined",
            Self::Null => "Null",
        }
    }
}

impl fmt::Display for RuntimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => write!(f, "{name}"),
            Self::Optional(inner) => write!(f, "?{inner}"),
            Self::Nullable(inner) => write!(f, "{inner}?"),
            Self::Array(inner) => write!(f, "Array[{inner}]"),
        }
    }
}

fn sets_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len() && left.iter().all(|value| contains_value(right, value))
}

fn contains_value(values: &[Value], needle: &Value) -> bool {
    values.iter().any(|value| value == needle)
}

fn maps_equal(left: &[(Value, Value)], right: &[(Value, Value)]) -> bool {
    left.len() == right.len()
        && left.iter().all(|(key, value)| {
            map_entry_value(right, key).is_some_and(|right_value| value == right_value)
        })
}

fn records_equal(left: &[(String, Value)], right: &[(String, Value)]) -> bool {
    left.len() == right.len()
        && left.iter().all(|(name, value)| {
            record_field_value(right, name).is_some_and(|right_value| value == right_value)
        })
}

fn fmt_array(values: &[Value], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    fmt_sequence("[", "]", values, f)
}

fn fmt_tuple(values: &[Value], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    fmt_sequence("(", ")", values, f)
}

fn fmt_sequence(
    open: &str,
    close: &str,
    values: &[Value],
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    write!(f, "{open}")?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            write!(f, ", ")?;
        }
        fmt_nested_value(value, f)?;
    }
    write!(f, "{close}")
}

fn fmt_set(values: &[Value], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "@{{")?;
    for (index, value) in values.iter().enumerate() {
        if index == 0 {
            write!(f, " ")?;
        } else {
            write!(f, ", ")?;
        }
        fmt_nested_value(value, f)?;
    }
    if !values.is_empty() {
        write!(f, " ")?;
    }
    write!(f, "}}")
}

fn fmt_map(entries: &[(Value, Value)], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Map{{")?;
    for (index, (key, value)) in entries.iter().enumerate() {
        if index == 0 {
            write!(f, " ")?;
        } else {
            write!(f, ", ")?;
        }
        fmt_nested_value(key, f)?;
        write!(f, ": ")?;
        fmt_nested_value(value, f)?;
    }
    if !entries.is_empty() {
        write!(f, " ")?;
    }
    write!(f, "}}")
}

fn fmt_record(fields: &[(String, Value)], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{{")?;
    for (index, (name, value)) in fields.iter().enumerate() {
        if index == 0 {
            write!(f, " ")?;
        } else {
            write!(f, ", ")?;
        }
        write!(f, "{name}: ")?;
        fmt_nested_value(value, f)?;
    }
    if !fields.is_empty() {
        write!(f, " ")?;
    }
    write!(f, "}}")
}

fn fmt_tag(name: &str, payload: &[Value], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "@{name}")?;
    if !payload.is_empty() {
        write!(f, "(")?;
        for (index, value) in payload.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            fmt_nested_value(value, f)?;
        }
        write!(f, ")")?;
    }
    Ok(())
}

fn fmt_nested_value(value: &Value, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match value {
        Value::Text(text) => write!(f, "\"{}\"", escape_string(text)),
        Value::Array(values) => fmt_array(values, f),
        Value::Tuple(values) => fmt_tuple(values, f),
        Value::Set(values) => fmt_set(values, f),
        Value::Map(entries) => fmt_map(entries, f),
        Value::Record(fields) => fmt_record(fields, f),
        Value::Tag { name, payload } => fmt_tag(name, payload, f),
        value => write!(f, "{value}"),
    }
}

fn escape_string(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[derive(Clone)]
pub struct Environment {
    scope: Rc<Scope>,
}

struct Scope {
    values: RefCell<HashMap<String, Value>>,
    parent: Option<Rc<Scope>>,
}

impl Scope {
    fn new(parent: Option<Rc<Scope>>) -> Self {
        Self {
            values: RefCell::new(HashMap::new()),
            parent,
        }
    }
}

impl fmt::Debug for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment")
            .field("scope", &Rc::as_ptr(&self.scope))
            .finish()
    }
}

impl PartialEq for Environment {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.scope, &other.scope)
    }
}

impl Environment {
    pub fn new() -> Self {
        Self {
            scope: Rc::new(Scope::new(None)),
        }
    }

    fn child(&self) -> Self {
        Self {
            scope: Rc::new(Scope::new(Some(Rc::clone(&self.scope)))),
        }
    }

    pub fn bind(&self, name: impl Into<String>, value: Value) {
        self.scope.values.borrow_mut().insert(name.into(), value);
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        let mut scope = Some(Rc::clone(&self.scope));

        while let Some(current) = scope {
            let value = { current.values.borrow().get(name).cloned() };
            if value.is_some() {
                return value;
            }
            scope = current.parent.as_ref().map(Rc::clone);
        }

        None
    }
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalOutcome {
    pub value: Option<Value>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Evaluate module items sequentially. Bindings update the environment for
/// later items, and the outcome value is produced only by a trailing expression.
pub fn eval_module(module: &Module) -> EvalOutcome {
    eval_module_with_globals(module, Vec::new())
}

/// Evaluate a module with host-provided globals pre-bound in the top-level
/// environment. Module bindings use normal top-level scope rules and may shadow
/// an injected global by rebinding the same name.
pub fn eval_module_with_globals(module: &Module, globals: Vec<(String, Value)>) -> EvalOutcome {
    let env = Environment::new();
    bind_intrinsics(&env);
    for (name, value) in globals {
        env.bind(name, value);
    }
    // Top-level: a propagated `@Err` (`?^` with no enclosing function) becomes
    // the program value and stops further items.
    match eval_items(&module.items, &env) {
        Ok(outcome) => outcome,
        Err(Flow::Propagate(value)) => EvalOutcome {
            value: Some(value),
            diagnostics: Vec::new(),
        },
        Err(Flow::Fail(diagnostics)) => EvalOutcome {
            value: None,
            diagnostics,
        },
    }
}

fn bind_intrinsics(env: &Environment) {
    for (name, value) in intrinsics() {
        env.bind(name, value);
    }
}

fn intrinsics() -> Vec<(String, Value)> {
    let mut intrinsics: Vec<(String, Value)> = TYPE_VALUE_NAMES
        .iter()
        .map(|name| ((*name).to_owned(), Value::named_type(*name)))
        .collect();

    intrinsics.push((
        "keysOf".to_owned(),
        Value::native(|args| {
            if args.len() != 1 {
                return Err(format!("keysOf expects 1 argument, got {}", args.len()));
            }

            let Value::Record(fields) = &args[0] else {
                return Err(format!(
                    "keysOf expects a Record, got {}",
                    args[0].type_name()
                ));
            };

            Ok(Value::Set(Rc::new(
                fields
                    .iter()
                    .map(|(name, _)| Value::Text(name.clone()))
                    .collect(),
            )))
        }),
    ));

    intrinsics.push(("Map".to_owned(), map_namespace()));

    intrinsics.push((
        "pick".to_owned(),
        Value::native(|args| select_record_fields("pick", args, true)),
    ));

    intrinsics.push((
        "omit".to_owned(),
        Value::native(|args| select_record_fields("omit", args, false)),
    ));

    intrinsics
}

fn map_namespace() -> Value {
    Value::record(vec![
        ("empty".to_owned(), Value::native(map_empty_intrinsic)),
        ("from".to_owned(), Value::native(map_from_intrinsic)),
    ])
}

fn map_empty_intrinsic(args: &[Value]) -> Result<Value, String> {
    if !args.is_empty() {
        return Err(format!("Map.empty expects 0 arguments, got {}", args.len()));
    }

    Ok(Value::Map(Rc::new(Vec::new())))
}

fn map_from_intrinsic(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("Map.from expects 1 argument, got {}", args.len()));
    }

    let Value::Array(items) = &args[0] else {
        return Err(format!(
            "Map.from expects an Array of key/value tuples, got {}",
            args[0].type_name()
        ));
    };

    let mut entries = Vec::new();
    for item in items.iter() {
        let Value::Tuple(values) = item else {
            return Err(format!(
                "Map.from expects (key, value) tuple entries, got {}",
                item.type_name()
            ));
        };
        let [key, value] = values.as_slice() else {
            return Err(format!(
                "Map.from expects 2-item tuples, got tuple with {} items",
                values.len()
            ));
        };
        ensure_map_key(key, "Map.from")?;
        insert_or_replace_map_entry(&mut entries, key.clone(), value.clone());
    }

    Ok(Value::Map(Rc::new(entries)))
}

/// Shared body of the `pick`/`omit` intrinsics. Both take `(record, labels)` —
/// a `Record` and a `Set` of `Text` labels (the shape `keysOf`/`@{...}` yield) —
/// and return a new `Record` preserving the source field order, keeping the
/// labelled fields when `keep_matched` is set (`pick`) or dropping them (`omit`).
/// A label absent from the record is simply skipped (intersection semantics).
/// Shape errors surface as `runtime.platform-error`.
fn select_record_fields(name: &str, args: &[Value], keep_matched: bool) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!("{name} expects 2 arguments, got {}", args.len()));
    }

    let Value::Record(fields) = &args[0] else {
        return Err(format!(
            "{name} expects a Record, got {}",
            args[0].type_name()
        ));
    };

    let Value::Set(members) = &args[1] else {
        return Err(format!(
            "{name} expects a Set of labels, got {}",
            args[1].type_name()
        ));
    };

    let labels = members
        .iter()
        .map(|member| match member {
            Value::Text(label) => Ok(label.as_str()),
            other => Err(format!(
                "{name} expects Text labels, got {}",
                other.type_name()
            )),
        })
        .collect::<Result<HashSet<_>, _>>()?;

    Ok(Value::Record(Rc::new(
        fields
            .iter()
            .filter(|(field, _)| labels.contains(field.as_str()) == keep_matched)
            .cloned()
            .collect(),
    )))
}

/// Evaluate a sequence of items, collecting `Flow::Fail` diagnostics across them
/// (recovery) while letting `Flow::Propagate` bubble out via `?`. Both the
/// top-level loop and blocks share this; only their callers decide whether to
/// catch `Propagate`.
fn eval_items(items: &[Item], env: &Environment) -> Eval<EvalOutcome> {
    let mut value = None;
    let mut diagnostics = Vec::new();

    for item in items {
        match item {
            Item::Expr(expr) => match eval_expr_many(expr, env) {
                Ok(next_value) => value = Some(next_value),
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
                Err(Flow::Fail(mut next_diagnostics)) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::Binding(binding) => match eval_expr_many(&binding.value, env) {
                Ok(next_value) => {
                    env.bind(binding.name.clone(), next_value);
                    value = None;
                }
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
                Err(Flow::Fail(mut next_diagnostics)) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::Signature(_) => value = None,
        }
    }

    Ok(EvalOutcome { value, diagnostics })
}

pub fn eval_expr(expr: &Expr, env: &Environment) -> Result<Value, Diagnostic> {
    eval_expr_many(expr, env).map_err(first_diagnostic)
}

fn eval_expr_many(expr: &Expr, env: &Environment) -> Eval {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(literal, expr.span).map_err(one_diagnostic),
        ExprKind::Interpolation(segments) => eval_interpolation(segments, env),
        ExprKind::Undefined => Ok(Value::Undefined),
        ExprKind::Null => Ok(Value::Null),
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => env
            .lookup(name)
            .ok_or_else(|| one_diagnostic(unbound_name(name, expr.span))),
        ExprKind::Group(inner) => eval_expr_many(inner, env),
        ExprKind::Optional(inner) => {
            eval_type_wrapper(inner, expr.span, env, RuntimeType::Optional)
        }
        ExprKind::Nullable(inner) => {
            eval_type_wrapper(inner, expr.span, env, RuntimeType::Nullable)
        }
        ExprKind::Unary {
            operator, value, ..
        } => eval_unary(operator, value, expr.span, env),
        ExprKind::Binary {
            left,
            operator,
            operator_span,
            right,
        } => eval_binary(left, operator, *operator_span, right, expr.span, env),
        ExprKind::Block(items) => eval_block(items, env),
        ExprKind::Lambda { params, body, .. } => Ok(Value::Closure(Closure {
            params: params
                .iter()
                .map(|param| ClosureParam {
                    name: param.name.clone(),
                    default: param.default.clone().map(Rc::new),
                })
                .collect(),
            body: Rc::new((**body).clone()),
            env: env.clone(),
        })),
        ExprKind::Tag(name) => Ok(Value::Tag {
            name: name.clone(),
            payload: Vec::new(),
        }),
        ExprKind::Array(items) => eval_array(items, env),
        ExprKind::Tuple(items) => eval_tuple(items, env),
        ExprKind::Set(entries) => eval_set(entries, env),
        ExprKind::Record(entries) => eval_record(entries, env),
        ExprKind::Match { subject, arms, .. } => eval_match(subject, arms, expr.span, env),
        ExprKind::FieldAccess {
            receiver,
            field,
            field_span,
            null_safe,
        } => eval_field_access(receiver, field, *field_span, *null_safe, env),
        ExprKind::Index { callee, args } => eval_index(callee, args, expr.span, env),
        ExprKind::Call { callee, args } => eval_call(callee, args, expr.span, env),
        ExprKind::Propagate {
            value,
            operator_span,
            mode,
        } => eval_propagate(value, *operator_span, *mode, env),
        _ => Err(one_diagnostic(unsupported_expr(
            expr.span,
            "this expression is not supported by the current evaluator",
        ))),
    }
}

fn eval_match(subject: &Expr, arms: &[MatchArm], span: Span, env: &Environment) -> Eval {
    let subject_value = eval_expr_many(subject, env)?;

    for arm in arms {
        let Some(bindings) =
            match_pattern(&arm.pattern, &subject_value, env).map_err(one_diagnostic)?
        else {
            continue;
        };

        let arm_env = env.child();
        for (name, value) in bindings {
            arm_env.bind(name, value);
        }

        if guards_pass(&arm.guards, &arm_env)? {
            return eval_expr_many(&arm.body, &arm_env);
        }
    }

    Err(one_diagnostic(no_match(span)))
}

fn guards_pass(guards: &[Expr], env: &Environment) -> Eval<bool> {
    for guard in guards {
        match eval_expr_many(guard, env)? {
            Value::Bool(true) => {}
            Value::Bool(false) => return Ok(false),
            value => {
                return Err(one_diagnostic(guard_type_error(
                    guard.span,
                    value.type_name(),
                )));
            }
        }
    }

    Ok(true)
}

fn match_pattern(
    pattern: &Expr,
    value: &Value,
    env: &Environment,
) -> Result<Option<Vec<(String, Value)>>, Diagnostic> {
    match &pattern.kind {
        ExprKind::Group(inner) => match_pattern(inner, value, env),
        ExprKind::Name(name) if name == "_" => Ok(Some(Vec::new())),
        ExprKind::Name(name) => Ok(bind_pattern_name(name, value)),
        ExprKind::Undefined => Ok((value == &Value::Undefined).then_some(Vec::new())),
        ExprKind::Null => Ok((value == &Value::Null).then_some(Vec::new())),
        ExprKind::Literal(literal) => match_literal_pattern(literal, pattern.span, value),
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } if operator == "|" => match_or_pattern(left, right, value, env),
        ExprKind::Tag(name) => match value {
            Value::Tag {
                name: value_name,
                payload,
            } if value_name == name && payload.is_empty() => Ok(Some(Vec::new())),
            _ => Ok(None),
        },
        ExprKind::Call { callee, args } => match_tag_payload_pattern(callee, args, value, env),
        ExprKind::Record(entries) => match_record_pattern(entries, value, env),
        ExprKind::Tuple(items) => match_tuple_pattern(items, value, env),
        _ => Ok(None),
    }
}

fn match_or_pattern(
    left: &Expr,
    right: &Expr,
    value: &Value,
    env: &Environment,
) -> Result<Option<Vec<(String, Value)>>, Diagnostic> {
    if let Some(bindings) = match_pattern(left, value, env)? {
        return Ok(Some(bindings));
    }

    match_pattern(right, value, env)
}

fn bind_pattern_name(name: &str, value: &Value) -> Option<Vec<(String, Value)>> {
    if matches!(value, Value::Undefined | Value::Null) {
        None
    } else {
        Some(vec![(name.to_owned(), value.clone())])
    }
}

fn match_literal_pattern(
    literal: &Literal,
    span: Span,
    value: &Value,
) -> Result<Option<Vec<(String, Value)>>, Diagnostic> {
    match literal {
        Literal::Bool(_) | Literal::Number(_) | Literal::String(_) => {
            let literal_value = eval_literal(literal, span)?;
            Ok((literal_value == *value).then_some(Vec::new()))
        }
        Literal::Regex(_) | Literal::Path(_) => Ok(None),
    }
}

fn match_tag_payload_pattern(
    callee: &Expr,
    args: &[Expr],
    value: &Value,
    env: &Environment,
) -> Result<Option<Vec<(String, Value)>>, Diagnostic> {
    let ExprKind::Tag(name) = &callee.kind else {
        return Ok(None);
    };

    let Value::Tag {
        name: value_name,
        payload,
    } = value
    else {
        return Ok(None);
    };

    if value_name != name || payload.len() != args.len() {
        return Ok(None);
    }

    let mut bindings = Vec::new();
    for (pattern, value) in args.iter().zip(payload) {
        let Some(mut next_bindings) = match_pattern(pattern, value, env)? else {
            return Ok(None);
        };
        bindings.append(&mut next_bindings);
    }

    Ok(Some(bindings))
}

fn match_tuple_pattern(
    items: &[Expr],
    value: &Value,
    env: &Environment,
) -> Result<Option<Vec<(String, Value)>>, Diagnostic> {
    let Value::Tuple(values) = value else {
        return Ok(None);
    };

    if values.len() != items.len() {
        return Ok(None);
    }

    let mut bindings = Vec::new();
    for (pattern, value) in items.iter().zip(values.iter()) {
        let Some(mut next_bindings) = match_pattern(pattern, value, env)? else {
            return Ok(None);
        };
        bindings.append(&mut next_bindings);
    }

    Ok(Some(bindings))
}

fn match_record_pattern(
    entries: &[RecordEntry],
    value: &Value,
    env: &Environment,
) -> Result<Option<Vec<(String, Value)>>, Diagnostic> {
    let Value::Record(fields) = value else {
        return Ok(None);
    };

    let mut bindings = Vec::new();

    for entry in entries {
        match entry {
            RecordEntry::Field { name, value, .. } => {
                let Some(field_value) = record_field_value(fields, name) else {
                    return Ok(None);
                };
                let Some(mut next_bindings) = match_pattern(value, field_value, env)? else {
                    return Ok(None);
                };
                bindings.append(&mut next_bindings);
            }
            RecordEntry::Shorthand { name, .. } => {
                let Some(field_value) = record_field_value(fields, name) else {
                    return Ok(None);
                };
                let Some(mut next_bindings) = bind_pattern_name(name, field_value) else {
                    return Ok(None);
                };
                bindings.append(&mut next_bindings);
            }
            RecordEntry::Open { .. } | RecordEntry::Spread { .. } => {}
            _ => return Ok(None),
        }
    }

    Ok(Some(bindings))
}

fn eval_block(items: &[Item], env: &Environment) -> Eval {
    let child = env.child();
    // `?` lets a `Flow::Propagate` from a binding value bubble past the block to
    // the enclosing function; blocks only recover `Flow::Fail`.
    let outcome = eval_items(items, &child)?;

    if outcome.diagnostics.is_empty() {
        Ok(outcome.value.unwrap_or(Value::Undefined))
    } else {
        Err(Flow::Fail(outcome.diagnostics))
    }
}

fn eval_call(callee: &Expr, args: &[Expr], span: Span, env: &Environment) -> Eval {
    if let ExprKind::Tag(name) = &callee.kind {
        let mut payload = Vec::with_capacity(args.len());
        for arg in args {
            payload.push(eval_expr_many(arg, env)?);
        }

        return Ok(Value::Tag {
            name: name.clone(),
            payload,
        });
    }

    let callee_value = eval_expr_many(callee, env)?;
    let closure = match callee_value {
        Value::Native(function) => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }

            return function(&arg_values)
                .map_err(|message| one_diagnostic(platform_error(span, message)));
        }
        Value::Closure(closure) => closure,
        value => return Err(one_diagnostic(not_callable(callee.span, value.type_name()))),
    };

    let total = closure.params.len();
    // Defaults are trailing, so the required count is the run of leading params
    // that have no default.
    let required = closure
        .params
        .iter()
        .take_while(|param| param.default.is_none())
        .count();

    if args.len() < required || args.len() > total {
        return Err(one_diagnostic(arity_mismatch(
            span,
            required,
            total,
            args.len(),
        )));
    }

    let mut arg_values = Vec::with_capacity(args.len());
    for arg in args {
        arg_values.push(eval_expr_many(arg, env)?);
    }

    let call_env = closure.env.child();
    for (param, value) in closure.params.iter().zip(arg_values) {
        call_env.bind(param.name.clone(), value);
    }
    // Bind each omitted trailing param by evaluating its default in `call_env`,
    // in order, so a later default may reference an earlier parameter. A default
    // runs only when its argument is omitted; failures propagate via `?`.
    for param in &closure.params[args.len()..] {
        let default = param
            .default
            .as_ref()
            .expect("omitted params past `required` always carry a default");
        let value = eval_expr_many(default, &call_env)?;
        call_env.bind(param.name.clone(), value);
    }

    // The closure body is a propagation boundary: a `?^` `@Err` early-returns the
    // function, so its `@Err` becomes the call's value. `Flow::Fail` still bubbles.
    match eval_expr_many(closure.body.as_ref(), &call_env) {
        Err(Flow::Propagate(value)) => Ok(value),
        other => other,
    }
}

/// Evaluate `expr?^` / `expr?!`. `Result` is the ordinary tagged value
/// `@Ok(v)` / `@Err(e)`; there is no dedicated Result value.
fn eval_propagate(
    value: &Expr,
    operator_span: Span,
    mode: PropagationMode,
    env: &Environment,
) -> Eval {
    let result = eval_expr_many(value, env)?;

    let Value::Tag { name, payload } = &result else {
        return Err(one_diagnostic(propagate_type_error(operator_span)));
    };

    match (name.as_str(), payload.as_slice()) {
        ("Ok", [inner]) => Ok(inner.clone()),
        ("Err", [_]) => match mode {
            // `?^` early-returns the enclosing function with the whole `@Err`.
            PropagationMode::ReturnError => Err(Flow::Propagate(result)),
            // `?!` panics, embedding the `@Err` payload in the diagnostic.
            PropagationMode::Panic => Err(one_diagnostic(panic(operator_span, &payload[0]))),
        },
        _ => Err(one_diagnostic(propagate_type_error(operator_span))),
    }
}

fn eval_array(items: &[Expr], env: &Environment) -> Eval {
    let mut values = Vec::with_capacity(items.len());

    for item in items {
        values.push(eval_expr_many(item, env)?);
    }

    Ok(Value::Array(Rc::new(values)))
}

fn eval_tuple(items: &[Expr], env: &Environment) -> Eval {
    let mut values = Vec::with_capacity(items.len());

    for item in items {
        values.push(eval_expr_many(item, env)?);
    }

    Ok(Value::Tuple(Rc::new(values)))
}

fn eval_set(entries: &[RecordEntry], env: &Environment) -> Eval {
    let mut values = Vec::new();

    for entry in entries {
        match entry {
            RecordEntry::Element(expr) => {
                let value = eval_expr_many(expr, env)?;
                if !contains_value(&values, &value) {
                    values.push(value);
                }
            }
            entry => {
                return Err(one_diagnostic(unsupported_expr(
                    record_entry_span(entry),
                    "only element entries are supported in set literals by the current evaluator",
                )));
            }
        }
    }

    Ok(Value::Set(Rc::new(values)))
}

fn eval_record(entries: &[RecordEntry], env: &Environment) -> Eval {
    let mut fields = Vec::new();

    for entry in entries {
        fold_record_entry(&mut fields, entry, env)?;
    }

    Ok(Value::Record(Rc::new(fields)))
}

fn fold_record_entry(
    fields: &mut Vec<(String, Value)>,
    entry: &RecordEntry,
    env: &Environment,
) -> Eval<()> {
    match entry {
        RecordEntry::Field { name, value, .. } => {
            let value = eval_expr_many(value, env)?;
            insert_or_replace_field(fields, name.clone(), value);
        }
        RecordEntry::FieldComputed { key, value, .. } => {
            let name = eval_text_key(key, key.span, env)?;
            let value = eval_expr_many(value, env)?;
            insert_or_replace_field(fields, name, value);
        }
        RecordEntry::Shorthand {
            name, name_span, ..
        } => {
            let value = env
                .lookup(name)
                .ok_or_else(|| one_diagnostic(unbound_name(name, *name_span)))?;
            insert_or_replace_field(fields, name.clone(), value);
        }
        RecordEntry::Spread {
            value: source_expr, ..
        } => {
            let source = eval_expr_many(source_expr, env)?;
            let source_fields = match source {
                Value::Record(fields) => fields,
                value => {
                    return Err(one_diagnostic(record_type_error(
                        source_expr.span,
                        "spread",
                        value.type_name(),
                        "Record",
                    )));
                }
            };

            for (name, value) in source_fields.iter() {
                insert_or_replace_field(fields, name.clone(), value.clone());
            }
        }
        RecordEntry::Delete { name, .. } => {
            remove_field(fields, name);
        }
        RecordEntry::DeleteComputed { key, .. } => {
            let name = eval_text_key(key, key.span, env)?;
            remove_field(fields, &name);
        }
        RecordEntry::Rename { from, to, .. } => {
            rename_field(fields, from, to);
        }
        RecordEntry::Iteration {
            source,
            binder,
            guard,
            body,
            ..
        } => {
            fold_record_iteration(fields, source, binder, guard.as_ref(), body, env)?;
        }
        RecordEntry::Open { span } => {
            return Err(one_diagnostic(record_type_error(
                *span,
                "record construction",
                "open row marker",
                "value record entry",
            )));
        }
        RecordEntry::Element(expr) => {
            fold_record_element(fields, expr, env)?;
        }
    }

    Ok(())
}

fn fold_record_iteration(
    fields: &mut Vec<(String, Value)>,
    source: &Expr,
    binder: &str,
    guard: Option<&Expr>,
    body: &[RecordEntry],
    env: &Environment,
) -> Eval<()> {
    let source_value = eval_expr_many(source, env)?;
    let values: Vec<Value> = match source_value {
        Value::Set(items) | Value::Array(items) => items.iter().cloned().collect(),
        Value::Record(source_fields) => source_fields
            .iter()
            .map(|(name, _)| Value::Text(name.clone()))
            .collect(),
        value => {
            return Err(one_diagnostic(record_type_error(
                source.span,
                "record comprehension source",
                value.type_name(),
                "Set, Array, or Record",
            )));
        }
    };

    for value in values {
        let child = env.child();
        child.bind(binder, value);

        if let Some(guard) = guard {
            match eval_expr_many(guard, &child)? {
                Value::Bool(true) => {}
                Value::Bool(false) => continue,
                value => {
                    return Err(one_diagnostic(guard_type_error(
                        guard.span,
                        value.type_name(),
                    )));
                }
            }
        }

        for entry in body {
            fold_record_entry(fields, entry, &child)?;
        }
    }

    Ok(())
}

fn fold_record_element(
    fields: &mut Vec<(String, Value)>,
    expr: &Expr,
    env: &Environment,
) -> Eval<()> {
    let value = eval_expr_many(expr, env)?;
    let Value::Tuple(values) = value else {
        return Err(one_diagnostic(record_tuple_emit_type_error(
            expr.span,
            value.type_name(),
        )));
    };

    let [label, field_value] = values.as_slice() else {
        return Err(one_diagnostic(record_tuple_emit_type_error(
            expr.span,
            "Tuple with wrong arity",
        )));
    };

    let Value::Text(name) = label else {
        return Err(one_diagnostic(record_tuple_emit_type_error(
            expr.span,
            label.type_name(),
        )));
    };

    insert_or_replace_field(fields, name.clone(), field_value.clone());
    Ok(())
}

fn eval_field_access(
    receiver: &Expr,
    field: &str,
    field_span: Span,
    null_safe: bool,
    env: &Environment,
) -> Eval {
    let receiver_value = eval_expr_many(receiver, env)?;
    if null_safe && matches!(receiver_value, Value::Undefined | Value::Null) {
        return Ok(receiver_value);
    }

    match receiver_value {
        Value::Record(fields) => record_field_value(&fields, field)
            .cloned()
            .ok_or_else(|| one_diagnostic(missing_field(field, field_span))),
        value => builtin_method(&value, field).ok_or_else(|| {
            one_diagnostic(record_type_error(
                receiver.span,
                "field access",
                value.type_name(),
                "Record",
            ))
        }),
    }
}

fn builtin_method(receiver: &Value, field: &str) -> Option<Value> {
    match (receiver, field) {
        (Value::Set(items), "has") => Some(collection_has_method("Set", Rc::clone(items))),
        (Value::Array(items), "has") => Some(collection_has_method("Array", Rc::clone(items))),
        (Value::Map(entries), "get") => Some(map_get_method(Rc::clone(entries))),
        (Value::Map(entries), "set") => Some(map_set_method(Rc::clone(entries))),
        (Value::Map(entries), "delete") => Some(map_delete_method(Rc::clone(entries))),
        (Value::Map(entries), "has") => Some(map_has_method(Rc::clone(entries))),
        (Value::Map(entries), "keys") => Some(map_keys_method(Rc::clone(entries))),
        (Value::Map(entries), "values") => Some(map_values_method(Rc::clone(entries))),
        (Value::Map(entries), "entries") => Some(map_entries_method(Rc::clone(entries))),
        (Value::Map(entries), "size") => Some(map_size_method(Rc::clone(entries))),
        (Value::Map(entries), "merge") => Some(map_merge_method(Rc::clone(entries))),
        _ => None,
    }
}

fn collection_has_method(kind: &'static str, items: Rc<Vec<Value>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("{kind}.has expects 1 argument, got {}", args.len()));
        }

        Ok(Value::Bool(contains_value(&items, &args[0])))
    })
}

fn map_get_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.get expects 1 argument, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.get")?;

        Ok(map_entry_value(&entries, &args[0])
            .cloned()
            .unwrap_or(Value::Undefined))
    })
}

fn map_set_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!("Map.set expects 2 arguments, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.set")?;

        let mut next = entries.as_ref().clone();
        insert_or_replace_map_entry(&mut next, args[0].clone(), args[1].clone());
        Ok(Value::Map(Rc::new(next)))
    })
}

fn map_delete_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.delete expects 1 argument, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.delete")?;

        let mut next = entries.as_ref().clone();
        remove_map_entry(&mut next, &args[0]);
        Ok(Value::Map(Rc::new(next)))
    })
}

fn map_has_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.has expects 1 argument, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.has")?;

        Ok(Value::Bool(map_entry_value(&entries, &args[0]).is_some()))
    })
}

fn map_keys_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("Map.keys expects 0 arguments, got {}", args.len()));
        }

        Ok(Value::Array(Rc::new(
            entries.iter().map(|(key, _)| key.clone()).collect(),
        )))
    })
}

fn map_values_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Map.values expects 0 arguments, got {}",
                args.len()
            ));
        }

        Ok(Value::Array(Rc::new(
            entries.iter().map(|(_, value)| value.clone()).collect(),
        )))
    })
}

fn map_entries_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Map.entries expects 0 arguments, got {}",
                args.len()
            ));
        }

        Ok(Value::Array(Rc::new(
            entries
                .iter()
                .map(|(key, value)| Value::Tuple(Rc::new(vec![key.clone(), value.clone()])))
                .collect(),
        )))
    })
}

fn map_size_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("Map.size expects 0 arguments, got {}", args.len()));
        }

        Ok(Value::Int(entries.len() as i64))
    })
}

fn map_merge_method(entries: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.merge expects 1 argument, got {}", args.len()));
        }

        let Value::Map(other) = &args[0] else {
            return Err(format!(
                "Map.merge expects a Map, got {}",
                args[0].type_name()
            ));
        };

        let mut next = entries.as_ref().clone();
        // Mirrors record `:..` overwrite-spread: the right-hand map wins on
        // conflicts while existing left-hand insertion positions are retained.
        for (key, value) in other.iter() {
            insert_or_replace_map_entry(&mut next, key.clone(), value.clone());
        }
        Ok(Value::Map(Rc::new(next)))
    })
}

fn eval_index(callee: &Expr, args: &[Expr], span: Span, env: &Environment) -> Eval {
    if args.len() != 1 {
        return Err(one_diagnostic(unsupported_expr(
            span,
            "only single-argument indexing is supported by the current evaluator",
        )));
    }

    let callee_value = eval_expr_many(callee, env)?;
    let arg_value = eval_expr_many(&args[0], env)?;
    if let (Value::Type(RuntimeType::Named(name)), arg_value) = (&callee_value, &arg_value)
        && name == "Array"
    {
        if runtime_type_target(arg_value) {
            return Ok(Value::Type(RuntimeType::Array(Box::new(arg_value.clone()))));
        }

        return Err(one_diagnostic(record_type_error(
            args[0].span,
            "array type construction",
            arg_value.type_name(),
            "Type",
        )));
    }

    match callee_value {
        Value::Array(values) => {
            let Value::Int(index) = arg_value else {
                return Err(one_diagnostic(record_type_error(
                    args[0].span,
                    "array indexing",
                    arg_value.type_name(),
                    "Int",
                )));
            };

            Ok(indexed_value(&values, index).unwrap_or(Value::Undefined))
        }
        Value::Tuple(values) => {
            let Value::Int(index) = arg_value else {
                return Err(one_diagnostic(record_type_error(
                    args[0].span,
                    "tuple indexing",
                    arg_value.type_name(),
                    "Int",
                )));
            };

            indexed_value(&values, index).ok_or_else(|| {
                one_diagnostic(index_out_of_bounds(args[0].span, index, values.len()))
            })
        }
        Value::Record(fields) => {
            let Value::Text(key) = arg_value else {
                return Err(one_diagnostic(record_type_error(
                    args[0].span,
                    "record indexing",
                    arg_value.type_name(),
                    "Text",
                )));
            };
            record_field_value(&fields, &key)
                .cloned()
                .ok_or_else(|| one_diagnostic(missing_field(&key, args[0].span)))
        }
        value => Err(one_diagnostic(record_type_error(
            callee.span,
            "indexing",
            value.type_name(),
            "Array, Tuple, or Record",
        ))),
    }
}

fn eval_type_wrapper(
    inner: &Expr,
    span: Span,
    env: &Environment,
    wrap: fn(Box<Value>) -> RuntimeType,
) -> Eval {
    let value = eval_expr_many(inner, env)?;
    if runtime_type_target(&value) {
        Ok(Value::Type(wrap(Box::new(value))))
    } else {
        Err(one_diagnostic(record_type_error(
            span,
            "type construction",
            value.type_name(),
            "Type",
        )))
    }
}

fn runtime_type_target(value: &Value) -> bool {
    match value {
        Value::Type(_) => true,
        Value::Record(fields) => fields
            .iter()
            .all(|(_, field_value)| runtime_type_target(field_value)),
        _ => false,
    }
}

fn indexed_value(values: &[Value], index: i64) -> Option<Value> {
    let index = usize::try_from(index).ok()?;
    values.get(index).cloned()
}

fn ensure_map_key(key: &Value, context: &str) -> Result<(), String> {
    if map_key_is_comparable(key) {
        Ok(())
    } else {
        Err(format!(
            "{context} cannot use {} as a Map key",
            key.type_name()
        ))
    }
}

fn map_key_is_comparable(key: &Value) -> bool {
    match key {
        Value::Closure(_) | Value::Native(_) => false,
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => {
            values.iter().all(map_key_is_comparable)
        }
        Value::Map(entries) => entries
            .iter()
            .all(|(key, value)| map_key_is_comparable(key) && map_key_is_comparable(value)),
        Value::Record(fields) => fields.iter().all(|(_, value)| map_key_is_comparable(value)),
        Value::Tag { payload, .. } => payload.iter().all(map_key_is_comparable),
        Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bool(_)
        | Value::Type(_)
        | Value::Undefined
        | Value::Null => true,
    }
}

fn map_entry_index(entries: &[(Value, Value)], key: &Value) -> Option<usize> {
    entries.iter().position(|(entry_key, _)| entry_key == key)
}

fn map_entry_value<'a>(entries: &'a [(Value, Value)], key: &Value) -> Option<&'a Value> {
    entries
        .iter()
        .find_map(|(entry_key, value)| (entry_key == key).then_some(value))
}

fn insert_or_replace_map_entry(entries: &mut Vec<(Value, Value)>, key: Value, value: Value) {
    if let Some(index) = map_entry_index(entries, &key) {
        entries[index] = (key, value);
    } else {
        entries.push((key, value));
    }
}

fn remove_map_entry(entries: &mut Vec<(Value, Value)>, key: &Value) {
    if let Some(index) = map_entry_index(entries, key) {
        entries.remove(index);
    }
}

fn eval_text_key(expr: &Expr, span: Span, env: &Environment) -> Eval<String> {
    match eval_expr_many(expr, env)? {
        Value::Text(text) => Ok(text),
        value => Err(one_diagnostic(record_type_error(
            span,
            "computed record key",
            value.type_name(),
            "Text",
        ))),
    }
}

fn insert_or_replace_field(fields: &mut Vec<(String, Value)>, name: String, value: Value) {
    if let Some(index) = record_field_index(fields, &name) {
        fields[index] = (name, value);
    } else {
        fields.push((name, value));
    }
}

fn remove_field(fields: &mut Vec<(String, Value)>, name: &str) {
    if let Some(index) = record_field_index(fields, name) {
        fields.remove(index);
    }
}

fn rename_field(fields: &mut Vec<(String, Value)>, from: &str, to: &str) {
    let Some(from_index) = record_field_index(fields, from) else {
        return;
    };

    let (_, value) = fields.remove(from_index);
    remove_field(fields, to);
    fields.insert(from_index.min(fields.len()), (to.to_owned(), value));
}

fn record_field_index(fields: &[(String, Value)], name: &str) -> Option<usize> {
    fields.iter().position(|(field, _)| field == name)
}

fn record_field_value<'a>(fields: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(field, value)| (field == name).then_some(value))
}

fn eval_literal(literal: &Literal, span: Span) -> Result<Value, Diagnostic> {
    match literal {
        Literal::Bool(value) => Ok(Value::Bool(*value)),
        Literal::Number(text) => eval_number_literal(text, span),
        Literal::String(text) => Ok(Value::Text(decode_string_literal(text))),
        Literal::Regex(_) | Literal::Path(_) => Err(unsupported_expr(
            span,
            "this literal kind is not supported by the current evaluator",
        )),
    }
}

fn eval_interpolation(segments: &[InterpolationSegment], env: &Environment) -> Eval {
    let mut text = String::new();

    for segment in segments {
        match segment {
            InterpolationSegment::Text(raw) => text.push_str(raw),
            InterpolationSegment::Expr(expr) => {
                text.push_str(&eval_expr_many(expr, env)?.to_string());
            }
        }
    }

    Ok(Value::Text(text))
}

fn eval_number_literal(text: &str, span: Span) -> Result<Value, Diagnostic> {
    let normalized = text.replace('_', "");

    if is_float_literal(text) {
        return normalized
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| invalid_numeric_literal(text, span, "Float"));
    }

    normalized
        .parse::<i64>()
        .map(Value::Int)
        .map_err(|_| invalid_numeric_literal(text, span, "Int"))
}

fn is_float_literal(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
}

fn eval_unary(operator: &str, value: &Expr, span: Span, env: &Environment) -> Eval {
    let value = eval_expr_many(value, env)?;

    match (operator, value) {
        ("-", Value::Int(value)) => value
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| one_diagnostic(integer_overflow(span, "unary `-`"))),
        ("-", Value::Float(value)) => Ok(Value::Float(-value)),
        ("-", value) => Err(one_diagnostic(unary_type_error(
            span,
            "-",
            value.type_name(),
            "a numeric operand",
        ))),
        ("!", Value::Bool(value)) => Ok(Value::Bool(!value)),
        ("!", value) => Err(one_diagnostic(unary_type_error(
            span,
            "!",
            value.type_name(),
            "a Bool operand",
        ))),
        _ => Err(one_diagnostic(unsupported_expr(
            span,
            "this unary operator is not supported by the current evaluator",
        ))),
    }
}

fn eval_binary(
    left: &Expr,
    operator: &str,
    operator_span: Span,
    right: &Expr,
    span: Span,
    env: &Environment,
) -> Eval {
    match operator {
        "&&" => eval_boolean_and(left, right, span, env),
        "||" => eval_boolean_or(left, right, span, env),
        "??" => eval_null_coalesce(left, right, env),
        _ => {
            let left_value = eval_expr_many(left, env)?;
            let right_value = eval_expr_many(right, env)?;
            apply_binary(
                left_value,
                operator,
                operator_span,
                right_value,
                right.span,
                span,
            )
            .map_err(one_diagnostic)
        }
    }
}

fn eval_null_coalesce(left: &Expr, right: &Expr, env: &Environment) -> Eval {
    let left_value = eval_expr_many(left, env)?;
    if matches!(left_value, Value::Undefined | Value::Null) {
        eval_expr_many(right, env)
    } else {
        Ok(left_value)
    }
}

fn eval_boolean_and(left: &Expr, right: &Expr, span: Span, env: &Environment) -> Eval {
    match eval_expr_many(left, env)? {
        Value::Bool(false) => Ok(Value::Bool(false)),
        Value::Bool(true) => match eval_expr_many(right, env)? {
            Value::Bool(value) => Ok(Value::Bool(value)),
            value => Err(one_diagnostic(binary_type_error(
                span,
                "&&",
                "Bool",
                value.type_name(),
                "Bool operands",
            ))),
        },
        value => Err(one_diagnostic(binary_type_error(
            span,
            "&&",
            value.type_name(),
            "Bool",
            "Bool operands",
        ))),
    }
}

fn eval_boolean_or(left: &Expr, right: &Expr, span: Span, env: &Environment) -> Eval {
    match eval_expr_many(left, env)? {
        Value::Bool(true) => Ok(Value::Bool(true)),
        Value::Bool(false) => match eval_expr_many(right, env)? {
            Value::Bool(value) => Ok(Value::Bool(value)),
            value => Err(one_diagnostic(binary_type_error(
                span,
                "||",
                "Bool",
                value.type_name(),
                "Bool operands",
            ))),
        },
        value => Err(one_diagnostic(binary_type_error(
            span,
            "||",
            value.type_name(),
            "Bool",
            "Bool operands",
        ))),
    }
}

fn apply_binary(
    left: Value,
    operator: &str,
    operator_span: Span,
    right: Value,
    right_span: Span,
    span: Span,
) -> Result<Value, Diagnostic> {
    match operator {
        "+" => add(left, right, span),
        "-" | "*" | "/" | "%" => numeric_arithmetic(left, operator, right, right_span, span),
        "==" | "!=" => equality(left, operator, right, span),
        "<" | ">" | "<=" | ">=" => numeric_comparison(left, operator, right, span),
        "|" => Ok(set_union(left, right)),
        _ => Err(unsupported_operator(operator, operator_span)),
    }
}

/// Set union with singleton-promotion: each operand contributes its members if
/// it is already a `Set`, otherwise it contributes itself as a single element.
/// Duplicates are removed (first occurrence wins) using `contains_value`, the
/// same equality `eval_set` uses so `|` and `{..}` agree on element identity.
fn set_union(left: Value, right: Value) -> Value {
    let mut members: Vec<Value> = Vec::new();
    for operand in [left, right] {
        match operand {
            Value::Set(items) => {
                for item in items.iter() {
                    if !contains_value(&members, item) {
                        members.push(item.clone());
                    }
                }
            }
            other => {
                if !contains_value(&members, &other) {
                    members.push(other);
                }
            }
        }
    }
    Value::Set(Rc::new(members))
}

fn add(left: Value, right: Value, span: Span) -> Result<Value, Diagnostic> {
    match (left, right) {
        (Value::Text(left), Value::Text(right)) => Ok(Value::Text(left + &right)),
        (left, right) => numeric_arithmetic(left, "+", right, span, span),
    }
}

fn numeric_arithmetic(
    left: Value,
    operator: &str,
    right: Value,
    right_span: Span,
    span: Span,
) -> Result<Value, Diagnostic> {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => {
            int_arithmetic(left, operator, right, right_span, span)
        }
        (Value::Float(left), Value::Float(right)) => {
            float_arithmetic(left, operator, right, right_span, span)
        }
        (Value::Int(left), Value::Float(right)) => {
            float_arithmetic(left as f64, operator, right, right_span, span)
        }
        (Value::Float(left), Value::Int(right)) => {
            float_arithmetic(left, operator, right as f64, right_span, span)
        }
        (left, right) => Err(binary_type_error(
            span,
            operator,
            left.type_name(),
            right.type_name(),
            "numeric operands",
        )),
    }
}

fn int_arithmetic(
    left: i64,
    operator: &str,
    right: i64,
    right_span: Span,
    span: Span,
) -> Result<Value, Diagnostic> {
    if matches!(operator, "/" | "%") && right == 0 {
        return Err(division_by_zero(right_span));
    }

    let result = match operator {
        "+" => left.checked_add(right),
        "-" => left.checked_sub(right),
        "*" => left.checked_mul(right),
        "/" => left.checked_div(right),
        "%" => left.checked_rem(right),
        _ => None,
    };

    result
        .map(Value::Int)
        .ok_or_else(|| integer_overflow(span, operator))
}

fn float_arithmetic(
    left: f64,
    operator: &str,
    right: f64,
    right_span: Span,
    span: Span,
) -> Result<Value, Diagnostic> {
    if matches!(operator, "/" | "%") && is_float_zero(right) {
        return Err(division_by_zero(right_span));
    }

    match operator {
        "+" => Ok(Value::Float(left + right)),
        "-" => Ok(Value::Float(left - right)),
        "*" => Ok(Value::Float(left * right)),
        "/" => Ok(Value::Float(left / right)),
        "%" => Ok(Value::Float(left % right)),
        _ => Err(unsupported_expr(
            span,
            "this numeric operator is not supported by the current evaluator",
        )),
    }
}

fn equality(left: Value, operator: &str, right: Value, span: Span) -> Result<Value, Diagnostic> {
    if matches!(
        (&left, &right),
        (Value::Closure(_), _) | (_, Value::Closure(_))
    ) {
        return Err(closure_equality_error(span, operator));
    }

    let equal = match (&left, &right) {
        (Value::Int(left), Value::Int(right)) => left == right,
        (Value::Float(left), Value::Float(right)) => {
            numeric_ordering(*left, *right).is_some_and(|ordering| ordering == Ordering::Equal)
        }
        (Value::Int(left), Value::Float(right)) => numeric_ordering(*left as f64, *right)
            .is_some_and(|ordering| ordering == Ordering::Equal),
        (Value::Float(left), Value::Int(right)) => numeric_ordering(*left, *right as f64)
            .is_some_and(|ordering| ordering == Ordering::Equal),
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Array(_), Value::Array(_)) => left == right,
        (Value::Tuple(_), Value::Tuple(_)) => left == right,
        (Value::Set(_), Value::Set(_)) => left == right,
        (Value::Map(_), Value::Map(_)) => left == right,
        (Value::Record(_), Value::Record(_)) => left == right,
        (Value::Tag { .. }, Value::Tag { .. }) => left == right,
        (Value::Type(left), Value::Type(right)) => left == right,
        (Value::Native(_), Value::Native(_)) => false,
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        _ => {
            return Err(binary_type_error(
                span,
                operator,
                left.type_name(),
                right.type_name(),
                "matching value kinds",
            ));
        }
    };

    Ok(Value::Bool(if operator == "==" { equal } else { !equal }))
}

fn numeric_comparison(
    left: Value,
    operator: &str,
    right: Value,
    span: Span,
) -> Result<Value, Diagnostic> {
    let Some(ordering) = numeric_value_ordering(&left, &right) else {
        return Err(binary_type_error(
            span,
            operator,
            left.type_name(),
            right.type_name(),
            "numeric operands",
        ));
    };

    let result = match operator {
        "<" => ordering == Ordering::Less,
        ">" => ordering == Ordering::Greater,
        "<=" => ordering != Ordering::Greater,
        ">=" => ordering != Ordering::Less,
        _ => false,
    };

    Ok(Value::Bool(result))
}

fn numeric_value_ordering(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => Some(left.cmp(right)),
        (Value::Float(left), Value::Float(right)) => numeric_ordering(*left, *right),
        (Value::Int(left), Value::Float(right)) => numeric_ordering(*left as f64, *right),
        (Value::Float(left), Value::Int(right)) => numeric_ordering(*left, *right as f64),
        _ => None,
    }
}

fn numeric_ordering(left: f64, right: f64) -> Option<Ordering> {
    left.partial_cmp(&right)
}

fn is_float_zero(value: f64) -> bool {
    value.to_bits() << 1 == 0
}

fn invalid_numeric_literal(text: &str, span: Span, kind: &str) -> Diagnostic {
    Diagnostic::error(format!("invalid {kind} literal `{text}`"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            "this numeric literal cannot be evaluated",
        ))
        .with_note("numeric literals currently evaluate as i64 Int or f64 Float values")
}

fn unary_type_error(span: Span, operator: &str, actual: &str, expected: &str) -> Diagnostic {
    Diagnostic::error(format!("cannot apply unary `{operator}` to {actual}"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, format!("expected {expected}")))
        .with_note(
            "runtime type errors are reported by the evaluator; static checking is a separate phase",
        )
}

fn binary_type_error(
    span: Span,
    operator: &str,
    left: &str,
    right: &str,
    expected: &str,
) -> Diagnostic {
    Diagnostic::error(format!("cannot apply `{operator}` to {left} and {right}"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, format!("expected {expected}")))
        .with_note("runtime type errors are reported by the evaluator; static checking is a separate phase")
}

fn record_type_error(span: Span, operation: &str, actual: &str, expected: &str) -> Diagnostic {
    Diagnostic::error(format!("cannot perform {operation} on {actual}"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, format!("expected {expected}")))
        .with_note(
            "runtime type errors are reported by the evaluator; static checking is a separate phase",
        )
}

fn index_out_of_bounds(span: Span, index: i64, length: usize) -> Diagnostic {
    Diagnostic::error("tuple index out of bounds")
        .with_code(codes::runtime::INDEX_OUT_OF_BOUNDS)
        .with_label(Label::primary(
            span,
            format!("index {index} is outside tuple arity {length}"),
        ))
        .with_note(
            "tuple indexing is fixed-arity; use an array when out-of-bounds should evaluate to undefined",
        )
}

fn missing_field(field: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("missing field `{field}`"))
        .with_code(codes::runtime::MISSING_FIELD)
        .with_label(Label::primary(span, "this field is not present at runtime"))
        .with_note("record field lookup only succeeds for fields present on the record value")
}

fn no_match(span: Span) -> Diagnostic {
    Diagnostic::error("no match arm matched")
        .with_code(codes::runtime::NO_MATCH)
        .with_label(Label::primary(
            span,
            "no pattern matched this value with passing guards",
        ))
        .with_note("the checker enforces match exhaustiveness; this is the evaluator safety net")
}

fn division_by_zero(span: Span) -> Diagnostic {
    Diagnostic::error("division by zero")
        .with_code(codes::runtime::DIVISION_BY_ZERO)
        .with_label(Label::primary(span, "this operand evaluates to zero"))
        .with_note("the right operand of `/` and `%` must be non-zero")
}

fn not_callable(span: Span, actual: &str) -> Diagnostic {
    Diagnostic::error(format!("cannot call {actual}"))
        .with_code(codes::runtime::NOT_CALLABLE)
        .with_label(Label::primary(
            span,
            "this expression does not evaluate to a function",
        ))
        .with_note(
            "only closures created by lambda expressions are callable in this evaluator slice",
        )
}

fn arity_mismatch(span: Span, required: usize, total: usize, got: usize) -> Diagnostic {
    let expected = if required == total {
        format!("{total} argument(s)")
    } else {
        format!("between {required} and {total} arguments")
    };

    Diagnostic::error("function arity mismatch")
        .with_code(codes::runtime::ARITY_MISMATCH)
        .with_label(Label::primary(
            span,
            format!("expected {expected}, got {got}"),
        ))
        .with_note(format!(
            "this function expects {expected}, but the call supplied {got}"
        ))
}

fn platform_error(span: Span, message: String) -> Diagnostic {
    Diagnostic::error("platform function failed")
        .with_code(codes::runtime::PLATFORM_ERROR)
        .with_label(Label::primary(span, message))
        .with_note("host platform functions report errors through the runtime boundary")
}

fn propagate_type_error(span: Span) -> Diagnostic {
    Diagnostic::error("error propagation expects a Result")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            "`?^` and `?!` operate on `@Ok(value)` or `@Err(error)`",
        ))
        .with_note("the operand of `?^`/`?!` must evaluate to a Result tagged `@Ok` or `@Err`")
}

fn panic(span: Span, error: &Value) -> Diagnostic {
    Diagnostic::error(format!("unwrapped an `@Err`: {error}"))
        .with_code(codes::runtime::PANIC)
        .with_label(Label::primary(span, "`?!` panicked on this `@Err` result"))
        .with_note(
            "use `?^` to propagate the `@Err` to the caller, or match on the Result to handle it",
        )
}

fn closure_equality_error(span: Span, operator: &str) -> Diagnostic {
    Diagnostic::error("closures are not comparable")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            format!("`{operator}` cannot compare function values"),
        ))
        .with_note("function values do not have runtime equality in this evaluator slice")
}

fn guard_type_error(span: Span, actual: &str) -> Diagnostic {
    Diagnostic::error(format!("guard evaluated to {actual}"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, "expected a Bool guard"))
        .with_note("guards must evaluate to true or false")
}

fn record_tuple_emit_type_error(span: Span, actual: &str) -> Diagnostic {
    Diagnostic::error(format!("record tuple emit evaluated to {actual}"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            "record comprehension body must emit a `(label, value)` tuple with a Text label",
        ))
        .with_note("record tuple emits insert or replace one field using the tuple's Text label")
}

fn integer_overflow(span: Span, operation: &str) -> Diagnostic {
    Diagnostic::error("integer arithmetic overflow")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, format!("`{operation}` overflowed i64")))
        .with_note("Aven Int currently uses i64; arbitrary precision integers are planned for a later milestone")
}

fn unbound_name(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("unbound name `{name}`"))
        .with_code(codes::runtime::UNBOUND_NAME)
        .with_label(Label::primary(span, "this name is not bound at runtime"))
        .with_note("the name may be undefined or defined later; runtime evaluation is sequential")
}

fn unsupported_expr(span: Span, label: &str) -> Diagnostic {
    Diagnostic::error("unsupported runtime expression")
        .with_code(codes::runtime::UNSUPPORTED)
        .with_label(Label::primary(span, label))
        .with_note(
            "the evaluator currently supports literals, names, bindings, blocks, lambdas, calls, matches, records, variants, collections, indexes, nullable field access, unary operators, and core binary operators",
        )
}

fn record_entry_span(entry: &RecordEntry) -> Span {
    match entry {
        RecordEntry::Field { span, .. }
        | RecordEntry::FieldComputed { span, .. }
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

fn unsupported_operator(operator: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!(
        "operator `{operator}` is not supported by the evaluator yet"
    ))
    .with_code(codes::runtime::UNSUPPORTED)
    .with_label(Label::primary(
        span,
        "this operator is planned for a later evaluator slice",
    ))
}

fn one_diagnostic(diagnostic: Diagnostic) -> Flow {
    Flow::Fail(vec![diagnostic])
}

fn first_diagnostic(flow: Flow) -> Diagnostic {
    flow_diagnostics(flow)
        .into_iter()
        .next()
        .expect("expression errors include at least one diagnostic")
}

/// Collapse a [`Flow`] into the diagnostics it reports. A [`Flow::Propagate`]
/// only reaches here when an `@Err` escaped past every catch boundary (a bare
/// `eval_expr` with no enclosing function); surface it as a runtime error rather
/// than swallow it.
fn flow_diagnostics(flow: Flow) -> Vec<Diagnostic> {
    match flow {
        Flow::Fail(diagnostics) => diagnostics,
        Flow::Propagate(value) => vec![propagate_escaped(&value)],
    }
}

fn propagate_escaped(value: &Value) -> Diagnostic {
    Diagnostic::error(format!("error propagated past the enclosing scope: {value}"))
        .with_code(codes::runtime::PANIC)
        .with_note("`?^` early-returns the enclosing function; with no enclosing function the `@Err` has nowhere to return to")
}

#[cfg(test)]
mod tests;
