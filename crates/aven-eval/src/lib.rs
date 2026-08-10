use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fmt,
    rc::Rc,
};

use aven_core::{BuiltinType, Diagnostic, Label, Span, codes};
use aven_parser::{
    Expr, ExprKind, InterpolationSegment, Item, Literal, MatchArm, Module, PropagationMode,
    RecordEntry, decode_string_literal, is_method_operator, is_method_requirement_row,
};

const MAX_MATERIALIZED_ARRAY_BYTES: usize = 256 * 1024 * 1024;

mod display;
mod fingerprint;
pub mod logging;
mod map;
mod runtime_type;
mod set;

pub use aven_core::Int;
pub use display::{display_text, repr_text};
pub use map::MapValue;
pub use runtime_type::{
    RuntimeType, RuntimeTypeBindings, RuntimeTypeDescriptor, RuntimeTypeGraph, RuntimeTypeId,
    RuntimeVariantDescriptor,
};
pub use set::SetValue;

/// The evaluator's control-flow channel. Most failures are ordinary runtime
/// errors ([`Flow::Fail`]); [`Flow::Propagate`] carries an `@Err` value that is
/// early-returning from the enclosing function via `?^`. Both bubble through `?`;
/// `Propagate` is caught only at the closure body and the top-level item loop.
enum Flow {
    /// A real runtime error: one or more diagnostics.
    Fail(Vec<Diagnostic>),
    /// An `@Err` value early-returning from the enclosing function (`?^`).
    Propagate(Box<Value>),
}

/// Internal evaluator result. `Ok` is the produced value; `Err` is a [`Flow`].
type Eval<T = Value> = Result<T, Flow>;

/// Host-provided native function. Prefer [`Value::native`] for
/// context-ignoring hosts; use [`Value::native_at`] when call-site context
/// matters.
pub type NativeFn = Rc<dyn Fn(&[Value], NativeContext) -> Result<Value, String>>;

/// Lexical source identity carried by an evaluator [`Environment`].
///
/// Location-aware natives use this to turn a [`Span`] into a greppable
/// `file:line` prefix. It is absent when evaluation is driven without a file
/// identity or when a native is invoked through an environment-free path.
#[derive(Debug)]
pub struct EvalSource {
    /// Display name: basename for path-backed files, bare specifier for
    /// embedded library modules.
    pub name: String,
    source: String,
    line_index: aven_core::LineIndex,
}

impl EvalSource {
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        let source = source.into();
        let line_index = aven_core::LineIndex::new(&source);
        Self {
            name: name.into(),
            source,
            line_index,
        }
    }

    /// `name:line: ` (1-based line) for the byte offset, suitable as a
    /// `dbg` prefix. Column is omitted: file+line is enough to distinguish
    /// two calls, and keeps the line greppable as `file:line:`.
    pub fn format_location(&self, span: Span) -> String {
        let position = self.line_index.offset_to_position(&self.source, span.start);
        format!("{}:{}: ", self.name, position.line + 1)
    }
}

/// Context supplied to a host native at an application site.
#[derive(Debug, Clone)]
pub struct NativeContext {
    pub span: Span,
    pub source: Option<Rc<EvalSource>>,
}

impl NativeContext {
    /// Context for application paths that have no lexical evaluator
    /// environment. Location-aware natives must not infer a source here.
    pub fn without_source(span: Span) -> Self {
        Self { span, source: None }
    }
}

#[derive(Clone)]
pub struct Closure {
    params: Vec<ClosureParam>,
    body: Rc<Expr>,
    env: Environment,
}

#[derive(Clone, Default)]
pub struct BuiltinMethodEnvironment {
    methods: Rc<RefCell<Vec<BuiltinMethodImplementation>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotReification {
    pub fields: Vec<String>,
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SlotReificationPlan {
    targets: HashMap<Span, SlotReification>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimitiveFamilyRuntime {
    pub owner: String,
    pub base: String,
    pub inherited_methods: Vec<InheritedPrimitiveMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedPrimitiveMethod {
    pub member: String,
    pub lifted_params: Vec<bool>,
    pub lifted_result: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimitiveFamilyCoercion {
    Brand { owner: String },
    Widen,
}

#[derive(Debug, Clone, Default)]
pub struct PrimitiveFamilyPlan {
    families: HashMap<String, PrimitiveFamilyRuntime>,
    coercions: HashMap<Span, PrimitiveFamilyCoercion>,
}

impl PrimitiveFamilyPlan {
    pub fn new(
        families: impl IntoIterator<Item = (String, PrimitiveFamilyRuntime)>,
        coercions: impl IntoIterator<Item = (Span, PrimitiveFamilyCoercion)>,
    ) -> Self {
        Self {
            families: families.into_iter().collect(),
            coercions: coercions.into_iter().collect(),
        }
    }

    fn family(&self, name: &str) -> Option<&PrimitiveFamilyRuntime> {
        self.families.get(name)
    }

    fn coercion(&self, span: Span) -> Option<&PrimitiveFamilyCoercion> {
        self.coercions.get(&span)
    }
}

/// Record-literal spans that directly initialize a slot-record target. The
/// evaluator materializes a `SlotRecord` from the literal's own entries at
/// these spans instead of reifying an evaluated source value.
#[derive(Debug, Clone, Default)]
pub struct DirectSlotInitPlan {
    targets: HashSet<Span>,
}

impl DirectSlotInitPlan {
    pub fn new(targets: impl IntoIterator<Item = Span>) -> Self {
        Self {
            targets: targets.into_iter().collect(),
        }
    }

    fn contains(&self, span: Span) -> bool {
        self.targets.contains(&span)
    }
}

#[derive(Debug, Clone, Default)]
pub struct EvalElaborationPlan {
    slot_reifications: SlotReificationPlan,
    direct_slot_inits: DirectSlotInitPlan,
    primitive_families: PrimitiveFamilyPlan,
}

impl EvalElaborationPlan {
    pub fn new(
        slot_reifications: SlotReificationPlan,
        direct_slot_inits: DirectSlotInitPlan,
        primitive_families: PrimitiveFamilyPlan,
    ) -> Self {
        Self {
            slot_reifications,
            direct_slot_inits,
            primitive_families,
        }
    }
}

impl SlotReificationPlan {
    pub fn new(targets: impl IntoIterator<Item = (Span, SlotReification)>) -> Self {
        Self {
            targets: targets.into_iter().collect(),
        }
    }

    fn get(&self, span: Span) -> Option<&SlotReification> {
        self.targets.get(&span)
    }
}

#[derive(Clone)]
struct BuiltinMethodImplementation {
    owner: String,
    member: String,
    implementation: Closure,
}

impl BuiltinMethodEnvironment {
    fn insert(&self, method: BuiltinMethodImplementation) {
        self.methods.borrow_mut().push(method);
    }

    fn lookup(&self, receiver: &Value, member: &str) -> Option<Closure> {
        let owner = runtime_builtin_owner(receiver)?;
        self.methods
            .borrow()
            .iter()
            .find(|method| method.owner == owner && method.member == member)
            .map(|method| method.implementation.clone())
    }
}

#[derive(Clone)]
pub struct NamedFamilyDescriptor {
    owner: String,
    primitive_base: Option<String>,
    fields: Vec<NamedFamilyField>,
    methods: HashMap<String, NamedMethodImplementation>,
}

#[derive(Clone)]
pub enum NamedMethodImplementation {
    Declared(Closure),
    Inherited(Rc<InheritedMethodImplementation>),
}

#[derive(Clone)]
pub struct InheritedMethodImplementation {
    member: String,
    lifted_params: Vec<bool>,
    lifted_result: bool,
    env: Environment,
}

#[derive(Clone)]
struct NamedFamilyField {
    name: String,
    optional: bool,
    default: Option<Rc<Expr>>,
}

/// A closure parameter: its binding name plus an optional default expression
/// (trailing-only, enforced by the parser/checker). The default is evaluated in
/// the call environment, in parameter order, only when the argument is omitted.
#[derive(Clone, Debug)]
struct ClosureParam {
    name: String,
    default: Option<Rc<Expr>>,
}

/// A lazy stream backed by an integer range and an ordered adapter chain.
///
/// The source cursor is a fixed-size handful of arbitrary-precision integers.
/// `map` and `filter` append callbacks without consuming the source; forcing
/// walks the source and stages iteratively, so stream length does not affect
/// evaluator stack usage.
#[derive(Clone)]
pub struct Stream {
    start: Int,
    end: Int,
    step: Int,
    next: Int,
    inclusive: bool,
    stages: Vec<StreamStage>,
}

#[derive(Clone)]
enum StreamStage {
    Map { callback: Value, identity: Rc<()> },
    Filter { callback: Value, identity: Rc<()> },
}

impl PartialEq for StreamStage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Map { identity: left, .. },
                Self::Map {
                    identity: right, ..
                },
            )
            | (
                Self::Filter { identity: left, .. },
                Self::Filter {
                    identity: right, ..
                },
            ) => Rc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl Eq for StreamStage {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    ZeroStep,
}

impl Stream {
    pub fn range(start: Int, end: Int, step: Int, inclusive: bool) -> Result<Self, StreamError> {
        if step.is_zero() {
            return Err(StreamError::ZeroStep);
        }
        Ok(Self {
            next: start.clone(),
            start,
            end,
            step,
            inclusive,
            stages: Vec::new(),
        })
    }

    pub fn start(&self) -> &Int {
        &self.start
    }

    pub fn end(&self) -> &Int {
        &self.end
    }

    pub fn increment(&self) -> &Int {
        &self.step
    }

    pub fn is_inclusive(&self) -> bool {
        self.inclusive
    }

    fn map(mut self, callback: Value) -> Self {
        self.stages.push(StreamStage::Map {
            callback,
            identity: Rc::new(()),
        });
        self
    }

    fn filter(mut self, callback: Value) -> Self {
        self.stages.push(StreamStage::Filter {
            callback,
            identity: Rc::new(()),
        });
        self
    }

    fn is_range(&self) -> bool {
        self.stages.is_empty()
    }

    fn next_source(&mut self) -> Option<Value> {
        let in_bounds = if self.step.is_negative() {
            if self.inclusive {
                self.next >= self.end
            } else {
                self.next > self.end
            }
        } else if self.inclusive {
            self.next <= self.end
        } else {
            self.next < self.end
        };
        if !in_bounds {
            return None;
        }

        let value = self.next.clone();
        self.next = &self.next + &self.step;
        Some(Value::Int(value))
    }

    fn next_value(&mut self, span: Span) -> Eval<Option<Value>> {
        'source: while let Some(mut value) = self.next_source() {
            for stage in &self.stages {
                match stage {
                    StreamStage::Map { callback, .. } => {
                        value = apply_callee_values(
                            callback.clone(),
                            span,
                            vec![value],
                            NativeContext::without_source(span),
                        )?;
                    }
                    StreamStage::Filter { callback, .. } => {
                        let keep = apply_callee_values(
                            callback.clone(),
                            span,
                            vec![value.clone()],
                            NativeContext::without_source(span),
                        )?;
                        match keep {
                            Value::Bool(true) => {}
                            Value::Bool(false) => continue 'source,
                            other => {
                                return Err(one_diagnostic(record_type_error(
                                    span,
                                    "Stream.filter callback",
                                    other.type_name(),
                                    "Bool",
                                )));
                            }
                        }
                    }
                }
            }
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn exact_remaining_len(&self) -> Option<usize> {
        if self
            .stages
            .iter()
            .any(|stage| matches!(stage, StreamStage::Filter { .. }))
        {
            return None;
        }
        let in_bounds = if self.step.is_negative() {
            if self.inclusive {
                self.next >= self.end
            } else {
                self.next > self.end
            }
        } else if self.inclusive {
            self.next <= self.end
        } else {
            self.next < self.end
        };
        if !in_bounds {
            return Some(0);
        }

        let distance = if self.step.is_negative() {
            &self.next - &self.end
        } else {
            &self.end - &self.next
        };
        let stride = self.step.abs();
        let one = Int::from(1);
        let count = if self.inclusive {
            &(&distance / &stride) + &one
        } else {
            let stride_minus_one = &stride - &one;
            &(&distance + &stride_minus_one) / &stride
        };
        count.to_usize()
    }
}

impl Iterator for Stream {
    type Item = Result<Value, Vec<Diagnostic>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_value(Span::new(0, 0)) {
            Ok(value) => value.map(Ok),
            Err(Flow::Fail(diagnostics)) => Some(Err(diagnostics)),
            Err(Flow::Propagate(value)) => Some(Ok(*value)),
        }
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("step", &self.step)
            .field("next", &self.next)
            .field("inclusive", &self.inclusive)
            .field("stages", &self.stages.len())
            .finish()
    }
}

impl PartialEq for Stream {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start
            && self.end == other.end
            && self.step == other.step
            && self.next == other.next
            && self.inclusive == other.inclusive
            && self.stages == other.stages
    }
}

impl Eq for Stream {}

impl fmt::Display for Stream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_stream(self, formatter)
    }
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
    Int(Int),
    Float(f64),
    Text(String),
    Bool(bool),
    Array(Rc<Vec<Value>>),
    Tuple(Rc<Vec<Value>>),
    Set(Rc<SetValue>),
    Stream(Stream),
    Map(Rc<MapValue>),
    Record(Rc<Vec<(String, Value)>>),
    SlotRecord {
        fields: Rc<Vec<(String, Value)>>,
        slots: Rc<Vec<(String, Value)>>,
    },
    NamedFamily(Rc<NamedFamilyDescriptor>),
    NamedRecord {
        descriptor: Rc<NamedFamilyDescriptor>,
        fields: Rc<Vec<(String, Value)>>,
    },
    BrandedPrimitive {
        descriptor: Rc<NamedFamilyDescriptor>,
        payload: PrimitivePayload,
    },
    NamedMethod {
        receiver: Box<Value>,
        member: String,
        implementation: NamedMethodImplementation,
    },
    UnboundNamedMethod {
        descriptor: Rc<NamedFamilyDescriptor>,
        member: String,
        implementation: NamedMethodImplementation,
    },
    Tag {
        name: String,
        payload: Vec<Value>,
    },
    ResultMethod {
        receiver: Box<Value>,
        kind: ResultMethod,
    },
    StreamMethod {
        receiver: Box<Stream>,
        kind: StreamMethod,
    },
    ArrayFlatMapMethod(Rc<Vec<Value>>),
    ArrayFoldMethod(Rc<Vec<Value>>),
    SetMethod {
        receiver: Rc<SetValue>,
        kind: SetMethod,
    },
    Closure(Closure),
    Native(NativeFn),
    /// Compiler-owned range construction, kept distinct from host natives so
    /// language diagnostics remain structured.
    RangeConstructor {
        inclusive: bool,
        materialize: bool,
    },
    /// `Array.collect` / `Set.collect`, the target-owned collection statics.
    /// Compiler-owned like `RangeConstructor` so the call keeps its span and
    /// the structured materialization diagnostics rather than degrading to a
    /// host platform error.
    CollectConstructor(CollectTarget),
    /// A runtime type descriptor. The evaluator keeps this intentionally small:
    /// named types plus the composite shapes format decode needs. Record types
    /// remain ordinary `Value::Record` values whose fields are type values.
    Type(RuntimeType),
    Undefined,
    Null,
}

#[derive(Debug, Clone)]
pub enum PrimitivePayload {
    Int(Int),
    Float(f64),
    Text(String),
    Bool(bool),
    Array(Rc<Vec<Value>>),
    Set(Rc<SetValue>),
    Map(Rc<MapValue>),
}

impl PartialEq for PrimitivePayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => float_eq(*left, *right),
            (Self::Int(left), Self::Float(right)) => int_float_eq(left, *right),
            (Self::Float(left), Self::Int(right)) => int_float_eq(right, *left),
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Set(left), Self::Set(right)) => sets_equal(left, right),
            (Self::Map(left), Self::Map(right)) => maps_equal(left, right),
            _ => false,
        }
    }
}

impl PrimitivePayload {
    fn into_value(self) -> Value {
        match self {
            Self::Int(value) => Value::Int(value),
            Self::Float(value) => Value::Float(value),
            Self::Text(value) => Value::Text(value),
            Self::Bool(value) => Value::Bool(value),
            Self::Array(value) => Value::Array(value),
            Self::Set(value) => Value::Set(value),
            Self::Map(value) => Value::Map(value),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Int(value) => Value::Int(value.clone()),
            Self::Float(value) => Value::Float(*value),
            Self::Text(value) => Value::Text(value.clone()),
            Self::Bool(value) => Value::Bool(*value),
            Self::Array(value) => Value::Array(Rc::clone(value)),
            Self::Set(value) => Value::Set(Rc::clone(value)),
            Self::Map(value) => Value::Map(Rc::clone(value)),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Text(_) => "Text",
            Self::Bool(_) => "Bool",
            Self::Array(_) => "Array",
            Self::Set(_) => "Set",
            Self::Map(_) => "Map",
        }
    }
}

impl fmt::Display for PrimitivePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write_float(f, *value),
            Self::Text(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Array(values) => fmt_array(values, f),
            Self::Set(values) => fmt_set(values, f),
            Self::Map(entries) => fmt_map(entries, f),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ResultMethod {
    MapErr,
    OrElse,
    Map,
    AndThen,
    UnwrapOr,
    IsOk,
    IsErr,
}

#[derive(Debug, Clone, Copy)]
pub enum StreamMethod {
    Map,
    Filter,
    Fold,
    Each,
    ToArray,
}

/// The `Set` methods that need more than a value-to-value native: `fold` calls
/// back into the evaluator, and `toArray` reports the shared materialization
/// limit as a diagnostic rather than a message.
#[derive(Debug, Clone, Copy)]
pub enum SetMethod {
    Fold,
    ToArray,
}

/// A type that carries a `collect` static. Adding one is adding a variant here
/// and a static in the checker's table; no collection source needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectTarget {
    Array,
    Set,
}

impl CollectTarget {
    fn type_name(self) -> &'static str {
        match self {
            Self::Array => "Array",
            Self::Set => "Set",
        }
    }
}

/// A value `collect` can draw elements from. Keeping the set of sources in one
/// place is what lets `[..source]`, `stream.toArray()`, and `collect` agree:
/// each decides which sources it accepts, but all of them append through
/// [`append_collection`].
enum CollectSource {
    Array(Rc<Vec<Value>>),
    Set(Rc<SetValue>),
    Stream(Stream),
}

impl CollectSource {
    fn of(value: Value) -> Option<Self> {
        match value {
            Value::Array(values) => Some(Self::Array(values)),
            Value::Set(members) => Some(Self::Set(members)),
            Value::Stream(stream) => Some(Self::Stream(stream)),
            _ => None,
        }
    }
}

pub const MAP_METHOD_NAMES: &[&str] = &[
    "get", "set", "delete", "has", "keys", "values", "entries", "size", "merge",
];

/// Roc-aligned Text helpers. Keep in lockstep with `aven_check::ty::TEXT_METHOD_NAMES`.
///
/// `length`/`chars` count and iterate Unicode scalar values (not graphemes);
/// see the checker's `TEXT_METHOD_NAMES` doc for the provisional decision.
pub const TEXT_METHOD_NAMES: &[&str] = &[
    "isEmpty",
    "length",
    "chars",
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
    "padLeft",
    "padRight",
    "toInt",
    "toFloat",
    "reverse",
    "indexOf",
    "slice",
    "capitalize",
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
            Self::Stream(stream) => f.debug_tuple("Stream").field(stream).finish(),
            Self::Map(entries) => f.debug_tuple("Map").field(entries).finish(),
            Self::Record(fields) => f.debug_tuple("Record").field(fields).finish(),
            Self::SlotRecord { fields, slots } => f
                .debug_struct("SlotRecord")
                .field("fields", fields)
                .field("slots", slots)
                .finish(),
            Self::NamedFamily(descriptor) => f
                .debug_tuple("NamedFamily")
                .field(&descriptor.owner)
                .finish(),
            Self::NamedRecord { descriptor, fields } => f
                .debug_struct("NamedRecord")
                .field("owner", &descriptor.owner)
                .field("fields", fields)
                .finish(),
            Self::BrandedPrimitive {
                descriptor,
                payload,
            } => f
                .debug_struct("BrandedPrimitive")
                .field("owner", &descriptor.owner)
                .field("payload", payload)
                .finish(),
            Self::NamedMethod { .. } => f.write_str("NamedMethod(<method>)"),
            Self::UnboundNamedMethod { .. } => f.write_str("UnboundNamedMethod(<method>)"),
            Self::Tag { name, payload } => f
                .debug_struct("Tag")
                .field("name", name)
                .field("payload", payload)
                .finish(),
            Self::ResultMethod { .. } => f.write_str("ResultMethod(<method>)"),
            Self::StreamMethod { kind, .. } => f.debug_tuple("StreamMethod").field(kind).finish(),
            Self::ArrayFlatMapMethod(_) => f.write_str("ArrayFlatMapMethod(<method>)"),
            Self::ArrayFoldMethod(_) => f.write_str("ArrayFoldMethod(<method>)"),
            Self::SetMethod { kind, .. } => f.debug_tuple("SetMethod").field(kind).finish(),
            Self::Closure(closure) => f.debug_tuple("Closure").field(closure).finish(),
            Self::Native(_) => f.write_str("Native(<native>)"),
            Self::RangeConstructor { .. } => f.write_str("RangeConstructor(<intrinsic>)"),
            Self::CollectConstructor(target) => {
                f.debug_tuple("CollectConstructor").field(target).finish()
            }
            Self::Type(ty) => f.debug_tuple("Type").field(ty).finish(),
            Self::Undefined => f.write_str("Undefined"),
            Self::Null => f.write_str("Null"),
        }
    }
}

