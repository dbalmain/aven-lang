use std::{cell::RefCell, cmp::Ordering, collections::HashMap, fmt, rc::Rc};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{Expr, ExprKind, Item, Literal, Module};

#[derive(Clone)]
pub struct Closure {
    params: Vec<String>,
    body: Rc<Expr>,
    env: Environment,
}

impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Closure")
            .field("params", &self.params)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Closure(Closure),
    Undefined,
    Null,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Undefined, Self::Undefined) | (Self::Null, Self::Null) => true,
            (Self::Closure(_), _) | (_, Self::Closure(_)) => false,
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
            Self::Closure(_) => write!(f, "<function>"),
            Self::Undefined => write!(f, "undefined"),
            Self::Null => write!(f, "null"),
        }
    }
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Text(_) => "Text",
            Self::Bool(_) => "Bool",
            Self::Closure(_) => "Function",
            Self::Undefined => "Undefined",
            Self::Null => "Null",
        }
    }
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
    let env = Environment::new();
    eval_items(&module.items, &env)
}

fn eval_items(items: &[Item], env: &Environment) -> EvalOutcome {
    let mut value = None;
    let mut diagnostics = Vec::new();

    for item in items {
        match item {
            Item::Expr(expr) => match eval_expr_many(expr, env) {
                Ok(next_value) => value = Some(next_value),
                Err(mut next_diagnostics) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::Binding(binding) => match eval_expr_many(&binding.value, env) {
                Ok(next_value) => {
                    env.bind(binding.name.clone(), next_value);
                    value = None;
                }
                Err(mut next_diagnostics) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::Signature(_) => value = None,
        }
    }

    EvalOutcome { value, diagnostics }
}

pub fn eval_expr(expr: &Expr, env: &Environment) -> Result<Value, Diagnostic> {
    eval_expr_many(expr, env).map_err(first_diagnostic)
}

fn eval_expr_many(expr: &Expr, env: &Environment) -> Result<Value, Vec<Diagnostic>> {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(literal, expr.span).map_err(one_diagnostic),
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
            params: params.iter().map(|param| param.name.clone()).collect(),
            body: Rc::new((**body).clone()),
            env: env.clone(),
        })),
        ExprKind::Call { callee, args } => eval_call(callee, args, expr.span, env),
        _ => Err(one_diagnostic(unsupported_expr(
            expr.span,
            "this expression is not supported by the current evaluator",
        ))),
    }
}

fn eval_block(items: &[Item], env: &Environment) -> Result<Value, Vec<Diagnostic>> {
    let child = env.child();
    let outcome = eval_items(items, &child);

    if outcome.diagnostics.is_empty() {
        Ok(outcome.value.unwrap_or(Value::Undefined))
    } else {
        Err(outcome.diagnostics)
    }
}

fn eval_call(
    callee: &Expr,
    args: &[Expr],
    span: Span,
    env: &Environment,
) -> Result<Value, Vec<Diagnostic>> {
    let callee_value = eval_expr_many(callee, env)?;
    let closure = match callee_value {
        Value::Closure(closure) => closure,
        value => return Err(one_diagnostic(not_callable(callee.span, value.type_name()))),
    };

    if args.len() != closure.params.len() {
        return Err(one_diagnostic(arity_mismatch(
            span,
            closure.params.len(),
            args.len(),
        )));
    }

    let mut arg_values = Vec::with_capacity(args.len());
    for arg in args {
        arg_values.push(eval_expr_many(arg, env)?);
    }

    let call_env = closure.env.child();
    for (name, value) in closure.params.iter().zip(arg_values) {
        call_env.bind(name.clone(), value);
    }

    eval_expr_many(closure.body.as_ref(), &call_env)
}

fn eval_literal(literal: &Literal, span: Span) -> Result<Value, Diagnostic> {
    match literal {
        Literal::Bool(value) => Ok(Value::Bool(*value)),
        Literal::Number(text) => eval_number_literal(text, span),
        Literal::String(text) => Ok(Value::Text(decode_string_literal(text))),
        Literal::Regex(_) | Literal::Path(_) | Literal::Label(_) => Err(unsupported_expr(
            span,
            "this literal kind is not supported by the current evaluator",
        )),
    }
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

    let mut decoded = String::new();
    let mut escaped = false;

    for ch in inner.chars() {
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

fn eval_unary(
    operator: &str,
    value: &Expr,
    span: Span,
    env: &Environment,
) -> Result<Value, Vec<Diagnostic>> {
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
) -> Result<Value, Vec<Diagnostic>> {
    match operator {
        "&&" => eval_boolean_and(left, right, span, env),
        "||" => eval_boolean_or(left, right, span, env),
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

fn eval_boolean_and(
    left: &Expr,
    right: &Expr,
    span: Span,
    env: &Environment,
) -> Result<Value, Vec<Diagnostic>> {
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

fn eval_boolean_or(
    left: &Expr,
    right: &Expr,
    span: Span,
    env: &Environment,
) -> Result<Value, Vec<Diagnostic>> {
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
        _ => Err(unsupported_operator(operator, operator_span)),
    }
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

fn arity_mismatch(span: Span, expected: usize, got: usize) -> Diagnostic {
    Diagnostic::error("function arity mismatch")
        .with_code(codes::runtime::ARITY_MISMATCH)
        .with_label(Label::primary(
            span,
            format!("expected {expected} argument(s), got {got}"),
        ))
        .with_note(format!(
            "this function expects {expected} argument(s), but the call supplied {got}"
        ))
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
            "the evaluator currently supports literals, names, bindings, blocks, lambdas, calls, unary operators, and core binary operators",
        )
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

fn one_diagnostic(diagnostic: Diagnostic) -> Vec<Diagnostic> {
    vec![diagnostic]
}

fn first_diagnostic(diagnostics: Vec<Diagnostic>) -> Diagnostic {
    diagnostics
        .into_iter()
        .next()
        .expect("expression errors include at least one diagnostic")
}

#[cfg(test)]
mod tests {
    use super::{Environment, EvalOutcome, Value, eval_expr, eval_module};
    use aven_core::codes;
    use aven_parser::{Item, Module, parse_module};

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
    fn reports_calling_non_function_values() {
        let diagnostic = eval_error("5(1)");

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::NOT_CALLABLE)
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

    fn eval_source(source: &str) -> Result<Value, aven_core::Diagnostic> {
        let module = parse_ok(source);
        let Item::Expr(expr) = &module.items[0] else {
            panic!("expected expression item");
        };
        eval_expr(expr, &Environment::new())
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
