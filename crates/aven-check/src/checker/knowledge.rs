//! Compile-time evidence recorded beside an ordinary inferred type.
//!
//! A proof says *what value* an expression has, never *what type* it has. It
//! is consulted only where a program demands knowledge — an annotation, a
//! typed argument — and it can only ever let such a demand succeed, never
//! change an inferred type, a unification, or a rendering. That separation is
//! what lets an upstream value become runtime input without retyping anything
//! downstream: the demand sites fail, and nothing else moves.
//!
//! Evidence is deliberately narrow. Only scalars and the two empties are
//! transportable; every other evaluator value is rejected rather than stored,
//! so no proof can retain a scope, a closure, or a lazily captured
//! environment. See `Known::from_value` for the support table.

use super::*;

/// An owned, allocation-free projection of an evaluator value.
///
/// The inner value is always one of `Int`, `Float`, `Text`, `Bool`,
/// `Undefined`, or `Null` — the `Value` variants that hold no `Rc` and so
/// cannot keep an evaluator environment alive. `from_value` is the only
/// constructor and enforces that.
#[derive(Debug, Clone)]
pub(crate) struct Known(aven_eval::Value);

impl Known {
    /// Project an evaluator value into transportable evidence, or refuse.
    ///
    /// Refusal is the conservative direction: no proof means a demand falls
    /// back to ordinary type checking, which is what it did before proofs
    /// existed. The notable gap is `BrandedPrimitive` — a `Money` whose
    /// payload is an integer. Certifying one needs the family's own
    /// elaboration, including its rendering, and the evaluator does not expose
    /// its descriptor's owner to the checker. Recording the bare payload
    /// instead would let a branded value satisfy a raw `Int`, which is exactly
    /// the confusion the family exists to prevent, so branded values are
    /// unsupported until that plan is available.
    pub(crate) fn from_value(value: &aven_eval::Value) -> Option<Self> {
        match value {
            aven_eval::Value::Int(_)
            | aven_eval::Value::Float(_)
            | aven_eval::Value::Text(_)
            | aven_eval::Value::Bool(_)
            | aven_eval::Value::Undefined
            | aven_eval::Value::Null => Some(Self(value.clone())),
            _ => None,
        }
    }

    /// The literal spelling of this evidence, for comparison against literal
    /// types. The text comes from the evaluator's own rendering, so a proof
    /// says what the program would actually produce. The two empties have no
    /// literal spelling and never satisfy a demand.
    fn literal(&self) -> Option<Literal> {
        match &self.0 {
            aven_eval::Value::Bool(value) => Some(Literal::Bool(*value)),
            aven_eval::Value::Int(_) | aven_eval::Value::Float(_) => Some(Literal::Number(
                aven_eval::display_text(&self.0).ok()?,
            )),
            aven_eval::Value::Text(_) => {
                Some(Literal::String(aven_eval::repr_text(&self.0)))
            }
            _ => None,
        }
    }

    pub(crate) fn is_absent(&self) -> bool {
        matches!(
            self.0,
            aven_eval::Value::Undefined | aven_eval::Value::Null
        )
    }

    /// How this evidence reads in a diagnostic.
    pub(crate) fn render(&self) -> String {
        match self.literal() {
            Some(literal) => render_literal_value(&literal).to_owned(),
            None => self.0.type_name().to_owned(),
        }
    }
}

impl<'a> Checker<'a> {
    /// Record evidence for `span` in the current execution context.
    ///
    /// Spans from an imported body are never recorded: they address the
    /// defining source, not this one, and the caller already discards that
    /// body's inferred types for the same reason.
    pub(super) fn record_known(&mut self, span: Span, known: Known) {
        if span.is_empty() {
            return;
        }
        self.knowledge.insert((span, self.execution_context), known);
    }

    pub(super) fn knowledge_at(&self, span: Span) -> Option<&Known> {
        self.knowledge.get(&(span, self.execution_context))
    }

    /// Does this evidence discharge a demand for `expected`?
    ///
    /// This is the old `literal_type_refines` predicate, unchanged in what it
    /// admits and moved to where it belongs. It used to decide whether to
    /// *rewrite* an expression's type to a singleton; it now decides whether a
    /// known value satisfies a type the expression keeps. The admissions are
    /// the same because the question always was "does this value sit inside
    /// that type", never "what is this expression's type".
    ///
    /// A present optional needs no special case: the evaluator represents one
    /// as its payload, so proving `1` against `Int` is the whole of it. The two
    /// empties have no literal spelling and so cannot satisfy any nonoptional
    /// demand.
    pub(super) fn knowledge_satisfies(&mut self, known: &Known, expected: &Type) -> bool {
        let Some(literal) = known.literal() else {
            return false;
        };
        let evidence = self.open_literal_variant(&literal);
        let Type::Variant(row) = &evidence else {
            return false;
        };
        let Some(base) = literal_variant_base(row) else {
            return false;
        };
        // A number proof also has to agree in *form*. `Number` matches both
        // `Int` and `Float`, so without this a known `1.5` would satisfy `Int`,
        // and so would a result that is not a number lexeme at all — `NaN` and
        // `Infinity` carry neither a point nor an exponent and would otherwise
        // read as integer-shaped.
        if let Some(RowEntry::Literal {
            value: Literal::Number(text),
        }) = row.entries.first()
            && !number_literal_text_is_finite(text)
        {
            return false;
        }
        match self.unifier.resolve(expected) {
            Type::Named(name) if base == LiteralBase::Number => {
                number_literal_row_fits_named(row, &name)
            }
            Type::Named(name) => base.matches_named(&name),
            Type::Variant(target) => match open_literal_variant_base(&target) {
                Some(target_base) => {
                    target_base == base && self.literal_is_in_variant(&target, &literal)
                }
                None => false,
            },
            _ => false,
        }
    }

    /// A closed literal union admits only the members it names. An open one
    /// admits any literal of its base, which is what its row variable means.
    fn literal_is_in_variant(&mut self, row: &Row, literal: &Literal) -> bool {
        if row.tail != RowTail::Closed {
            return true;
        }
        row.entries.iter().any(|entry| match entry {
            RowEntry::Literal { value } => value == literal,
            _ => false,
        })
    }
}