/// Structural equality for values, including Int/Float numeric coercion.
///
/// Language `==` and collection identity (set members, map keys, `.has`) all
/// use this so a single rule covers scalars and every structural container.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => float_eq(*left, *right),
            (Self::Int(left), Self::Float(right)) => int_float_eq(left, *right),
            (Self::Float(left), Self::Int(right)) => int_float_eq(right, *left),
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Tuple(left), Self::Tuple(right)) => left == right,
            (Self::Set(left), Self::Set(right)) => sets_equal(left, right),
            (Self::Stream(left), Self::Stream(right)) => left == right,
            (Self::Map(left), Self::Map(right)) => maps_equal(left, right),
            (Self::Record(left), Self::Record(right)) => records_equal(left, right),
            (
                Self::SlotRecord {
                    fields: left_fields,
                    slots: left_slots,
                },
                Self::SlotRecord {
                    fields: right_fields,
                    slots: right_slots,
                },
            ) => records_equal(left_fields, right_fields) && records_equal(left_slots, right_slots),
            (
                Self::NamedRecord {
                    descriptor: left_owner,
                    fields: left,
                },
                Self::NamedRecord {
                    descriptor: right_owner,
                    fields: right,
                },
            ) => Rc::ptr_eq(left_owner, right_owner) && records_equal(left, right),
            (
                Self::BrandedPrimitive { payload: left, .. },
                Self::BrandedPrimitive { payload: right, .. },
            ) => left == right,
            (Self::BrandedPrimitive { payload, .. }, other)
            | (other, Self::BrandedPrimitive { payload, .. }) => {
                primitive_payload_matches_value(payload, other)
            }
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
            (Self::ResultMethod { .. }, _) | (_, Self::ResultMethod { .. }) => false,
            (Self::StreamMethod { .. }, _) | (_, Self::StreamMethod { .. }) => false,
            (Self::ArrayFlatMapMethod(_), _) | (_, Self::ArrayFlatMapMethod(_)) => false,
            (Self::ArrayFoldMethod(_), _) | (_, Self::ArrayFoldMethod(_)) => false,
            (Self::SetMethod { .. }, _) | (_, Self::SetMethod { .. }) => false,
            (Self::NamedFamily(_), _) | (_, Self::NamedFamily(_)) => false,
            (Self::NamedMethod { .. }, _) | (_, Self::NamedMethod { .. }) => false,
            (Self::UnboundNamedMethod { .. }, _) | (_, Self::UnboundNamedMethod { .. }) => false,
            (Self::Undefined, Self::Undefined) | (Self::Null, Self::Null) => true,
            (Self::Closure(_), _) | (_, Self::Closure(_)) => false,
            (Self::Native(_), _) | (_, Self::Native(_)) => false,
            (Self::RangeConstructor { .. }, _) | (_, Self::RangeConstructor { .. }) => false,
            (Self::CollectConstructor(_), _) | (_, Self::CollectConstructor(_)) => false,
            _ => false,
        }
    }
}

fn primitive_payload_matches_value(payload: &PrimitivePayload, value: &Value) -> bool {
    match (payload, value) {
        (PrimitivePayload::Int(left), Value::Int(right)) => left == right,
        (PrimitivePayload::Float(left), Value::Float(right)) => float_eq(*left, *right),
        (PrimitivePayload::Int(left), Value::Float(right)) => int_float_eq(left, *right),
        (PrimitivePayload::Float(left), Value::Int(right)) => int_float_eq(right, *left),
        (PrimitivePayload::Text(left), Value::Text(right)) => left == right,
        (PrimitivePayload::Bool(left), Value::Bool(right)) => left == right,
        (PrimitivePayload::Array(left), Value::Array(right)) => left == right,
        (PrimitivePayload::Set(left), Value::Set(right)) => sets_equal(left, right),
        (PrimitivePayload::Map(left), Value::Map(right)) => maps_equal(left, right),
        _ => false,
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write_float(f, *value),
            Self::Text(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Array(values) => fmt_array(values, f),
            Self::Tuple(values) => fmt_tuple(values, f),
            Self::Set(values) => fmt_set(values, f),
            Self::Stream(stream) => fmt_stream(stream, f),
            Self::Map(entries) => fmt_map(entries, f),
            Self::Record(fields) => fmt_record(fields, f),
            Self::SlotRecord { fields, slots } => {
                let mut members = fields.as_ref().clone();
                members.extend(slots.iter().cloned());
                fmt_record(&members, f)
            }
            Self::NamedRecord { fields, .. } => fmt_record(fields, f),
            Self::BrandedPrimitive { payload, .. } => write!(f, "{payload}"),
            Self::NamedFamily(descriptor) => write!(f, "{}", descriptor.owner),
            Self::NamedMethod { .. } => write!(f, "<method>"),
            Self::UnboundNamedMethod { .. } => write!(f, "<method>"),
            Self::Tag { name, payload } => fmt_tag(name, payload, f),
            Self::ResultMethod { .. } => write!(f, "<method>"),
            Self::StreamMethod { .. }
            | Self::ArrayFlatMapMethod(_)
            | Self::ArrayFoldMethod(_)
            | Self::SetMethod { .. } => write!(f, "<method>"),
            Self::Closure(_) => write!(f, "<function>"),
            Self::Native(_) => write!(f, "<native>"),
            Self::RangeConstructor { .. } | Self::CollectConstructor(_) => write!(f, "<native>"),
            Self::Type(ty) => write!(f, "{ty}"),
            Self::Undefined => write!(f, "undefined"),
            Self::Null => write!(f, "null"),
        }
    }
}

impl Value {
    pub fn int(value: impl Into<Int>) -> Self {
        Self::Int(value.into())
    }

    /// Wrap a context-ignoring native. Existing host sites keep this shape;
    /// call-site context is available only via [`Self::native_at`].
    pub fn native(function: impl Fn(&[Value]) -> Result<Value, String> + 'static) -> Self {
        Self::Native(Rc::new(move |args, _context| function(args)))
    }

    /// Wrap a native that receives call-site context.
    pub fn native_at(
        function: impl Fn(&[Value], NativeContext) -> Result<Value, String> + 'static,
    ) -> Self {
        Self::Native(Rc::new(function))
    }

    pub fn record(fields: Vec<(String, Value)>) -> Self {
        Self::Record(Rc::new(fields))
    }

    pub fn named_type(name: impl Into<String>) -> Self {
        Self::Type(RuntimeType::named(name))
    }

    pub fn recursive_type(
        id: RuntimeTypeId,
        name: impl Into<String>,
        graph: Rc<RuntimeTypeGraph>,
    ) -> Self {
        Self::Type(RuntimeType::recursive(id, name, graph))
    }

    pub fn unit() -> Self {
        Self::Tuple(Rc::new(Vec::new()))
    }

    pub fn is_unit(&self) -> bool {
        matches!(self, Self::Tuple(values) if values.is_empty())
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Text(_) => "Text",
            Self::Bool(_) => "Bool",
            Self::Array(_) => "Array",
            Self::Tuple(_) => "Tuple",
            Self::Set(_) => "Set",
            Self::Stream(_) => "Stream",
            Self::Map(_) => "Map",
            Self::Record(_) => "Record",
            Self::SlotRecord { .. } => "Record",
            Self::NamedFamily(_) => "Type",
            Self::NamedRecord { .. } => "Record",
            Self::BrandedPrimitive { payload, .. } => payload.type_name(),
            Self::NamedMethod { .. } => "Function",
            Self::UnboundNamedMethod { .. } => "Function",
            Self::Tag { .. } => "Tag",
            Self::ResultMethod { .. } => "Function",
            Self::StreamMethod { .. }
            | Self::ArrayFlatMapMethod(_)
            | Self::ArrayFoldMethod(_)
            | Self::SetMethod { .. } => "Function",
            Self::Closure(_) => "Function",
            Self::Native(_) => "Native",
            Self::RangeConstructor { .. } | Self::CollectConstructor(_) => "Native",
            Self::Type(_) => "Type",
            Self::Undefined => "Undefined",
            Self::Null => "Null",
        }
    }

    fn as_type_name(&self) -> Option<&str> {
        match self {
            Self::Type(ty) => ty.named_name(),
            _ => None,
        }
    }
}

fn sets_equal(left: &SetValue, right: &SetValue) -> bool {
    left == right
}

fn contains_value(values: &[Value], needle: &Value) -> bool {
    values.iter().any(|value| value == needle)
}

fn maps_equal(left: &MapValue, right: &MapValue) -> bool {
    left == right
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

fn fmt_set(members: &SetValue, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "@{{")?;
    for (index, value) in members.iter().enumerate() {
        if index == 0 {
            write!(f, " ")?;
        } else {
            write!(f, ", ")?;
        }
        fmt_nested_value(value, f)?;
    }
    if !members.is_empty() {
        write!(f, " ")?;
    }
    write!(f, "}}")
}

fn fmt_stream(stream: &Stream, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if !stream.is_range() {
        return write!(f, "<stream>");
    }
    let name = if stream.inclusive {
        "Stream.rangeInclusive"
    } else {
        "Stream.range"
    };
    let default_step = default_range_step(&stream.start, &stream.end);
    if stream.step == default_step {
        write!(f, "{name}({}, {})", stream.start, stream.end)
    } else {
        write!(
            f,
            "{name}({}, {}, {{ step: {} }})",
            stream.start, stream.end, stream.step
        )
    }
}

fn fmt_map(entries: &MapValue, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
        Value::Stream(stream) => fmt_stream(stream, f),
        Value::Map(entries) => fmt_map(entries, f),
        Value::Record(fields) => fmt_record(fields, f),
        Value::SlotRecord { fields, slots } => {
            let mut members = fields.as_ref().clone();
            members.extend(slots.iter().cloned());
            fmt_record(&members, f)
        }
        Value::NamedRecord { fields, .. } => fmt_record(fields, f),
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
    source: Option<Rc<EvalSource>>,
    imports: Rc<ModuleImports>,
    builtin_methods: BuiltinMethodEnvironment,
    slot_reifications: Rc<SlotReificationPlan>,
    direct_slot_inits: Rc<DirectSlotInitPlan>,
    primitive_families: Rc<PrimitiveFamilyPlan>,
    family_descriptors: Rc<RefCell<HashMap<String, Rc<NamedFamilyDescriptor>>>>,
    allow_builtin_method_attachments: bool,
    stack_segment_limit: usize,
    stack_growth: StackGrowth,
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
        Self::with_imports(ModuleImports::default())
    }

    pub fn with_imports(imports: ModuleImports) -> Self {
        Self::with_imports_builtin_methods_and_reifications(
            imports,
            BuiltinMethodEnvironment::default(),
            false,
            SlotReificationPlan::default(),
            DirectSlotInitPlan::default(),
            PrimitiveFamilyPlan::default(),
            None,
        )
    }

    fn with_imports_builtin_methods_and_reifications(
        imports: ModuleImports,
        builtin_methods: BuiltinMethodEnvironment,
        allow_builtin_method_attachments: bool,
        slot_reifications: SlotReificationPlan,
        direct_slot_inits: DirectSlotInitPlan,
        primitive_families: PrimitiveFamilyPlan,
        source: Option<Rc<EvalSource>>,
    ) -> Self {
        Self {
            scope: Rc::new(Scope::new(None)),
            source,
            imports: Rc::new(imports),
            builtin_methods,
            slot_reifications: Rc::new(slot_reifications),
            direct_slot_inits: Rc::new(direct_slot_inits),
            primitive_families: Rc::new(primitive_families),
            family_descriptors: Rc::new(RefCell::new(HashMap::new())),
            allow_builtin_method_attachments,
            stack_segment_limit: DEFAULT_STACK_SEGMENT_LIMIT,
            stack_growth: StackGrowth::System,
        }
    }

    fn child(&self) -> Self {
        Self {
            scope: Rc::new(Scope::new(Some(Rc::clone(&self.scope)))),
            source: self.source.as_ref().map(Rc::clone),
            imports: Rc::clone(&self.imports),
            builtin_methods: self.builtin_methods.clone(),
            slot_reifications: Rc::clone(&self.slot_reifications),
            direct_slot_inits: Rc::clone(&self.direct_slot_inits),
            primitive_families: Rc::clone(&self.primitive_families),
            family_descriptors: Rc::clone(&self.family_descriptors),
            allow_builtin_method_attachments: self.allow_builtin_method_attachments,
            stack_segment_limit: self.stack_segment_limit,
            stack_growth: self.stack_growth,
        }
    }

    fn native_context(&self, span: Span) -> NativeContext {
        NativeContext {
            span,
            source: self.source.as_ref().map(Rc::clone),
        }
    }

    pub fn bind(&self, name: impl Into<String>, value: Value) {
        if let Value::NamedFamily(descriptor) = &value {
            self.family_descriptors
                .borrow_mut()
                .insert(descriptor.owner.clone(), Rc::clone(descriptor));
        }
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModuleImports {
    values: HashMap<String, Option<Value>>,
}

impl ModuleImports {
    pub fn new(values: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(specifier, value)| (specifier, Some(value)))
                .collect(),
        }
    }

    pub fn with_failed(specifiers: impl IntoIterator<Item = String>) -> Self {
        Self {
            values: specifiers
                .into_iter()
                .map(|specifier| (specifier, None))
                .collect(),
        }
    }

    pub fn insert(&mut self, specifier: impl Into<String>, value: Value) {
        self.values.insert(specifier.into(), Some(value));
    }

    pub fn insert_failed(&mut self, specifier: impl Into<String>) {
        self.values.insert(specifier.into(), None);
    }

    pub fn has_failed(&self) -> bool {
        self.values.values().any(Option::is_none)
    }

    fn get(&self, specifier: &str) -> Option<Option<Value>> {
        self.values.get(specifier).cloned()
    }
}

/// Evaluate module items sequentially. Bindings update the environment for
/// later items, and the outcome value is produced only by a trailing expression.
pub fn eval_module(module: &Module) -> EvalOutcome {
    eval_module_with_options(module, EvalModuleOptions::default())
}

/// Optional inputs for [`eval_module_with_options`].
pub struct EvalModuleOptions<'a> {
    globals: Vec<(String, Value)>,
    imports: Option<&'a ModuleImports>,
    runtime_types: Option<&'a RuntimeTypeBindings>,
    builtin_methods: Option<&'a BuiltinMethodEnvironment>,
    allow_builtin_method_attachments: bool,
    elaborations: Option<&'a EvalElaborationPlan>,
    source: Option<Rc<EvalSource>>,
    stack_segment_limit: usize,
    stack_growth: StackGrowth,
}

impl Default for EvalModuleOptions<'_> {
    fn default() -> Self {
        Self {
            globals: Vec::new(),
            imports: None,
            runtime_types: None,
            builtin_methods: None,
            allow_builtin_method_attachments: false,
            elaborations: None,
            source: None,
            stack_segment_limit: DEFAULT_STACK_SEGMENT_LIMIT,
            stack_growth: StackGrowth::System,
        }
    }
}

impl<'a> EvalModuleOptions<'a> {
    pub fn with_globals(mut self, globals: Vec<(String, Value)>) -> Self {
        self.globals = globals;
        self
    }

    pub fn with_imports(mut self, imports: &'a ModuleImports) -> Self {
        self.imports = Some(imports);
        self
    }

    pub fn with_runtime_types(mut self, runtime_types: &'a RuntimeTypeBindings) -> Self {
        self.runtime_types = Some(runtime_types);
        self
    }

    pub fn with_builtin_methods(
        mut self,
        builtin_methods: &'a BuiltinMethodEnvironment,
        allow_attachments: bool,
    ) -> Self {
        self.builtin_methods = Some(builtin_methods);
        self.allow_builtin_method_attachments = allow_attachments;
        self
    }

    pub fn with_elaborations(mut self, elaborations: &'a EvalElaborationPlan) -> Self {
        self.elaborations = Some(elaborations);
        self
    }

    pub fn with_source(mut self, source: EvalSource) -> Self {
        self.source = Some(Rc::new(source));
        self
    }

    /// Cap active stacker segments for this evaluation.
    ///
    /// Each segment is [`STACK_SEGMENT_SIZE`] bytes. Callers that set nothing
    /// inherit [`DEFAULT_STACK_SEGMENT_LIMIT`] (64 MiB). Tooling such as
    /// `aven run` may raise this for developer machines; leaving it unset is
    /// the right default for constrained host embeddings.
    pub fn with_stack_segment_limit(mut self, limit: usize) -> Self {
        self.stack_segment_limit = limit;
        self
    }

    #[cfg(test)]
    fn with_failing_stack_growth(mut self) -> Self {
        self.stack_growth = StackGrowth::Fail;
        self
    }
}

