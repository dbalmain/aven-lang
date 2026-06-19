use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{Expr, ExprKind, Literal};

use crate::ty::{Row, RowEntry, RowTail, Type, is_concrete_type};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComptimeValue {
    ReifiedType(Type),
    LabelSet(Vec<String>),
}

impl ComptimeValue {
    pub(crate) fn reify_type_position(self) -> Self {
        match self {
            ComptimeValue::ReifiedType(ty) => ComptimeValue::ReifiedType(ty),
            ComptimeValue::LabelSet(labels) => ComptimeValue::ReifiedType(label_set_type(labels)),
        }
    }

    pub(crate) fn into_reified_type(self) -> Option<Type> {
        match self {
            ComptimeValue::ReifiedType(ty) => Some(ty),
            ComptimeValue::LabelSet(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evaluation {
    Evaluated(ComptimeValue),
    Deferred,
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

    fn diagnostic(diagnostic: Diagnostic) -> Self {
        Self {
            evaluation: Evaluation::Deferred,
            diagnostics: vec![diagnostic],
        }
    }
}

pub(crate) fn keys_of_argument(expr: &Expr) -> Option<&Expr> {
    let expr = ungroup(expr);
    let ExprKind::Call { callee, args } = &expr.kind else {
        return None;
    };

    if !matches!(&ungroup(callee).kind, ExprKind::Name(name) if name == "keysOf") {
        return None;
    }

    let [arg] = args.as_slice() else {
        return None;
    };

    Some(arg)
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

fn reflection_type_mismatch(span: Span) -> Diagnostic {
    Diagnostic::error("reflection function `keysOf` expected a record type")
        .with_code(codes::comptime::REFLECTION_TYPE_MISMATCH)
        .with_label(Label::primary(span, "this type is not a record"))
        .with_note("`keysOf` needs a record type")
}

fn ungroup(mut expr: &Expr) -> &Expr {
    while let ExprKind::Group(inner) = &expr.kind {
        expr = inner;
    }
    expr
}
