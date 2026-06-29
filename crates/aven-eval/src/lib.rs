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
    RecordEntry,
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
    Record(Rc<Vec<(String, Value)>>),
    Tag {
        name: String,
        payload: Vec<Value>,
    },
    Closure(Closure),
    Native(NativeFn),
    /// An opaque named type value (e.g. `Text`). Types are first-class runtime
    /// values: this holds only the name; the real type IR lives in `aven-check`.
    Type(String),
    Undefined,
    Null,
}

/// Atomic primitive type names bound as `Value::Type` intrinsics. Matches
/// `CHECKED_NAMED_TYPES` in `aven-check`.
const PRIMITIVE_TYPE_NAMES: [&str; 7] =
    ["Bool", "Float", "Int", "Null", "Text", "Undefined", "Unit"];

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
            Self::Record(fields) => f.debug_tuple("Record").field(fields).finish(),
            Self::Tag { name, payload } => f
                .debug_struct("Tag")
                .field("name", name)
                .field("payload", payload)
                .finish(),
            Self::Closure(closure) => f.debug_tuple("Closure").field(closure).finish(),
            Self::Native(_) => f.write_str("Native(<native>)"),
            Self::Type(name) => f.debug_tuple("Type").field(name).finish(),
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
            Self::Record(fields) => fmt_record(fields, f),
            Self::Tag { name, payload } => fmt_tag(name, payload, f),
            Self::Closure(_) => write!(f, "<function>"),
            Self::Native(_) => write!(f, "<native>"),
            Self::Type(name) => write!(f, "{name}"),
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

fn sets_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len() && left.iter().all(|value| contains_value(right, value))
}