/// Evaluate a module with the supplied host bindings and evaluator metadata.
///
/// Globals are pre-bound in the top-level environment. Module bindings use
/// normal top-level scope rules and may shadow an injected global.
pub fn eval_module_with_options(module: &Module, options: EvalModuleOptions<'_>) -> EvalOutcome {
    let default_imports = ModuleImports::default();
    let default_runtime_types = RuntimeTypeBindings::default();
    let default_builtin_methods = BuiltinMethodEnvironment::default();
    let default_elaborations = EvalElaborationPlan::default();
    let imports = options.imports.unwrap_or(&default_imports);
    let runtime_types = options.runtime_types.unwrap_or(&default_runtime_types);
    let builtin_methods = options.builtin_methods.unwrap_or(&default_builtin_methods);
    let elaborations = options.elaborations.unwrap_or(&default_elaborations);
    let mut env = Environment::with_imports_builtin_methods_and_reifications(
        imports.clone(),
        builtin_methods.clone(),
        options.allow_builtin_method_attachments,
        elaborations.slot_reifications.clone(),
        elaborations.direct_slot_inits.clone(),
        elaborations.primitive_families.clone(),
        options.source,
    );
    env.stack_segment_limit = options.stack_segment_limit;
    env.stack_growth = options.stack_growth;
    bind_intrinsics(&env);
    for (name, value) in options.globals {
        env.bind(name, value);
    }
    // Top-level: a propagated `@Err` (`?^` with no enclosing function) becomes
    // the program value and stops further items.
    match eval_items(&module.items, &env, Some(runtime_types)) {
        Ok(outcome) => outcome,
        Err(Flow::Propagate(value)) => EvalOutcome {
            value: Some(*value),
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
    for (name, inclusive, materialize) in [
        ("Stream.range", false, false),
        ("Stream.rangeInclusive", true, false),
        ("Array.range", false, true),
        ("Array.rangeInclusive", true, true),
    ] {
        env.bind(
            name,
            Value::RangeConstructor {
                inclusive,
                materialize,
            },
        );
    }
    // `collect` is target-owned: the type being built carries the static, so a
    // new collectible type is one more entry here and no change to any source.
    for target in [CollectTarget::Array, CollectTarget::Set] {
        env.bind(
            format!("{}.collect", target.type_name()),
            Value::CollectConstructor(target),
        );
    }
}

fn default_range_step(start: &Int, end: &Int) -> Int {
    if start > end {
        Int::from(-1)
    } else {
        Int::from(1)
    }
}

fn intrinsic_type_value(builtin: BuiltinType) -> Value {
    Value::named_type(builtin.name())
}

fn intrinsics() -> Vec<(String, Value)> {
    let mut intrinsics: Vec<(String, Value)> = BuiltinType::ALL
        .iter()
        .copied()
        .filter(|builtin| builtin.has_runtime_value())
        .map(|builtin| (builtin.name().to_owned(), intrinsic_type_value(builtin)))
        .collect();

    intrinsics.push((
        "keysOf".to_owned(),
        Value::native(|args| {
            if args.len() != 1 {
                return Err(format!("keysOf expects 1 argument, got {}", args.len()));
            }

            let names: Vec<String> = match &args[0] {
                Value::Record(fields) => fields.iter().map(|(name, _)| name.clone()).collect(),
                Value::Type(ty) => match ty.descriptor() {
                    RuntimeTypeDescriptor::Record(fields) => {
                        fields.iter().map(|(name, _)| name.clone()).collect()
                    }
                    _ => {
                        return Err(format!(
                            "keysOf expects a Record, got {}",
                            args[0].type_name()
                        ));
                    }
                },
                _ => {
                    return Err(format!(
                        "keysOf expects a Record, got {}",
                        args[0].type_name()
                    ));
                }
            };

            Ok(Value::Set(Rc::new(
                names.into_iter().map(Value::Text).collect(),
            )))
        }),
    ));

    // `Map` binds to a type value (see `BuiltinType::has_runtime_value`); its statics resolve
    // through `"Map.static"`-keyed globals consulted on `Value::Type` field
    // access.
    intrinsics.push(("Map.empty".to_owned(), Value::native(map_empty_intrinsic)));
    intrinsics.push(("Map.from".to_owned(), Value::native(map_from_intrinsic)));

    intrinsics.push((
        "repr".to_owned(),
        Value::native(|args| {
            let [value] = args else {
                return Err(format!("repr expects 1 argument, got {}", args.len()));
            };
            Ok(Value::Text(display::repr_text(value)))
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

fn map_empty_intrinsic(args: &[Value]) -> Result<Value, String> {
    if !args.is_empty() {
        return Err(format!("Map.empty expects 0 arguments, got {}", args.len()));
    }

    Ok(Value::Map(Rc::new(MapValue::new())))
}

fn map_from_intrinsic(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("Map.from expects 1 argument, got {}", args.len()));
    }
    map_from_pair_array(&args[0], "Map.from")
}

/// Build a map from an array of `(key, value)` tuples. Shared by `Map.from` and
/// value-position `Map(pairs)` construction (ruling 5: no map literal syntax).
fn map_from_pair_array(arg: &Value, context: &str) -> Result<Value, String> {
    let Value::Array(items) = arg else {
        return Err(format!(
            "{context} expects an Array of key/value tuples, got {}",
            arg.type_name()
        ));
    };

    let mut entries = MapValue::new();
    for item in items.iter() {
        let Value::Tuple(values) = item else {
            return Err(format!(
                "{context} expects (key, value) tuple entries, got {}",
                item.type_name()
            ));
        };
        let [key, value] = values.as_slice() else {
            return Err(format!(
                "{context} expects 2-item tuples, got tuple with {} items",
                values.len()
            ));
        };
        ensure_map_key(key, context)?;
        entries.insert(key.clone(), value.clone());
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

    match &args[0] {
        Value::Record(fields) => Ok(Value::Record(Rc::new(
            fields
                .iter()
                .filter(|(field, _)| labels.contains(field.as_str()) == keep_matched)
                .cloned()
                .collect(),
        ))),
        Value::Type(ty) => {
            let RuntimeTypeDescriptor::Record(fields) = ty.descriptor() else {
                return Err(format!(
                    "{name} expects a Record, got {}",
                    args[0].type_name()
                ));
            };
            Ok(Value::Type(
                ty.with_descriptor(RuntimeTypeDescriptor::Record(
                    fields
                        .iter()
                        .filter(|(field, _)| labels.contains(field.as_str()) == keep_matched)
                        .cloned()
                        .collect(),
                )),
            ))
        }
        _ => Err(format!(
            "{name} expects a Record, got {}",
            args[0].type_name()
        )),
    }
}

/// Evaluate a sequence of items, collecting `Flow::Fail` diagnostics across them
/// (recovery) while letting `Flow::Propagate` bubble out via `?`. Both the
/// top-level loop and blocks share this; only their callers decide whether to
/// catch `Propagate`.
///
/// `:=` (explicit shadow) installs a **fresh** binding by pushing a child scope
/// frame rather than overwriting the current map slot. Closures that already
/// captured the pre-shadow environment keep seeing the old value; later items
/// (and later closures) use the extended chain. Plain `=` still binds into the
/// current frame (name resolution forbids same-scope rebind with `=`).
fn eval_items(
    items: &[Item],
    env: &Environment,
    runtime_types: Option<&RuntimeTypeBindings>,
) -> Eval<EvalOutcome> {
    let mut env = env.clone();
    let mut value = None;
    let mut diagnostics = Vec::new();

    for item in items {
        // A body-bearing method record defines a named-family provider only for
        // an uppercase (type) name. A lowercase binding with method bodies is a
        // direct slot-record initializer, materialized through the plan path.
        if let Item::Binding(binding) = item
            && binding.name.chars().next().is_some_and(char::is_uppercase)
            && (aven_parser::is_named_method_provider(&binding.value)
                || aven_parser::is_primitive_family_provider(&binding.value))
        {
            match eval_named_family(binding.name.as_str(), &binding.value, &env) {
                Ok(descriptor) => env.bind(binding.name.clone(), descriptor),
                Err(Flow::Fail(mut next_diagnostics)) => diagnostics.append(&mut next_diagnostics),
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
            }
            value = None;
            continue;
        }

        if let Item::Binding(binding) = item
            && is_method_requirement_row(&binding.value)
        {
            value = None;
            continue;
        }

        // A closed slot-record *type* alias (`Csv = { csv(): Text }`) carries
        // bodyless arrow methods; it defines a type, not a runtime value, so it
        // is never evaluated.
        if let Item::Binding(binding) = item
            && is_slot_record_type_alias(&binding.value)
        {
            value = None;
            continue;
        }

        if let Item::Binding(binding) = item
            && let Some(runtime_type) = runtime_types.and_then(|types| types.get(&binding.name))
        {
            env.bind(binding.name.clone(), runtime_type);
            value = None;
            continue;
        }

        match item {
            Item::Expr(expr) => match eval_expr_many(expr, &env) {
                Ok(next_value) => value = Some(next_value),
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
                Err(Flow::Fail(mut next_diagnostics)) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::Binding(binding) => match eval_expr_many(&binding.value, &env) {
                Ok(next_value) => {
                    if binding.shadow_span.is_some() {
                        let next = env.child();
                        next.bind(binding.name.clone(), next_value);
                        env = next;
                    } else {
                        env.bind(binding.name.clone(), next_value);
                    }
                    value = None;
                }
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
                Err(Flow::Fail(mut next_diagnostics)) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::PatternBinding(binding) => match eval_expr_many(&binding.value, &env)
                .and_then(|next_value| bind_pattern_item(&binding.pattern, &next_value, &env))
            {
                Ok(()) => value = None,
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
                Err(Flow::Fail(mut next_diagnostics)) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::SpreadBinding(binding) => match eval_expr_many(&binding.value, &env)
                .and_then(|next_value| bind_spread_item(&next_value, binding.value.span, &env))
            {
                Ok(()) => value = None,
                Err(flow @ Flow::Propagate(_)) => return Err(flow),
                Err(Flow::Fail(mut next_diagnostics)) => {
                    value = None;
                    diagnostics.append(&mut next_diagnostics);
                }
            },
            Item::MethodAttachment(attachment) => {
                if env.allow_builtin_method_attachments {
                    install_builtin_method_attachment(attachment, &env);
                }
                value = None;
            }
            Item::Signature(_) => value = None,
        }
    }

    Ok(EvalOutcome { value, diagnostics })
}

fn install_builtin_method_attachment(
    attachment: &aven_parser::MethodAttachment,
    env: &Environment,
) {
    let Some(owner) = attachment_owner_head(&attachment.owner) else {
        return;
    };
    for member in &attachment.members {
        let RecordEntry::Method { name, value, .. } = member else {
            continue;
        };
        let ExprKind::Lambda { params, body, .. } = &value.kind else {
            continue;
        };
        let mut closure_params = Vec::with_capacity(params.len() + 1);
        closure_params.push(ClosureParam {
            name: aven_parser::METHOD_RECEIVER_NAME.to_owned(),
            default: None,
        });
        closure_params.extend(params.iter().map(|param| ClosureParam {
            name: param.name.clone(),
            default: param.default.clone().map(Rc::new),
        }));
        env.builtin_methods.insert(BuiltinMethodImplementation {
            owner: owner.to_owned(),
            member: name.clone(),
            implementation: Closure {
                params: closure_params,
                body: Rc::new((**body).clone()),
                env: env.clone(),
            },
        });
    }
}

fn attachment_owner_head(owner: &Expr) -> Option<&str> {
    match &owner.kind {
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => Some(name),
        ExprKind::Call { callee, .. } | ExprKind::Group(callee) => attachment_owner_head(callee),
        _ => None,
    }
}

fn runtime_builtin_owner(value: &Value) -> Option<&'static str> {
    match value {
        Value::Array(_) => Some("Array"),
        Value::Stream(_) => Some("Stream"),
        Value::Set(_) => Some("Set"),
        Value::Map(_) => Some("Map"),
        Value::Text(_) => Some("Text"),
        Value::Int(_) => Some("Int"),
        Value::Float(_) => Some("Float"),
        Value::Bool(_) => Some("Bool"),
        _ => None,
    }
}

fn eval_named_family(owner: &str, value: &Expr, env: &Environment) -> Eval {
    let (entries, primitive) =
        match &value.kind {
            ExprKind::Record(entries) => (entries.as_slice(), None),
            ExprKind::PrimitiveFamily { base, members } => {
                let fallback_base = attachment_owner_head(base).unwrap_or("?").to_owned();
                let runtime = env.primitive_families.family(owner).cloned().unwrap_or(
                    PrimitiveFamilyRuntime {
                        owner: owner.to_owned(),
                        base: fallback_base,
                        inherited_methods: Vec::new(),
                    },
                );
                (members.as_slice(), Some(runtime))
            }
            _ => {
                return Err(one_diagnostic(unsupported_expr(
                    value.span,
                    "named-family declaration must carry a record or primitive payload",
                )));
            }
        };
    let mut fields = Vec::new();
    let mut methods = HashMap::new();
    if let Some(runtime) = &primitive {
        for method in &runtime.inherited_methods {
            methods.insert(
                method.member.clone(),
                NamedMethodImplementation::Inherited(Rc::new(InheritedMethodImplementation {
                    member: method.member.clone(),
                    lifted_params: method.lifted_params.clone(),
                    lifted_result: method.lifted_result,
                    env: env.clone(),
                })),
            );
        }
    }
    for entry in entries {
        match entry {
            RecordEntry::Field { name, value, .. } => fields.push(NamedFamilyField {
                name: name.clone(),
                optional: matches!(value.kind, ExprKind::Optional(_)),
                default: None,
            }),
            RecordEntry::FieldDefault {
                name,
                annotation,
                default,
                ..
            } => fields.push(NamedFamilyField {
                name: name.clone(),
                optional: matches!(annotation.kind, ExprKind::Optional(_)),
                default: Some(Rc::new(default.clone())),
            }),
            RecordEntry::Method { name, value, .. } => {
                let ExprKind::Lambda { params, body, .. } = &value.kind else {
                    continue;
                };
                let mut closure_params = Vec::with_capacity(params.len() + 1);
                closure_params.push(ClosureParam {
                    name: aven_parser::METHOD_RECEIVER_NAME.to_owned(),
                    default: None,
                });
                closure_params.extend(params.iter().map(|param| ClosureParam {
                    name: param.name.clone(),
                    default: param.default.clone().map(Rc::new),
                }));
                methods.insert(
                    name.clone(),
                    NamedMethodImplementation::Declared(Closure {
                        params: closure_params,
                        body: Rc::new((**body).clone()),
                        env: env.clone(),
                    }),
                );
            }
            _ => {}
        }
    }
    Ok(Value::NamedFamily(Rc::new(NamedFamilyDescriptor {
        owner: primitive
            .as_ref()
            .map_or_else(|| owner.to_owned(), |runtime| runtime.owner.clone()),
        primitive_base: primitive.map(|runtime| runtime.base),
        fields,
        methods,
    })))
}

pub fn eval_expr(expr: &Expr, env: &Environment) -> Result<Value, Diagnostic> {
    eval_expr_many(expr, env).map_err(first_diagnostic)
}

fn eval_expr_many(expr: &Expr, env: &Environment) -> Eval {
    let mut value = eval_expr_unreified(expr, env)?;
    if let Some(coercion) = env.primitive_families.coercion(expr.span) {
        value = apply_primitive_family_coercion(value, coercion, expr.span, env)?;
    }
    let Some(target) = env.slot_reifications.get(expr.span) else {
        return Ok(value);
    };
    reify_slot_record(value, target, expr.span, env)
}

fn apply_primitive_family_coercion(
    value: Value,
    coercion: &PrimitiveFamilyCoercion,
    span: Span,
    env: &Environment,
) -> Eval {
    match coercion {
        PrimitiveFamilyCoercion::Brand { owner } => {
            let descriptor = env
                .family_descriptors
                .borrow()
                .get(owner)
                .cloned()
                .ok_or_else(|| one_diagnostic(unbound_name(owner, span)))?;
            let found = value.type_name();
            let payload = primitive_payload_from_value(value).ok_or_else(|| {
                one_diagnostic(record_type_error(
                    span,
                    "primitive-family branding",
                    found,
                    "Int, Float, Text, or Bool",
                ))
            })?;
            if !primitive_base_accepts_payload(
                descriptor.primitive_base.as_deref(),
                payload.type_name(),
            ) {
                return Err(one_diagnostic(record_type_error(
                    span,
                    "primitive-family branding",
                    payload.type_name(),
                    descriptor.primitive_base.as_deref().unwrap_or("primitive"),
                )));
            }
            Ok(Value::BrandedPrimitive {
                descriptor,
                payload,
            })
        }
        PrimitiveFamilyCoercion::Widen => Ok(erase_primitive_brand(value)),
    }
}

fn erase_primitive_brand(value: Value) -> Value {
    match value {
        Value::BrandedPrimitive { payload, .. } => payload.into_value(),
        value => value,
    }
}

fn primitive_payload_from_value(value: Value) -> Option<PrimitivePayload> {
    match value {
        Value::Int(value) => Some(PrimitivePayload::Int(value)),
        Value::Float(value) => Some(PrimitivePayload::Float(value)),
        Value::Text(value) => Some(PrimitivePayload::Text(value)),
        Value::Bool(value) => Some(PrimitivePayload::Bool(value)),
        Value::Array(value) => Some(PrimitivePayload::Array(value)),
        Value::Set(value) => Some(PrimitivePayload::Set(value)),
        Value::Map(value) => Some(PrimitivePayload::Map(value)),
        _ => None,
    }
}

fn primitive_base_accepts_payload(base: Option<&str>, payload: &str) -> bool {
    base.is_some_and(|base| base.split_once('(').map_or(base, |(head, _)| head) == payload)
}

fn eval_expr_unreified(expr: &Expr, env: &Environment) -> Eval {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(literal, expr.span).map_err(one_diagnostic),
        ExprKind::Regex(_) => Err(one_diagnostic(unsupported_expr(
            expr.span,
            "regex literals have no runtime value; use Text for executable patterns",
        ))),
        ExprKind::Interpolation(segments) => eval_interpolation(segments, env),
        ExprKind::Undefined => Ok(Value::Undefined),
        ExprKind::Null => Ok(Value::Null),
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => env
            .lookup(name)
            .ok_or_else(|| one_diagnostic(unbound_name(name, expr.span))),
        ExprKind::Group(inner) => eval_expr_many(inner, env),
        ExprKind::Optional(inner) => {
            eval_type_wrapper(inner, expr.span, env, RuntimeType::optional)
        }
        ExprKind::Nullable(inner) => {
            eval_type_wrapper(inner, expr.span, env, RuntimeType::nullable)
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
        ExprKind::Record(entries) if env.direct_slot_inits.contains(expr.span) => {
            eval_direct_slot_init(entries, env)
        }
        ExprKind::Record(entries) => eval_record(entries, env),
        ExprKind::Match { subject, arms, .. } => eval_match(subject, arms, expr.span, env),
        ExprKind::FieldAccess {
            receiver,
            field,
            field_span,
            null_safe,
        } => eval_field_access(receiver, field, *field_span, *null_safe, env),
        ExprKind::Index {
            callee,
            args,
            null_safe,
        } => eval_index(callee, args, *null_safe, expr.span, env),
        ExprKind::Call { callee, args } => eval_type_application(callee, args, expr.span, env)
            .unwrap_or_else(|| eval_call(callee, args, expr.span, env)),
        ExprKind::Propagate {
            value,
            operator_span,
            mode,
        } => eval_propagate(value, *operator_span, *mode, env),
        _ => Err(one_diagnostic(unsupported_expr(
            expr.span,
            "the evaluator cannot run this expression form",
        ))),
    }
}

fn reify_slot_record(
    source: Value,
    target: &SlotReification,
    span: Span,
    env: &Environment,
) -> Eval {
    if let Value::SlotRecord { fields, slots } = &source {
        let fields = project_reified_members(fields, &target.fields, span)?;
        let slots = project_reified_members(slots, &target.slots, span)?;
        return Ok(Value::SlotRecord {
            fields: Rc::new(fields),
            slots: Rc::new(slots),
        });
    }

    let fields = match &source {
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
            project_reified_members(fields, &target.fields, span)?
        }
        _ if target.fields.is_empty() => Vec::new(),
        value => {
            return Err(one_diagnostic(record_type_error(
                span,
                "method-slot reification",
                value.type_name(),
                "a source with the target data fields",
            )));
        }
    };
    let mut slots = Vec::with_capacity(target.slots.len());
    for name in &target.slots {
        let implementation = reification_method(&source, name, env)
            .ok_or_else(|| one_diagnostic(missing_field(name, span)))?;
        slots.push((name.clone(), implementation));
    }
    Ok(Value::SlotRecord {
        fields: Rc::new(fields),
        slots: Rc::new(slots),
    })
}

fn project_reified_members(
    source: &[(String, Value)],
    requested: &[String],
    span: Span,
) -> Eval<Vec<(String, Value)>> {
    requested
        .iter()
        .map(|name| {
            record_field_value(source, name)
                .cloned()
                .map(|value| (name.clone(), value))
                .ok_or_else(|| one_diagnostic(missing_field(name, span)))
        })
        .collect()
}

fn reification_method(source: &Value, name: &str, env: &Environment) -> Option<Value> {
    match source {
        Value::NamedRecord { descriptor, .. } | Value::BrandedPrimitive { descriptor, .. } => {
            descriptor
                .methods
                .get(name)
                .cloned()
                .map(|implementation| Value::NamedMethod {
                    receiver: Box::new(source.clone()),
                    member: name.to_owned(),
                    implementation,
                })
        }
        value => builtin_method(value, name, env),
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
        ExprKind::Regex(_) => Err(unsupported_expr(
            pattern.span,
            "regex literals have no runtime value; match a Text value instead",
        )),
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

fn bind_pattern_item(pattern: &Expr, value: &Value, env: &Environment) -> Eval<()> {
    let bindings = destructure_pattern_binding(pattern, value)?;
    for (name, value) in bindings {
        env.bind(name, value);
    }
    Ok(())
}

fn bind_spread_item(value: &Value, span: Span, env: &Environment) -> Eval<()> {
    let Value::Record(fields) = value else {
        return Err(one_diagnostic(record_type_error(
            span,
            "block spread",
            value.type_name(),
            "Record",
        )));
    };

    for (name, value) in fields.iter() {
        env.bind(name.clone(), value.clone());
    }
    Ok(())
}

fn destructure_pattern_binding(pattern: &Expr, value: &Value) -> Eval<Vec<(String, Value)>> {
    match &pattern.kind {
        ExprKind::Group(inner) => destructure_pattern_binding(inner, value),
        ExprKind::Name(name) | ExprKind::ComptimeName(name) if name != "_" => {
            Ok(vec![(name.clone(), value.clone())])
        }
        ExprKind::Record(entries) => destructure_record_binding(entries, value),
        ExprKind::Tuple(items) => destructure_tuple_binding(items, value),
        _ => match match_pattern(pattern, value, &Environment::new()) {
            Ok(Some(bindings)) => Ok(bindings),
            Ok(None) => Err(one_diagnostic(record_type_error(
                pattern.span,
                "pattern binding",
                value.type_name(),
                "matching value",
            ))),
            Err(diagnostic) => Err(one_diagnostic(diagnostic)),
        },
    }
}

fn destructure_tuple_binding(items: &[Expr], value: &Value) -> Eval<Vec<(String, Value)>> {
    let Value::Tuple(values) = value else {
        return Err(one_diagnostic(record_type_error(
            items.first().map_or(Span::point(0), |item| item.span),
            "tuple pattern binding",
            value.type_name(),
            "Tuple",
        )));
    };

    let mut bindings = Vec::new();
    for (pattern, value) in items.iter().zip(values.iter()) {
        bindings.extend(destructure_pattern_binding(pattern, value)?);
    }
    Ok(bindings)
}

fn destructure_record_binding(
    entries: &[RecordEntry],
    value: &Value,
) -> Eval<Vec<(String, Value)>> {
    let Value::Record(fields) = value else {
        return Err(one_diagnostic(record_type_error(
            entries.first().map_or(Span::point(0), record_entry_span),
            "record pattern binding",
            value.type_name(),
            "Record",
        )));
    };

    let mut bindings = Vec::new();
    for entry in entries {
        match entry {
            RecordEntry::Field {
                name,
                value: pattern,
                name_span,
                ..
            } => {
                let field_value = record_field_value(fields, name)
                    .ok_or_else(|| one_diagnostic(missing_field(name, *name_span)))?;
                bindings.extend(destructure_pattern_binding(pattern, field_value)?);
            }
            RecordEntry::Shorthand {
                name, name_span, ..
            } => {
                let field_value = record_field_value(fields, name)
                    .ok_or_else(|| one_diagnostic(missing_field(name, *name_span)))?;
                bindings.push((name.clone(), field_value.clone()));
            }
            RecordEntry::Rename {
                from,
                from_span,
                to,
                ..
            } => {
                let field_value = record_field_value(fields, from)
                    .ok_or_else(|| one_diagnostic(missing_field(from, *from_span)))?;
                bindings.push((to.clone(), field_value.clone()));
            }
            RecordEntry::Spread { .. } | RecordEntry::Open { .. } => {}
            _ => {
                return Err(one_diagnostic(record_type_error(
                    record_entry_span(entry),
                    "record pattern binding",
                    "record transform entry",
                    "record pattern entry",
                )));
            }
        }
    }
    Ok(bindings)
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
    let fields = match value {
        Value::Record(fields) | Value::NamedRecord { fields, .. } => fields,
        _ => return Ok(None),
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
    let outcome = eval_items(items, &child, None)?;

    if outcome.diagnostics.is_empty() {
        Ok(outcome.value.unwrap_or(Value::Undefined))
    } else {
        Err(Flow::Fail(outcome.diagnostics))
    }
}

fn eval_call(callee: &Expr, args: &[Expr], span: Span, env: &Environment) -> Eval {
    if let Some(value) = eval_import_call(callee, args, env) {
        return value;
    }

    if let ExprKind::FieldAccess {
        receiver,
        field,
        null_safe: false,
        ..
    } = &callee.kind
        && field == "to"
        && args.len() == 1
        && env.slot_reifications.get(receiver.span).is_some()
    {
        return eval_expr_many(receiver, env);
    }

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

    // `text.decode(Fmt, ...)` desugars to `Fmt.decode(text, ...)`: the format
    // arrives first and supplies the decoder. Mirror the checker's call-site
    // treatment — only a `Text` receiver takes the method form; any other
    // receiver keeps ordinary field-access-call behavior (single receiver eval).
    if let ExprKind::FieldAccess {
        receiver,
        field,
        field_span,
        null_safe: false,
    } = &callee.kind
        && field == "decode"
    {
        let receiver_value = eval_expr_many(receiver, env)?;
        if matches!(receiver_value, Value::Text(_))
            && let Some(format) = args.first()
        {
            let decode_fn = format_static_value(format, "decode", env)?;
            let arg_values = receiver_prefixed_arg_values(receiver_value, &args[1..], env)?;
            return apply_callee_values(
                decode_fn,
                format.span,
                arg_values,
                env.native_context(span),
            );
        }
        let callee_value =
            field_access_value(receiver_value, receiver.span, field, *field_span, env)?;
        return apply_callee(callee_value, callee.span, args, span, env);
    }

    // `value.encode(Fmt, ...)` desugars to `Fmt.encode(value, ...)` when the
    // receiver does not itself carry `encode`. A real receiver member keeps
    // ordinary field-call semantics, matching the checker's closed lookup rule.
    if let ExprKind::FieldAccess {
        receiver,
        field,
        field_span,
        null_safe: false,
    } = &callee.kind
        && field == "encode"
    {
        let receiver_value = eval_expr_many(receiver, env)?;
        if !value_carries_member(&receiver_value, field, env)
            && let Some(format) = args.first()
        {
            let encode_fn = format_static_value(format, "encode", env)?;
            let arg_values = receiver_prefixed_arg_values(receiver_value, &args[1..], env)?;
            return apply_callee_values(
                encode_fn,
                format.span,
                arg_values,
                env.native_context(span),
            );
        }
        let callee_value =
            field_access_value(receiver_value, receiver.span, field, *field_span, env)?;
        return apply_callee(callee_value, callee.span, args, span, env);
    }

    // `source.collect(Target)` desugars to `Target.collect(source)`, the same
    // target-owned shape as `encode`. The static form is written directly and
    // lands on the same `CollectConstructor`, so the two spellings are one
    // implementation.
    if let ExprKind::FieldAccess {
        receiver,
        field,
        field_span,
        null_safe: false,
    } = &callee.kind
        && field == "collect"
    {
        let receiver_value = eval_expr_many(receiver, env)?;
        if !value_carries_member(&receiver_value, field, env)
            && let Some(target) = args.first()
        {
            let collect_fn = format_static_value(target, "collect", env)?;
            let arg_values = receiver_prefixed_arg_values(receiver_value, &args[1..], env)?;
            return apply_callee_values(
                collect_fn,
                target.span,
                arg_values,
                env.native_context(span),
            );
        }
        let callee_value =
            field_access_value(receiver_value, receiver.span, field, *field_span, env)?;
        return apply_callee(callee_value, callee.span, args, span, env);
    }

    let callee_value = eval_expr_many(callee, env)?;
    apply_callee(callee_value, callee.span, args, span, env)
}

fn eval_import_call(callee: &Expr, args: &[Expr], env: &Environment) -> Option<Eval> {
    let ExprKind::Name(name) = &callee.kind else {
        return None;
    };
    if name != "import" {
        return None;
    }

    let Some(arg) = args.first() else {
        return Some(Err(one_diagnostic(dynamic_import(callee.span))));
    };
    if args.len() != 1 {
        return Some(Err(one_diagnostic(dynamic_import(callee.span))));
    }

    let ExprKind::Literal(Literal::String(raw)) = &arg.kind else {
        return Some(Err(one_diagnostic(dynamic_import(arg.span))));
    };
    let specifier = decode_string_literal(raw);
    match env.imports.get(&specifier) {
        Some(Some(value)) => Some(Ok(value)),
        Some(None) => Some(Err(one_diagnostic(import_failed(&specifier, arg.span)))),
        None if aven_core::is_local_import_specifier(&specifier) => {
            Some(Err(one_diagnostic(unresolved_import(&specifier, arg.span))))
        }
        None => Some(Err(one_diagnostic(unsupported_import_root(
            &specifier, arg.span,
        )))),
    }
}

fn format_static_value(format: &Expr, member: &str, env: &Environment) -> Eval {
    let format_value = eval_expr_many(format, env)?;
    field_access_value(format_value, format.span, member, format.span, env)
}

fn receiver_prefixed_arg_values(
    receiver_value: Value,
    args: &[Expr],
    env: &Environment,
) -> Result<Vec<Value>, Flow> {
    let mut arg_values = Vec::with_capacity(args.len() + 1);
    arg_values.push(receiver_value);
    for arg in args {
        arg_values.push(eval_expr_many(arg, env)?);
    }
    Ok(arg_values)
}

/// Apply an already-evaluated callee value to the argument expressions.
fn apply_callee(
    callee_value: Value,
    callee_span: Span,
    args: &[Expr],
    span: Span,
    env: &Environment,
) -> Eval {
    match callee_value {
        Value::Native(function) => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_native(function, arg_values, env.native_context(span))
        }
        Value::RangeConstructor {
            inclusive,
            materialize,
        } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_range_constructor(arg_values, inclusive, materialize, span)
        }
        Value::CollectConstructor(target) => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_collect_constructor(arg_values, target, span)
        }
        Value::ResultMethod { receiver, kind } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_result_method(*receiver, kind, arg_values, callee_span, span)
        }
        Value::StreamMethod { receiver, kind } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_stream_method(*receiver, kind, arg_values, span)
        }
        Value::ArrayFlatMapMethod(items) => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_array_flat_map(items, arg_values, span)
        }
        Value::ArrayFoldMethod(items) => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_array_fold(items, arg_values, span)
        }
        Value::SetMethod { receiver, kind } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_set_method(receiver, kind, arg_values, span)
        }
        Value::NamedFamily(descriptor) => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_named_family_constructor(descriptor, arg_values, span)
        }
        Value::NamedMethod {
            receiver,
            member,
            implementation,
        } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_named_method_for_member(*receiver, &member, implementation, arg_values, span)
        }
        Value::UnboundNamedMethod {
            descriptor,
            member,
            implementation,
        } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_unbound_named_method(descriptor, &member, implementation, arg_values, span)
        }
        Value::Closure(closure) => apply_closure(closure, args, span, env),
        value => Err(one_diagnostic(not_callable(callee_span, value.type_name()))),
    }
}

