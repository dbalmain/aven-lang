use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{Expr, ExprKind, Literal, Param};

use crate::checker::string_literal_label;
use crate::ty::{Row, RowEntry, RowTail, Type, is_concrete_type};

const DEFAULT_EVALUATION_FUEL: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ComptimeValue {
    ReifiedType(Type),
    LabelSet(Vec<String>),
    Literal(Literal),
    Bool(bool),
}

impl ComptimeValue {
    pub(crate) fn reify_type_position(self) -> Self {
        match self {
            ComptimeValue::ReifiedType(ty) => ComptimeValue::ReifiedType(ty),
            ComptimeValue::LabelSet(labels) => ComptimeValue::ReifiedType(label_set_type(labels)),
            ComptimeValue::Literal(literal) => ComptimeValue::ReifiedType(literal_type(literal)),
            ComptimeValue::Bool(value) => {
                ComptimeValue::ReifiedType(literal_type(Literal::Bool(value)))
            }
        }
    }

    pub(crate) fn into_reified_type(self) -> Option<Type> {
        match self {
            ComptimeValue::ReifiedType(ty) => Some(ty),
            ComptimeValue::LabelSet(_) | ComptimeValue::Literal(_) | ComptimeValue::Bool(_) => None,
        }
    }

    pub(crate) fn as_literal(&self) -> Option<&Literal> {
        match self {
            ComptimeValue::Literal(literal) => Some(literal),
            ComptimeValue::ReifiedType(_) | ComptimeValue::LabelSet(_) | ComptimeValue::Bool(_) => {
                None
            }
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

/// A comptime-evaluable function definition that can be stored and carried
/// across module boundaries (owned AST — no borrows into the defining module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComptimeExport {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Expr,
}

impl ComptimeExport {
    pub fn from_lambda(name: impl Into<String>, params: &[Param], body: &Expr) -> Self {
        Self {
            name: name.into(),
            params: params.to_vec(),
            body: body.clone(),
        }
    }
}

pub(crate) type ComptimeFunction = ComptimeExport;

pub(crate) trait EvalContext {
    fn lower_comptime_type(
        &mut self,
        expr: &Expr,
        bindings: &HashMap<String, ComptimeValue>,
    ) -> LoweredType;
    fn lookup_comptime_function(&self, name: &str) -> Option<ComptimeFunction>;
    fn cached_specialization(&self, key: &SpecializationKey) -> Option<EvaluationResult>;
    fn cache_specialization(&mut self, key: SpecializationKey, result: EvaluationResult);
    fn infer_value_type(&mut self, expr: &Expr) -> Type;
    fn type_is_unresolved(&self, ty: &Type) -> bool;
}

pub(crate) fn evaluate_type_position(
    context: &mut impl EvalContext,
    expr: &Expr,
) -> EvaluationResult {
    evaluate_type_position_with_bindings(context, expr, &HashMap::new())
}

pub(crate) fn evaluate_type_position_with_bindings(
    context: &mut impl EvalContext,
    expr: &Expr,
    bindings: &HashMap<String, ComptimeValue>,
) -> EvaluationResult {
    let mut evaluator = Evaluator {
        context,
        visited: HashSet::new(),
        fuel: DEFAULT_EVALUATION_FUEL,
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
        ExprKind::Literal(Literal::Bool(value)) => {
            EvaluationResult::evaluated(ComptimeValue::Bool(*value))
        }
        ExprKind::Literal(literal @ (Literal::Number(_) | Literal::String(_))) => {
            EvaluationResult::evaluated(ComptimeValue::Literal(literal.clone()))
        }
        ExprKind::Call { callee, args } => evaluate_runtime_call(callee, args, bindings),
        ExprKind::Unary {
            operator, value, ..
        } if operator == "!" => match evaluate_runtime_value(value, bindings).evaluation {
            Evaluation::Evaluated(ComptimeValue::Bool(value)) => {
                EvaluationResult::evaluated(ComptimeValue::Bool(!value))
            }
            Evaluation::Deferred => EvaluationResult::deferred(),
            Evaluation::Evaluated(_) | Evaluation::Unsupported => EvaluationResult::unsupported(),
        },
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } if operator == "&&" || operator == "||" => {
            evaluate_runtime_bool_binary(left, operator, right, bindings)
        }
        ExprKind::Group(_) => unreachable!("group expressions are removed before evaluation"),
        ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Interpolation(_)
        | ExprKind::Undefined
        | ExprKind::Null
        | ExprKind::Tag(_)
        | ExprKind::Array(_)
        | ExprKind::Tuple(_)
        | ExprKind::Record(_)
        | ExprKind::Set(_)
        | ExprKind::Index { .. }
        | ExprKind::Optional(_)
        | ExprKind::Nullable(_)
        | ExprKind::NonNull(_)
        | ExprKind::Arrow { .. }
        | ExprKind::FieldAccess { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Propagate { .. }
        | ExprKind::Match { .. }
        | ExprKind::Lambda { .. }
        | ExprKind::Block(_) => EvaluationResult::unsupported(),
    }
}

fn evaluate_runtime_call(
    callee: &Expr,
    args: &[Expr],
    bindings: &HashMap<String, ComptimeValue>,
) -> EvaluationResult {
    let ExprKind::FieldAccess {
        receiver, field, ..
    } = &ungroup(callee).kind
    else {
        return EvaluationResult::unsupported();
    };

    if field != "has" {
        return EvaluationResult::unsupported();
    }

    let [arg] = args else {
        return EvaluationResult::unsupported();
    };

    let receiver = match evaluate_runtime_value(receiver, bindings).evaluation {
        Evaluation::Evaluated(ComptimeValue::LabelSet(labels)) => labels,
        Evaluation::Deferred => return EvaluationResult::deferred(),
        Evaluation::Evaluated(_) | Evaluation::Unsupported => {
            return EvaluationResult::unsupported();
        }
    };

    let label = match evaluate_runtime_value(arg, bindings).evaluation {
        Evaluation::Evaluated(ComptimeValue::Literal(Literal::String(text))) => {
            string_literal_label(&text)
        }
        Evaluation::Deferred => return EvaluationResult::deferred(),
        Evaluation::Evaluated(_) | Evaluation::Unsupported => {
            return EvaluationResult::unsupported();
        }
    };
    let Some(label) = label else {
        return EvaluationResult::unsupported();
    };

    EvaluationResult::evaluated(ComptimeValue::Bool(receiver.contains(&label)))
}

fn evaluate_runtime_bool_binary(
    left: &Expr,
    operator: &str,
    right: &Expr,
    bindings: &HashMap<String, ComptimeValue>,
) -> EvaluationResult {
    let left = match evaluate_runtime_value(left, bindings).evaluation {
        Evaluation::Evaluated(ComptimeValue::Bool(value)) => value,
        Evaluation::Deferred => return EvaluationResult::deferred(),
        Evaluation::Evaluated(_) | Evaluation::Unsupported => {
            return EvaluationResult::unsupported();
        }
    };
    let right = match evaluate_runtime_value(right, bindings).evaluation {
        Evaluation::Evaluated(ComptimeValue::Bool(value)) => value,
        Evaluation::Deferred => return EvaluationResult::deferred(),
        Evaluation::Evaluated(_) | Evaluation::Unsupported => {
            return EvaluationResult::unsupported();
        }
    };

    let value = match operator {
        "&&" => left && right,
        "||" => left || right,
        _ => return EvaluationResult::unsupported(),
    };

    EvaluationResult::evaluated(ComptimeValue::Bool(value))
}

struct Evaluator<'ctx, C>
where
    C: EvalContext + ?Sized,
{
    context: &'ctx mut C,
    visited: HashSet<SpecializationKey>,
    fuel: usize,
}

impl<C> Evaluator<'_, C>
where
    C: EvalContext + ?Sized,
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

                self.evaluate_type_term(expr, env)
            }
            ExprKind::Call { callee, args } => {
                self.evaluate_application(expr.span, callee, args, env)
            }
            ExprKind::Index { callee, args } => self.evaluate_type_application(callee, args, env),
            ExprKind::Optional(_)
            | ExprKind::Nullable(_)
            | ExprKind::NonNull(_)
            | ExprKind::Arrow { .. }
            | ExprKind::Tuple(_)
            | ExprKind::Record(_)
            | ExprKind::Set(_) => self.evaluate_type_term(expr, env),
            ExprKind::Group(_) => unreachable!("group expressions are removed before evaluation"),
            ExprKind::Missing
            | ExprKind::Literal(_)
            | ExprKind::Interpolation(_)
            | ExprKind::Undefined
            | ExprKind::Null
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