fn contains_value(values: &[Value], needle: &Value) -> bool {
    values.iter().any(|value| value == needle)
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
    let mut intrinsics: Vec<(String, Value)> = PRIMITIVE_TYPE_NAMES
        .iter()
        .map(|name| ((*name).to_owned(), Value::Type((*name).to_owned())))
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

fn eval_index(callee: &Expr, args: &[Expr], span: Span, env: &Environment) -> Eval {
    if args.len() != 1 {
        return Err(one_diagnostic(unsupported_expr(
            span,
            "only single-argument indexing is supported by the current evaluator",
        )));
    }

    let callee_value = eval_expr_many(callee, env)?;
    let arg_value = eval_expr_many(&args[0], env)?;
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

fn indexed_value(values: &[Value], index: i64) -> Option<Value> {
    let index = usize::try_from(index).ok()?;
    values.get(index).cloned()
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
            InterpolationSegment::Text(raw) => text.push_str(&decode_string_body(raw)),
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

fn decode_string_literal(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|stripped| stripped.strip_suffix('"'))
        .unwrap_or(text);

    decode_string_body(inner)
}

fn decode_string_body(text: &str) -> String {
    let mut decoded = String::new();
    let mut escaped = false;

    for ch in text.chars() {
        if escaped {
            decoded.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            decoded.push(ch);
        }
    }

    if escaped {
        decoded.push('\\');
    }

    decoded
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
mod tests {
    use super::{
        Environment, EvalOutcome, Value, eval_expr, eval_module, eval_module_with_globals, logging,
        record_field_value,
    };
    use aven_core::codes;
    use aven_parser::{Item, Module, parse_module};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn evaluates_arithmetic_with_parser_precedence() {
        assert_eval("1 + 2 * 3", Value::Int(7));
    }

    #[test]
    fn evaluates_grouping_before_multiplication() {
        assert_eval("(1 + 2) * 3", Value::Int(9));
    }

    #[test]
    fn evaluates_unary_minus_and_bool_not() {
        assert_eval("-5", Value::Int(-5));
        assert_eval("!false", Value::Bool(true));
    }

    #[test]
    fn evaluates_integer_and_float_division() {
        assert_eval("7 / 2", Value::Int(3));
        assert_eval("7.0 / 2", Value::Float(3.5));
    }

    #[test]
    fn reports_division_by_zero() {
        let diagnostic = eval_error("1 / 0");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::DIVISION_BY_ZERO)
        );
    }

    #[test]
    fn evaluates_comparisons() {
        assert_eval("1 < 2", Value::Bool(true));
        assert_eval("2 >= 2.0", Value::Bool(true));
        assert_eval("\"a\" == \"a\"", Value::Bool(true));
        assert_eval("true != false", Value::Bool(true));
    }

    #[test]
    fn evaluates_boolean_short_circuiting() {
        assert_eval("false && 1 / 0", Value::Bool(false));
        assert_eval("true || 1 / 0", Value::Bool(true));
    }

    #[test]
    fn concatenates_text_with_plus() {
        assert_eval("\"a\" + \"b\"", Value::Text("ab".to_owned()));
    }

    #[test]
    fn evaluates_string_interpolation_with_stringified_values() {
        assert_eval("\"a${1 + 2}b\"", Value::Text("a3b".to_owned()));
    }

    #[test]
    fn evaluates_interpolation_field_access() {
        assert_module_value(
            "user = { name: \"Ada\" }\n\"${user.name}\"\n",
            Value::Text("Ada".to_owned()),
        );
    }

    #[test]
    fn escaped_interpolation_marker_evaluates_literally() {
        assert_eval(r#""\${x}""#, Value::Text("${x}".to_owned()));
    }

    #[test]
    fn evaluates_nested_record_expression_inside_interpolation() {
        assert_eval("\"${ { a: 1 }.a }\"", Value::Text("1".to_owned()));
    }

    #[test]
    fn reports_type_errors() {
        let diagnostic = eval_error("1 + \"a\"");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    }

    #[test]
    fn evaluates_module_to_last_expression_value() {
        let module = parse_ok("1\n2 * 3\n");
        let outcome = eval_module(&module);

        assert_eq!(
            outcome,
            EvalOutcome {
                value: Some(Value::Int(6)),
                diagnostics: Vec::new()
            }
        );
    }

    #[test]
    fn evaluates_sequential_bindings() {
        assert_module_value("x = 5\ny = x + 1\ny\n", Value::Int(6));
    }

    #[test]
    fn evaluates_simple_function_call() {
        assert_module_value("double = (x) => x * 2\ndouble(5)\n", Value::Int(10));
    }

    #[test]
    fn evaluates_higher_order_function_call() {
        assert_module_value(
            "twice = (f, x) => f(f(x))\ninc = (n) => n + 1\ntwice(inc, 1)\n",
            Value::Int(3),
        );
    }

    #[test]
    fn closures_capture_their_defining_scope() {
        assert_module_value(
            "add_base =\n  base = 10\n  (x) => x + base\nbase = 1\nadd_base(2)\n",
            Value::Int(12),
        );
    }

    #[test]
    fn reports_function_arity_mismatch() {
        let diagnostic = module_error("id = (x) => x\nid()\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::ARITY_MISMATCH)
        );
    }

    #[test]
    fn applies_trailing_parameter_default_when_omitted() {
        assert_module_value("f = (x, y = 10) => x + y\nf(1)\n", Value::Int(11));
    }

    #[test]
    fn supplied_argument_overrides_parameter_default() {
        assert_module_value("f = (x, y = 10) => x + y\nf(1, 2)\n", Value::Int(3));
    }

    #[test]
    fn default_may_reference_an_earlier_parameter() {
        assert_module_value("g = (x, y = x + 1) => y\ng(5)\n", Value::Int(6));
    }

    #[test]
    fn unannotated_single_default_applies_with_no_args() {
        assert_module_value(
            "greet = (name = \"world\") => name\ngreet()\n",
            Value::Text("world".to_owned()),
        );
    }

    #[test]
    fn default_is_not_evaluated_when_argument_supplied() {
        assert_module_value("h = (x, y = 1 / 0) => x\nh(7, 2)\n", Value::Int(7));
    }

    #[test]
    fn omitted_default_evaluates_and_can_fail() {
        let diagnostic = module_error("h = (x, y = 1 / 0) => x\nh(7)\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::DIVISION_BY_ZERO)
        );
    }

    #[test]
    fn reports_too_few_arguments_below_required() {
        let diagnostic = module_error("f = (x, y = 10) => x + y\nf()\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::ARITY_MISMATCH)
        );
    }

    #[test]
    fn reports_too_many_arguments_above_total() {
        let diagnostic = module_error("f = (x, y = 10) => x + y\nf(1, 2, 3)\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::ARITY_MISMATCH)
        );
    }

    #[test]
    fn reports_calling_non_function_values() {
        let diagnostic = eval_error("5(1)");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::NOT_CALLABLE)
        );
    }

    #[test]
    fn evaluates_native_host_function_through_field_access() {
        let captured = Rc::new(RefCell::new(Vec::new()));
        let capture = Rc::clone(&captured);
        let host = host_with(
            "log",
            Value::native(move |args| {
                if args.len() != 1 || args.first() != Some(&Value::Text("hi".to_owned())) {
                    return Err(format!("unexpected args: {args:?}"));
                }
                capture.borrow_mut().push(args[0].to_string());
                Ok(Value::unit())
            }),
        );
        let module = parse_ok("Host.Native.log(\"hi\")\n");

        let outcome = eval_module_with_globals(&module, vec![("Host".to_owned(), host)]);

        assert_eq!(
            outcome,
            EvalOutcome {
                value: Some(Value::unit()),
                diagnostics: Vec::new()
            }
        );
        assert_eq!(captured.borrow().clone(), vec!["hi".to_owned()]);
    }

    #[test]
    fn reports_native_host_errors_at_call_span() {
        let host = host_with("fail", Value::native(|_| Err("native failure".to_owned())));
        let module = parse_ok("Host.Native.fail(\"hi\")\n");
        let call_span = module_expr_span(&module);

        let outcome = eval_module_with_globals(&module, vec![("Host".to_owned(), host)]);

        assert_eq!(outcome.value, None);
        assert_eq!(outcome.diagnostics.len(), 1);
        let diagnostic = &outcome.diagnostics[0];
        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
        assert_eq!(diagnostic.labels[0].span, call_span);
        assert_eq!(diagnostic.labels[0].message, "native failure");
    }

    #[test]
    fn log_info_emits_message_fields_and_trace_context() {
        let records = Rc::new(RefCell::new(Vec::new()));
        let logger = capturing_logger(Rc::clone(&records));
        let module = parse_ok("logger.info(\"hi\", { userId: 42 })\n");

        let outcome = eval_module_with_globals(&module, vec![("logger".to_owned(), logger)]);

        assert_eq!(
            outcome,
            EvalOutcome {
                value: Some(Value::unit()),
                diagnostics: Vec::new()
            }
        );
        let records = records.borrow();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.level, logging::Level::Info);
        assert_eq!(record.message, "hi");
        assert_eq!(
            record_field_value(&record.attributes, "userId"),
            Some(&Value::Int(42))
        );
        assert_eq!(record.trace, fixed_trace_context());
    }

    #[test]
    fn child_logger_inherits_trace_and_merges_bound_context() {
        let records = Rc::new(RefCell::new(Vec::new()));
        let logger = capturing_logger(Rc::clone(&records));
        let module = parse_ok(
            "requestLog = logger.child({ requestId: \"r1\" })\nrequestLog.info(\"child\")\n",
        );

        let outcome = eval_module_with_globals(&module, vec![("logger".to_owned(), logger)]);

        assert_eq!(outcome.value, Some(Value::unit()));
        assert!(outcome.diagnostics.is_empty());
        let records = records.borrow();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(
            record_field_value(&record.attributes, "requestId"),
            Some(&Value::Text("r1".to_owned()))
        );
        assert_eq!(record.trace, fixed_trace_context());
    }

    #[test]
    fn child_logger_trace_keys_update_trace_context_not_attributes() {
        let records = Rc::new(RefCell::new(Vec::new()));
        let logger = capturing_logger(Rc::clone(&records));
        let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        let module = parse_ok(&format!(
            "child = logger.child({{ traceId: \"{trace_id}\", requestId: \"r1\" }})\nchild.info(\"child\")\n"
        ));

        let outcome = eval_module_with_globals(&module, vec![("logger".to_owned(), logger)]);

        assert_eq!(outcome.value, Some(Value::unit()));
        assert!(outcome.diagnostics.is_empty());
        let records = records.borrow();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.trace.trace_id, trace_id);
        assert_eq!(record.trace.span_id, fixed_trace_context().span_id);
        assert!(record_field_value(&record.attributes, "traceId").is_none());
        assert_eq!(
            record_field_value(&record.attributes, "requestId"),
            Some(&Value::Text("r1".to_owned()))
        );
    }

    #[test]
    fn log_message_validation_reports_platform_error() {
        let records = Rc::new(RefCell::new(Vec::new()));
        let logger = capturing_logger(records);
        let diagnostic =
            module_error_with_globals("logger.info(5)\n", vec![("logger".to_owned(), logger)]);

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
        assert!(
            diagnostic.labels[0]
                .message
                .contains("log.info message must be Text"),
            "expected message-first validation, got {:?}",
            diagnostic.labels
        );
    }

    #[test]
    fn log_level_severity_numbers_match_otel() {
        assert_eq!(logging::Level::Trace.severity_number(), 1);
        assert_eq!(logging::Level::Debug.severity_number(), 5);
        assert_eq!(logging::Level::Info.severity_number(), 9);
        assert_eq!(logging::Level::Warn.severity_number(), 13);
        assert_eq!(logging::Level::Error.severity_number(), 17);
        assert_eq!(logging::Level::Fatal.severity_number(), 21);
    }

    #[test]
    fn module_bindings_can_shadow_injected_globals() {
        let toolbox = host_with(
            "log",
            Value::native(|_| Err("injected host should be shadowed".to_owned())),
        );
        let module = parse_ok(
            "toolbox = { Native: { log: (message) => message } }\ntoolbox.Native.log(\"local\")\n",
        );

        let outcome = eval_module_with_globals(&module, vec![("toolbox".to_owned(), toolbox)]);

        assert_eq!(
            outcome,
            EvalOutcome {
                value: Some(Value::Text("local".to_owned())),
                diagnostics: Vec::new()
            }
        );
    }

    #[test]
    fn closures_resolve_sibling_top_level_functions_at_call_time() {
        assert_module_value("f = (x) => g(x)\ng = (x) => x + 1\nf(2)\n", Value::Int(3));
    }

    #[test]
    fn evaluates_block_bindings_and_result() {
        assert_module_value(
            "result =\n  a = 2\n  b = a * 3\n  b + 1\nresult\n",
            Value::Int(7),
        );
    }

    #[test]
    fn block_local_shadowing_does_not_leak() {
        assert_module_value("x = 1\nshadow =\n  x = 2\n  x\nx\n", Value::Int(1));
    }

    #[test]
    fn explicit_shadow_rhs_sees_old_binding_and_does_not_leak() {
        assert_module_value(
            "make = (value) =>\n  inner =\n    value := value + 1\n    value\n  (inner, value)\nmake(10)\n",
            tuple_value(vec![Value::Int(11), Value::Int(10)]),
        );
    }

    #[test]
    fn evaluates_block_without_trailing_expression_to_undefined() {
        assert_module_value("value =\n  x = 1\nvalue\n", Value::Undefined);
    }

    #[test]
    fn reports_unbound_name_references() {
        let diagnostic = eval_error("missing");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::UNBOUND_NAME)
        );
    }

    #[test]
    fn reports_forward_references_as_unbound() {
        let module = parse_ok("a = b\nb = 1\n");
        let outcome = eval_module(&module);

        assert_eq!(outcome.value, None);
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(
            outcome.diagnostics[0].code.as_deref(),
            Some(codes::runtime::UNBOUND_NAME)
        );
    }

    #[test]
    fn evaluates_record_literals_and_field_access() {
        assert_module_value(
            "user = { name: \"Ada\", age: 36 }\nuser.name\n",
            Value::Text("Ada".to_owned()),
        );
        assert_eq!(
            format!(
                "{}",
                record_value(vec![
                    ("name", Value::Text("Ada".to_owned())),
                    ("age", Value::Int(36))
                ])
            ),
            "{ name: \"Ada\", age: 36 }"
        );
    }

    #[test]
    fn evaluates_record_spread_with_overwrite() {
        assert_module_value(
            "user = { name: \"Ada\", age: 36 }\nolder = { ..user, age :: 37 }\nolder.age\n",
            Value::Int(37),
        );
    }

    #[test]
    fn evaluates_record_delete() {
        assert_module_value(
            "user = { name: \"Ada\", age: 36 }\ncleaned = { ..user, -age }\ncleaned\n",
            record_value(vec![("name", Value::Text("Ada".to_owned()))]),
        );
    }

    #[test]
    fn evaluates_record_rename() {
        assert_module_value(
            "user = { name: \"Ada\", age: 36 }\nrenamed = { ..user, name -> handle }\nrenamed.handle\n",
            Value::Text("Ada".to_owned()),
        );
    }

    #[test]
    fn evaluates_record_shorthand() {
        assert_module_value(
            "name = \"Ada\"\nage = 36\nuser = { name, age }\nuser.age\n",
            Value::Int(36),
        );
    }

    #[test]
    fn evaluates_computed_record_field_and_delete() {
        assert_module_value(
            "key = \"handle\"\nremove = \"age\"\nuser = { name: \"Ada\", age: 36, [key]: \"ada\" }\ncleaned = { ..user, -[remove] }\ncleaned[\"handle\"]\n",
            Value::Text("ada".to_owned()),
        );
    }

    #[test]
    fn evaluates_nested_record_access() {
        assert_module_value(
            "user = { profile: { name: \"Ada\" } }\nuser.profile.name\n",
            Value::Text("Ada".to_owned()),
        );
    }

    #[test]
    fn record_equality_is_order_independent() {
        assert_eval("{ a: 1, b: 2 } == { b: 2, a: 1 }", Value::Bool(true));
    }

    #[test]
    fn evaluates_record_comprehension_pick_over_literal_set() {
        assert_module_value(
            "user = { name: \"Ada\", email: \"ada@x.dev\", age: 36 }\n\
             pick = (o, keys) => { keys -> k; (k, o[k]) }\n\
             pick(user, @{\"name\", \"email\"})\n",
            record_value(vec![
                ("name", Value::Text("Ada".to_owned())),
                ("email", Value::Text("ada@x.dev".to_owned())),
            ]),
        );
    }

    #[test]
    fn evaluates_record_comprehension_omit_with_keysof_and_has_guard() {
        assert_module_value(
            "user = { name: \"Ada\", email: \"ada@x.dev\" }\n\
             omit = (o, drop) => { keysOf(o) -> k, !drop.has(k); (k, o[k]) }\n\
             omit(user, @{\"name\"})\n",
            record_value(vec![("email", Value::Text("ada@x.dev".to_owned()))]),
        );
    }

    #[test]
    fn record_comprehension_guard_filters_iterations() {
        assert_eval(
            "{ @{\"name\", \"email\"} -> k, k == \"email\"; (k, k) }",
            record_value(vec![("email", Value::Text("email".to_owned()))]),
        );
    }

    #[test]
    fn record_comprehension_non_bool_guard_reports_type_error() {
        let diagnostic = eval_error("{ @{\"name\"} -> k, k; (k, 1) }");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
        assert_eq!(diagnostic.labels[0].message, "expected a Bool guard");
    }

    #[test]
    fn record_comprehension_can_iterate_record_labels() {
        assert_eval(
            "{ { name: \"Ada\", email: \"ada@x.dev\" } -> k; (k, k) }",
            record_value(vec![
                ("name", Value::Text("name".to_owned())),
                ("email", Value::Text("email".to_owned())),
            ]),
        );
    }

    #[test]
    fn record_comprehension_source_type_error_reports_type_error() {
        let diagnostic = eval_error("{ 1 -> k; (k, 1) }");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    }

    #[test]
    fn tuple_emit_in_record_inserts_field() {
        assert_eval(
            "{ (\"name\", \"Ada\") }",
            record_value(vec![("name", Value::Text("Ada".to_owned()))]),
        );
    }

    #[test]
    fn tuple_emit_requires_text_label() {
        let diagnostic = eval_error("{ (1, \"Ada\") }");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
        assert!(diagnostic.labels[0].message.contains("Text label"));
    }

    #[test]
    fn tuple_emit_requires_arity_two_tuple() {
        let diagnostic = eval_error("{ (\"name\", \"Ada\", 1) }");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
        assert!(diagnostic.labels[0].message.contains("Text label"));
    }

    #[test]
    fn keyof_returns_record_labels_as_set() {
        assert_module_value(
            "keysOf({ name: \"Ada\", email: \"ada@x.dev\" })\n",
            set_value(vec![
                Value::Text("name".to_owned()),
                Value::Text("email".to_owned()),
            ]),
        );
    }

    #[test]
    fn keyof_non_record_reports_platform_error() {
        let diagnostic = module_error("keysOf(1)\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }

    #[test]
    fn pick_keeps_named_fields_in_record_order() {
        assert_module_value(
            "pick({ name: \"Ada\", email: \"a@x\", age: 3 }, @{\"name\", \"email\"})\n",
            record_value(vec![
                ("name", Value::Text("Ada".to_owned())),
                ("email", Value::Text("a@x".to_owned())),
            ]),
        );
    }

    #[test]
    fn omit_drops_named_fields_in_record_order() {
        assert_module_value(
            "omit({ name: \"Ada\", email: \"a@x\" }, @{\"name\"})\n",
            record_value(vec![("email", Value::Text("a@x".to_owned()))]),
        );
    }

    #[test]
    fn omit_runs_uniformly_on_a_type_record() {
        // The headline case: a record *type* is just a record whose values are
        // types, so `omit` runs at runtime over it with no special casing.
        assert_module_value(
            "omit({ name: Text, email: Text }, @{\"name\"})\n",
            record_value(vec![("email", Value::Type("Text".to_owned()))]),
        );
    }

    #[test]
    fn pick_skips_keys_absent_from_the_record() {
        assert_module_value(
            "pick({ name: \"Ada\" }, @{\"name\", \"missing\"})\n",
            record_value(vec![("name", Value::Text("Ada".to_owned()))]),
        );
    }

    #[test]
    fn pick_non_record_reports_platform_error() {
        let diagnostic = module_error("pick(5, @{\"a\"})\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }

    #[test]
    fn pick_non_set_reports_platform_error() {
        for source in ["pick({ a: 1 }, [1])\n", "pick({ a: 1 }, \"a\")\n"] {
            let diagnostic = module_error(source);

            assert_eq!(
                diagnostic.code.as_deref(),
                Some(codes::runtime::PLATFORM_ERROR)
            );
        }
    }

    #[test]
    fn pick_non_text_set_member_reports_platform_error() {
        let diagnostic = module_error("pick({ a: 1 }, @{1})\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }

    #[test]
    fn pick_wrong_arity_reports_platform_error() {
        let diagnostic = module_error("pick({ a: 1 })\n");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }

    #[test]
    fn user_binding_shadows_pick_builtin() {
        assert_module_value("pick = 5\npick\n", Value::Int(5));
    }

    #[test]
    fn set_and_array_has_report_membership() {
        assert_eval("@{\"name\", \"email\"}.has(\"name\")", Value::Bool(true));
        assert_eval("@{\"name\", \"email\"}.has(\"age\")", Value::Bool(false));
        assert_eval("[1, 2, 3].has(2)", Value::Bool(true));
        assert_eval("[1, 2, 3].has(4)", Value::Bool(false));
    }

    #[test]
    fn has_on_unsupported_receiver_still_reports_type_error() {
        let diagnostic = eval_error("1.has(1)");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    }

    #[test]
    fn evaluates_array_literals_and_indexing() {
        assert_eval(
            "[10, 20, 30]",
            array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)]),
        );
        assert_module_value("xs = [10, 20, 30]\nxs[1]\n", Value::Int(20));
        assert_module_value("xs = [10, 20, 30]\nxs[9]\n", Value::Undefined);
        assert_module_value("xs = [10, 20, 30]\nxs[-1]\n", Value::Undefined);
        assert_eq!(
            format!(
                "{}",
                array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
            ),
            "[10, 20, 30]"
        );
    }

    #[test]
    fn evaluates_tuple_literals_and_indexing() {
        assert_eval(
            "(1, \"a\")",
            tuple_value(vec![Value::Int(1), Value::Text("a".to_owned())]),
        );
        assert_eval("(1, \"a\")[0]", Value::Int(1));
        assert_eq!(
            format!(
                "{}",
                tuple_value(vec![Value::Int(1), Value::Text("a".to_owned())])
            ),
            "(1, \"a\")"
        );
    }

    #[test]
    fn reports_tuple_index_out_of_bounds() {
        let diagnostic = eval_error("(1, \"a\")[2]");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::INDEX_OUT_OF_BOUNDS)
        );
    }

    #[test]
    fn evaluates_empty_tuple_as_unit() {
        assert_eval("()", tuple_value(Vec::new()));
        assert_eq!(format!("{}", tuple_value(Vec::new())), "()");
    }

    #[test]
    fn evaluates_set_literals_with_deduplication() {
        assert_eval(
            "@{ 1, 2, 2, 3 }",
            set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
        assert_eval("@{ 1, 2, 3 } == @{ 3, 2, 1 }", Value::Bool(true));
        assert_eq!(
            format!(
                "{}",
                set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            ),
            "@{ 1, 2, 3 }"
        );
    }

    #[test]
    fn evaluates_set_union_promotes_singletons() {
        assert_eval(
            "\"r\" | \"w\"",
            set_value(vec![
                Value::Text("r".to_owned()),
                Value::Text("w".to_owned()),
            ]),
        );
    }

    #[test]
    fn evaluates_set_union_splices_set_operands() {
        assert_eval(
            "@{ 1, 2 } | 3",
            set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
        assert_eval(
            "@{ 1, 2 } | @{ 2, 3 }",
            set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn evaluates_set_union_deduplicates() {
        assert_eval("1 | 1", set_value(vec![Value::Int(1)]));
    }

    #[test]
    fn evaluates_tuple_patterns() {
        assert_module_value("pair = (1, \"a\")\npair ?>\n  (n, t) => n\n", Value::Int(1));
    }

    #[test]
    fn evaluates_null_safe_field_access() {
        assert_eval("undefined?.name", Value::Undefined);
        assert_eval("null?.name", Value::Null);
        assert_eval("{ name: \"Ada\" }?.name", Value::Text("Ada".to_owned()));
    }

    #[test]
    fn evaluates_null_coalescing_with_short_circuiting() {
        assert_eval("undefined ?? 5", Value::Int(5));
        assert_eval("null ?? 6", Value::Int(6));
        assert_eval("7 ?? 1 / 0", Value::Int(7));
    }

    #[test]
    fn evaluates_variant_tags() {
        assert_eval(
            "@Ok(1)",
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(1)],
            },
        );
        assert_eval(
            "@Red",
            Value::Tag {
                name: "Red".to_owned(),
                payload: Vec::new(),
            },
        );
    }

    #[test]
    fn evaluates_variant_tags_with_multiple_payload_args() {
        assert_eval(
            "@Rgb(1, 2, 3)",
            Value::Tag {
                name: "Rgb".to_owned(),
                payload: vec![Value::Int(1), Value::Int(2), Value::Int(3)],
            },
        );
    }

    #[test]
    fn evaluates_literal_union_match() {
        assert_eval(
            "1 ?>\n  0 => \"zero\"\n  1 => \"one\"\n  _ => \"many\"\n",
            Value::Text("one".to_owned()),
        );
    }

    #[test]
    fn evaluates_literal_or_pattern_first_alternative() {
        assert_eval("\"r\" ?>\n  \"r\" | \"w\" => 1\n  _ => 0\n", Value::Int(1));
    }

    #[test]
    fn evaluates_literal_or_pattern_second_alternative() {
        assert_eval("\"w\" ?>\n  \"r\" | \"w\" => 1\n  _ => 0\n", Value::Int(1));
    }

    #[test]
    fn evaluates_tag_or_pattern() {
        assert_eval(
            "@Green ?>\n  @Red | @Green => 1\n  @Blue => 0\n",
            Value::Int(1),
        );
    }

    #[test]
    fn evaluates_default_match_arm() {
        assert_eval(
            "2 ?>\n  0 => \"zero\"\n  1 => \"one\"\n  _ => \"many\"\n",
            Value::Text("many".to_owned()),
        );
    }

    #[test]
    fn evaluates_variant_match_payload_bindings() {
        assert_module_value(
            "result = @Ok(41)\nresult ?>\n  @Ok(x) => x + 1\n  @Err(error) => error\n",
            Value::Int(42),
        );
    }

    #[test]
    fn evaluates_guarded_match_arms() {
        assert_eval(
            "1 ?>\n  n, n > 0 => \"pos\"\n  _ => \"other\"\n",
            Value::Text("pos".to_owned()),
        );
        assert_eval(
            "-1 ?>\n  n, n > 0 => \"pos\"\n  _ => \"other\"\n",
            Value::Text("other".to_owned()),
        );
    }

    #[test]
    fn variable_patterns_do_not_match_undefined() {
        assert_eval(
            "undefined ?>\n  value => value\n  undefined => \"empty\"\n",
            Value::Text("empty".to_owned()),
        );
    }

    #[test]
    fn evaluates_record_patterns() {
        assert_module_value(
            "user = { name: \"Ada\", age: 36 }\nuser ?>\n  { name } => name\n",
            Value::Text("Ada".to_owned()),
        );
    }

    #[test]
    fn reports_match_without_matching_arm() {
        let diagnostic = eval_error("2 ?>\n  0 => \"zero\"\n");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::NO_MATCH));
    }

    #[test]
    fn evaluates_recursive_factorial_with_match_base_case() {
        assert_module_value(
            "fact = (n) =>\n  n ?>\n    0 => 1\n    _ => n * fact(n - 1)\nfact(5)\n",
            Value::Int(120),
        );
    }

    #[test]
    fn evaluates_mutually_recursive_functions_with_match_base_cases() {
        assert_module_value(
            "isEven = (n) =>\n  n ?>\n    0 => true\n    _ => isOdd(n - 1)\nisOdd = (n) =>\n  n ?>\n    0 => false\n    _ => isEven(n - 1)\nisEven(6)\n",
            Value::Bool(true),
        );
    }

    #[test]
    fn reports_missing_record_fields() {
        let diagnostic = eval_error("{ name: \"Ada\" }.age");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::MISSING_FIELD)
        );
    }

    #[test]
    fn reports_field_access_on_non_record() {
        let diagnostic = eval_error("1.name");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    }

    #[test]
    fn primitive_type_name_evaluates_to_type_value() {
        assert_module_value("Text\n", Value::Type("Text".to_owned()));
        assert_eq!(format!("{}", Value::Type("Text".to_owned())), "Text");
    }

    #[test]
    fn record_of_types_evaluates_and_displays_as_type_record() {
        let expected = record_value(vec![
            ("name", Value::Type("Text".to_owned())),
            ("age", Value::Type("Int".to_owned())),
        ]);
        assert_module_value("{ name: Text, age: Int }\n", expected.clone());
        assert_eq!(format!("{expected}"), "{ name: Text, age: Int }");
    }

    #[test]
    fn type_alias_binding_yields_record_of_types_and_keysof() {
        assert_module_value(
            "User = { name: Text, email: Text }\nUser\n",
            record_value(vec![
                ("name", Value::Type("Text".to_owned())),
                ("email", Value::Type("Text".to_owned())),
            ]),
        );
        assert_module_value(
            "User = { name: Text, email: Text }\nkeysOf(User)\n",
            set_value(vec![
                Value::Text("name".to_owned()),
                Value::Text("email".to_owned()),
            ]),
        );
    }

    #[test]
    fn type_values_compare_by_name() {
        assert_module_value("Text == Text\n", Value::Bool(true));
        assert_module_value("Text == Int\n", Value::Bool(false));
    }

    #[test]
    fn user_binding_shadows_primitive_type_name() {
        assert_module_value("Text = 5\nText\n", Value::Int(5));
    }

    #[test]
    fn propagate_unwraps_ok_payload() {
        assert_eval("@Ok(7)?^", Value::Int(7));
    }

    #[test]
    fn propagate_err_early_returns_enclosing_function() {
        // `?^` on `@Err` returns that whole `@Err` as the function's value, and
        // short-circuits: the unbound `missing` after it must never evaluate.
        assert_module_value(
            "f = (r) =>\n  x = r?^\n  missing\nf(@Err(\"boom\"))\n",
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("boom".to_owned())],
            },
        );
    }

    #[test]
    fn propagate_ok_threads_value_through_function() {
        assert_module_value(
            "f = (r) =>\n  x = r?^\n  x + 1\nf(@Ok(41))\n",
            Value::Int(42),
        );
    }

    #[test]
    fn top_level_propagate_err_becomes_program_value_and_stops() {
        // The `@Err` becomes the program value; the unbound `missing` after it
        // must not run.
        let module = parse_ok("@Err(\"top\")?^\nmissing\n");
        let outcome = eval_module(&module);

        assert_eq!(
            outcome,
            EvalOutcome {
                value: Some(Value::Tag {
                    name: "Err".to_owned(),
                    payload: vec![Value::Text("top".to_owned())],
                }),
                diagnostics: Vec::new(),
            }
        );
    }

    #[test]
    fn propagate_through_binding_block_bubbles_to_enclosing_function() {
        // A `?^` inside a binding-value block must early-return the function, not
        // make `x` the `@Err` and continue.
        assert_module_value(
            "f = (r) =>\n  x =\n    a = r?^\n    a + 1\n  x + 100\nf(@Err(\"inner\"))\n",
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("inner".to_owned())],
            },
        );
    }

    #[test]
    fn propagate_on_non_result_reports_type_error() {
        let diagnostic = eval_error("5?^");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    }

    #[test]
    fn panic_unwraps_ok_payload() {
        assert_eval("@Ok(9)?!", Value::Int(9));
    }

    #[test]
    fn panic_on_err_reports_runtime_panic_with_payload() {
        let diagnostic = eval_error("@Err(\"kaboom\")?!");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::PANIC));
        assert!(
            diagnostic.message.contains("kaboom"),
            "panic message should embed the @Err payload, got {:?}",
            diagnostic.message
        );
    }

    #[test]
    fn panic_on_non_result_reports_type_error() {
        let diagnostic = eval_error("5?!");

        assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    }

    #[test]
    fn chained_propagation_returns_ok_on_happy_path_and_first_err_on_sad_path() {
        let program = "parse = (n) =>\n  n ?>\n    0 => @Err(\"zero\")\n    _ => @Ok(n)\n\
             chain = (a, b) =>\n  x = parse(a)?^\n  y = parse(b)?^\n  @Ok(x + y)\n";
        assert_module_value(
            &format!("{program}chain(2, 3)\n"),
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(5)],
            },
        );
        assert_module_value(
            &format!("{program}chain(0, 3)\n"),
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("zero".to_owned())],
            },
        );
    }

    fn assert_module_value(source: &str, expected: Value) {
        let module = parse_ok(source);
        let outcome = eval_module(&module);

        assert_eq!(
            outcome,
            EvalOutcome {
                value: Some(expected),
                diagnostics: Vec::new()
            }
        );
    }

    fn assert_eval(source: &str, expected: Value) {
        assert_eq!(eval_source(source).expect("evaluation failed"), expected);
    }

    fn eval_error(source: &str) -> aven_core::Diagnostic {
        eval_source(source).expect_err("expected evaluation error")
    }

    fn module_error(source: &str) -> aven_core::Diagnostic {
        let module = parse_ok(source);
        let mut diagnostics = eval_module(&module).diagnostics;

        assert_eq!(diagnostics.len(), 1);
        diagnostics.remove(0)
    }

    fn module_error_with_globals(
        source: &str,
        globals: Vec<(String, Value)>,
    ) -> aven_core::Diagnostic {
        let module = parse_ok(source);
        let mut diagnostics = eval_module_with_globals(&module, globals).diagnostics;

        assert_eq!(diagnostics.len(), 1);
        diagnostics.remove(0)
    }

    fn eval_source(source: &str) -> Result<Value, aven_core::Diagnostic> {
        let module = parse_ok(source);
        let Item::Expr(expr) = &module.items[0] else {
            panic!("expected expression item");
        };
        eval_expr(expr, &Environment::new())
    }

    fn record_value(fields: Vec<(&str, Value)>) -> Value {
        Value::record(
            fields
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
        )
    }

    fn array_value(values: Vec<Value>) -> Value {
        Value::Array(Rc::new(values))
    }

    fn tuple_value(values: Vec<Value>) -> Value {
        Value::Tuple(Rc::new(values))
    }

    fn set_value(values: Vec<Value>) -> Value {
        Value::Set(Rc::new(values))
    }

    fn host_with(name: &str, function: Value) -> Value {
        Value::record(vec![(
            "Native".to_owned(),
            Value::record(vec![(name.to_owned(), function)]),
        )])
    }

    #[derive(Debug, Clone, PartialEq)]
    struct CapturedLogRecord {
        level: logging::Level,
        message: String,
        attributes: Vec<(String, Value)>,
        trace: logging::TraceContext,
    }

    struct CapturingLogSink {
        records: Rc<RefCell<Vec<CapturedLogRecord>>>,
    }

    impl logging::LogSink for CapturingLogSink {
        fn emit(&self, record: &logging::LogRecord<'_>) {
            self.records.borrow_mut().push(CapturedLogRecord {
                level: record.level,
                message: record.message.clone(),
                attributes: record.attributes.to_vec(),
                trace: record.trace.clone(),
            });
        }
    }

    fn capturing_logger(records: Rc<RefCell<Vec<CapturedLogRecord>>>) -> Value {
        logging::logger(Rc::new(CapturingLogSink { records }), fixed_trace_context())
    }

    fn fixed_trace_context() -> logging::TraceContext {
        logging::TraceContext {
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_owned(),
            span_id: "b7ad6b7169203331".to_owned(),
            trace_flags: "01".to_owned(),
            trace_state: "test=state".to_owned(),
        }
    }

    fn module_expr_span(module: &Module) -> aven_core::Span {
        let Item::Expr(expr) = &module.items[0] else {
            panic!("expected expression item");
        };
        expr.span
    }

    fn parse_ok(source: &str) -> Module {
        let output = parse_module(source);
        assert!(
            output.diagnostics.is_empty(),
            "unexpected parse diagnostics: {:?}",
            output.diagnostics
        );
        output.module
    }
}