fn apply_callee_values(
    callee_value: Value,
    callee_span: Span,
    arg_values: Vec<Value>,
    context: NativeContext,
) -> Eval {
    let span = context.span;
    match callee_value {
        Value::Native(function) => apply_native(function, arg_values, context),
        Value::RangeConstructor {
            inclusive,
            materialize,
        } => apply_range_constructor(arg_values, inclusive, materialize, span),
        Value::CollectConstructor(target) => apply_collect_constructor(arg_values, target, span),
        Value::ResultMethod { receiver, kind } => {
            apply_result_method(*receiver, kind, arg_values, callee_span, span)
        }
        Value::StreamMethod { receiver, kind } => {
            apply_stream_method(*receiver, kind, arg_values, span)
        }
        Value::ArrayFlatMapMethod(items) => apply_array_flat_map(items, arg_values, span),
        Value::ArrayFoldMethod(items) => apply_array_fold(items, arg_values, span),
        Value::SetMethod { receiver, kind } => apply_set_method(receiver, kind, arg_values, span),
        Value::NamedFamily(descriptor) => {
            apply_named_family_constructor(descriptor, arg_values, span)
        }
        Value::NamedMethod {
            receiver,
            member,
            implementation,
        } => apply_named_method_for_member(*receiver, &member, implementation, arg_values, span),
        Value::UnboundNamedMethod {
            descriptor,
            member,
            implementation,
        } => apply_unbound_named_method(descriptor, &member, implementation, arg_values, span),
        Value::Closure(closure) => apply_closure_values(closure, arg_values, span),
        value => Err(one_diagnostic(not_callable(callee_span, value.type_name()))),
    }
}

fn apply_unbound_named_method(
    descriptor: Rc<NamedFamilyDescriptor>,
    member: &str,
    implementation: NamedMethodImplementation,
    mut args: Vec<Value>,
    span: Span,
) -> Eval {
    if args.is_empty() {
        return Err(one_diagnostic(arity_mismatch(span, 1, 1, 0)));
    }
    let receiver = args.remove(0);
    let receiver_matches = match &receiver {
        Value::NamedRecord {
            descriptor: actual, ..
        }
        | Value::BrandedPrimitive {
            descriptor: actual, ..
        } => Rc::ptr_eq(actual, &descriptor),
        _ => false,
    };
    if !receiver_matches {
        return Err(one_diagnostic(record_type_error(
            span,
            "unbound method receiver",
            receiver.type_name(),
            &descriptor.owner,
        )));
    }
    apply_named_method_for_member(receiver, member, implementation, args, span)
}

fn apply_named_method_for_member(
    receiver: Value,
    member: &str,
    implementation: NamedMethodImplementation,
    args: Vec<Value>,
    span: Span,
) -> Eval {
    let Value::BrandedPrimitive { descriptor, .. } = &receiver else {
        return apply_named_method(receiver, implementation, args, span);
    };
    if member != "toText" {
        return apply_named_method(receiver, implementation, args, span);
    }
    // Same re-entry rule as the display-protocol path: a family `toText` body
    // that calls `.toText()` on a derived branded value (e.g. `. % 100`) must
    // see the base Int/Float rendering, not recurse into the override.
    if display::to_text_owner_is_active(&descriptor.owner) {
        let base = erase_primitive_brand(receiver);
        return match display::to_text(&base, None, span) {
            Ok(text) => Ok(Value::Text(text)),
            Err(flow) => Err(flow),
        };
    }
    let owner = descriptor.owner.clone();
    display::with_active_to_text_owner(&owner, || {
        apply_named_method(receiver, implementation, args, span)
    })
}

fn apply_named_method(
    receiver: Value,
    implementation: NamedMethodImplementation,
    args: Vec<Value>,
    span: Span,
) -> Eval {
    match implementation {
        NamedMethodImplementation::Declared(implementation) => {
            let mut values = Vec::with_capacity(args.len() + 1);
            values.push(receiver);
            values.extend(args);
            apply_closure_values(implementation, values, span)
        }
        NamedMethodImplementation::Inherited(implementation) => {
            apply_inherited_primitive_method(receiver, implementation, args, span)
        }
    }
}

fn apply_inherited_primitive_method(
    receiver: Value,
    implementation: Rc<InheritedMethodImplementation>,
    args: Vec<Value>,
    span: Span,
) -> Eval {
    let descriptor = match &receiver {
        Value::BrandedPrimitive { descriptor, .. } => Rc::clone(descriptor),
        value => {
            return Err(one_diagnostic(record_type_error(
                span,
                "inherited primitive method",
                value.type_name(),
                "a branded primitive receiver",
            )));
        }
    };
    let receiver = erase_primitive_brand(receiver);
    let args = args
        .into_iter()
        .zip(
            implementation
                .lifted_params
                .iter()
                .copied()
                .chain(std::iter::repeat(false)),
        )
        .map(|(arg, lifted)| {
            if lifted {
                erase_primitive_brand(arg)
            } else {
                arg
            }
        })
        .collect::<Vec<_>>();

    let result = if is_binary_method(&implementation.member) && args.len() == 1 {
        let mut args = args;
        let right = args.remove(0);
        apply_binary(
            receiver,
            &implementation.member,
            Span::new(0, 0),
            right,
            span,
            span,
        )?
    } else {
        let method = builtin_method(&receiver, &implementation.member, &implementation.env)
            .ok_or_else(|| one_diagnostic(missing_field(&implementation.member, span)))?;
        apply_callee_values(method, span, args, implementation.env.native_context(span))?
    };

    if implementation.lifted_result {
        let found = result.type_name();
        let payload = primitive_payload_from_value(result).ok_or_else(|| {
            one_diagnostic(record_type_error(
                span,
                "inherited primitive-family result",
                found,
                descriptor.primitive_base.as_deref().unwrap_or("primitive"),
            ))
        })?;
        Ok(Value::BrandedPrimitive {
            descriptor,
            payload,
        })
    } else {
        Ok(result)
    }
}

fn is_binary_method(member: &str) -> bool {
    is_method_operator(member)
}

fn apply_named_family_constructor(
    descriptor: Rc<NamedFamilyDescriptor>,
    args: Vec<Value>,
    span: Span,
) -> Eval {
    let [payload] = args.as_slice() else {
        return Err(one_diagnostic(arity_mismatch(span, 1, 1, args.len())));
    };
    if descriptor.primitive_base.is_some() {
        let found = payload.type_name();
        let Some(payload) = primitive_payload_from_value(payload.clone()) else {
            return Err(one_diagnostic(record_type_error(
                span,
                "primitive-family construction",
                found,
                descriptor.primitive_base.as_deref().unwrap_or("primitive"),
            )));
        };
        if !primitive_base_accepts_payload(
            descriptor.primitive_base.as_deref(),
            payload.type_name(),
        ) {
            return Err(one_diagnostic(record_type_error(
                span,
                "primitive-family construction",
                payload.type_name(),
                descriptor.primitive_base.as_deref().unwrap_or("primitive"),
            )));
        }
        return Ok(Value::BrandedPrimitive {
            descriptor,
            payload,
        });
    }
    let payload_fields = match payload {
        Value::Record(fields) | Value::NamedRecord { fields, .. } => fields,
        value => {
            return Err(one_diagnostic(record_type_error(
                span,
                "named-family construction",
                value.type_name(),
                "Record",
            )));
        }
    };
    if let Some((extra, _)) = payload_fields
        .iter()
        .find(|(name, _)| !descriptor.fields.iter().any(|field| field.name == *name))
    {
        return Err(one_diagnostic(record_type_error(
            span,
            "named-family construction",
            &format!("extra field `{extra}`"),
            "the exact declared data row",
        )));
    }

    let call_env = descriptor
        .methods
        .values()
        .next()
        .map(|method| match method {
            NamedMethodImplementation::Declared(method) => method.env.clone(),
            NamedMethodImplementation::Inherited(method) => method.env.clone(),
        })
        .unwrap_or_default();
    let mut fields = Vec::with_capacity(descriptor.fields.len());
    for field in &descriptor.fields {
        if let Some(value) = record_field_value(payload_fields, &field.name) {
            fields.push((field.name.clone(), value.clone()));
        } else if let Some(default) = &field.default {
            fields.push((
                field.name.clone(),
                eval_expr_many(default.as_ref(), &call_env)?,
            ));
        } else if !field.optional {
            return Err(one_diagnostic(record_type_error(
                span,
                "named-family construction",
                &format!("missing field `{}`", field.name),
                "the exact declared data row",
            )));
        }
    }
    Ok(Value::NamedRecord {
        descriptor,
        fields: Rc::new(fields),
    })
}

fn apply_result_method(
    receiver: Value,
    kind: ResultMethod,
    args: Vec<Value>,
    callee_span: Span,
    span: Span,
) -> Eval {
    let expected_arity = match kind {
        ResultMethod::IsOk | ResultMethod::IsErr => 0,
        ResultMethod::MapErr
        | ResultMethod::OrElse
        | ResultMethod::Map
        | ResultMethod::AndThen
        | ResultMethod::UnwrapOr => 1,
    };
    if args.len() != expected_arity {
        return Err(one_diagnostic(arity_mismatch(
            span,
            expected_arity,
            expected_arity,
            args.len(),
        )));
    }

    let Value::Tag { name, mut payload } = receiver else {
        return Err(one_diagnostic(not_callable(callee_span, "Tag")));
    };
    let [value] = payload.as_mut_slice() else {
        return Err(one_diagnostic(not_callable(callee_span, "Tag")));
    };

    match kind {
        ResultMethod::IsOk => Ok(Value::Bool(name == "Ok")),
        ResultMethod::IsErr => Ok(Value::Bool(name == "Err")),
        ResultMethod::UnwrapOr => {
            if name == "Ok" {
                Ok(value.clone())
            } else {
                Ok(args[0].clone())
            }
        }
        ResultMethod::Map => {
            if name == "Ok" {
                let transformed = apply_callee_values(
                    args[0].clone(),
                    callee_span,
                    vec![value.clone()],
                    NativeContext::without_source(span),
                )?;
                Ok(Value::Tag {
                    name,
                    payload: vec![transformed],
                })
            } else {
                Ok(Value::Tag {
                    name,
                    payload: vec![value.clone()],
                })
            }
        }
        ResultMethod::AndThen => {
            if name == "Ok" {
                apply_callee_values(
                    args[0].clone(),
                    callee_span,
                    vec![value.clone()],
                    NativeContext::without_source(span),
                )
            } else {
                Ok(Value::Tag {
                    name,
                    payload: vec![value.clone()],
                })
            }
        }
        ResultMethod::MapErr => {
            if name == "Ok" {
                Ok(Value::Tag {
                    name,
                    payload: vec![value.clone()],
                })
            } else {
                let transformed = apply_callee_values(
                    args[0].clone(),
                    callee_span,
                    vec![value.clone()],
                    NativeContext::without_source(span),
                )?;
                Ok(Value::Tag {
                    name,
                    payload: vec![transformed],
                })
            }
        }
        ResultMethod::OrElse => {
            if name == "Ok" {
                Ok(Value::Tag {
                    name,
                    payload: vec![value.clone()],
                })
            } else {
                apply_callee_values(
                    args[0].clone(),
                    callee_span,
                    vec![value.clone()],
                    NativeContext::without_source(span),
                )
            }
        }
    }
}

fn apply_stream_method(stream: Stream, kind: StreamMethod, args: Vec<Value>, span: Span) -> Eval {
    let expected = match kind {
        StreamMethod::Map | StreamMethod::Filter | StreamMethod::Each => 1,
        StreamMethod::Fold => 2,
        StreamMethod::ToArray => 0,
    };
    if args.len() != expected {
        return Err(one_diagnostic(arity_mismatch(
            span,
            expected,
            expected,
            args.len(),
        )));
    }

    match kind {
        StreamMethod::Map => Ok(Value::Stream(stream.map(args[0].clone()))),
        StreamMethod::Filter => Ok(Value::Stream(stream.filter(args[0].clone()))),
        StreamMethod::Fold => fold_stream(stream, args[0].clone(), args[1].clone(), span),
        StreamMethod::Each => {
            let callback = args[0].clone();
            let mut stream = stream;
            while let Some(value) = stream.next_value(span)? {
                apply_callee_values(
                    callback.clone(),
                    span,
                    vec![value],
                    NativeContext::without_source(span),
                )?;
            }
            Ok(Value::unit())
        }
        StreamMethod::ToArray => materialize_stream(stream, span),
    }
}

fn fold_stream(mut stream: Stream, mut accumulator: Value, callback: Value, span: Span) -> Eval {
    while let Some(value) = stream.next_value(span)? {
        accumulator = apply_callee_values(
            callback.clone(),
            span,
            vec![accumulator, value],
            NativeContext::without_source(span),
        )?;
    }
    Ok(accumulator)
}

fn apply_array_flat_map(items: Rc<Vec<Value>>, args: Vec<Value>, span: Span) -> Eval {
    let [callback] = args.as_slice() else {
        return Err(one_diagnostic(arity_mismatch(span, 1, 1, args.len())));
    };
    let mut values = Vec::new();
    for item in items.iter() {
        let part = apply_callee_values(
            callback.clone(),
            span,
            vec![item.clone()],
            NativeContext::without_source(span),
        )?;
        let Value::Array(part) = part else {
            return Err(one_diagnostic(array_flat_map_result_type_error(
                span,
                part.type_name(),
            )));
        };
        append_array(&mut values, &part, span)?;
    }
    Ok(Value::Array(Rc::new(values)))
}

fn apply_array_fold(items: Rc<Vec<Value>>, args: Vec<Value>, span: Span) -> Eval {
    let [initial, callback] = args.as_slice() else {
        return Err(one_diagnostic(arity_mismatch(span, 2, 2, args.len())));
    };
    let mut accumulator = initial.clone();
    for value in items.iter() {
        accumulator = apply_callee_values(
            callback.clone(),
            span,
            vec![accumulator, value.clone()],
            NativeContext::without_source(span),
        )?;
    }
    Ok(accumulator)
}

/// Folding walks insertion order, so `fold`, `each` and `toArray` all observe
/// the same sequence a set renders in.
fn apply_set_method(members: Rc<SetValue>, kind: SetMethod, args: Vec<Value>, span: Span) -> Eval {
    match kind {
        SetMethod::Fold => {
            let [initial, callback] = args.as_slice() else {
                return Err(one_diagnostic(arity_mismatch(span, 2, 2, args.len())));
            };
            let mut accumulator = initial.clone();
            for member in members.iter() {
                accumulator = apply_callee_values(
                    callback.clone(),
                    span,
                    vec![accumulator, member.clone()],
                    NativeContext::without_source(span),
                )?;
            }
            Ok(accumulator)
        }
        SetMethod::ToArray => {
            if !args.is_empty() {
                return Err(one_diagnostic(arity_mismatch(span, 0, 0, args.len())));
            }
            collect_into_array(CollectSource::Set(members), span)
        }
    }
}

fn materialize_stream(stream: Stream, span: Span) -> Eval {
    collect_into_array(CollectSource::Stream(stream), span)
}

/// The one materialization path. `[..source]`, `stream.toArray()` and
/// `Array.collect(source)` all reach it, so element order and the
/// materialization limit are the same fact for all three rather than three
/// facts that can drift apart.
fn collect_into_array(source: CollectSource, span: Span) -> Eval {
    let mut values = Vec::new();
    append_collection(&mut values, source, span)?;
    Ok(Value::Array(Rc::new(values)))
}

/// `Set.collect(source)`. Collecting a set adopts its trees outright rather
/// than re-inserting every member; anything else drains through the shared
/// append path first, so a set inherits the same materialization limit an
/// array gets.
fn collect_into_set(source: CollectSource, span: Span) -> Eval {
    if let CollectSource::Set(members) = source {
        for member in members.iter() {
            ensure_set_element(member, "Set.collect")
                .map_err(|message| one_diagnostic(platform_error(span, message)))?;
        }
        return Ok(Value::Set(members));
    }
    let mut values = Vec::new();
    append_collection(&mut values, source, span)?;
    let mut members = SetValue::default();
    for value in values {
        ensure_set_element(&value, "Set.collect")
            .map_err(|message| one_diagnostic(platform_error(span, message)))?;
        members.insert(value);
    }
    Ok(Value::Set(Rc::new(members)))
}

fn append_collection(values: &mut Vec<Value>, source: CollectSource, span: Span) -> Eval<()> {
    match source {
        CollectSource::Array(part) => append_array(values, &part, span),
        CollectSource::Set(members) => {
            append_exact(values, members.len(), members.iter().cloned(), span)
        }
        CollectSource::Stream(mut stream) => append_stream(values, &mut stream, span),
    }
}

fn append_stream(values: &mut Vec<Value>, stream: &mut Stream, span: Span) -> Eval<()> {
    let maximum_len = MAX_MATERIALIZED_ARRAY_BYTES / std::mem::size_of::<Value>();
    if let Some(additional) = stream.exact_remaining_len() {
        let total = values.len().checked_add(additional);
        if total.is_none_or(|total| total > maximum_len)
            || values.try_reserve_exact(additional).is_err()
        {
            return Err(one_diagnostic(collection_too_large(span)));
        }
    }
    while let Some(value) = stream.next_value(span)? {
        if values.len() >= maximum_len || values.try_reserve(1).is_err() {
            return Err(one_diagnostic(collection_too_large(span)));
        }
        values.push(value);
    }
    Ok(())
}

fn append_array(values: &mut Vec<Value>, part: &[Value], span: Span) -> Eval<()> {
    append_exact(values, part.len(), part.iter().cloned(), span)
}

/// Append a run of known length, holding the same materialization limit
/// [`append_stream`] enforces element by element.
fn append_exact(
    values: &mut Vec<Value>,
    additional: usize,
    part: impl Iterator<Item = Value>,
    span: Span,
) -> Eval<()> {
    let maximum_len = MAX_MATERIALIZED_ARRAY_BYTES / std::mem::size_of::<Value>();
    let total = values.len().checked_add(additional);
    if total.is_none_or(|total| total > maximum_len) || values.try_reserve(additional).is_err() {
        return Err(one_diagnostic(collection_too_large(span)));
    }
    values.extend(part);
    Ok(())
}

fn apply_native(function: NativeFn, arg_values: Vec<Value>, context: NativeContext) -> Eval {
    let span = context.span;
    function(&arg_values, context).map_err(|message| one_diagnostic(platform_error(span, message)))
}

fn apply_collect_constructor(args: Vec<Value>, target: CollectTarget, span: Span) -> Eval {
    let [source] = <[Value; 1]>::try_from(args)
        .map_err(|args| one_diagnostic(arity_mismatch(span, 1, 1, args.len())))?;
    let type_name = source.type_name();
    let Some(source) = CollectSource::of(source) else {
        return Err(one_diagnostic(record_type_error(
            span,
            "collect",
            type_name,
            "Stream, Array, or Set",
        )));
    };
    match target {
        CollectTarget::Array => collect_into_array(source, span),
        CollectTarget::Set => collect_into_set(source, span),
    }
}

fn apply_range_constructor(
    args: Vec<Value>,
    inclusive: bool,
    materialize: bool,
    span: Span,
) -> Eval {
    let (start, end, options) = match args.as_slice() {
        [start, end] => (start, end, None),
        [start, end, options] => (start, end, Some(options)),
        _ => return Err(one_diagnostic(arity_mismatch(span, 2, 3, args.len()))),
    };
    let Value::Int(start) = start else {
        return Err(one_diagnostic(range_bound_type_error(
            span,
            "start",
            start.type_name(),
        )));
    };
    let Value::Int(end) = end else {
        return Err(one_diagnostic(range_bound_type_error(
            span,
            "end",
            end.type_name(),
        )));
    };
    let step = match options {
        Some(options) => range_options_step(options, span)?,
        None => default_range_step(start, end),
    };
    range_value(
        start.clone(),
        end.clone(),
        step,
        inclusive,
        materialize,
        span,
    )
}

