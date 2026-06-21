use std::cmp::Ordering;
use std::fmt;

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{Binding, Expr, ExprKind, Item, Literal, Module};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Undefined,
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::Text(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
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
            Self::Undefined => "Undefined",
            Self::Null => "Null",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalOutcome {
    pub value: Option<Value>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn eval_module(module: &Module) -> EvalOutcome {
    let mut value = None;
    let mut diagnostics = Vec::new();

    for item in &module.items {
        match item {
            Item::Expr(expr) => match eval_expr(expr) {
                Ok(next_value) => value = Some(next_value),
                Err(diagnostic) => diagnostics.push(diagnostic),
            },
            Item::Signature(_) => {}
            Item::Binding(binding) => diagnostics.push(unsupported_binding(binding)),
        }
    }

    EvalOutcome { value, diagnostics }
}

pub fn eval_expr(expr: &Expr) -> Result<Value, Diagnostic> {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(literal, expr.span),
        ExprKind::Undefined => Ok(Value::Undefined),
        ExprKind::Null => Ok(Value::Null),
        ExprKind::Group(inner) => eval_expr(inner),
        ExprKind::Unary {
            operator, value, ..
        } => eval_unary(operator, value, expr.span),
        ExprKind::Binary {
            left,
            operator,
            operator_span,
            right,
        } => eval_binary(left, operator, *operator_span, right, expr.span),
        _ => Err(unsupported_expr(
            expr.span,
            "this expression is not supported by the E1 evaluator",
        )),
    }
}

fn eval_literal(literal: &Literal, span: Span) -> Result<Value, Diagnostic> {
    match literal {
        Literal::Bool(value) => Ok(Value::Bool(*value)),
        Literal::Number(text) => eval_number_literal(text, span),
        Literal::String(text) => Ok(Value::Text(decode_string_literal(text))),
        Literal::Regex(_) | Literal::Path(_) | Literal::Label(_) => Err(unsupported_expr(
            span,
            "this literal kind is not supported by the E1 evaluator",
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

fn eval_unary(operator: &str, value: &Expr, span: Span) -> Result<Value, Diagnostic> {
    let value = eval_expr(value)?;

    match (operator, value) {
        ("-", Value::Int(value)) => value
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| integer_overflow(span, "unary `-`")),
        ("-", Value::Float(value)) => Ok(Value::Float(-value)),
        ("-", value) => Err(unary_type_error(
            span,
            "-",
            value.type_name(),
            "a numeric operand",
        )),
        ("!", Value::Bool(value)) => Ok(Value::Bool(!value)),
        ("!", value) => Err(unary_type_error(
            span,
            "!",
            value.type_name(),
            "a Bool operand",
        )),
        _ => Err(unsupported_expr(
            span,
            "this unary operator is not supported by the E1 evaluator",
        )),
    }
}

fn eval_binary(
    left: &Expr,
    operator: &str,
    operator_span: Span,
    right: &Expr,
    span: Span,
) -> Result<Value, Diagnostic> {
    match operator {
        "&&" => eval_boolean_and(left, right, span),
        "||" => eval_boolean_or(left, right, span),
        _ => {
            let left_value = eval_expr(left)?;
            let right_value = eval_expr(right)?;
            apply_binary(
                left_value,
                operator,
                operator_span,
                right_value,
                right.span,
                span,
            )
        }
    }
}

fn eval_boolean_and(left: &Expr, right: &Expr, span: Span) -> Result<Value, Diagnostic> {
    match eval_expr(left)? {
        Value::Bool(false) => Ok(Value::Bool(false)),
        Value::Bool(true) => match eval_expr(right)? {
            Value::Bool(value) => Ok(Value::Bool(value)),
            value => Err(binary_type_error(
                span,
                "&&",
                "Bool",
                value.type_name(),
                "Bool operands",
            )),
        },
        value => Err(binary_type_error(
            span,
            "&&",
            value.type_name(),
            "Bool",
            "Bool operands",
        )),
    }
}

fn eval_boolean_or(left: &Expr, right: &Expr, span: Span) -> Result<Value, Diagnostic> {
    match eval_expr(left)? {
        Value::Bool(true) => Ok(Value::Bool(true)),
        Value::Bool(false) => match eval_expr(right)? {
            Value::Bool(value) => Ok(Value::Bool(value)),
            value => Err(binary_type_error(
                span,
                "||",
                "Bool",
                value.type_name(),
                "Bool operands",
            )),
        },
        value => Err(binary_type_error(
            span,
            "||",
            value.type_name(),
            "Bool",
            "Bool operands",
        )),
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
            "this numeric operator is not supported by the E1 evaluator",
        )),
    }
}

fn equality(left: Value, operator: &str, right: Value, span: Span) -> Result<Value, Diagnostic> {
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

fn integer_overflow(span: Span, operation: &str) -> Diagnostic {
    Diagnostic::error("integer arithmetic overflow")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, format!("`{operation}` overflowed i64")))
        .with_note("Aven Int currently uses i64; arbitrary precision integers are planned for a later milestone")
}

fn unsupported_binding(binding: &Binding) -> Diagnostic {
    Diagnostic::error("bindings are not supported by the evaluator yet")
        .with_code(codes::runtime::UNSUPPORTED)
        .with_label(Label::primary(
            binding.span,
            "this binding will be evaluated in Milestone E2",
        ))
        .with_note(
            "Milestone E1 evaluates expression items only; bindings arrive with environments in E2",
        )
}

fn unsupported_expr(span: Span, label: &str) -> Diagnostic {
    Diagnostic::error("unsupported runtime expression")
        .with_code(codes::runtime::UNSUPPORTED)
        .with_label(Label::primary(span, label))
        .with_note(
            "Milestone E1 supports literals, grouping, unary operators, and core binary operators",
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

#[cfg(test)]
mod tests {
    use super::{EvalOutcome, Value, eval_expr, eval_module};
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
    fn reports_bindings_as_unsupported_and_continues() {
        let module = parse_ok("value = 1\n2\n");
        let outcome = eval_module(&module);

        assert_eq!(outcome.value, Some(Value::Int(2)));
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(
            outcome.diagnostics[0].code.as_deref(),
            Some(codes::runtime::UNSUPPORTED)
        );
    }

    fn assert_eval(source: &str, expected: Value) {
        assert_eq!(eval_source(source).expect("evaluation failed"), expected);
    }

    fn eval_error(source: &str) -> aven_core::Diagnostic {
        eval_source(source).expect_err("expected evaluation error")
    }

    fn eval_source(source: &str) -> Result<Value, aven_core::Diagnostic> {
        let module = parse_ok(source);
        let Item::Expr(expr) = &module.items[0] else {
            panic!("expected expression item");
        };
        eval_expr(expr)
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