    fn evaluate_type_application(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        env: &Environment,
    ) -> EvaluationResult {
        let callee = match self.evaluate_expr(callee, env) {
            EvaluationResult {
                evaluation: Evaluation::Evaluated(value),
                diagnostics,
            } => {
                if let Some(ty) = value.reify_type_position().into_reified_type() {
                    if !diagnostics.is_empty() {
                        return EvaluationResult::deferred_with_diagnostics(diagnostics);
                    }
                    ty
                } else {
                    return EvaluationResult::deferred_with_diagnostics(diagnostics);
                }
            }
            EvaluationResult {
                evaluation: Evaluation::Deferred,
                diagnostics,
            } => return EvaluationResult::deferred_with_diagnostics(diagnostics),
            EvaluationResult {
                evaluation: Evaluation::Unsupported,
                diagnostics,
            } => {
                return EvaluationResult {
                    evaluation: Evaluation::Unsupported,
                    diagnostics,
                };
            }
        };

        let mut arg_types = Vec::new();
        for arg in args {
            match self.evaluate_expr(arg, env) {
                EvaluationResult {
                    evaluation: Evaluation::Evaluated(value),
                    diagnostics,
                } => {
                    if let Some(ty) = value.reify_type_position().into_reified_type() {
                        if !diagnostics.is_empty() {
                            return EvaluationResult::deferred_with_diagnostics(diagnostics);
                        }
                        arg_types.push(ty);
                    } else {
                        return EvaluationResult::deferred_with_diagnostics(diagnostics);
                    }
                }
                EvaluationResult {
                    evaluation: Evaluation::Deferred,
                    diagnostics,
                } => return EvaluationResult::deferred_with_diagnostics(diagnostics),
                EvaluationResult {
                    evaluation: Evaluation::Unsupported,
                    diagnostics,
                } => {
                    return EvaluationResult {
                        evaluation: Evaluation::Unsupported,
                        diagnostics,
                    };
                }
            }
        }

        EvaluationResult::evaluated(ComptimeValue::ReifiedType(Type::Apply {
            callee: Box::new(callee),
            args: arg_types,
        }))
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

        if let Some(kind) = ReflectionKind::from_name(name) {
            return self.evaluate_reflection_application(args, env, kind);
        }

        if name == "typeOf" {
            return self.evaluate_type_of(args);
        }

        if let Some(function) = self.context.lookup_comptime_function(name) {
            return self.evaluate_function_application(function, call_span, args, env);
        }

        if name.chars().next().is_some_and(char::is_uppercase) {
            self.evaluate_type_application(callee, args, env)
        } else {
            EvaluationResult::unsupported()
        }
    }