fn range_options_step(options: &Value, span: Span) -> Eval<Int> {
    let Value::Record(fields) = options else {
        return Err(one_diagnostic(range_options_type_error(
            span,
            options.type_name(),
        )));
    };
    if let Some((field, _)) = fields.iter().find(|(name, _)| name != "step") {
        return Err(one_diagnostic(range_unknown_option(span, field)));
    }
    let Some(step) = record_field_value(fields, "step") else {
        return Err(one_diagnostic(range_missing_step(span)));
    };
    let Value::Int(step) = step else {
        return Err(one_diagnostic(range_bound_type_error(
            span,
            "step",
            step.type_name(),
        )));
    };
    Ok(step.clone())
}

fn apply_closure(closure: Closure, args: &[Expr], span: Span, env: &Environment) -> Eval {
    let (required, total) = closure_arity(&closure);
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
    apply_closure_values(closure, arg_values, span)
}

fn apply_closure_values(closure: Closure, arg_values: Vec<Value>, span: Span) -> Eval {
    let (required, total) = closure_arity(&closure);
    let provided = arg_values.len();
    if provided < required || provided > total {
        return Err(one_diagnostic(arity_mismatch(
            span, required, total, provided,
        )));
    }

    bind_and_eval_closure(closure, arg_values, provided, span)
}

/// Call a value with already-evaluated arguments.
///
/// This is the public boundary counterpart of internal call evaluation: runtime
/// errors surface as `Err(diagnostics)` rather than the evaluator's private
/// [`Flow`] channel.
///
/// ## `Flow::Propagate` at this boundary
///
/// `?^` early-returns an `@Err` from the enclosing function as
/// [`Flow::Propagate`]. Ordinary closure application already converts that into
/// a successful return of the `@Err` value at the function body boundary (so a
/// zero-arg thunk that ends with `someResult?^` yields `@Err(...)` as its
/// return value, not a diagnostic).
///
/// If `Propagate` still reaches this public API — for example from a non-closure
/// callable that somehow produces it — it is treated the same way: the
/// propagated `@Err` is returned as `Ok(value)`. Turning it into a diagnostic
/// would reclassify intentional error returns as crashes; surfacing the value
/// matches what a normal function call does for its caller.
pub fn call_value(callee: &Value, args: Vec<Value>) -> Result<Value, Vec<Diagnostic>> {
    let span = Span::new(0, 0);
    match apply_callee_values(
        callee.clone(),
        span,
        args,
        NativeContext::without_source(span),
    ) {
        Ok(value) => Ok(value),
        Err(Flow::Fail(diagnostics)) => Err(diagnostics),
        Err(Flow::Propagate(value)) => Ok(*value),
    }
}

/// Required and total parameter counts when the value is callable and arity is
/// known statically. Closures report their declared parameters; other callables
/// return `None` for the counts (still callable via [`call_value`]). Non-callables
/// also return `None`.
pub fn callable_arity(value: &Value) -> Option<(usize, usize)> {
    match value {
        Value::Closure(closure) => Some(closure_arity(closure)),
        // Every other callable (natives, method values, named families) carries
        // no statically-known arity; see `is_callable` for the callable set.
        _ => None,
    }
}

/// Whether the value can be applied with [`call_value`].
pub fn is_callable(value: &Value) -> bool {
    matches!(
        value,
        Value::Closure(_)
            | Value::Native(_)
            | Value::ResultMethod { .. }
            | Value::NamedFamily(_)
            | Value::NamedMethod { .. }
            | Value::UnboundNamedMethod { .. }
    )
}

fn closure_arity(closure: &Closure) -> (usize, usize) {
    let total = closure.params.len();
    // Defaults are trailing, so the required count is the run of leading params
    // that have no default.
    let required = closure
        .params
        .iter()
        .take_while(|param| param.default.is_none())
        .count();
    (required, total)
}

/// Remaining stack space that triggers a fresh segment via `stacker::grow`.
/// This leaves headroom for evaluator work between instrumented call sites.
const STACK_RED_ZONE: usize = 256 * 1024;

/// Size of each new stack segment allocated by `stacker` when the red zone is hit.
///
/// Public so tooling can convert a byte budget into a segment count without
/// owning the guard semantics: `budget_bytes / STACK_SEGMENT_SIZE`.
pub const STACK_SEGMENT_SIZE: usize = 1024 * 1024;

/// Maximum memory committed to nested stacker segments during one call chain
/// when the caller sets no override. Exact Aven call depth remains
/// body-dependent; the memory backstop does not.
const STACK_GROW_BUDGET: usize = 64 * 1024 * 1024;

/// Default active-segment cap when [`EvalModuleOptions`] leaves the budget unset.
pub const DEFAULT_STACK_SEGMENT_LIMIT: usize = STACK_GROW_BUDGET / STACK_SEGMENT_SIZE;

thread_local! {
    static ACTIVE_STACK_SEGMENTS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
enum StackGrowth {
    System,
    #[cfg(test)]
    Fail,
}

impl StackGrowth {
    fn try_grow<R>(self, callback: impl FnOnce() -> R) -> Result<R, ()> {
        #[cfg(test)]
        if matches!(self, Self::Fail) {
            return Err(());
        }
        stacker::try_grow(STACK_SEGMENT_SIZE, callback).map_err(|_| ())
    }
}

/// Tracks stacker segments that remain live while a recursive call chain is
/// active. The guard enforces the configured policy limit; fallible stack
/// growth handles a host ceiling below that limit.
struct StackSegmentGuard;

impl StackSegmentGuard {
    fn enter(span: Span, limit: usize) -> Result<Self, Flow> {
        ACTIVE_STACK_SEGMENTS.with(|segments| {
            let current = segments.get();
            if current >= limit {
                return Err(one_diagnostic(recursion_limit(
                    span,
                    limit.saturating_mul(STACK_SEGMENT_SIZE),
                )));
            }
            segments.set(current + 1);
            Ok(Self)
        })
    }
}

impl Drop for StackSegmentGuard {
    fn drop(&mut self) {
        ACTIVE_STACK_SEGMENTS.with(|segments| segments.set(segments.get() - 1));
    }
}

fn bind_and_eval_closure(
    closure: Closure,
    arg_values: Vec<Value>,
    provided: usize,
    span: Span,
) -> Eval {
    let call_env = closure.env.child();
    for (param, value) in closure.params.iter().zip(arg_values) {
        call_env.bind(param.name.clone(), value);
    }
    // Bind each omitted trailing param by evaluating its default in `call_env`,
    // in order, so a later default may reference an earlier parameter. A default
    // runs only when its argument is omitted; failures propagate via `?`.
    for param in &closure.params[provided..] {
        let default = param
            .default
            .as_ref()
            .expect("omitted params past `required` always carry a default");
        let value = eval_expr_many(default, &call_env)?;
        call_env.bind(param.name.clone(), value);
    }

    let eval_body = || match eval_expr_many(closure.body.as_ref(), &call_env) {
        Err(Flow::Propagate(value)) => Ok(*value),
        other => other,
    };

    if stacker::remaining_stack().is_none_or(|remaining| remaining < STACK_RED_ZONE) {
        let _segment = StackSegmentGuard::enter(span, closure.env.stack_segment_limit)?;
        closure.env.stack_growth.try_grow(eval_body).map_err(|()| {
            one_diagnostic(recursion_limit(
                span,
                closure
                    .env
                    .stack_segment_limit
                    .saturating_mul(STACK_SEGMENT_SIZE),
            ))
        })?
    } else {
        eval_body()
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
            PropagationMode::ReturnError => Err(Flow::Propagate(Box::new(result))),
            // `?!` panics, embedding the `@Err` payload in the diagnostic.
            PropagationMode::Panic => Err(one_diagnostic(panic(operator_span, &payload[0]))),
        },
        _ => Err(one_diagnostic(propagate_type_error(operator_span))),
    }
}

fn eval_array(entries: &[RecordEntry], env: &Environment) -> Eval {
    let mut values = Vec::new();

    for entry in entries {
        match entry {
            RecordEntry::Element(expr) => {
                values.push(eval_expr_many(expr, env)?);
            }
            RecordEntry::Spread {
                value: source_expr, ..
            } => {
                let source = eval_expr_many(source_expr, env)?;
                let type_name = source.type_name();
                // An array literal spreads sequences only; a set reaches an
                // array through `Set` -> `Array` collection, which says so.
                match CollectSource::of(source) {
                    Some(source @ (CollectSource::Array(_) | CollectSource::Stream(_))) => {
                        append_collection(&mut values, source, source_expr.span)?;
                    }
                    _ => {
                        return Err(one_diagnostic(record_type_error(
                            source_expr.span,
                            "spread",
                            type_name,
                            "Array or Stream",
                        )));
                    }
                }
            }
            entry => {
                return Err(one_diagnostic(unsupported_expr(
                    record_entry_span(entry),
                    "array literals accept only elements and spreads; remove or rewrite this entry",
                )));
            }
        }
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
    // Held back until the first entry lands so that a leading spread can adopt
    // its source's trees outright. `@{..s, x}` then shares all of `s` and pays
    // for a single insertion rather than re-inserting every member, which is
    // what makes folding a set over a sequence linearithmic instead of cubic.
    let mut members: Option<SetValue> = None;

    for entry in entries {
        match entry {
            RecordEntry::Element(expr) => {
                let value = eval_expr_many(expr, env)?;
                ensure_set_element(&value, "Set")
                    .map_err(|message| one_diagnostic(platform_error(expr.span, message)))?;
                members.get_or_insert_default().insert(value);
            }
            RecordEntry::Spread {
                value: source_expr, ..
            } => {
                let source = eval_expr_many(source_expr, env)?;
                let Value::Set(source_members) = source else {
                    return Err(one_diagnostic(record_type_error(
                        source_expr.span,
                        "spread",
                        source.type_name(),
                        "Set",
                    )));
                };

                match &mut members {
                    Some(members) => members.extend(source_members.iter().cloned()),
                    None => members = Some(Rc::unwrap_or_clone(source_members)),
                }
            }
            entry => {
                return Err(one_diagnostic(unsupported_expr(
                    record_entry_span(entry),
                    "set literals accept only elements and spreads; remove or rewrite this entry",
                )));
            }
        }
    }

    Ok(Value::Set(Rc::new(members.unwrap_or_default())))
}

fn eval_record(entries: &[RecordEntry], env: &Environment) -> Eval {
    let mut fields = Vec::new();

    for entry in entries {
        fold_record_entry(&mut fields, entry, env)?;
    }

    Ok(Value::Record(Rc::new(fields)))
}

/// Whether a binding value is a closed slot-record type alias: a record with
/// at least one bodyless arrow method (`name(): T`) and no method bodies. Such
/// a declaration defines a structural slot-record type and is not evaluated.
fn is_slot_record_type_alias(value: &Expr) -> bool {
    let mut value = value;
    while let ExprKind::Group(inner) = &value.kind {
        value = inner;
    }
    let ExprKind::Record(entries) = &value.kind else {
        return false;
    };
    let mut has_arrow_slot = false;
    for entry in entries {
        match entry {
            RecordEntry::Method { value, .. } => match &value.kind {
                ExprKind::Arrow { .. } => has_arrow_slot = true,
                _ => return false,
            },
            RecordEntry::Field { .. } | RecordEntry::Open { .. } => {}
            _ => return false,
        }
    }
    has_arrow_slot
}

/// Build a `SlotRecord` directly from an initializer literal. Data fields are
/// evaluated to stored data; each method slot becomes a bound method closure
/// whose hidden receiver is the constructed data record (matching the shape a
/// reified `NamedRecord` produces) and which captures the lexical environment.
fn eval_direct_slot_init(entries: &[RecordEntry], env: &Environment) -> Eval {
    let mut fields = Vec::new();
    for entry in entries {
        if let RecordEntry::Field { name, value, .. } = entry {
            let value = eval_expr_many(value, env)?;
            insert_or_replace_field(&mut fields, name.clone(), value);
        }
    }
    // The hidden receiver is the record's data fields: bare `.field` reads in a
    // slot body resolve against this snapshot, exactly as target-declared data
    // fields do for a reified value.
    let receiver = Value::Record(Rc::new(fields.clone()));

    let mut slots = Vec::new();
    for entry in entries {
        let RecordEntry::Method { name, value, .. } = entry else {
            continue;
        };
        let ExprKind::Lambda { params, body, .. } = &value.kind else {
            return Err(one_diagnostic(unsupported_expr(
                value.span,
                "a slot initializer method requires an implementation body",
            )));
        };
        let mut closure_params = Vec::with_capacity(params.len() + 1);
        closure_params.push(ClosureParam {
            name: aven_parser::METHOD_RECEIVER_NAME.to_owned(),
            default: None,
        });
        closure_params.extend(params.iter().map(|param| ClosureParam {
            name: param.name.clone(),
            default: param.default.clone().map(Rc::new),
        }));
        let implementation = NamedMethodImplementation::Declared(Closure {
            params: closure_params,
            body: Rc::new((**body).clone()),
            env: env.clone(),
        });
        slots.push((
            name.clone(),
            Value::NamedMethod {
                receiver: Box::new(receiver.clone()),
                member: name.clone(),
                implementation,
            },
        ));
    }

    Ok(Value::SlotRecord {
        fields: Rc::new(fields),
        slots: Rc::new(slots),
    })
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
        RecordEntry::Method { .. } | RecordEntry::FieldDefault { .. } => {
            return Err(one_diagnostic(record_type_error(
                record_entry_span(entry),
                "record construction",
                "type member",
                "value record entry",
            )));
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
                Value::Record(fields) | Value::NamedRecord { fields, .. } => fields,
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
        Value::Set(members) => members.iter().cloned().collect(),
        Value::Array(items) => items.iter().cloned().collect(),
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

    field_access_value(receiver_value, receiver.span, field, field_span, env)
}

/// Read `field` off an already-evaluated receiver value.
fn field_access_value(
    receiver_value: Value,
    receiver_span: Span,
    field: &str,
    field_span: Span,
    env: &Environment,
) -> Eval {
    match &receiver_value {
        // Optional record fields can be omitted physically at runtime. Reads
        // treat an absent key as `undefined`; record transforms keep their
        // stricter, separate missing-field checks.
        Value::Record(fields) => Ok(record_field_value(fields, field)
            .cloned()
            .or_else(|| ambient_record_method(&receiver_value, field, env))
            .unwrap_or(Value::Undefined)),
        Value::SlotRecord { fields, slots } => Ok(record_field_value(fields, field)
            .or_else(|| record_field_value(slots, field))
            .cloned()
            .unwrap_or(Value::Undefined)),
        Value::NamedRecord { descriptor, fields } => {
            if let Some(value) = record_field_value(fields, field) {
                return Ok(value.clone());
            }
            descriptor.methods.get(field).cloned().map_or_else(
                || {
                    Ok(ambient_record_method(&receiver_value, field, env)
                        .unwrap_or(Value::Undefined))
                },
                |implementation| {
                    Ok(Value::NamedMethod {
                        receiver: Box::new(receiver_value.clone()),
                        member: field.to_owned(),
                        implementation,
                    })
                },
            )
        }
        Value::BrandedPrimitive { descriptor, .. } => {
            descriptor.methods.get(field).cloned().map_or_else(
                || {
                    Ok(ambient_record_method(&receiver_value, field, env)
                        .unwrap_or(Value::Undefined))
                },
                |implementation| {
                    Ok(Value::NamedMethod {
                        receiver: Box::new(receiver_value.clone()),
                        member: field.to_owned(),
                        implementation,
                    })
                },
            )
        }
        Value::NamedFamily(descriptor) => descriptor.methods.get(field).cloned().map_or_else(
            || {
                Err(one_diagnostic(missing_type_member(
                    &descriptor.owner,
                    field,
                    field_span,
                )))
            },
            |implementation| {
                Ok(Value::UnboundNamedMethod {
                    descriptor: Rc::clone(descriptor),
                    member: field.to_owned(),
                    implementation,
                })
            },
        ),
        // A type value (`Map`, `Json`, ...) carries statics: field access
        // resolves the `"Type.static"`-keyed global bound alongside the type.
        // Concrete scalar builtins also publish unbound methods (`Int.+`,
        // `Int.div`) as first-class values for base delegation.
        Value::Type(ty) => {
            if let Some(owner) = runtime_type_static_owner(ty.descriptor()) {
                let member = env
                    .lookup(&format!("{owner}.{field}"))
                    .or_else(|| unbound_builtin_type_method(owner, field))
                    .ok_or_else(|| {
                        one_diagnostic(missing_type_member(&ty.to_string(), field, field_span))
                    })?;
                Ok(member)
            } else {
                match ty.descriptor() {
                    RuntimeTypeDescriptor::Record(fields) => fields
                        .iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, field_ty)| Value::Type(ty.with_descriptor(field_ty.clone())))
                        .ok_or_else(|| {
                            one_diagnostic(missing_type_member(&ty.to_string(), field, field_span))
                        }),
                    _ => Err(one_diagnostic(missing_type_member(
                        &ty.to_string(),
                        field,
                        field_span,
                    ))),
                }
            }
        }
        value => builtin_method(value, field, env).ok_or_else(|| {
            one_diagnostic(record_type_error(
                receiver_span,
                "field access",
                value.type_name(),
                "Record",
            ))
        }),
    }
}

fn value_carries_member(value: &Value, field: &str, env: &Environment) -> bool {
    match value {
        Value::Record(fields) => {
            record_field_value(fields, field).is_some()
                || ambient_record_method(value, field, env).is_some()
        }
        Value::SlotRecord { fields, slots } => {
            record_field_value(fields, field).is_some()
                || record_field_value(slots, field).is_some()
        }
        Value::NamedRecord { descriptor, fields } => {
            record_field_value(fields, field).is_some()
                || descriptor.methods.contains_key(field)
                || ambient_record_method(value, field, env).is_some()
        }
        Value::BrandedPrimitive { descriptor, .. } => {
            descriptor.methods.contains_key(field)
                || ambient_record_method(value, field, env).is_some()
        }
        Value::NamedFamily(descriptor) => descriptor.methods.contains_key(field),
        Value::Type(ty) => match ty.descriptor() {
            RuntimeTypeDescriptor::Named(name) => {
                env.lookup(&format!("{name}.{field}")).is_some()
                    || unbound_builtin_type_method(name, field).is_some()
            }
            RuntimeTypeDescriptor::Record(fields) => fields.iter().any(|(name, _)| name == field),
            _ => false,
        },
        value => builtin_method(value, field, env).is_some(),
    }
}

fn runtime_type_static_owner(descriptor: &RuntimeTypeDescriptor) -> Option<&str> {
    match descriptor {
        RuntimeTypeDescriptor::Named(name) => Some(name),
        RuntimeTypeDescriptor::Apply { callee, .. } => runtime_type_static_owner(callee),
        RuntimeTypeDescriptor::Function { .. }
        | RuntimeTypeDescriptor::Optional(_)
        | RuntimeTypeDescriptor::Nullable(_)
        | RuntimeTypeDescriptor::Tuple(_)
        | RuntimeTypeDescriptor::Record(_)
        | RuntimeTypeDescriptor::SlotRecord { .. }
        | RuntimeTypeDescriptor::Variant(_)
        | RuntimeTypeDescriptor::Recursive { .. } => None,
    }
}

/// Ambient methods readable off record-shaped receivers whose declared-member
/// lookup missed. Only `toText`: plain `.` reads of other absent fields keep
/// their `undefined` semantics.
fn ambient_record_method(receiver: &Value, field: &str, env: &Environment) -> Option<Value> {
    (field == "toText")
        .then(|| builtin_method(receiver, field, env))
        .flatten()
}

/// Unbound method value for a concrete scalar builtin owner (`Int.+`,
/// `Float.isFinite`). Takes the explicit receiver as the first argument.
fn unbound_builtin_type_method(owner: &str, field: &str) -> Option<Value> {
    match (owner, field) {
        ("Int" | "Float" | "Text" | "Bool", "toText") => Some(unbound_to_text_method()),
        ("Int", "+") => Some(unbound_binary_operator("+")),
        ("Int", "-") => Some(unbound_binary_operator("-")),
        ("Int", "*") => Some(unbound_binary_operator("*")),
        ("Int", "/") => Some(unbound_binary_operator("/")),
        ("Int", "%") => Some(unbound_binary_operator("%")),
        ("Int", "^") => Some(unbound_binary_operator("^")),
        ("Int", "<") => Some(unbound_binary_operator("<")),
        ("Int", "<=") => Some(unbound_binary_operator("<=")),
        ("Int", ">") => Some(unbound_binary_operator(">")),
        ("Int", ">=") => Some(unbound_binary_operator(">=")),
        ("Int", "div") => Some(unbound_int_division_method("div")),
        ("Int", "mod") => Some(unbound_int_division_method("mod")),
        ("Int", "toGrouped") => Some(unbound_int_to_grouped_method()),
        ("Int", "abs") => Some(unbound_int_nullary_method("abs", Int::abs)),
        ("Int", "min") => Some(unbound_int_binary_method("min", int_min)),
        ("Int", "max") => Some(unbound_int_binary_method("max", int_max)),
        ("Int", "clamp") => Some(unbound_int_clamp_method()),
        ("Int", "pow") => Some(unbound_int_pow_method()),
        ("Int", "sign") => Some(unbound_int_nullary_method("sign", Int::signum)),
        ("Int", "toFloat") => Some(unbound_int_to_float_method()),
        ("Float", "+") => Some(unbound_binary_operator("+")),
        ("Float", "-") => Some(unbound_binary_operator("-")),
        ("Float", "*") => Some(unbound_binary_operator("*")),
        ("Float", "/") => Some(unbound_binary_operator("/")),
        ("Float", "%") => Some(unbound_binary_operator("%")),
        ("Float", "^") => Some(unbound_binary_operator("^")),
        ("Float", "<") => Some(unbound_binary_operator("<")),
        ("Float", "<=") => Some(unbound_binary_operator("<=")),
        ("Float", ">") => Some(unbound_binary_operator(">")),
        ("Float", ">=") => Some(unbound_binary_operator(">=")),
        ("Float", "isFinite") => Some(unbound_float_nullary_method("isFinite", f64::is_finite)),
        ("Float", "isNaN") => Some(unbound_float_nullary_method("isNaN", f64::is_nan)),
        ("Float", "isInfinite") => {
            Some(unbound_float_nullary_method("isInfinite", f64::is_infinite))
        }
        ("Float", "ieeeEquals") => Some(unbound_float_ieee_equals_method()),
        ("Float", "toFixed") => Some(unbound_float_to_fixed_method()),
        ("Float", "abs") => Some(unbound_float_nullary_float("abs", f64::abs)),
        ("Float", "min") => Some(unbound_float_binary_method("min", f64::min)),
        ("Float", "max") => Some(unbound_float_binary_method("max", f64::max)),
        ("Float", "clamp") => Some(unbound_float_clamp_method()),
        ("Float", "pow") => Some(unbound_float_binary_method("pow", f64::powf)),
        ("Float", "round") => Some(unbound_float_nullary_float("round", f64::round)),
        ("Float", "floor") => Some(unbound_float_nullary_float("floor", f64::floor)),
        ("Float", "ceil") => Some(unbound_float_nullary_float("ceil", f64::ceil)),
        ("Float", "truncate") => Some(unbound_float_nullary_float("truncate", f64::trunc)),
        ("Float", "sqrt") => Some(unbound_float_nullary_float("sqrt", f64::sqrt)),
        ("Text", "+") => Some(unbound_binary_operator("+")),
        _ => None,
    }
}

