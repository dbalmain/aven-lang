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
    /// # What a proof can be spent on
    ///
    /// Evidence is only ever consulted where a demand would otherwise fail, so
    /// it can only help where the demanded type is narrower than what
    /// inference already produced. The types narrower in that way are the
    /// literal types, and `aven_parser::Literal` has exactly three
    /// constructors: `Bool`, `Number`, and `String`. A demand for anything
    /// else --- a record, an array, a named family --- is already answered by
    /// the expression's inferred type or not at all, and a proof would change
    /// nothing.
    ///
    /// So the supported set below is not a convenient starting subset to be
    /// widened later. It is every value a demand can currently spend, and
    /// widening it is work for whichever slice first introduces a demand that
    /// a non-scalar could discharge.
    ///
    /// # Support table
    ///
    /// | Evaluator value | Transported | Why |
    /// |---|---|---|
    /// | `Int`, `Float` | yes | `Literal::Number`, compared by the evaluator's own rendering |
    /// | `Text` | yes | `Literal::String` |
    /// | `Bool` | yes | `Literal::Bool` |
    /// | `Undefined`, `Null` | recorded, never discharges | the two empties have no literal spelling, so a demand falls through to ordinary checking |
    /// | `Array`, `Tuple`, `Set`, `Map`, `Record`, `SlotRecord` | no | no literal type to satisfy; the inferred type already carries the shape |
    /// | `Tag`, results | no | variant membership is decided by the row, not by a value |
    /// | `NamedRecord`, `NamedFamily` | no | a family's identity lives in its descriptor, which the checker cannot read from here |
    /// | `BrandedPrimitive` | no | see below |
    /// | `Closure`, `Stream`, `Native`, method values, `Type` | no | not values a type can name, and each owns an evaluator environment |
    ///
    /// # Why refusal is the safe direction
    ///
    /// No proof means a demand falls back to ordinary type checking, which is
    /// what it did before proofs existed. An unsupported value therefore costs
    /// an error message that could have been avoided, never a wrong answer.
    ///
    /// Refusal also carries a lifetime guarantee. Every transported variant is
    /// `Rc`-free, so a proof cannot hold a scope, a closure, or a captured
    /// environment, and the knowledge maps cannot become a second place the
    /// evaluator's memory survives. The evaluation's own retention is cut when
    /// it ends; see `Teardown` in `aven-eval`.
    ///
    /// # The one real gap
    ///
    /// `BrandedPrimitive` --- a `Money` whose payload is an integer --- is the
    /// case where a proof would genuinely help and cannot be made here.
    /// Certifying one needs the family's own elaboration, including its
    /// rendering, and the evaluator does not expose its descriptor's owner to
    /// the checker. Recording the bare payload instead would let a branded
    /// value satisfy a raw `Int`, which is exactly the confusion the family
    /// exists to prevent.
    ///
    /// Note what this gap is and is not. Branding is keyed on a literal
    /// *written* at the annotated position, not on the value's type being a
    /// singleton: given `Money = Int { ... }`, `price: Money = 99` is accepted
    /// while `price: Money = 40 + 59` is rejected, though both have type `99`.
    /// So `price: Money = comptime(99)` failing is the existing rule applying
    /// evenly rather than evidence being treated worse than an ordinary value.
    /// Widening it is a language decision about where branding applies, and
    /// belongs with whoever owns that rule.
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
    pub(super) fn literal(&self) -> Option<Literal> {
        match &self.0 {
            aven_eval::Value::Bool(value) => Some(Literal::Bool(*value)),
            aven_eval::Value::Int(_) | aven_eval::Value::Float(_) => {
                Some(Literal::Number(aven_eval::display_text(&self.0).ok()?))
            }
            aven_eval::Value::Text(_) => Some(Literal::String(aven_eval::repr_text(&self.0))),
            _ => None,
        }
    }
}

impl<'a> Checker<'a> {
    /// Record evidence for `span` in the current execution context.
    ///
    /// Spans from an imported body are never recorded: they address the
    /// defining source, not this one, and a span is a bare offset pair with no
    /// file identity, so an imported body's offsets can collide with this
    /// file's. The caller already discards that body's inferred types for the
    /// same reason; `foreign_body_depth` is what makes the claim true here.
    pub(super) fn record_known(&mut self, span: Span, known: Known) {
        if span.is_empty() || self.foreign_body_depth > 0 {
            return;
        }
        self.knowledge.insert((span, self.execution_context), known);
    }

    /// Run `body` with evidence recording suppressed, for a function body that
    /// belongs to another source.
    pub(super) fn in_foreign_body<T>(
        &mut self,
        foreign: bool,
        body: impl FnOnce(&mut Self) -> T,
    ) -> T {
        if !foreign {
            return body(self);
        }
        self.foreign_body_depth += 1;
        let result = body(self);
        self.foreign_body_depth -= 1;
        result
    }

    pub(super) fn push_local_value_scope(&mut self) {
        self.local_types.push_values();
    }