    fn evaluate_reflection_application(
        &mut self,
        args: &[Expr],
        env: &Environment,
        kind: ReflectionKind,
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

        kind.evaluate(
            &subject,
            arg.span,
            self.context.type_is_unresolved(&subject),
        )
    }

    fn evaluate_type_of(&mut self, args: &[Expr]) -> EvaluationResult {
        let [arg] = args else {
            return EvaluationResult::unsupported();
        };

        let ty = self.context.infer_value_type(arg);
        if self.context.type_is_unresolved(&ty) || !is_concrete_type(&ty) {
            return EvaluationResult::deferred();
        }

        EvaluationResult::evaluated(ComptimeValue::ReifiedType(ty))
    }

    fn evaluate_function_application(
        &mut self,
        function: ComptimeFunction,
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
        let key = SpecializationKey::new(&function.name, &values);

        if let Some(result) = self.context.cached_specialization(&key) {
            return result;
        }

        if !self.visited.insert(key.clone()) {
            return EvaluationResult::diagnostic(evaluation_cycle(call_span, &function.name));
        }

        let body_env = Environment::from_params(&function.params, values);
        let result = self.evaluate_expr(&function.body, &body_env);
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

    fn evaluate_type_term(&mut self, expr: &Expr, env: &Environment) -> EvaluationResult {
        let lowering = self.context.lower_comptime_type(expr, env.bindings());
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
        return EvaluationResult::diagnostic(reflection_type_mismatch(
            arg_span, "keysOf", "record",
        ));
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

pub(crate) fn evaluate_tags_of(
    subject: &Type,
    arg_span: Span,
    subject_is_unresolved: bool,
) -> EvaluationResult {
    if subject_is_unresolved || !is_concrete_type(subject) {
        return EvaluationResult::deferred();
    }

    let Type::Variant(row) = subject else {
        return EvaluationResult::diagnostic(reflection_type_mismatch(
            arg_span, "tagsOf", "variant",
        ));
    };

    if row.tail != RowTail::Closed {
        return EvaluationResult::deferred();
    }

    let mut labels = Vec::new();
    for entry in &row.entries {
        let RowEntry::Tag { name, .. } = entry else {
            return EvaluationResult::deferred();
        };
        labels.push(name.clone());
    }
    labels.sort();

    EvaluationResult::evaluated(ComptimeValue::LabelSet(labels))
}

pub(crate) fn evaluate_record_selection(
    subject: &Type,
    labels: &[String],
    arg_span: Span,
    subject_is_unresolved: bool,
    kind: RecordSelectionKind,
) -> EvaluationResult {
    if subject_is_unresolved || !is_concrete_type(subject) {
        return EvaluationResult::deferred();
    }

    let Type::Record(row) = subject else {
        return EvaluationResult::diagnostic(reflection_type_mismatch(
            arg_span,
            kind.name(),
            "record",
        ));
    };

    if row.tail != RowTail::Closed {
        return EvaluationResult::deferred();
    }

    let labels: HashSet<_> = labels.iter().map(String::as_str).collect();
    let mut entries = Vec::new();
    for entry in &row.entries {
        let RowEntry::Field { name, .. } = entry else {
            return EvaluationResult::deferred();
        };
        if kind.keeps(labels.contains(name.as_str())) {
            entries.push(entry.clone());
        }
    }

    EvaluationResult::evaluated(ComptimeValue::ReifiedType(Type::Record(Row {
        entries,
        tail: RowTail::Closed,
    })))
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

#[derive(Debug, Clone, Copy)]
pub(crate) enum RecordSelectionKind {
    Pick,
    Omit,
}

impl RecordSelectionKind {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "pick" => Some(Self::Pick),
            "omit" => Some(Self::Omit),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Omit => "omit",
        }
    }

    fn keeps(self, label_matches: bool) -> bool {
        match self {
            Self::Pick => label_matches,
            Self::Omit => !label_matches,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ReflectionKind {
    KeysOf,
    TagsOf,
}

impl ReflectionKind {
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
    ) -> EvaluationResult {
        match self {
            Self::KeysOf => evaluate_keys_of(subject, arg_span, subject_is_unresolved),
            Self::TagsOf => evaluate_tags_of(subject, arg_span, subject_is_unresolved),
        }
    }
}

fn reflection_type_mismatch(span: Span, function: &str, expected_kind: &str) -> Diagnostic {
    Diagnostic::error(format!(
        "reflection function `{function}` expected a {expected_kind} type"
    ))
    .with_code(codes::comptime::REFLECTION_TYPE_MISMATCH)
    .with_label(Label::primary(
        span,
        format!("this type is not a {expected_kind}"),
    ))
    .with_note(format!("`{function}` needs a {expected_kind} type"))
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

    fn bindings(&self) -> &HashMap<String, ComptimeValue> {
        &self.bindings
    }
}

fn callee_name(expr: &Expr) -> Option<&str> {
    match &ungroup(expr).kind {
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => Some(name),
        _ => None,
    }
}

fn ungroup(mut expr: &Expr) -> &Expr {
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    expr
}