/// Unbound `Owner.toText` for scalar builtins: base-view rendering, so a
/// family method can delegate (`Int.toText(.)`) without re-entering its own
/// override.
fn unbound_to_text_method() -> Value {
    Value::native(|args| {
        let [receiver] = args else {
            return Err(format!(
                "unbound toText expects 1 argument, got {}",
                args.len()
            ));
        };
        let receiver = erase_primitive_brand(receiver.clone());
        match display::to_text(&receiver, None, Span::new(0, 0)) {
            Ok(text) => Ok(Value::Text(text)),
            Err(flow) => Err(first_diagnostic(flow).message),
        }
    })
}

fn unbound_binary_operator(operator: &'static str) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound `{operator}` expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left = erase_primitive_brand(args[0].clone());
        let right = erase_primitive_brand(args[1].clone());
        let span = Span::new(0, 0);
        match apply_binary(left, operator, span, right, span, span) {
            Ok(value) => Ok(value),
            Err(Flow::Fail(diagnostics)) => Err(diagnostics
                .into_iter()
                .next()
                .map(|diagnostic| diagnostic.message)
                .unwrap_or_else(|| format!("unbound `{operator}` failed"))),
            Err(Flow::Propagate(value)) => Ok(*value),
        }
    })
}

fn unbound_int_division_method(name: &'static str) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Int.{name} expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left = erase_primitive_brand(args[0].clone());
        let right = erase_primitive_brand(args[1].clone());
        let Value::Int(left) = left else {
            return Err(format!(
                "unbound Int.{name} expects Int receiver, got {}",
                left.type_name()
            ));
        };
        let Value::Int(right) = right else {
            return Err(format!(
                "unbound Int.{name} expects Int, got {}",
                right.type_name()
            ));
        };
        if right.is_zero() {
            return Ok(Value::Undefined);
        }
        Ok(Value::Int(int_division(name, &left, &right)))
    })
}

fn unbound_float_nullary_method(name: &'static str, predicate: fn(f64) -> bool) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "unbound Float.{name} expects 1 argument, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Float(value) = receiver else {
            return Err(format!(
                "unbound Float.{name} expects Float receiver, got {}",
                receiver.type_name()
            ));
        };
        Ok(Value::Bool(predicate(value)))
    })
}

fn unbound_float_ieee_equals_method() -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Float.ieeeEquals expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left = erase_primitive_brand(args[0].clone());
        let right = erase_primitive_brand(args[1].clone());
        let Value::Float(left) = left else {
            return Err(format!(
                "unbound Float.ieeeEquals expects Float receiver, got {}",
                left.type_name()
            ));
        };
        let Value::Float(right) = right else {
            return Err(format!(
                "unbound Float.ieeeEquals expects Float, got {}",
                right.type_name()
            ));
        };
        Ok(Value::Bool(left == right))
    })
}

fn builtin_method(receiver: &Value, field: &str, env: &Environment) -> Option<Value> {
    if let (Value::Array(items), "flatMap") = (receiver, field) {
        return Some(Value::ArrayFlatMapMethod(Rc::clone(items)));
    }
    if let Some(implementation) = env.builtin_methods.lookup(receiver, field) {
        return Some(Value::NamedMethod {
            receiver: Box::new(receiver.clone()),
            member: field.to_owned(),
            implementation: NamedMethodImplementation::Declared(implementation),
        });
    }
    match (receiver, field) {
        (receiver, "toResult") => Some(optional_to_result_method(receiver.clone())),
        (receiver, "toText") if display::carries_ambient_to_text(receiver) => Some(
            ambient_to_text_method(receiver.clone(), env.builtin_methods.clone()),
        ),
        (Value::Set(members), "has") => Some(set_has_method(Rc::clone(members))),
        (Value::Set(members), "size") => Some(set_size_method(Rc::clone(members))),
        (Value::Set(members), "add") => Some(set_add_method(Rc::clone(members))),
        (Value::Set(members), "delete") => Some(set_delete_method(Rc::clone(members))),
        (Value::Set(members), "union") => Some(set_union_method(Rc::clone(members))),
        (Value::Set(members), "intersection") => Some(set_intersection_method(Rc::clone(members))),
        (Value::Set(members), "difference") => Some(set_difference_method(Rc::clone(members))),
        (Value::Set(members), "isDisjoint") => Some(set_is_disjoint_method(Rc::clone(members))),
        (Value::Set(members), "fold") => Some(Value::SetMethod {
            receiver: Rc::clone(members),
            kind: SetMethod::Fold,
        }),
        (Value::Set(members), "toArray") => Some(Value::SetMethod {
            receiver: Rc::clone(members),
            kind: SetMethod::ToArray,
        }),
        (Value::Array(items), "has") => Some(array_has_method(Rc::clone(items))),
        (Value::Array(items), "length") => Some(array_length_method(Rc::clone(items))),
        (Value::Array(items), "push") => Some(array_push_method(Rc::clone(items))),
        (Value::Array(items), "fold") => Some(Value::ArrayFoldMethod(Rc::clone(items))),
        (Value::Array(items), "joinWith") => Some(array_join_with_method(Rc::clone(items))),
        (Value::Stream(stream), field) => {
            let kind = match field {
                "map" => StreamMethod::Map,
                "filter" => StreamMethod::Filter,
                "fold" => StreamMethod::Fold,
                "each" => StreamMethod::Each,
                "toArray" => StreamMethod::ToArray,
                _ => return None,
            };
            Some(Value::StreamMethod {
                receiver: Box::new(stream.clone()),
                kind,
            })
        }
        (Value::Map(entries), "get") => Some(map_get_method(Rc::clone(entries))),
        (Value::Map(entries), "set") => Some(map_set_method(Rc::clone(entries))),
        (Value::Map(entries), "delete") => Some(map_delete_method(Rc::clone(entries))),
        (Value::Map(entries), "has") => Some(map_has_method(Rc::clone(entries))),
        (Value::Map(entries), "keys") => Some(map_keys_method(Rc::clone(entries))),
        (Value::Map(entries), "values") => Some(map_values_method(Rc::clone(entries))),
        (Value::Map(entries), "entries") => Some(map_entries_method(Rc::clone(entries))),
        (Value::Map(entries), "size") => Some(map_size_method(Rc::clone(entries))),
        (Value::Map(entries), "merge") => Some(map_merge_method(Rc::clone(entries))),
        (Value::Int(value), "div") => Some(int_division_method(value.clone(), "div")),
        (Value::Int(value), "mod") => Some(int_division_method(value.clone(), "mod")),
        (Value::Int(value), "toGrouped") => Some(int_to_grouped_method(value.clone())),
        (Value::Int(value), "abs") => Some(int_nullary_method(value.clone(), "abs", Int::abs)),
        (Value::Int(value), "min") => Some(int_binary_method(value.clone(), "min", int_min)),
        (Value::Int(value), "max") => Some(int_binary_method(value.clone(), "max", int_max)),
        (Value::Int(value), "clamp") => Some(int_clamp_method(value.clone())),
        (Value::Int(value), "pow") => Some(int_pow_method(value.clone())),
        (Value::Int(value), "sign") => Some(int_nullary_method(value.clone(), "sign", Int::signum)),
        (Value::Int(value), "toFloat") => Some(int_to_float_method(value.clone())),
        (Value::Float(value), "isFinite") => {
            Some(float_nullary_bool(*value, "isFinite", f64::is_finite))
        }
        (Value::Float(value), "isNaN") => Some(float_nullary_bool(*value, "isNaN", f64::is_nan)),
        (Value::Float(value), "isInfinite") => {
            Some(float_nullary_bool(*value, "isInfinite", f64::is_infinite))
        }
        (Value::Float(value), "ieeeEquals") => Some(float_ieee_equals_method(*value)),
        (Value::Float(value), "toFixed") => Some(float_to_fixed_method(*value)),
        (Value::Float(value), "abs") => Some(float_nullary_float(*value, "abs", f64::abs)),
        (Value::Float(value), "min") => Some(float_binary_method(*value, "min", f64::min)),
        (Value::Float(value), "max") => Some(float_binary_method(*value, "max", f64::max)),
        (Value::Float(value), "clamp") => Some(float_clamp_method(*value)),
        (Value::Float(value), "pow") => Some(float_binary_method(*value, "pow", f64::powf)),
        (Value::Float(value), "round") => Some(float_nullary_float(*value, "round", f64::round)),
        (Value::Float(value), "floor") => Some(float_nullary_float(*value, "floor", f64::floor)),
        (Value::Float(value), "ceil") => Some(float_nullary_float(*value, "ceil", f64::ceil)),
        (Value::Float(value), "truncate") => {
            Some(float_nullary_float(*value, "truncate", f64::trunc))
        }
        (Value::Float(value), "sqrt") => Some(float_nullary_float(*value, "sqrt", f64::sqrt)),
        (Value::Text(text), field) => text_method(text, field),
        (
            Value::Tag { name, payload },
            "mapErr" | "orElse" | "map" | "andThen" | "unwrapOr" | "isOk" | "isErr",
        ) if matches!(name.as_str(), "Ok" | "Err") && payload.len() == 1 => {
            let kind = match field {
                "mapErr" => ResultMethod::MapErr,
                "orElse" => ResultMethod::OrElse,
                "map" => ResultMethod::Map,
                "andThen" => ResultMethod::AndThen,
                "unwrapOr" => ResultMethod::UnwrapOr,
                "isOk" => ResultMethod::IsOk,
                "isErr" => ResultMethod::IsErr,
                _ => unreachable!("matched result method names"),
            };
            Some(Value::ResultMethod {
                receiver: Box::new(receiver.clone()),
                kind,
            })
        }
        _ => None,
    }
}

fn int_division_method(left: Int, name: &'static str) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Int.{name} expects 1 argument, got {}", args.len()));
        }
        let Value::Int(right) = &args[0] else {
            return Err(format!(
                "Int.{name} expects Int, got {}",
                args[0].type_name()
            ));
        };
        if right.is_zero() {
            return Ok(Value::Undefined);
        }
        Ok(Value::Int(int_division(name, &left, right)))
    })
}

fn int_division(name: &str, left: &Int, right: &Int) -> Int {
    if name == "div" {
        left / right
    } else {
        left % right
    }
}

fn float_nullary_bool(value: f64, name: &'static str, predicate: fn(f64) -> bool) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Float.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Bool(predicate(value)))
    })
}

fn float_ieee_equals_method(left: f64) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Float.ieeeEquals expects 1 argument, got {}",
                args.len()
            ));
        }
        let Value::Float(right) = &args[0] else {
            return Err(format!(
                "Float.ieeeEquals expects Float, got {}",
                args[0].type_name()
            ));
        };
        Ok(Value::Bool(left == *right))
    })
}

fn int_nullary_method(value: Int, name: &'static str, f: fn(&Int) -> Int) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Int.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Int(f(&value)))
    })
}

fn int_binary_method(left: Int, name: &'static str, f: fn(&Int, &Int) -> Int) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Int.{name} expects 1 argument, got {}", args.len()));
        }
        let Value::Int(right) = &args[0] else {
            return Err(format!(
                "Int.{name} expects Int, got {}",
                args[0].type_name()
            ));
        };
        Ok(Value::Int(f(&left, right)))
    })
}

/// Total clamp. When `min > max`, returns `min` (easy-to-revisit choice).
fn int_clamp(value: &Int, min: &Int, max: &Int) -> Int {
    if min > max {
        min.clone()
    } else {
        value.clamp(min, max).clone()
    }
}

fn int_min(left: &Int, right: &Int) -> Int {
    left.min(right).clone()
}

fn int_max(left: &Int, right: &Int) -> Int {
    left.max(right).clone()
}

fn int_clamp_method(value: Int) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!("Int.clamp expects 2 arguments, got {}", args.len()));
        }
        let Value::Int(min) = &args[0] else {
            return Err(format!(
                "Int.clamp expects Int min, got {}",
                args[0].type_name()
            ));
        };
        let Value::Int(max) = &args[1] else {
            return Err(format!(
                "Int.clamp expects Int max, got {}",
                args[1].type_name()
            ));
        };
        Ok(Value::Int(int_clamp(&value, min, max)))
    })
}

/// Negative exponents clamp to 0 (result `1`).
fn int_pow(base: &Int, exponent: &Int) -> Result<Int, String> {
    let exponent = if exponent.is_negative() {
        0
    } else if let Some(exponent) = exponent.to_u32() {
        exponent
    } else {
        return Err("Int.pow exponent is too large".to_owned());
    };
    Ok(base.pow(exponent))
}

fn int_pow_method(base: Int) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Int.pow expects 1 argument, got {}", args.len()));
        }
        let Value::Int(exponent) = &args[0] else {
            return Err(format!("Int.pow expects Int, got {}", args[0].type_name()));
        };
        int_pow(&base, exponent).map(Value::Int)
    })
}

fn int_to_float_method(value: Int) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Int.toFloat expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Float(int_to_f64(&value)))
    })
}

fn unbound_int_nullary_method(name: &'static str, f: fn(&Int) -> Int) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "unbound Int.{name} expects 1 argument, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Int(value) = receiver else {
            return Err(format!(
                "unbound Int.{name} expects Int receiver, got {}",
                receiver.type_name()
            ));
        };
        Ok(Value::Int(f(&value)))
    })
}

fn unbound_int_binary_method(name: &'static str, f: fn(&Int, &Int) -> Int) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Int.{name} expects 2 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Int(left) = receiver else {
            return Err(format!(
                "unbound Int.{name} expects Int receiver, got {}",
                receiver.type_name()
            ));
        };
        let Value::Int(right) = &args[1] else {
            return Err(format!(
                "unbound Int.{name} expects Int, got {}",
                args[1].type_name()
            ));
        };
        Ok(Value::Int(f(&left, right)))
    })
}

fn unbound_int_clamp_method() -> Value {
    Value::native(move |args| {
        if args.len() != 3 {
            return Err(format!(
                "unbound Int.clamp expects 3 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Int(value) = receiver else {
            return Err(format!(
                "unbound Int.clamp expects Int receiver, got {}",
                receiver.type_name()
            ));
        };
        let Value::Int(min) = &args[1] else {
            return Err(format!(
                "unbound Int.clamp expects Int min, got {}",
                args[1].type_name()
            ));
        };
        let Value::Int(max) = &args[2] else {
            return Err(format!(
                "unbound Int.clamp expects Int max, got {}",
                args[2].type_name()
            ));
        };
        Ok(Value::Int(int_clamp(&value, min, max)))
    })
}

fn unbound_int_pow_method() -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Int.pow expects 2 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Int(base) = receiver else {
            return Err(format!(
                "unbound Int.pow expects Int receiver, got {}",
                receiver.type_name()
            ));
        };
        let Value::Int(exponent) = &args[1] else {
            return Err(format!(
                "unbound Int.pow expects Int, got {}",
                args[1].type_name()
            ));
        };
        int_pow(&base, exponent).map(Value::Int)
    })
}

fn unbound_int_to_float_method() -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "unbound Int.toFloat expects 1 argument, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Int(value) = receiver else {
            return Err(format!(
                "unbound Int.toFloat expects Int receiver, got {}",
                receiver.type_name()
            ));
        };
        Ok(Value::Float(int_to_f64(&value)))
    })
}

fn float_nullary_float(value: f64, name: &'static str, f: fn(f64) -> f64) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Float.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Float(f(value)))
    })
}

fn float_binary_method(left: f64, name: &'static str, f: fn(f64, f64) -> f64) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Float.{name} expects 1 argument, got {}",
                args.len()
            ));
        }
        let Value::Float(right) = &args[0] else {
            return Err(format!(
                "Float.{name} expects Float, got {}",
                args[0].type_name()
            ));
        };
        Ok(Value::Float(f(left, *right)))
    })
}

/// Total clamp. When `min > max`, returns `min`. NaN receiver is preserved.
/// Bounds use `f64::max`/`f64::min` (non-NaN wins if one bound is NaN).
fn float_clamp(value: f64, min: f64, max: f64) -> f64 {
    if value.is_nan() {
        return value;
    }
    if min > max {
        return min;
    }
    value.max(min).min(max)
}

fn float_clamp_method(value: f64) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "Float.clamp expects 2 arguments, got {}",
                args.len()
            ));
        }
        let Value::Float(min) = &args[0] else {
            return Err(format!(
                "Float.clamp expects Float min, got {}",
                args[0].type_name()
            ));
        };
        let Value::Float(max) = &args[1] else {
            return Err(format!(
                "Float.clamp expects Float max, got {}",
                args[1].type_name()
            ));
        };
        Ok(Value::Float(float_clamp(value, *min, *max)))
    })
}

fn unbound_float_nullary_float(name: &'static str, f: fn(f64) -> f64) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "unbound Float.{name} expects 1 argument, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Float(value) = receiver else {
            return Err(format!(
                "unbound Float.{name} expects Float receiver, got {}",
                receiver.type_name()
            ));
        };
        Ok(Value::Float(f(value)))
    })
}

fn unbound_float_binary_method(name: &'static str, f: fn(f64, f64) -> f64) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Float.{name} expects 2 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Float(left) = receiver else {
            return Err(format!(
                "unbound Float.{name} expects Float receiver, got {}",
                receiver.type_name()
            ));
        };
        let Value::Float(right) = &args[1] else {
            return Err(format!(
                "unbound Float.{name} expects Float, got {}",
                args[1].type_name()
            ));
        };
        Ok(Value::Float(f(left, *right)))
    })
}

fn unbound_float_clamp_method() -> Value {
    Value::native(move |args| {
        if args.len() != 3 {
            return Err(format!(
                "unbound Float.clamp expects 3 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Float(value) = receiver else {
            return Err(format!(
                "unbound Float.clamp expects Float receiver, got {}",
                receiver.type_name()
            ));
        };
        let Value::Float(min) = &args[1] else {
            return Err(format!(
                "unbound Float.clamp expects Float min, got {}",
                args[1].type_name()
            ));
        };
        let Value::Float(max) = &args[2] else {
            return Err(format!(
                "unbound Float.clamp expects Float max, got {}",
                args[2].type_name()
            ));
        };
        Ok(Value::Float(float_clamp(value, *min, *max)))
    })
}

/// The ambient `toText` method: renders the receiver with the display
/// protocol, so container elements observe family overrides and attachments.
fn ambient_to_text_method(receiver: Value, attachments: BuiltinMethodEnvironment) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("toText expects 0 arguments, got {}", args.len()));
        }
        match display::to_text(&receiver, Some(&attachments), Span::new(0, 0)) {
            Ok(text) => Ok(Value::Text(text)),
            Err(flow) => Err(first_diagnostic(flow).message),
        }
    })
}

fn optional_to_result_method(receiver: Value) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("toResult expects 1 argument, got {}", args.len()));
        }
        let (name, value) = if matches!(receiver, Value::Undefined | Value::Null) {
            ("Err", args[0].clone())
        } else {
            ("Ok", receiver.clone())
        };
        Ok(Value::Tag {
            name: name.to_owned(),
            payload: vec![value],
        })
    })
}

fn text_method(text: &str, field: &str) -> Option<Value> {
    let text = text.to_owned();
    match field {
        "isEmpty" => Some(text_nullary_bool(text, "isEmpty", |s| s.is_empty())),
        "length" => Some(text_nullary_int(text, "length", |s| {
            Ok(Int::from(s.chars().count()))
        })),
        "chars" => Some(text_nullary_value(text, "chars", |s| {
            Value::Array(Rc::new(
                s.chars().map(|c| Value::Text(c.to_string())).collect(),
            ))
        })),
        "contains" => Some(text_predicate_method(text, "contains", |s, needle| {
            s.contains(needle)
        })),
        "startsWith" => Some(text_predicate_method(text, "startsWith", |s, prefix| {
            s.starts_with(prefix)
        })),
        "endsWith" => Some(text_predicate_method(text, "endsWith", |s, suffix| {
            s.ends_with(suffix)
        })),
        "trim" => Some(text_nullary_text(text, "trim", |s| s.trim().to_owned())),
        "trimStart" => Some(text_nullary_text(text, "trimStart", |s| {
            s.trim_start().to_owned()
        })),
        "trimEnd" => Some(text_nullary_text(text, "trimEnd", |s| {
            s.trim_end().to_owned()
        })),
        // Full Unicode case mapping (Rust `to_lowercase` / `to_uppercase`), not
        // Roc's ASCII-only `toAsciiLowercase` / `toAsciiUppercase`.
        "toLower" => Some(text_nullary_text(text, "toLower", |s| s.to_lowercase())),
        "toUpper" => Some(text_nullary_text(text, "toUpper", |s| s.to_uppercase())),
        "replaceEach" => Some(text_replace_method(text, "replaceEach", false)),
        "replaceFirst" => Some(text_replace_method(text, "replaceFirst", true)),
        "dropPrefix" => Some(text_drop_affix_method(text, "dropPrefix", true)),
        "dropSuffix" => Some(text_drop_affix_method(text, "dropSuffix", false)),
        "repeat" => Some(text_repeat_method(text)),
        "splitOn" => Some(text_split_on_method(text)),
        "padLeft" => Some(text_pad_method(text, true)),
        "padRight" => Some(text_pad_method(text, false)),
        "toInt" => Some(text_nullary_optional(text, "toInt", |s| {
            s.parse::<Int>().ok().map(Value::Int)
        })),
        "toFloat" => Some(text_nullary_optional(text, "toFloat", |s| {
            s.parse::<f64>().ok().map(Value::Float)
        })),
        "reverse" => Some(text_nullary_text(text, "reverse", |s| {
            s.chars().rev().collect()
        })),
        "indexOf" => Some(text_index_of_method(text)),
        "slice" => Some(text_slice_method(text)),
        "capitalize" => Some(text_nullary_text(text, "capitalize", text_capitalize)),
        _ => None,
    }
}