    pub(super) fn pop_local_value_scope(&mut self) {
        self.local_types.pop_values();
    }

    pub(super) fn record_local_value(&mut self, name: &str, initializer: &Expr, shadows: bool) {
        self.local_types
            .define_value(name, initializer.clone(), shadows);
    }

    /// Every local value in scope, in written order.
    pub(super) fn local_values_in_scope(&self) -> impl Iterator<Item = (&String, &LocalValue)> {
        self.local_types.values_in_scope()
    }

    /// Evidence for a *module* binding, with the initialization boundary it
    /// became valid at. Module bindings are one flat, mutually recursive
    /// namespace, so they are ordered by boundary rather than by scope; a
    /// local binding's evidence lives in `local_types` instead, where a scope
    /// pop takes it with it.
    pub(super) fn record_module_binding_knowledge(&mut self, name: &str, known: Known) {
        self.known_bindings
            .insert(name.to_owned(), (self.execution_context, known));
    }

    /// Is evidence written in `origin` readable from the current context?
    ///
    /// Initialization boundaries are source offsets, so a larger one runs
    /// later. Evidence from an earlier boundary is available; evidence from a
    /// later one is not, which is what stops `comptime(double(later))` above
    /// `later = 3` from certifying a value the program cannot produce yet.
    /// The artifact and runtime-unknown regimes are not ordered against each
    /// other and must match exactly.
    fn knowledge_is_in_scope(&self, origin: comptime::ExecutionContext) -> bool {
        match (origin, self.execution_context) {
            (
                comptime::ExecutionContext::RuntimeKnown(written),
                comptime::ExecutionContext::RuntimeKnown(reading),
            ) => written <= reading,
            (origin, reading) => origin == reading,
        }
    }

    /// Evidence for an expression, if a comptime demand established any.
    ///
    /// Evidence is never derived here. A value is known because an explicit
    /// demand evaluated it, not because this position went looking — that
    /// distinction is what keeps an ordinary runtime call from certifying a
    /// type its signature does not give, and it is why `g = f(0)` stays
    /// `1 | 1.0` rather than becoming whichever branch happened to run.
    pub(super) fn known_for_expression(&self, expr: &Expr) -> Option<Known> {
        let expr = ungroup_expr(expr);
        if let Some(known) = self.knowledge.get(&(expr.span, self.execution_context)) {
            return Some(known.clone());
        }
        let (ExprKind::Name(name) | ExprKind::ComptimeName(name)) = &expr.kind else {
            return None;
        };
        // The nearest binding of the name decides, and it decides even when it
        // has nothing to say: a mask stops the search rather than letting an
        // outer binding, or a module binding, answer for a name that is no
        // longer theirs.
        if let Some(local) = self.local_types.proof(name) {
            return local
                .filter(|(origin, _)| self.knowledge_is_in_scope(*origin))
                .map(|(_, known)| known.clone());
        }
        self.known_bindings
            .get(name)
            .filter(|(origin, _)| self.knowledge_is_in_scope(*origin))
            .map(|(_, known)| known.clone())
    }

    /// Carry a module binding's evidence to its name, so a later reference can
    /// answer a demand its initializer already proved.
    pub(super) fn propagate_module_binding_knowledge(&mut self, name: &str, value: &Expr) {
        if let Some(known) = self.known_for_expression(value) {
            self.record_module_binding_knowledge(name, known);
        }
    }

    /// The same for a local binding, except that it is *total*: a binding
    /// whose value proves nothing records a mask.
    ///
    /// Totality is the whole point. `x = first(1)` proves `x` is `1`; the
    /// `x := [2][0]` below it proves nothing, and if that silence left the
    /// earlier proof standing then `checked: 1 = x` would be certified by a
    /// binding the program has already replaced.
    pub(super) fn propagate_local_binding_knowledge(&mut self, name: &str, value: &Expr) {
        match self.known_for_expression(value) {
            Some(known) => {
                let origin = self.execution_context;
                self.local_types.define_proof(name, origin, known);
            }
            None => self.local_types.mask_proof(name),
        }
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
            // A known *present* optional discharges a nonoptional demand. The
            // evaluator represents a present optional as its payload, so the
            // evidence here is already the payload and the only question is
            // whether it fits. An absent optional has no literal spelling and
            // was rejected above, so the conversion can never smuggle an empty
            // through. This is what lets a value the program is known to have
            // be used where the program requires one.
            Type::Optional(inner) | Type::Nullable(inner) => {
                self.knowledge_satisfies(known, &inner)
            }
            Type::Named(name) if base == LiteralBase::Number => {
                number_literal_row_fits_named(row, &name)
            }
            Type::Named(name) => base.matches_named(&name),
            // A closed union admits exactly the literals it names. Type
            // refinement could not use one — a singleton is not *narrower* than
            // a union it belongs to — but evidence can, because the question
            // here is membership of a known value rather than a relation
            // between two types.
            Type::Variant(target) => {
                literal_variant_base(&target) == Some(base)
                    && self.literal_is_in_variant(&target, &literal)
            }
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
