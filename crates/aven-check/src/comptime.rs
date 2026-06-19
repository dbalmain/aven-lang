use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{Expr, ExprKind, Literal, Param};

use crate::ty::{Row, RowEntry, RowTail, Type, is_concrete_type};

const DEFAULT_EVALUATION_FUEL: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ComptimeValue {
    ReifiedType(Type),
    LabelSet(Vec<String>),
    Literal(Literal),
}

impl ComptimeValue {
    pub(crate) fn reify_type_position(self) -> Self {
        match self {
            ComptimeValue::ReifiedType(ty) => ComptimeValue::ReifiedType(ty),
            ComptimeValue::LabelSet(labels) => ComptimeValue::ReifiedType(label_set_type(labels)),
            ComptimeValue::Literal(literal) => ComptimeValue::ReifiedType(literal_type(literal)),
        }
    }

    pub(crate) fn into_reified_type(self) -> Option<Type> {
        match self {
            ComptimeValue::ReifiedType(ty) => Some(ty),
            ComptimeValue::LabelSet(_) | ComptimeValue::Literal(_) => None,
        }
    }

    pub(crate) fn as_literal(&self) -> Option<&Literal> {
        match self {
            ComptimeValue::Literal(literal) => Some(literal),
            ComptimeValue::ReifiedType(_) | ComptimeValue::LabelSet(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evaluation {
    Evaluated(ComptimeValue),
    Deferred,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvaluationResult {
    pub(crate) evaluation: Evaluation,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl EvaluationResult {
    fn evaluated(value: ComptimeValue) -> Self {
        Self {
            evaluation: Evaluation::Evaluated(value),
            diagnostics: Vec::new(),
        }
    }

    fn deferred() -> Self {
        Self {
            evaluation: Evaluation::Deferred,
            diagnostics: Vec::new(),
        }
    }

    fn unsupported() -> Self {
        Self {
            evaluation: Evaluation::Unsupported,
            diagnostics: Vec::new(),
        }
    }

    fn diagnostic(diagnostic: Diagnostic) -> Self {
        Self {
            evaluation: Evaluation::Deferred,
            diagnostics: vec![diagnostic],
        }
    }

    fn deferred_with_diagnostics(diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            evaluation: Evaluation::Deferred,
            diagnostics,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SpecializationKey {
    function: String,
    args: Vec<ComptimeValue>,
}

impl SpecializationKey {
    pub(crate) fn new(function: &str, args: &[ComptimeValue]) -> Self {
        Self {
            function: function.to_owned(),
            args: args.to_vec(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredType {
    pub(crate) ty: Type,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComptimeFunction<'a> {
    pub(crate) name: &'a str,
    pub(crate) params: &'a [Param],
    pub(crate) body: &'a Expr,
}

pub(crate) trait EvalContext<'a> {
    fn lower_comptime_type(&mut self, expr: &Expr) -> LoweredType;
    fn lookup_comptime_function(&self, name: &str) -> Option<ComptimeFunction<'a>>;
    fn cached_specialization(&self, key: &SpecializationKey) -> Option<EvaluationResult>;
    fn cache_specialization(&mut self, key: SpecializationKey, result: EvaluationResult);
    fn type_is_unresolved(&self, ty: &Type) -> bool;
}

pub(crate) fn evaluate_type_position<'a>(
    context: &mut impl EvalContext<'a>,
    expr: &Expr,
) -> EvaluationResult {
    evaluate_type_position_with_bindings(context, expr, &HashMap::new())
}

pub(crate) fn evaluate_type_position_with_bindings<'a>(
    context: &mut impl EvalContext<'a>,
    expr: &Expr,
    bindings: &HashMap<String, ComptimeValue>,
) -> EvaluationResult {
    let mut evaluator = Evaluator {
        context,
        visited: HashSet::new(),
        fuel: DEFAULT_EVALUATION_FUEL,
        module: PhantomData,
    };
    evaluator.evaluate_expr(expr, &Environment::from_bindings(bindings))
}

pub(crate) fn evaluate_runtime_value(
    expr: &Expr,
    bindings: &HashMap<String, ComptimeValue>,
) -> EvaluationResult {
    let expr = ungroup(expr);
    match &expr.kind {
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => bindings
            .get(name)
            .cloned()
            .map(EvaluationResult::evaluated)
            .unwrap_or_else(EvaluationResult::unsupported),
        ExprKind::Literal(literal @ (Literal::Number(_) | Literal::String(_))) => {
            EvaluationResult::evaluated(ComptimeValue::Literal(literal.clone()))
        }
        ExprKind::Group(_) => unreachable!("group expressions are removed before evaluation"),
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Tag(_)
        | ExprKind::Array(_)
        | ExprKind::Tuple(_)
        | ExprKind::Record(_)
        | ExprKind::Set(_)
        | ExprKind::Index { .. }
        | ExprKind::Nullable(_)
        | ExprKind::Arrow { .. }
        | ExprKind::FieldAccess { .. }
        | ExprKind::Call { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Propagate { .. }
        | ExprKind::Match { .. }
        | ExprKind::Lambda { .. }
        | ExprKind::Block(_) => EvaluationResult::unsupported(),
    }
}

struct Evaluator<'ctx, 'a, C>
where
    C: EvalContext<'a> + ?Sized,
{
    context: &'ctx mut C,
    visited: HashSet<SpecializationKey>,
    fuel: usize,
    module: PhantomData<&'a ()>,
}

impl<'a, C> Evaluator<'_, 'a, C>
where
    C: EvalContext<'a> + ?Sized,
{
    fn evaluate_expr(&mut self, expr: &Expr, env: &Environment) -> EvaluationResult {
        let expr = ungroup(expr);
        if let Err(result) = self.consume_fuel(expr.span) {
            return result;
        }

        match &expr.kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                if let Some(value) = env.get(name) {
                    return EvaluationResult::evaluated(value.clone());
                }

                self.evaluate_type_term(expr)
            }
            ExprKind::Call { callee, args } => {
                self.evaluate_application(expr.span, callee, args, env)
            }
            ExprKind::Index { .. }
            | ExprKind::Nullable(_)
            | ExprKind::Arrow { .. }
            | ExprKind::Tuple(_)
            | ExprKind::Record(_)
            | ExprKind::Set(_) => self.evaluate_type_term(expr),
            ExprKind::Group(_) => unreachable!("group expressions are removed before evaluation"),
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Tag(_)
            | ExprKind::Array(_)
            | ExprKind::FieldAccess { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Propagate { .. }
            | ExprKind::Match { .. }
            | ExprKind::Lambda { .. }
            | ExprKind::Block(_) => EvaluationResult::unsupported(),
        }
    }

    fn consume_fuel(&mut self, span: Span) -> Result<(), EvaluationResult> {
        if self.fuel == 0 {
            return Err(EvaluationResult::diagnostic(evaluation_limit(span)));
        }

        self.fuel -= 1;
        Ok(())
    }

    fn evaluate_application(
        &mut self,
        call_span: Span,
        callee: &Expr,
        args: &[Expr],
        env: &Environment,
    ) -> EvaluationResult {
        let Some(name) = callee_name(callee) else {
            return EvaluationResult::unsupported();
        };

        if name == "keysOf" {
            return self.evaluate_keys_of_application(args, env);
        }

        let Some(function) = self.context.lookup_comptime_function(name) else {
            return EvaluationResult::unsupported();
        };

        self.evaluate_function_application(function, call_span, args, env)
    }

    fn evaluate_keys_of_application(
        &mut self,
        args: &[Expr],
        env: &Environment,
    ) -> EvaluationResult {
        let [arg] = args else {
            return EvaluationResult::unsupported();
        };

        let arg_result = self.evaluate_expr(arg, env);
        let subject = match arg_result.evaluation {
            Evaluation::Evaluated(value) => value.reify_type_position().into_reified_type(),
            Evaluation::Deferred => {
                return EvaluationResult::deferred_with_diagnostics(arg_result.diagnostics);
            }
            Evaluation::Unsupported => {
                return EvaluationResult {
                    evaluation: Evaluation::Unsupported,
                    diagnostics: arg_result.diagnostics,
                };
            }
        };
        let Some(subject) = subject else {
            return EvaluationResult::deferred();
        };

        evaluate_keys_of(
            &subject,
            arg.span,
            self.context.type_is_unresolved(&subject),
        )
    }

    fn evaluate_function_application(
        &mut self,
        function: ComptimeFunction<'a>,
        call_span: Span,
        args: &[Expr],
        env: &Environment,
    ) -> EvaluationResult {
        if function.params.len() != args.len() {
            return EvaluationResult::unsupported();
        }

        let values = match self.evaluate_args(args, env) {
            Ok(values) => values,
            Err(result) => return result,
        };
        let key = SpecializationKey::new(function.name, &values);

        if let Some(result) = self.context.cached_specialization(&key) {
            return result;
        }

        if !self.visited.insert(key.clone()) {
            return EvaluationResult::diagnostic(evaluation_cycle(call_span, function.name));
        }

        let body_env = Environment::from_params(function.params, values);
        let result = self.evaluate_expr(function.body, &body_env);
        self.visited.remove(&key);

        if !matches!(result.evaluation, Evaluation::Unsupported) {
            self.context.cache_specialization(key, result.clone());
        }

        result
    }

    fn evaluate_args(
        &mut self,
        args: &[Expr],
        env: &Environment,
    ) -> Result<Vec<ComptimeValue>, EvaluationResult> {
        let mut values = Vec::new();

        for arg in args {
            let arg_result = self.evaluate_expr(arg, env);
            match arg_result.evaluation {
                Evaluation::Evaluated(value) => values.push(value),
                Evaluation::Deferred => {
                    return Err(EvaluationResult::deferred_with_diagnostics(
                        arg_result.diagnostics,
                    ));
                }
                Evaluation::Unsupported => {
                    return Err(EvaluationResult {
                        evaluation: Evaluation::Unsupported,
                        diagnostics: arg_result.diagnostics,
                    });
                }
            }
        }

        Ok(values)
    }

    fn evaluate_type_term(&mut self, expr: &Expr) -> EvaluationResult {
        let lowering = self.context.lower_comptime_type(expr);
        if !lowering.diagnostics.is_empty() {
            return EvaluationResult::deferred_with_diagnostics(lowering.diagnostics);
        }

        if self.context.type_is_unresolved(&lowering.ty) || !is_concrete_type(&lowering.ty) {
            return EvaluationResult::deferred();
        }

        EvaluationResult::evaluated(ComptimeValue::ReifiedType(lowering.ty))
    }
}

pub(crate) fn evaluate_keys_of(
    subject: &Type,
    arg_span: Span,
    subject_is_unresolved: bool,
) -> EvaluationResult {
    if subject_is_unresolved || !is_concrete_type(subject) {
        return EvaluationResult::deferred();
    }

    let Type::Record(row) = subject else {
        return EvaluationResult::diagnostic(reflection_type_mismatch(arg_span));
    };

    if row.tail != RowTail::Closed {
        return EvaluationResult::deferred();
    }

    let mut labels = Vec::new();
    for entry in &row.entries {
        let RowEntry::Field { name, .. } = entry else {
            return EvaluationResult::deferred();
        };
        labels.push(name.clone());
    }
    labels.sort();

    EvaluationResult::evaluated(ComptimeValue::LabelSet(labels))
}

fn label_set_type(labels: Vec<String>) -> Type {
    Type::Variant(Row {
        entries: labels
            .into_iter()
            .map(|label| RowEntry::Literal {
                value: Literal::String(format!("\"{label}\"")),
            })
            .collect(),
        tail: RowTail::Closed,
    })
}

fn literal_type(literal: Literal) -> Type {
    Type::Variant(Row {
        entries: vec![RowEntry::Literal { value: literal }],
        tail: RowTail::Closed,
    })
}

fn reflection_type_mismatch(span: Span) -> Diagnostic {
    Diagnostic::error("reflection function `keysOf` expected a record type")
        .with_code(codes::comptime::REFLECTION_TYPE_MISMATCH)
        .with_label(Label::primary(span, "this type is not a record"))
        .with_note("`keysOf` needs a record type")
}

fn evaluation_cycle(span: Span, function: &str) -> Diagnostic {
    Diagnostic::error(format!(
        "comptime evaluation cycle while specializing `{function}`"
    ))
    .with_code(codes::comptime::EVALUATION_CYCLE)
    .with_label(Label::primary(
        span,
        "this comptime specialization recursively depends on itself",
    ))
    .with_note(
        "comptime function specialization is memoized by function and comptime argument tuple; recursive specializations must bottom out before repeating the same tuple",
    )
}

fn evaluation_limit(span: Span) -> Diagnostic {
    Diagnostic::error("comptime evaluation limit exceeded")
        .with_code(codes::comptime::EVALUATION_LIMIT)
        .with_label(Label::primary(
            span,
            "this comptime expression exceeded the evaluation budget",
        ))
        .with_note("the comptime evaluator uses a fuel budget to keep specialization finite")
}

#[derive(Debug, Clone, Default)]
struct Environment {
    bindings: HashMap<String, ComptimeValue>,
}

impl Environment {
    fn from_bindings(bindings: &HashMap<String, ComptimeValue>) -> Self {
        Self {
            bindings: bindings.clone(),
        }
    }

    fn from_params(params: &[Param], values: Vec<ComptimeValue>) -> Self {
        let bindings = params
            .iter()
            .zip(values)
            .map(|(param, value)| (param.name.clone(), value))
            .collect();
        Self { bindings }
    }

    fn get(&self, name: &str) -> Option<&ComptimeValue> {
        self.bindings.get(name)
    }
}

fn callee_name(expr: &Expr) -> Option<&str> {
    match &ungroup(expr).kind {
        ExprKind::Name(name) => Some(name),
        _ => None,
    }
}

fn ungroup(mut expr: &Expr) -> &Expr {
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    expr
}