fn text_nullary_bool(
    text: String,
    name: &'static str,
    f: impl Fn(&str) -> bool + 'static,
) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Text.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Bool(f(&text)))
    })
}

fn text_nullary_int(
    text: String,
    name: &'static str,
    f: impl Fn(&str) -> Result<Int, String> + 'static,
) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Text.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Int(f(&text)?))
    })
}

fn text_nullary_value(
    text: String,
    name: &'static str,
    f: impl Fn(&str) -> Value + 'static,
) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Text.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(f(&text))
    })
}

fn text_nullary_text(
    text: String,
    name: &'static str,
    f: impl Fn(&str) -> String + 'static,
) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Text.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(Value::Text(f(&text)))
    })
}

fn text_nullary_optional(
    text: String,
    name: &'static str,
    f: impl Fn(&str) -> Option<Value> + 'static,
) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Text.{name} expects 0 arguments, got {}",
                args.len()
            ));
        }
        Ok(f(&text).unwrap_or(Value::Undefined))
    })
}

fn text_predicate_method(
    text: String,
    name: &'static str,
    f: impl Fn(&str, &str) -> bool + 'static,
) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Text.{name} expects 1 argument, got {}",
                args.len()
            ));
        }
        let needle = expect_text_arg(&args[0], &format!("Text.{name}"))?;
        Ok(Value::Bool(f(&text, needle)))
    })
}

fn text_replace_method(text: String, name: &'static str, first_only: bool) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "Text.{name} expects 2 arguments, got {}",
                args.len()
            ));
        }
        let from = expect_text_arg(&args[0], &format!("Text.{name}"))?;
        let to = expect_text_arg(&args[1], &format!("Text.{name}"))?;
        let replaced = if first_only {
            text.replacen(from, to, 1)
        } else {
            text.replace(from, to)
        };
        Ok(Value::Text(replaced))
    })
}

fn text_drop_affix_method(text: String, name: &'static str, prefix: bool) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Text.{name} expects 1 argument, got {}",
                args.len()
            ));
        }
        let affix = expect_text_arg(&args[0], &format!("Text.{name}"))?;
        // Roc semantics: no match leaves the input unchanged.
        let next = if prefix {
            text.strip_prefix(affix).unwrap_or(&text)
        } else {
            text.strip_suffix(affix).unwrap_or(&text)
        };
        Ok(Value::Text(next.to_owned()))
    })
}

fn text_repeat_method(text: String) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Text.repeat expects 1 argument, got {}",
                args.len()
            ));
        }
        let Value::Int(count) = &args[0] else {
            return Err(format!(
                "Text.repeat expects Int, got {}",
                args[0].type_name()
            ));
        };
        // Negative count → empty text (same as count 0). Documented choice.
        if count.is_negative() || count.is_zero() {
            return Ok(Value::Text(String::new()));
        }
        let Some(n) = count.to_usize() else {
            return Err("Text.repeat count is too large".to_owned());
        };
        Ok(Value::Text(text.repeat(n)))
    })
}

fn text_split_on_method(text: String) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Text.splitOn expects 1 argument, got {}",
                args.len()
            ));
        }
        let sep = expect_text_arg(&args[0], "Text.splitOn")?;
        // Empty separator is not useful (and panics in Rust `str::split`);
        // match Roc: return the original string wrapped in a one-element list.
        if sep.is_empty() {
            return Ok(Value::Array(Rc::new(vec![Value::Text(text.clone())])));
        }
        // Rust `str::split` semantics: no match and empty input still yield at
        // least one element (`[""]` for empty input; `[self]` when sep absent).
        let parts = text
            .split(sep)
            .map(|part| Value::Text(part.to_owned()))
            .collect::<Vec<_>>();
        Ok(Value::Array(Rc::new(parts)))
    })
}

fn text_pad_method(text: String, left: bool) -> Value {
    let name = if left { "padLeft" } else { "padRight" };
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "Text.{name} expects 2 arguments, got {}",
                args.len()
            ));
        }
        let Value::Int(width) = &args[0] else {
            return Err(format!(
                "Text.{name} expects Int width, got {}",
                args[0].type_name()
            ));
        };
        let pad = expect_text_arg(&args[1], &format!("Text.{name}"))?;
        Ok(Value::Text(text_pad(&text, width, pad, left)?))
    })
}

/// Char-offset of the first occurrence of `needle` (Unicode scalar positions).
/// Missing → `undefined` (`?Int`).
fn text_index_of_method(text: String) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Text.indexOf expects 1 argument, got {}",
                args.len()
            ));
        }
        let needle = expect_text_arg(&args[0], "Text.indexOf")?;
        let Some(byte_offset) = text.find(needle) else {
            return Ok(Value::Undefined);
        };
        let char_offset = text[..byte_offset].chars().count();
        Ok(Value::int(char_offset))
    })
}

/// Char-range substring. Clamps `start`/`end` into `[0, len]`; if
/// `start > end` after clamping, returns empty text. No negative indexing.
fn text_slice(text: &str, start: &Int, end: &Int) -> String {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let start = clamp_char_index(start, len);
    let end = clamp_char_index(end, len);
    if start >= end {
        return String::new();
    }
    chars[start..end].iter().collect()
}

fn clamp_char_index(index: &Int, len: usize) -> usize {
    if index.is_negative() || index.is_zero() {
        return 0;
    }
    let Some(index) = index.to_usize() else {
        return len;
    };
    index.min(len)
}

fn text_slice_method(text: String) -> Value {
    Value::native(move |args| {
        if args.is_empty() || args.len() > 2 {
            return Err(format!(
                "Text.slice expects 1 or 2 arguments, got {}",
                args.len()
            ));
        }
        let Value::Int(start) = &args[0] else {
            return Err(format!(
                "Text.slice expects Int start, got {}",
                args[0].type_name()
            ));
        };
        // Omitting `end` slices through to the end of the text.
        let end = match args.get(1) {
            None | Some(Value::Undefined) => Int::from(text.chars().count()),
            Some(Value::Int(end)) => end.clone(),
            Some(other) => {
                return Err(format!(
                    "Text.slice expects Int end, got {}",
                    other.type_name()
                ));
            }
        };
        Ok(Value::Text(text_slice(&text, start, &end)))
    })
}

/// Uppercase the first Unicode scalar; leave the rest unchanged. Empty → empty.
fn text_capitalize(text: &str) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut result: String = first.to_uppercase().collect();
    result.extend(chars);
    result
}

/// Pad `text` to `width` Unicode scalar values (`chars().count()`), matching
/// the unit other Text helpers implicitly use (no grapheme segmentation).
/// Empty `pad` or already-wide text leaves the input unchanged. Multi-char
/// `pad` is repeated from the start and truncated to the needed length.
fn text_pad(text: &str, width: &Int, pad: &str, left: bool) -> Result<String, String> {
    if pad.is_empty() {
        return Ok(text.to_owned());
    }
    if width.is_negative() {
        return Ok(text.to_owned());
    }
    let Some(width) = width.to_usize() else {
        return Err("Text padding width is too large".to_owned());
    };
    let text_len = text.chars().count();
    if text_len >= width {
        return Ok(text.to_owned());
    }
    let need = width - text_len;
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut padding = String::new();
    for index in 0..need {
        padding.push(pad_chars[index % pad_chars.len()]);
    }
    if left {
        Ok(format!("{padding}{text}"))
    } else {
        Ok(format!("{text}{padding}"))
    }
}

fn int_to_grouped_method(value: Int) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Int.toGrouped expects 1 argument, got {}",
                args.len()
            ));
        }
        let separator = expect_text_arg(&args[0], "Int.toGrouped")?;
        Ok(Value::Text(int_to_grouped(&value, separator)))
    })
}

fn unbound_int_to_grouped_method() -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Int.toGrouped expects 2 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Int(value) = receiver else {
            return Err(format!(
                "unbound Int.toGrouped expects Int receiver, got {}",
                receiver.type_name()
            ));
        };
        let separator = expect_text_arg(&args[1], "unbound Int.toGrouped")?;
        Ok(Value::Text(int_to_grouped(&value, separator)))
    })
}

/// Group digits in threes from the right with `separator`. Sign is preserved;
/// values under 1000 (absolute) insert no separator. Empty separator is plain
/// `to_string`.
fn int_to_grouped(value: &Int, separator: &str) -> String {
    let text = value.to_string();
    if separator.is_empty() {
        return text;
    }
    let (sign, digits) = match text.strip_prefix('-') {
        Some(digits) => ("-", digits),
        None => ("", text.as_str()),
    };
    let chars: Vec<char> = digits.chars().collect();
    let mut groups = Vec::new();
    let mut end = chars.len();
    while end > 0 {
        let start = end.saturating_sub(3);
        groups.push(chars[start..end].iter().collect::<String>());
        end = start;
    }
    groups.reverse();
    format!("{sign}{}", groups.join(separator))
}

fn float_to_fixed_method(value: f64) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Float.toFixed expects 1 argument, got {}",
                args.len()
            ));
        }
        let Value::Int(decimals) = &args[0] else {
            return Err(format!(
                "Float.toFixed expects Int, got {}",
                args[0].type_name()
            ));
        };
        float_to_fixed(value, decimals).map(Value::Text)
    })
}

fn unbound_float_to_fixed_method() -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!(
                "unbound Float.toFixed expects 2 arguments, got {}",
                args.len()
            ));
        }
        let receiver = erase_primitive_brand(args[0].clone());
        let Value::Float(value) = receiver else {
            return Err(format!(
                "unbound Float.toFixed expects Float receiver, got {}",
                receiver.type_name()
            ));
        };
        let Value::Int(decimals) = &args[1] else {
            return Err(format!(
                "unbound Float.toFixed expects Int, got {}",
                args[1].type_name()
            ));
        };
        float_to_fixed(value, decimals).map(Value::Text)
    })
}

/// Fixed-decimal rendering with half-away-from-zero rounding on the shortest
/// round-trip decimal of the IEEE value (Rust `f64::to_string` / ryu). Negative
/// `decimals` clamps to 0. Non-finite values use display words regardless of
/// `decimals`.
fn float_to_fixed(value: f64, decimals: &Int) -> Result<String, String> {
    if value.is_nan() {
        return Ok("NaN".to_owned());
    }
    if value == f64::INFINITY {
        return Ok("Infinity".to_owned());
    }
    if value == f64::NEG_INFINITY {
        return Ok("-Infinity".to_owned());
    }

    let decimals = if decimals.is_negative() {
        0
    } else {
        decimals
            .to_usize()
            .ok_or_else(|| "Float.toFixed decimal count is too large".to_owned())?
    };
    // `-0.0 == 0.0`, so signed zero displays without a minus.
    let negative = value.is_sign_negative() && value != 0.0;
    let raw = value.abs().to_string();
    let (int_str, frac_str) = match raw.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (raw.as_str(), ""),
    };

    let mut int_digits: Vec<u8> = int_str.bytes().map(|b| b - b'0').collect();
    if int_digits.is_empty() {
        int_digits.push(0);
    }
    let frac_bytes: Vec<u8> = frac_str.bytes().map(|b| b - b'0').collect();
    let round_up = frac_bytes.get(decimals).is_some_and(|digit| *digit >= 5);

    let mut frac_digits: Vec<u8> = frac_bytes.into_iter().take(decimals).collect();
    while frac_digits.len() < decimals {
        frac_digits.push(0);
    }

    if round_up {
        if decimals == 0 {
            add_one_digits(&mut int_digits);
        } else {
            round_up_fractional(&mut frac_digits, &mut int_digits);
        }
    }

    let int_out: String = int_digits
        .into_iter()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    let mut result = if negative {
        format!("-{int_out}")
    } else {
        int_out
    };
    if decimals > 0 {
        result.push('.');
        result.extend(
            frac_digits
                .into_iter()
                .map(|digit| char::from(b'0' + digit)),
        );
    }
    Ok(result)
}

/// Add one at the least-significant digit, carrying left and prepending `1`
/// when the whole digit vector overflows (e.g. `999` → `1000`).
fn add_one_digits(digits: &mut Vec<u8>) {
    let mut i = digits.len();
    while i > 0 {
        i -= 1;
        if digits[i] < 9 {
            digits[i] += 1;
            return;
        }
        digits[i] = 0;
    }
    digits.insert(0, 1);
}

/// Round fractional digits up by one ulp; full overflow carries into
/// `int_digits` and leaves the fractional digits zeroed (width preserved).
fn round_up_fractional(frac_digits: &mut [u8], int_digits: &mut Vec<u8>) {
    let mut i = frac_digits.len();
    while i > 0 {
        i -= 1;
        if frac_digits[i] < 9 {
            frac_digits[i] += 1;
            return;
        }
        frac_digits[i] = 0;
    }
    add_one_digits(int_digits);
}

fn expect_text_arg<'a>(value: &'a Value, context: &str) -> Result<&'a str, String> {
    match value {
        Value::Text(text) => Ok(text),
        other => Err(format!("{context} expects Text, got {}", other.type_name())),
    }
}

fn array_join_with_method(items: Rc<Vec<Value>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!(
                "Array.joinWith expects 1 argument, got {}",
                args.len()
            ));
        }
        let sep = expect_text_arg(&args[0], "Array.joinWith")?;
        let mut parts = Vec::with_capacity(items.len());
        for item in items.iter() {
            let Value::Text(text) = item else {
                return Err(format!(
                    "Array.joinWith expects Array(Text), got element {}",
                    item.type_name()
                ));
            };
            parts.push(text.as_str());
        }
        Ok(Value::Text(parts.join(sep)))
    })
}

fn array_has_method(items: Rc<Vec<Value>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Array.has expects 1 argument, got {}", args.len()));
        }

        Ok(Value::Bool(contains_value(&items, &args[0])))
    })
}

fn set_has_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Set.has expects 1 argument, got {}", args.len()));
        }
        ensure_set_element(&args[0], "Set.has")?;

        Ok(Value::Bool(members.contains(&args[0])))
    })
}

fn set_size_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("Set.size expects 0 arguments, got {}", args.len()));
        }

        Ok(Value::int(members.len()))
    })
}

/// Adding shares the receiver's trees with the result: cloning a `SetValue`
/// copies two persistent handles, not n members, so a fold that adds in a loop
/// stays linear and the receiver stays observably unchanged.
fn set_add_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Set.add expects 1 argument, got {}", args.len()));
        }
        ensure_set_element(&args[0], "Set.add")?;

        let mut next = members.as_ref().clone();
        next.insert(args[0].clone());
        Ok(Value::Set(Rc::new(next)))
    })
}

fn set_delete_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Set.delete expects 1 argument, got {}", args.len()));
        }
        ensure_set_element(&args[0], "Set.delete")?;

        let mut next = members.as_ref().clone();
        next.remove(&args[0]);
        Ok(Value::Set(Rc::new(next)))
    })
}

/// `union` is the `|` operator under a name, sharing one implementation so the
/// two spellings cannot drift on element identity or on which side is adopted.
fn set_union_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        let other = set_operand(args, "Set.union")?;
        set_union(Value::Set(Rc::clone(&members)), Value::Set(other))
    })
}

/// Both `intersection` and `difference` start from the receiver and remove,
/// rather than rebuilding into an empty set. Removal is the cheaper direction
/// (the receiver's trees are shared, and only the dropped members are
/// rewritten) and it is what keeps the survivors in the receiver's insertion
/// order without a sort.
fn set_intersection_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        let other = set_operand(args, "Set.intersection")?;
        let mut next = members.as_ref().clone();
        for member in members.iter() {
            if !other.contains(member) {
                next.remove(member);
            }
        }
        Ok(Value::Set(Rc::new(next)))
    })
}

fn set_difference_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        let other = set_operand(args, "Set.difference")?;
        let mut next = members.as_ref().clone();
        for member in other.iter() {
            next.remove(member);
        }
        Ok(Value::Set(Rc::new(next)))
    })
}

/// Walks the smaller side and stops at the first shared member, so a pair that
/// overlaps early costs nothing like a full scan.
fn set_is_disjoint_method(members: Rc<SetValue>) -> Value {
    Value::native(move |args| {
        let other = set_operand(args, "Set.isDisjoint")?;
        let (walked, probed) = if members.len() <= other.len() {
            (members.as_ref(), other.as_ref())
        } else {
            (other.as_ref(), members.as_ref())
        };
        Ok(Value::Bool(
            !walked.iter().any(|member| probed.contains(member)),
        ))
    })
}

/// The single `Set` argument the binary operations take. The checker rejects a
/// non-set operand; this is the runtime backstop for an untyped host call.
fn set_operand(args: &[Value], context: &str) -> Result<Rc<SetValue>, String> {
    let [Value::Set(other)] = args else {
        let [other] = args else {
            return Err(format!("{context} expects 1 argument, got {}", args.len()));
        };
        return Err(format!(
            "{context} expects a Set, got {}",
            other.type_name()
        ));
    };
    Ok(Rc::clone(other))
}

fn array_length_method(items: Rc<Vec<Value>>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!(
                "Array.length expects 0 arguments, got {}",
                args.len()
            ));
        }

        Ok(Value::int(items.len()))
    })
}

fn array_push_method(items: Rc<Vec<Value>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Array.push expects 1 argument, got {}", args.len()));
        }

        let mut next = items.as_ref().clone();
        next.push(args[0].clone());
        Ok(Value::Array(Rc::new(next)))
    })
}

fn map_get_method(entries: Rc<MapValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.get expects 1 argument, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.get")?;

        Ok(entries.get(&args[0]).cloned().unwrap_or(Value::Undefined))
    })
}

fn map_set_method(entries: Rc<MapValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 2 {
            return Err(format!("Map.set expects 2 arguments, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.set")?;

        let mut next = entries.as_ref().clone();
        next.insert(args[0].clone(), args[1].clone());
        Ok(Value::Map(Rc::new(next)))
    })
}

fn map_delete_method(entries: Rc<MapValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.delete expects 1 argument, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.delete")?;

        let mut next = entries.as_ref().clone();
        next.remove(&args[0]);
        Ok(Value::Map(Rc::new(next)))
    })
}

fn map_has_method(entries: Rc<MapValue>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("Map.has expects 1 argument, got {}", args.len()));
        }
        ensure_map_key(&args[0], "Map.has")?;

        Ok(Value::Bool(entries.get(&args[0]).is_some()))
    })
}

fn map_keys_method(entries: Rc<MapValue>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("Map.keys expects 0 arguments, got {}", args.len()));
        }

        Ok(Value::Array(Rc::new(
            entries.iter().map(|(key, _)| key.clone()).collect(),
        )))
    })
}

fn map_values_method(entries: Rc<MapValue>) -> Value {
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

fn map_entries_method(entries: Rc<MapValue>) -> Value {
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

fn map_size_method(entries: Rc<MapValue>) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("Map.size expects 0 arguments, got {}", args.len()));
        }

        Ok(Value::int(entries.len()))
    })
}

fn map_merge_method(entries: Rc<MapValue>) -> Value {
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
            next.insert(key.clone(), value.clone());
        }
        Ok(Value::Map(Rc::new(next)))
    })
}

fn eval_type_application(
    callee: &Expr,
    args: &[Expr],
    span: Span,
    env: &Environment,
) -> Option<Eval> {
    let (ExprKind::Name(name) | ExprKind::ComptimeName(name)) = &callee.kind else {
        return None;
    };
    let source_builtin = BuiltinType::from_name(name)?;
    source_builtin.application_arity()?;

    let callee_value = match eval_expr_many(callee, env) {
        Ok(value) => value,
        Err(diagnostics) => return Some(Err(diagnostics)),
    };

    // `Map` is overloaded by arity in value position:
    // - `Map(K, V)` — type application (two type arguments) → composite type
    // - `Map(pairs)` — construction from `Array((k, v))` (same as `Map.from`)
    if callee_value.as_type_name().and_then(BuiltinType::from_name) == Some(BuiltinType::Map) {
        match args {
            [key_expr, value_expr] => {
                let key = match eval_expr_many(key_expr, env) {
                    Ok(value) => value,
                    Err(diagnostics) => return Some(Err(diagnostics)),
                };
                let value = match eval_expr_many(value_expr, env) {
                    Ok(value) => value,
                    Err(diagnostics) => return Some(Err(diagnostics)),
                };
                let mut type_args = Vec::with_capacity(2);
                for (arg_value, arg) in [(key, key_expr), (value, value_expr)] {
                    let Ok(arg_type) = RuntimeType::from_value(&arg_value) else {
                        return Some(Err(one_diagnostic(record_type_error(
                            arg.span,
                            "map type construction",
                            arg_value.type_name(),
                            "Type",
                        ))));
                    };
                    type_args.push(arg_type);
                }
                return Some(
                    RuntimeType::apply(RuntimeType::named(BuiltinType::Map.name()), type_args)
                        .map(Value::Type)
                        .map_err(|message| one_diagnostic(platform_error(span, message))),
                );
            }
            [pairs_expr] => {
                let pairs = match eval_expr_many(pairs_expr, env) {
                    Ok(value) => value,
                    Err(diagnostics) => return Some(Err(diagnostics)),
                };
                return Some(
                    map_from_pair_array(&pairs, "Map")
                        .map_err(|message| one_diagnostic(platform_error(span, message))),
                );
            }
            _ => {
                return Some(Err(one_diagnostic(unsupported_expr(
                    span,
                    "Map expects Map(key, value) type application or Map(pairs) construction",
                ))));
            }
        }
    }

    if let Some(
        builtin @ (BuiltinType::Array
        | BuiltinType::Set
        | BuiltinType::Stream
        | BuiltinType::Result),
    ) = callee_value.as_type_name().and_then(BuiltinType::from_name)
    {
        let name = builtin.name();
        let arity = builtin
            .application_arity()
            .expect("type application builtin has an arity");
        if args.len() != arity {
            return Some(Err(one_diagnostic(unsupported_expr(
                span,
                &format!("{name} type application takes {arity} type argument(s)"),
            ))));
        }
        let mut type_args = Vec::with_capacity(args.len());
        for arg in args {
            let arg_value = match eval_expr_many(arg, env) {
                Ok(value) => value,
                Err(diagnostics) => return Some(Err(diagnostics)),
            };
            let Ok(arg_type) = RuntimeType::from_value(&arg_value) else {
                return Some(Err(one_diagnostic(record_type_error(
                    arg.span,
                    &format!("{} type construction", name.to_ascii_lowercase()),
                    arg_value.type_name(),
                    "Type",
                ))));
            };
            type_args.push(arg_type);
        }
        return Some(
            RuntimeType::apply(RuntimeType::named(name), type_args)
                .map(Value::Type)
                .map_err(|message| one_diagnostic(platform_error(span, message))),
        );
    }

    None
}

fn eval_index(
    callee: &Expr,
    args: &[Expr],
    null_safe: bool,
    span: Span,
    env: &Environment,
) -> Eval {
    let callee_value = eval_expr_many(callee, env)?;
    // Mirror field-access `?.`: empty receiver short-circuits without evaluating
    // the index expression.
    if null_safe && matches!(callee_value, Value::Undefined | Value::Null) {
        return Ok(callee_value);
    }

    if args.len() != 1 {
        return Err(one_diagnostic(unsupported_expr(
            span,
            "indexing takes exactly one argument; pass a single index",
        )));
    }

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

            // Negative indexes wrap from the end (Python-style): `-1` is last.
            // Still-out-of-bounds after wrap → `undefined`, same as past-the-end.
            Ok(array_indexed_value(&values, &index).unwrap_or(Value::Undefined))
        }
        Value::Text(text) => {
            let Value::Int(index) = arg_value else {
                return Err(one_diagnostic(record_type_error(
                    args[0].span,
                    "text indexing",
                    arg_value.type_name(),
                    "Int",
                )));
            };

            // Scalar-value index, same wrap/OOB rule as arrays (via `resolve_index`).
            Ok(text_indexed_value(&text, &index).unwrap_or(Value::Undefined))
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

            // Tuples do not wrap: fixed arity, out-of-bounds is a hard error.
            indexed_value(&values, &index).ok_or_else(|| {
                one_diagnostic(index_out_of_bounds(args[0].span, &index, values.len()))
            })
        }
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
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
        Value::Type(ty) => {
            let Value::Text(key) = arg_value else {
                return Err(one_diagnostic(record_type_error(
                    args[0].span,
                    "record type indexing",
                    arg_value.type_name(),
                    "Text",
                )));
            };
            let RuntimeTypeDescriptor::Record(fields) = ty.descriptor() else {
                return Err(one_diagnostic(record_type_error(
                    callee.span,
                    "indexing",
                    "Type",
                    "Record type",
                )));
            };
            fields
                .iter()
                .find(|(name, _)| name == &key)
                .map(|(_, field_ty)| Value::Type(ty.with_descriptor(field_ty.clone())))
                .ok_or_else(|| one_diagnostic(missing_field(&key, args[0].span)))
        }
        Value::Map(entries) => {
            // `m[key]` sugars to `m.get(key)`: reuse the method's native
            // closure rather than duplicating the lookup.
            let Value::Native(get) = map_get_method(entries) else {
                unreachable!("map_get_method always returns Value::Native")
            };
            get(&[arg_value], env.native_context(span))
                .map_err(|message| one_diagnostic(platform_error(span, message)))
        }
        value => Err(one_diagnostic(record_type_error(
            callee.span,
            "indexing",
            value.type_name(),
            "Array, Text, Tuple, Record, or Map",
        ))),
    }
}

fn eval_type_wrapper(
    inner: &Expr,
    span: Span,
    env: &Environment,
    wrap: fn(RuntimeType) -> RuntimeType,
) -> Eval {
    let value = eval_expr_many(inner, env)?;
    RuntimeType::from_value(&value)
        .map(wrap)
        .map(Value::Type)
        .map_err(|_| {
            one_diagnostic(record_type_error(
                span,
                "type construction",
                value.type_name(),
                "Type",
            ))
        })
}

fn runtime_type_target(value: &Value) -> bool {
    RuntimeType::from_value(value).is_ok()
}

/// Resolve a Python-style index: `i < 0` → `length + i`.
/// Returns `None` when the resolved index is still out of bounds.
/// Shared by array and Text indexing so the wrap/OOB rules cannot drift.
fn resolve_index(len: usize, index: &Int) -> Option<usize> {
    let resolved = if index.is_negative() {
        index + &Int::from(len)
    } else {
        index.clone()
    };
    if resolved.is_negative() {
        return None;
    }
    let resolved = resolved.to_usize()?;
    (resolved < len).then_some(resolved)
}

/// Array index with Python-style negative wrap (see `resolve_index`).
fn array_indexed_value(values: &[Value], index: &Int) -> Option<Value> {
    resolve_index(values.len(), index).map(|i| values[i].clone())
}

/// Text index by Unicode scalar value, same wrap/OOB rule as arrays.
/// Returns a single-scalar `Text`, or `None` → runtime `undefined`.
fn text_indexed_value(text: &str, index: &Int) -> Option<Value> {
    let chars: Vec<char> = text.chars().collect();
    resolve_index(chars.len(), index).map(|i| Value::Text(chars[i].to_string()))
}

/// Tuple index: no negative wrap; negative or past-end yields `None`.
fn indexed_value(values: &[Value], index: &Int) -> Option<Value> {
    let index = index.to_usize()?;
    values.get(index).cloned()
}

fn ensure_map_key(key: &Value, context: &str) -> Result<(), String> {
    if value_is_comparable(key) {
        Ok(())
    } else {
        Err(format!(
            "{context} cannot use {} as a Map key",
            key.type_name()
        ))
    }
}

/// Set membership uses the same equality as Map keys; reject values that have
/// none (functions, streams, …) so a set cannot break its own `has` invariant.
fn ensure_set_element(element: &Value, context: &str) -> Result<(), String> {
    if value_is_comparable(element) {
        Ok(())
    } else {
        Err(format!(
            "{context} cannot use {} as a Set element",
            element.type_name()
        ))
    }
}

fn value_is_comparable(value: &Value) -> bool {
    match value {
        Value::Closure(_)
        | Value::Native(_)
        | Value::RangeConstructor { .. }
        | Value::CollectConstructor(_)
        | Value::Stream(_)
        | Value::ResultMethod { .. }
        | Value::StreamMethod { .. }
        | Value::ArrayFlatMapMethod(_)
        | Value::ArrayFoldMethod(_)
        | Value::SetMethod { .. }
        | Value::NamedFamily(_)
        | Value::NamedMethod { .. }
        | Value::UnboundNamedMethod { .. } => false,
        Value::BrandedPrimitive { .. } => true,
        Value::Array(values) | Value::Tuple(values) => values.iter().all(value_is_comparable),
        Value::Set(members) => members.iter().all(value_is_comparable),
        Value::Map(entries) => entries
            .iter()
            .all(|(key, value)| value_is_comparable(key) && value_is_comparable(value)),
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
            fields.iter().all(|(_, value)| value_is_comparable(value))
        }
        Value::SlotRecord { fields, slots } => fields
            .iter()
            .chain(slots.iter())
            .all(|(_, value)| value_is_comparable(value)),
        Value::Tag { payload, .. } => payload.iter().all(value_is_comparable),
        Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bool(_)
        | Value::Type(_)
        | Value::Undefined
        | Value::Null => true,
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
    }
}

fn eval_interpolation(segments: &[InterpolationSegment], env: &Environment) -> Eval {
    let mut text = String::new();

    for segment in segments {
        match segment {
            InterpolationSegment::Text(raw) => text.push_str(raw),
            InterpolationSegment::Expr(expr) => {
                let value = eval_expr_many(expr, env)?;
                text.push_str(&display::to_text(
                    &value,
                    Some(&env.builtin_methods),
                    expr.span,
                )?);
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
        .parse::<Int>()
        .map(Value::Int)
        .map_err(|_| invalid_numeric_literal(text, span, "Int"))
}

fn is_float_literal(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
}

fn eval_unary(operator: &str, value: &Expr, span: Span, env: &Environment) -> Eval {
    let value = eval_expr_many(value, env)?;

    match (operator, value) {
        ("-", Value::Int(value)) => Ok(Value::Int(-&value)),
        ("-", Value::Float(value)) => Ok(Value::Float(-value)),
        ("-", value) => Err(one_diagnostic(unary_type_error(
            span,
            "-",
            value.type_name(),
            "a numeric operand",
        ))),
        ("!", Value::Bool(value)) => Ok(Value::Bool(!value)),
        // Type position: `!T` strips the outer `Optional` (the runtime mirror
        // of the checker's N5 rule), so mapped types like `required` evaluate.
        ("!", Value::Type(ty)) if matches!(ty.descriptor(), RuntimeTypeDescriptor::Optional(_)) => {
            let RuntimeTypeDescriptor::Optional(inner) = ty.descriptor() else {
                unreachable!("guard matches Optional")
            };
            Ok(Value::Type(ty.with_descriptor((**inner).clone())))
        }
        ("!", value) if runtime_type_target(&value) => Ok(value),
        ("!", value) => Err(one_diagnostic(unary_type_error(
            span,
            "!",
            value.type_name(),
            "a Bool operand",
        ))),
        _ => Err(one_diagnostic(unsupported_expr(
            span,
            "the evaluator cannot apply this unary operator",
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
        ".." | "..=" => {
            let start = eval_expr_many(left, env)?;
            let end = eval_expr_many(right, env)?;
            let Value::Int(start) = start else {
                return Err(one_diagnostic(range_bound_type_error(
                    left.span,
                    "start",
                    start.type_name(),
                )));
            };
            let Value::Int(end) = end else {
                return Err(one_diagnostic(range_bound_type_error(
                    right.span,
                    "end",
                    end.type_name(),
                )));
            };
            let step = default_range_step(&start, &end);
            range_value(start, end, step, operator == "..=", false, span)
        }
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
        }
    }
}

fn range_value(
    start: Int,
    end: Int,
    step: Int,
    inclusive: bool,
    materialize: bool,
    span: Span,
) -> Eval {
    let stream = Stream::range(start, end, step, inclusive)
        .map_err(|StreamError::ZeroStep| one_diagnostic(range_step_zero(span)))?;
    if materialize {
        materialize_stream(stream, span)
    } else {
        Ok(Value::Stream(stream))
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
) -> Eval {
    if let Value::NamedRecord { descriptor, .. } | Value::BrandedPrimitive { descriptor, .. } =
        &left
        && let Some(implementation) = descriptor.methods.get(operator).cloned()
    {
        return apply_named_method(left, implementation, vec![right], span);
    }
    match operator {
        "+" => add(left, right, span).map_err(one_diagnostic),
        "-" | "*" | "/" | "%" | "^" => {
            numeric_arithmetic(left, operator, right, right_span, span).map_err(one_diagnostic)
        }
        "==" | "!=" => equality(left, operator, right, span).map_err(one_diagnostic),
        "<" | ">" | "<=" | ">=" => {
            numeric_comparison(left, operator, right, span).map_err(one_diagnostic)
        }
        "|" => {
            set_union(left, right).map_err(|message| one_diagnostic(platform_error(span, message)))
        }
        _ => Err(one_diagnostic(unsupported_operator(
            operator,
            operator_span,
        ))),
    }
}

/// Set union with singleton-promotion: each operand contributes its members if
/// it is already a `Set`, otherwise it contributes itself as a single element.
/// Duplicates are dropped (first occurrence wins) by `SetValue`, the same
/// membership `eval_set` uses, so `|` and `@{..}` agree on element identity.
/// A `Set` on the left is adopted rather than rebuilt, so its structure is
/// shared with the result.
fn set_union(left: Value, right: Value) -> Result<Value, String> {
    let mut members = match left {
        Value::Set(members) => Rc::unwrap_or_clone(members),
        other => {
            ensure_set_element(&other, "Set union")?;
            SetValue::from_values([other])
        }
    };
    match right {
        Value::Set(other) => members.extend(other.iter().cloned()),
        other => {
            ensure_set_element(&other, "Set union")?;
            members.insert(other);
        }
    }
    Ok(Value::Set(Rc::new(members)))
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
            float_arithmetic(int_to_f64(&left), operator, right, right_span, span)
        }
        (Value::Float(left), Value::Int(right)) => {
            float_arithmetic(left, operator, int_to_f64(&right), right_span, span)
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
    left: Int,
    operator: &str,
    right: Int,
    right_span: Span,
    span: Span,
) -> Result<Value, Diagnostic> {
    if matches!(operator, "/" | "%") && right.is_zero() {
        return Err(division_by_zero(right_span));
    }

    let result = match operator {
        "+" => &left + &right,
        "-" => &left - &right,
        "*" => &left * &right,
        "/" => &left / &right,
        "%" => &left % &right,
        "^" => {
            let Some(exponent) = right.to_u32() else {
                return Err(invalid_integer_exponent(span));
            };
            left.pow(exponent)
        }
        _ => {
            return Err(unsupported_expr(
                span,
                "the evaluator cannot apply this operator to Int values",
            ));
        }
    };

    Ok(Value::Int(result))
}

fn float_arithmetic(
    left: f64,
    operator: &str,
    right: f64,
    right_span: Span,
    span: Span,
) -> Result<Value, Diagnostic> {
    if operator == "%" && is_float_zero(right) {
        return Err(division_by_zero(right_span));
    }

    match operator {
        "+" => Ok(Value::Float(left + right)),
        "-" => Ok(Value::Float(left - right)),
        "*" => Ok(Value::Float(left * right)),
        "/" => Ok(Value::Float(left / right)),
        "%" => Ok(Value::Float(left % right)),
        "^" => Ok(Value::Float(left.powf(right))),
        _ => Err(unsupported_expr(
            span,
            "the evaluator cannot apply this operator to Float values",
        )),
    }
}

fn equality(left: Value, operator: &str, right: Value, span: Span) -> Result<Value, Diagnostic> {
    let left = erase_primitive_brand(left);
    let right = erase_primitive_brand(right);
    if matches!(
        (&left, &right),
        (Value::Closure(_), _) | (_, Value::Closure(_))
    ) {
        return Err(closure_equality_error(span, operator));
    }

    // Kind gate only: Int/Float may cross; everything else needs matching kinds.
    // The actual comparison is `PartialEq`, which carries the numeric rule into
    // arrays, tuples, records, sets, maps, and tag payloads.
    if !equality_kinds_compatible(&left, &right) {
        return Err(binary_type_error(
            span,
            operator,
            left.type_name(),
            right.type_name(),
            "matching value kinds",
        ));
    }

    let equal = left == right;
    Ok(Value::Bool(if operator == "==" { equal } else { !equal }))
}

fn equality_kinds_compatible(left: &Value, right: &Value) -> bool {
    matches!(
        (left, right),
        (
            Value::Int(_) | Value::Float(_),
            Value::Int(_) | Value::Float(_)
        ) | (Value::Text(_), Value::Text(_))
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Tuple(_), Value::Tuple(_))
            | (Value::Set(_), Value::Set(_))
            | (Value::Map(_), Value::Map(_))
            | (Value::Record(_), Value::Record(_))
            | (Value::Tag { .. }, Value::Tag { .. })
            | (Value::Type(_), Value::Type(_))
            | (Value::Native(_), Value::Native(_))
            | (Value::Undefined, Value::Undefined)
            | (Value::Null, Value::Null)
    )
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
        (Value::Float(left), Value::Float(right)) => Some(float_total_cmp(*left, *right)),
        (Value::Int(left), Value::Float(right)) => Some(int_float_cmp(left, *right)),
        (Value::Float(left), Value::Int(right)) => Some(int_float_cmp(right, *left).reverse()),
        _ => None,
    }
}

/// Aven Float equality: NaN equals itself; `-0.0` equals `0.0` (IEEE).
fn float_eq(left: f64, right: f64) -> bool {
    (left.is_nan() && right.is_nan()) || left == right
}

/// Int/Float ordering, from the integer's side.
///
/// [`Int`] compares against `f64` exactly, so only NaN needs a decision here,
/// and [`float_total_cmp`] already places it above every number.
fn int_float_cmp(int: &Int, float: f64) -> Ordering {
    int.partial_cmp(&float).unwrap_or(Ordering::Less)
}

/// Int/Float equality, exact in both directions: an integer equals a float
/// only when the float carries that integer and nothing more.
fn int_float_eq(int: &Int, float: f64) -> bool {
    *int == float
}

/// Total order for Aven Float: `-Infinity < finite < Infinity < NaN`.
///
/// Both NaNs compare equal. Finite and infinite values keep IEEE ordering,
/// including `-0.0 == 0.0`. This is intentionally *not* `f64::total_cmp`, which
/// distinguishes signed zeros and orders NaN payloads separately.
fn float_total_cmp(left: f64, right: f64) -> Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left
            .partial_cmp(&right)
            .expect("non-NaN f64 values are totally ordered by partial_cmp"),
    }
}

fn write_float(f: &mut fmt::Formatter<'_>, value: f64) -> fmt::Result {
    write!(f, "{}", display::float_text(value))
}

fn is_float_zero(value: f64) -> bool {
    value.to_bits() << 1 == 0
}

fn int_to_f64(value: &Int) -> f64 {
    value.to_f64().unwrap_or_else(|| {
        if value.is_negative() {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }
    })
}

fn invalid_numeric_literal(text: &str, span: Span, kind: &str) -> Diagnostic {
    Diagnostic::error(format!("invalid {kind} literal `{text}`"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            "this numeric literal cannot be evaluated",
        ))
        .with_note("integer literals are arbitrary precision; Float literals use f64")
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

fn index_out_of_bounds(span: Span, index: &Int, length: usize) -> Diagnostic {
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

fn missing_type_member(owner: &str, member: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("type `{owner}` has no member `{member}`"))
        .with_code(codes::runtime::MISSING_TYPE_MEMBER)
        .with_label(Label::primary(span, "this member is not carried by the type"))
        .with_note(format!(
            "use a static or unbound method declared on `{owner}`, or access `{member}` on a runtime value"
        ))
}

fn dynamic_import(span: Span) -> Diagnostic {
    Diagnostic::error("dynamic import specifier")
        .with_code(codes::module::DYNAMIC_IMPORT)
        .with_label(Label::primary(
            span,
            "import specifier must be a static string literal",
        ))
        .with_note("import specifiers must be static strings; dynamic imports never run at runtime")
}

fn unsupported_import_root(specifier: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("unsupported import specifier `{specifier}`"))
        .with_code(codes::module::UNSUPPORTED_ROOT)
        .with_label(Label::primary(span, "this import root is unavailable"))
        .with_note("use a local relative specifier or a root prefix provided by the host")
        .with_note("bare library and package specifiers cannot be resolved")
}

/// A static relative import evaluated without an injected imports map: this
/// evaluation entered through `eval_module` or options without imports (an
/// embedding) instead of the module-graph driver, so no module was loaded.
/// Unlike the checker's warning, evaluation cannot produce a value here, so
/// this is an error.
fn unresolved_import(specifier: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("import `{specifier}` is not resolved here"))
        .with_code(codes::module::UNRESOLVED_IMPORT)
        .with_label(Label::primary(
            span,
            "this evaluation context loads one file, so the module is not available",
        ))
        .with_note("`aven run` resolves imports through the module graph")
}

fn import_failed(specifier: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("import `{specifier}` failed"))
        .with_code(codes::module::IMPORT_HAS_ERRORS)
        .with_label(Label::primary(span, "this imported module has errors"))
        .with_note("fix the imported module before running this file")
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

fn recursion_limit(span: Span, stack_budget: usize) -> Diagnostic {
    let stack_budget_mib = stack_budget / (1024 * 1024);
    Diagnostic::error("recursion limit exceeded")
        .with_code(codes::runtime::RECURSION_LIMIT)
        .with_label(Label::primary(
            span,
            format!("this call cannot continue within the {stack_budget_mib} MiB evaluator stack budget"),
        ))
        .with_note(
            "check for a missing or unreachable base case; for intentional recursion, rewrite the algorithm to use less stack or split the work",
        )
}

fn collection_too_large(span: Span) -> Diagnostic {
    let limit_mib = MAX_MATERIALIZED_ARRAY_BYTES / (1024 * 1024);
    Diagnostic::error("collection is too large to materialize")
        .with_code(codes::runtime::COLLECTION_TOO_LARGE)
        .with_label(Label::primary(
            span,
            format!("this array would exceed the {limit_mib} MiB materialization limit"),
        ))
        .with_note(
            "produce fewer array elements, or consume a stream with `fold` or `each` instead of materializing it",
        )
}

fn array_flat_map_result_type_error(span: Span, found: &str) -> Diagnostic {
    Diagnostic::error("Array.flatMap callback must return an Array")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            format!("this callback returned `{found}`"),
        ))
        .with_note("return an Array from every callback path, such as `[value]` or `[]`")
}

fn platform_error(span: Span, message: String) -> Diagnostic {
    Diagnostic::error("platform function failed")
        .with_code(codes::runtime::PLATFORM_ERROR)
        .with_label(Label::primary(span, message))
        .with_note("host platform functions report errors through the runtime boundary")
}

fn range_bound_type_error(span: Span, bound: &str, found: &str) -> Diagnostic {
    Diagnostic::error("range bounds and step must be Int")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            format!("the {bound} value has type `{found}`"),
        ))
        .with_note("pass Int bounds and, when needed, an options record such as `{ step: 2 }`")
}

fn range_options_type_error(span: Span, found: &str) -> Diagnostic {
    Diagnostic::error("range options must be a record")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            format!("the options value has type `{found}`"),
        ))
        .with_note("pass an options record such as `{ step: 2 }`, or omit the third argument")
}

fn range_unknown_option(span: Span, field: &str) -> Diagnostic {
    Diagnostic::error(format!("unknown range option `{field}`"))
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(span, "this options record is not supported"))
        .with_note("remove the unknown field; `step` is the only range option")
}

fn range_missing_step(span: Span) -> Diagnostic {
    Diagnostic::error("range options require a `step` field")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            "this options record has no `step` field",
        ))
        .with_note("add an Int `step` field, or omit the options record to use the default step")
}

fn range_step_zero(span: Span) -> Diagnostic {
    Diagnostic::error("range step cannot be zero")
        .with_code(codes::runtime::RANGE_STEP_ZERO)
        .with_label(Label::primary(span, "this range cannot advance"))
        .with_note("pass a positive or negative non-zero `step` field in the options record")
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

fn invalid_integer_exponent(span: Span) -> Diagnostic {
    Diagnostic::error("integer exponent is out of range")
        .with_code(codes::runtime::TYPE_ERROR)
        .with_label(Label::primary(
            span,
            "the exponent must be a non-negative value no larger than 4294967295",
        ))
        .with_note("Int values are arbitrary precision, but exponentiation uses a u32 exponent")
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
            "rewrite with literals, names, bindings, blocks, lambdas, calls, matches, records, variants, collections, indexes, nullable field access, unary operators, or core binary operators",
        )
}

fn record_entry_span(entry: &RecordEntry) -> Span {
    match entry {
        RecordEntry::Field { span, .. }
        | RecordEntry::Method { span, .. }
        | RecordEntry::FieldDefault { span, .. }
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
    Diagnostic::error(format!("operator `{operator}` has no runtime behavior"))
        .with_code(codes::runtime::UNSUPPORTED)
        .with_label(Label::primary(
            span,
            "rewrite with a core operator or an explicit function call",
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
