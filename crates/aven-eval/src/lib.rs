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
    RecordEntry, decode_string_literal, is_method_requirement_row,
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

/// An artifact-local key for a recursive runtime type descriptor.
///
/// The checker-to-runtime adapter assigns these compact keys while copying a
/// checked unfolding table. They are meaningful only together with the
/// [`RuntimeTypeGraph`] carried by a [`RuntimeTypeReference`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeTypeId(pub u32);

/// A finite runtime type-description node.
///
/// Recursive children are IDs rather than nested descriptor values. The
/// corresponding one-level heads live in [`RuntimeTypeGraph`], so even mutual
/// recursion has a finite representation.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeTypeDescriptor {
    Named(String),
    Optional(Box<Self>),
    Nullable(Box<Self>),
    Array(Box<Self>),
    Map(Box<Self>, Box<Self>),
    Tuple(Vec<Self>),
    Record(Vec<(String, Self)>),
    Variant(Vec<RuntimeVariantDescriptor>),
    Recursive { id: RuntimeTypeId, name: String },
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeVariantDescriptor {
    Tag {
        name: String,
        payload: Vec<RuntimeTypeDescriptor>,
    },
    Literal(String),
}

/// Shared one-level heads for a finite recursive descriptor graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeTypeGraph {
    unfoldings: HashMap<RuntimeTypeId, RuntimeTypeDescriptor>,
}

impl RuntimeTypeGraph {
    pub fn new(
        unfoldings: impl IntoIterator<Item = (RuntimeTypeId, RuntimeTypeDescriptor)>,
    ) -> Self {
        Self {
            unfoldings: unfoldings.into_iter().collect(),
        }
    }

    pub fn unfolding(&self, id: RuntimeTypeId) -> Option<&RuntimeTypeDescriptor> {
        self.unfoldings.get(&id)
    }

    pub fn len(&self) -> usize {
        self.unfoldings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.unfoldings.is_empty()
    }
}

/// A keyed recursive descriptor plus the finite graph that resolves it.
/// Cloning this artifact clones only two strings/words and an [`Rc`].
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTypeReference {
    pub id: RuntimeTypeId,
    pub name: Rc<str>,
    pub graph: Rc<RuntimeTypeGraph>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeType {
    Named(String),
    Optional(Box<Value>),
    Nullable(Box<Value>),
    Array(Box<Value>),
    Map(Box<Value>, Box<Value>),
    Recursive(RuntimeTypeReference),
}

/// Checked runtime type bindings which replace evaluation of their source
/// type expressions. This is necessary for recursive bindings: evaluating the
/// source expression eagerly would try to build an infinite boxed value.
#[derive(Debug, Clone, Default)]
pub struct RuntimeTypeBindings {
    values: HashMap<String, Value>,
}

impl RuntimeTypeBindings {
    pub fn new(values: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }

    pub fn insert(&mut self, name: impl Into<String>, value: Value) {
        self.values.insert(name.into(), value);
    }

    fn get(&self, name: &str) -> Option<Value> {
        self.values.get(name).cloned()
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
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Array(Rc<Vec<Value>>),
    Tuple(Rc<Vec<Value>>),
    Set(Rc<Vec<Value>>),
    Map(Rc<Vec<(Value, Value)>>),
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
        implementation: NamedMethodImplementation,
    },
    UnboundNamedMethod {
        descriptor: Rc<NamedFamilyDescriptor>,
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
    Closure(Closure),
    Native(NativeFn),
    /// A runtime type descriptor. The evaluator keeps this intentionally small:
    /// named types plus the composite shapes format decode needs. Record types
    /// remain ordinary `Value::Record` values whose fields are type values.
    Type(RuntimeType),
    Undefined,
    Null,
}

#[derive(Debug, Clone)]
pub enum PrimitivePayload {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Array(Rc<Vec<Value>>),
    Set(Rc<Vec<Value>>),
    Map(Rc<Vec<(Value, Value)>>),
}

impl PartialEq for PrimitivePayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => float_eq(*left, *right),
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
            Self::Int(value) => Value::Int(*value),
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

/// Type names bound as `Value::Type` intrinsics. `Array`/`Data`/`Map` are
/// included so `Array(T)`/`Map(K, V)` type application and dynamic format decode
/// targets can evaluate to the minimal composite type values these need at
/// runtime. Each type carrying statics (`Map.from`, `Json.decode`) resolves the
/// static through a `"Type.static"`-keyed global (see [`eval_field_access`]).
const TYPE_VALUE_NAMES: [&str; 13] = [
    "Array",
    "Bool",
    "Data",
    "Float",
    "Int",
    "Json",
    "Map",
    "Null",
    "Text",
    "Toml",
    "Undefined",
    "Unit",
    "Yaml",
];

pub const MAP_METHOD_NAMES: &[&str] = &[
    "get", "set", "delete", "has", "keys", "values", "entries", "size", "merge",
];

/// Roc-aligned Text helpers. Keep in lockstep with `aven_check::ty::TEXT_METHOD_NAMES`.
pub const TEXT_METHOD_NAMES: &[&str] = &[
    "isEmpty",
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
    "toInt",
    "toFloat",
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
            (Self::Float(left), Self::Float(right)) => float_eq(*left, *right),
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Tuple(left), Self::Tuple(right)) => left == right,
            (Self::Set(left), Self::Set(right)) => sets_equal(left, right),
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
            (Self::NamedFamily(_), _) | (_, Self::NamedFamily(_)) => false,
            (Self::NamedMethod { .. }, _) | (_, Self::NamedMethod { .. }) => false,
            (Self::UnboundNamedMethod { .. }, _) | (_, Self::UnboundNamedMethod { .. }) => false,
            (Self::Undefined, Self::Undefined) | (Self::Null, Self::Null) => true,
            (Self::Closure(_), _) | (_, Self::Closure(_)) => false,
            (Self::Native(_), _) | (_, Self::Native(_)) => false,
            _ => false,
        }
    }
}

fn primitive_payload_matches_value(payload: &PrimitivePayload, value: &Value) -> bool {
    match (payload, value) {
        (PrimitivePayload::Int(left), Value::Int(right)) => left == right,
        (PrimitivePayload::Float(left), Value::Float(right)) => float_eq(*left, *right),
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

    pub fn recursive_type(
        id: RuntimeTypeId,
        name: impl Into<String>,
        graph: Rc<RuntimeTypeGraph>,
    ) -> Self {
        Self::Type(RuntimeType::Recursive(RuntimeTypeReference {
            id,
            name: Rc::from(name.into()),
            graph,
        }))
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
            Self::SlotRecord { .. } => "Record",
            Self::NamedFamily(_) => "Type",
            Self::NamedRecord { .. } => "Record",
            Self::BrandedPrimitive { payload, .. } => payload.type_name(),
            Self::NamedMethod { .. } => "Function",
            Self::UnboundNamedMethod { .. } => "Function",
            Self::Tag { .. } => "Tag",
            Self::ResultMethod { .. } => "Function",
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
            Self::Array(inner) => write!(f, "Array({inner})"),
            Self::Map(key, value) => write!(f, "Map({key}, {value})"),
            Self::Recursive(reference) => write!(f, "{}", reference.name),
        }
    }
}

impl fmt::Display for RuntimeTypeDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) | Self::Unsupported(name) => write!(f, "{name}"),
            Self::Optional(inner) => write!(f, "?{inner}"),
            Self::Nullable(inner) => write!(f, "{inner}?"),
            Self::Array(inner) => write!(f, "Array({inner})"),
            Self::Map(key, value) => write!(f, "Map({key}, {value})"),
            Self::Tuple(items) => {
                write!(f, "(")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, ")")
            }
            Self::Record(fields) => {
                write!(f, "{{")?;
                for (index, (name, ty)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                write!(f, "}}")
            }
            Self::Variant(entries) => {
                write!(f, "@{{")?;
                for (index, entry) in entries.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{entry}")?;
                }
                write!(f, "}}")
            }
            Self::Recursive { name, .. } => write!(f, "{name}"),
        }
    }
}

impl fmt::Display for RuntimeVariantDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag { name, payload } if payload.is_empty() => write!(f, "@{name}"),
            Self::Tag { name, payload } => {
                write!(f, "@{name}(")?;
                for (index, ty) in payload.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{ty}")?;
                }
                write!(f, ")")
            }
            Self::Literal(value) => write!(f, "{value}"),
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
    imports: Rc<ModuleImports>,
    builtin_methods: BuiltinMethodEnvironment,
    slot_reifications: Rc<SlotReificationPlan>,
    direct_slot_inits: Rc<DirectSlotInitPlan>,
    primitive_families: Rc<PrimitiveFamilyPlan>,
    family_descriptors: Rc<RefCell<HashMap<String, Rc<NamedFamilyDescriptor>>>>,
    allow_builtin_method_attachments: bool,
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
        )
    }

    fn with_imports_builtin_methods_and_reifications(
        imports: ModuleImports,
        builtin_methods: BuiltinMethodEnvironment,
        allow_builtin_method_attachments: bool,
        slot_reifications: SlotReificationPlan,
        direct_slot_inits: DirectSlotInitPlan,
        primitive_families: PrimitiveFamilyPlan,
    ) -> Self {
        Self {
            scope: Rc::new(Scope::new(None)),
            imports: Rc::new(imports),
            builtin_methods,
            slot_reifications: Rc::new(slot_reifications),
            direct_slot_inits: Rc::new(direct_slot_inits),
            primitive_families: Rc::new(primitive_families),
            family_descriptors: Rc::new(RefCell::new(HashMap::new())),
            allow_builtin_method_attachments,
        }
    }

    fn child(&self) -> Self {
        Self {
            scope: Rc::new(Scope::new(Some(Rc::clone(&self.scope)))),
            imports: Rc::clone(&self.imports),
            builtin_methods: self.builtin_methods.clone(),
            slot_reifications: Rc::clone(&self.slot_reifications),
            direct_slot_inits: Rc::clone(&self.direct_slot_inits),
            primitive_families: Rc::clone(&self.primitive_families),
            family_descriptors: Rc::clone(&self.family_descriptors),
            allow_builtin_method_attachments: self.allow_builtin_method_attachments,
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

    fn get(&self, specifier: &str) -> Option<Option<Value>> {
        self.values.get(specifier).cloned()
    }
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
    eval_module_with_globals_and_imports(module, globals, &ModuleImports::default())
}

pub fn eval_module_with_globals_and_imports(
    module: &Module,
    globals: Vec<(String, Value)>,
    imports: &ModuleImports,
) -> EvalOutcome {
    eval_module_with_globals_imports_and_runtime_types(
        module,
        globals,
        imports,
        &RuntimeTypeBindings::default(),
    )
}

pub fn eval_module_with_globals_imports_and_runtime_types(
    module: &Module,
    globals: Vec<(String, Value)>,
    imports: &ModuleImports,
    runtime_types: &RuntimeTypeBindings,
) -> EvalOutcome {
    eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        module,
        globals,
        imports,
        runtime_types,
        &BuiltinMethodEnvironment::default(),
        false,
    )
}

pub fn eval_module_with_globals_imports_runtime_types_and_builtin_methods(
    module: &Module,
    globals: Vec<(String, Value)>,
    imports: &ModuleImports,
    runtime_types: &RuntimeTypeBindings,
    builtin_methods: &BuiltinMethodEnvironment,
    allow_builtin_method_attachments: bool,
) -> EvalOutcome {
    eval_module_with_globals_imports_runtime_types_builtin_methods_and_reifications(
        module,
        globals,
        imports,
        runtime_types,
        builtin_methods,
        allow_builtin_method_attachments,
        &EvalElaborationPlan::default(),
    )
}

pub fn eval_module_with_globals_imports_runtime_types_builtin_methods_and_reifications(
    module: &Module,
    globals: Vec<(String, Value)>,
    imports: &ModuleImports,
    runtime_types: &RuntimeTypeBindings,
    builtin_methods: &BuiltinMethodEnvironment,
    allow_builtin_method_attachments: bool,
    elaborations: &EvalElaborationPlan,
) -> EvalOutcome {
    let env = Environment::with_imports_builtin_methods_and_reifications(
        imports.clone(),
        builtin_methods.clone(),
        allow_builtin_method_attachments,
        elaborations.slot_reifications.clone(),
        elaborations.direct_slot_inits.clone(),
        elaborations.primitive_families.clone(),
    );
    bind_intrinsics(&env);
    for (name, value) in globals {
        env.bind(name, value);
    }
    // Top-level: a propagated `@Err` (`?^` with no enclosing function) becomes
    // the program value and stops further items.
    match eval_items(&module.items, &env, Some(runtime_types)) {
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

    // `Map` binds to a type value (see `TYPE_VALUE_NAMES`); its statics resolve
    // through `"Map.static"`-keyed globals consulted on `Value::Type` field
    // access.
    intrinsics.push(("Map.empty".to_owned(), Value::native(map_empty_intrinsic)));
    intrinsics.push(("Map.from".to_owned(), Value::native(map_from_intrinsic)));

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
        ExprKind::Index { callee, args } => eval_index(callee, args, expr.span, env),
        ExprKind::Call { callee, args } => eval_type_application(callee, args, expr.span, env)
            .unwrap_or_else(|| eval_call(callee, args, expr.span, env)),
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
        Literal::Regex(_) => Ok(None),
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
            return apply_callee_values(decode_fn, format.span, arg_values, span);
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
            return apply_callee_values(encode_fn, format.span, arg_values, span);
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
            apply_native(function, arg_values, span)
        }
        Value::ResultMethod { receiver, kind } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_result_method(*receiver, kind, arg_values, callee_span, span)
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
            implementation,
        } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_named_method(*receiver, implementation, arg_values, span)
        }
        Value::UnboundNamedMethod {
            descriptor,
            implementation,
        } => {
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr_many(arg, env)?);
            }
            apply_unbound_named_method(descriptor, implementation, arg_values, span)
        }
        Value::Closure(closure) => apply_closure(closure, args, span, env),
        value => Err(one_diagnostic(not_callable(callee_span, value.type_name()))),
    }
}

fn apply_callee_values(
    callee_value: Value,
    callee_span: Span,
    arg_values: Vec<Value>,
    span: Span,
) -> Eval {
    match callee_value {
        Value::Native(function) => apply_native(function, arg_values, span),
        Value::ResultMethod { receiver, kind } => {
            apply_result_method(*receiver, kind, arg_values, callee_span, span)
        }
        Value::NamedFamily(descriptor) => {
            apply_named_family_constructor(descriptor, arg_values, span)
        }
        Value::NamedMethod {
            receiver,
            implementation,
        } => apply_named_method(*receiver, implementation, arg_values, span),
        Value::UnboundNamedMethod {
            descriptor,
            implementation,
        } => apply_unbound_named_method(descriptor, implementation, arg_values, span),
        Value::Closure(closure) => apply_closure_values(closure, arg_values, span),
        value => Err(one_diagnostic(not_callable(callee_span, value.type_name()))),
    }
}

fn apply_unbound_named_method(
    descriptor: Rc<NamedFamilyDescriptor>,
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
    apply_named_method(receiver, implementation, args, span)
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
        apply_callee_values(method, span, args, span)?
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
    matches!(
        member,
        "+" | "-" | "*" | "/" | "%" | "^" | "<" | "<=" | ">" | ">="
    )
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
                let transformed =
                    apply_callee_values(args[0].clone(), callee_span, vec![value.clone()], span)?;
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
                apply_callee_values(args[0].clone(), callee_span, vec![value.clone()], span)
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
                let transformed =
                    apply_callee_values(args[0].clone(), callee_span, vec![value.clone()], span)?;
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
                apply_callee_values(args[0].clone(), callee_span, vec![value.clone()], span)
            }
        }
    }
}

fn apply_native(function: NativeFn, arg_values: Vec<Value>, span: Span) -> Eval {
    function(&arg_values).map_err(|message| one_diagnostic(platform_error(span, message)))
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

    bind_and_eval_closure(closure, arg_values, provided)
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

fn bind_and_eval_closure(closure: Closure, arg_values: Vec<Value>, provided: usize) -> Eval {
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
                let Value::Array(members) = source else {
                    return Err(one_diagnostic(record_type_error(
                        source_expr.span,
                        "spread",
                        source.type_name(),
                        "Array",
                    )));
                };

                values.extend(members.iter().cloned());
            }
            entry => {
                return Err(one_diagnostic(unsupported_expr(
                    record_entry_span(entry),
                    "only element and spread entries are supported in array literals by the current evaluator",
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
    let mut values = Vec::new();

    for entry in entries {
        match entry {
            RecordEntry::Element(expr) => {
                let value = eval_expr_many(expr, env)?;
                if !contains_value(&values, &value) {
                    values.push(value);
                }
            }
            RecordEntry::Spread {
                value: source_expr, ..
            } => {
                let source = eval_expr_many(source_expr, env)?;
                let Value::Set(members) = source else {
                    return Err(one_diagnostic(record_type_error(
                        source_expr.span,
                        "spread",
                        source.type_name(),
                        "Set",
                    )));
                };

                for member in members.iter() {
                    if !contains_value(&values, member) {
                        values.push(member.clone());
                    }
                }
            }
            entry => {
                return Err(one_diagnostic(unsupported_expr(
                    record_entry_span(entry),
                    "only element and spread entries are supported in set literals by the current evaluator",
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
                || Ok(Value::Undefined),
                |implementation| {
                    Ok(Value::NamedMethod {
                        receiver: Box::new(receiver_value),
                        implementation,
                    })
                },
            )
        }
        Value::BrandedPrimitive { descriptor, .. } => {
            descriptor.methods.get(field).cloned().map_or_else(
                || Ok(Value::Undefined),
                |implementation| {
                    Ok(Value::NamedMethod {
                        receiver: Box::new(receiver_value),
                        implementation,
                    })
                },
            )
        }
        Value::NamedFamily(descriptor) => descriptor
            .methods
            .get(field)
            .cloned()
            .map(|implementation| Value::UnboundNamedMethod {
                descriptor: Rc::clone(descriptor),
                implementation,
            })
            .ok_or_else(|| one_diagnostic(missing_field(field, field_span))),
        // A type value (`Map`, `Json`, ...) carries statics: field access
        // resolves the `"Type.static"`-keyed global bound alongside the type.
        Value::Type(RuntimeType::Named(name)) => env
            .lookup(&format!("{name}.{field}"))
            .ok_or_else(|| one_diagnostic(missing_field(field, field_span))),
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
        Value::Record(fields) => record_field_value(fields, field).is_some(),
        Value::SlotRecord { fields, slots } => {
            record_field_value(fields, field).is_some()
                || record_field_value(slots, field).is_some()
        }
        Value::NamedRecord { descriptor, fields } => {
            record_field_value(fields, field).is_some() || descriptor.methods.contains_key(field)
        }
        Value::BrandedPrimitive { descriptor, .. } | Value::NamedFamily(descriptor) => {
            descriptor.methods.contains_key(field)
        }
        Value::Type(RuntimeType::Named(name)) => env.lookup(&format!("{name}.{field}")).is_some(),
        value => builtin_method(value, field, env).is_some(),
    }
}

fn builtin_method(receiver: &Value, field: &str, env: &Environment) -> Option<Value> {
    if let Some(implementation) = env.builtin_methods.lookup(receiver, field) {
        return Some(Value::NamedMethod {
            receiver: Box::new(receiver.clone()),
            implementation: NamedMethodImplementation::Declared(implementation),
        });
    }
    match (receiver, field) {
        (receiver, "toResult") => Some(optional_to_result_method(receiver.clone())),
        (Value::Set(items), "has") => Some(collection_has_method("Set", Rc::clone(items))),
        (Value::Array(items), "has") => Some(collection_has_method("Array", Rc::clone(items))),
        (Value::Array(items), "push") => Some(array_push_method(Rc::clone(items))),
        (Value::Array(items), "joinWith") => Some(array_join_with_method(Rc::clone(items))),
        (Value::Map(entries), "get") => Some(map_get_method(Rc::clone(entries))),
        (Value::Map(entries), "set") => Some(map_set_method(Rc::clone(entries))),
        (Value::Map(entries), "delete") => Some(map_delete_method(Rc::clone(entries))),
        (Value::Map(entries), "has") => Some(map_has_method(Rc::clone(entries))),
        (Value::Map(entries), "keys") => Some(map_keys_method(Rc::clone(entries))),
        (Value::Map(entries), "values") => Some(map_values_method(Rc::clone(entries))),
        (Value::Map(entries), "entries") => Some(map_entries_method(Rc::clone(entries))),
        (Value::Map(entries), "size") => Some(map_size_method(Rc::clone(entries))),
        (Value::Map(entries), "merge") => Some(map_merge_method(Rc::clone(entries))),
        (Value::Int(value), "div") => Some(int_checked_method(*value, "div", i64::checked_div)),
        (Value::Int(value), "mod") => Some(int_checked_method(*value, "mod", i64::checked_rem)),
        (Value::Float(value), "isFinite") => {
            Some(float_nullary_bool(*value, "isFinite", f64::is_finite))
        }
        (Value::Float(value), "isNaN") => Some(float_nullary_bool(*value, "isNaN", f64::is_nan)),
        (Value::Float(value), "isInfinite") => {
            Some(float_nullary_bool(*value, "isInfinite", f64::is_infinite))
        }
        (Value::Float(value), "ieeeEquals") => Some(float_ieee_equals_method(*value)),
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

fn int_checked_method(
    left: i64,
    name: &'static str,
    operation: fn(i64, i64) -> Option<i64>,
) -> Value {
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
        if *right == 0 {
            return Ok(Value::Undefined);
        }
        operation(left, *right)
            .map(Value::Int)
            .ok_or_else(|| format!("integer overflow in Int.{name}"))
    })
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
        "toInt" => Some(text_nullary_optional(text, "toInt", |s| {
            s.parse::<i64>().ok().map(Value::Int)
        })),
        "toFloat" => Some(text_nullary_optional(text, "toFloat", |s| {
            s.parse::<f64>().ok().map(Value::Float)
        })),
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
        if *count <= 0 {
            return Ok(Value::Text(String::new()));
        }
        let Ok(n) = usize::try_from(*count) else {
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

fn collection_has_method(kind: &'static str, items: Rc<Vec<Value>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("{kind}.has expects 1 argument, got {}", args.len()));
        }

        Ok(Value::Bool(contains_value(&items, &args[0])))
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

fn eval_type_application(
    callee: &Expr,
    args: &[Expr],
    span: Span,
    env: &Environment,
) -> Option<Eval> {
    let (ExprKind::Name(name) | ExprKind::ComptimeName(name)) = &callee.kind else {
        return None;
    };
    if !matches!(name.as_str(), "Array" | "Map") {
        return None;
    }

    let callee_value = match eval_expr_many(callee, env) {
        Ok(value) => value,
        Err(diagnostics) => return Some(Err(diagnostics)),
    };

    // `Map(K, V)` type application in value position: build a composite type
    // value rather than record-index the (now type-valued) `Map`.
    if let Value::Type(RuntimeType::Named(name)) = &callee_value
        && name == "Map"
    {
        let [key_expr, value_expr] = args else {
            return Some(Err(one_diagnostic(unsupported_expr(
                span,
                "Map type application takes two type arguments (Map(key, value))",
            ))));
        };
        let key = match eval_expr_many(key_expr, env) {
            Ok(value) => value,
            Err(diagnostics) => return Some(Err(diagnostics)),
        };
        let value = match eval_expr_many(value_expr, env) {
            Ok(value) => value,
            Err(diagnostics) => return Some(Err(diagnostics)),
        };
        for (arg_value, arg) in [(&key, key_expr), (&value, value_expr)] {
            if !runtime_type_target(arg_value) {
                return Some(Err(one_diagnostic(record_type_error(
                    arg.span,
                    "map type construction",
                    arg_value.type_name(),
                    "Type",
                ))));
            }
        }
        return Some(Ok(Value::Type(RuntimeType::Map(
            Box::new(key),
            Box::new(value),
        ))));
    }

    if let Value::Type(RuntimeType::Named(name)) = &callee_value
        && name == "Array"
    {
        let [arg] = args else {
            return Some(Err(one_diagnostic(unsupported_expr(
                span,
                "Array type application takes one type argument (Array(element))",
            ))));
        };
        let arg_value = match eval_expr_many(arg, env) {
            Ok(value) => value,
            Err(diagnostics) => return Some(Err(diagnostics)),
        };
        if runtime_type_target(&arg_value) {
            return Some(Ok(Value::Type(RuntimeType::Array(Box::new(
                arg_value.clone(),
            )))));
        }

        return Some(Err(one_diagnostic(record_type_error(
            arg.span,
            "array type construction",
            arg_value.type_name(),
            "Type",
        ))));
    }

    None
}

fn eval_index(callee: &Expr, args: &[Expr], span: Span, env: &Environment) -> Eval {
    let callee_value = eval_expr_many(callee, env)?;

    if args.len() != 1 {
        return Err(one_diagnostic(unsupported_expr(
            span,
            "only single-argument indexing is supported by the current evaluator",
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
            Ok(array_indexed_value(&values, index).unwrap_or(Value::Undefined))
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
            indexed_value(&values, index).ok_or_else(|| {
                one_diagnostic(index_out_of_bounds(args[0].span, index, values.len()))
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
        Value::Map(entries) => {
            // `m[key]` sugars to `m.get(key)`: reuse the method's native
            // closure rather than duplicating the lookup.
            let Value::Native(get) = map_get_method(entries) else {
                unreachable!("map_get_method always returns Value::Native")
            };
            get(&[arg_value]).map_err(|message| one_diagnostic(platform_error(span, message)))
        }
        value => Err(one_diagnostic(record_type_error(
            callee.span,
            "indexing",
            value.type_name(),
            "Array, Tuple, Record, or Map",
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
        Value::Record(fields) | Value::NamedRecord { fields, .. } => fields
            .iter()
            .all(|(_, field_value)| runtime_type_target(field_value)),
        _ => false,
    }
}

/// Array index with Python-style negative wrap: `i < 0` → `length + i`.
/// Returns `None` when the resolved index is still out of bounds.
fn array_indexed_value(values: &[Value], index: i64) -> Option<Value> {
    let len = i64::try_from(values.len()).ok()?;
    let resolved = if index < 0 {
        index.checked_add(len)?
    } else {
        index
    };
    if resolved < 0 {
        return None;
    }
    let resolved = usize::try_from(resolved).ok()?;
    values.get(resolved).cloned()
}

/// Tuple index: no negative wrap; negative or past-end yields `None`.
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
        Value::Closure(_)
        | Value::Native(_)
        | Value::ResultMethod { .. }
        | Value::NamedFamily(_)
        | Value::NamedMethod { .. }
        | Value::UnboundNamedMethod { .. } => false,
        Value::BrandedPrimitive { .. } => true,
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => {
            values.iter().all(map_key_is_comparable)
        }
        Value::Map(entries) => entries
            .iter()
            .all(|(key, value)| map_key_is_comparable(key) && map_key_is_comparable(value)),
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
            fields.iter().all(|(_, value)| map_key_is_comparable(value))
        }
        Value::SlotRecord { fields, slots } => fields
            .iter()
            .chain(slots.iter())
            .all(|(_, value)| map_key_is_comparable(value)),
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
        Literal::Regex(_) => Err(unsupported_expr(
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
        // Type position: `!T` strips the outer `Optional` (the runtime mirror
        // of the checker's N5 rule), so mapped types like `required` evaluate.
        ("!", Value::Type(RuntimeType::Optional(inner))) => Ok(*inner),
        ("!", value) if runtime_type_target(&value) => Ok(value),
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
        "|" => Ok(set_union(left, right)),
        _ => Err(one_diagnostic(unsupported_operator(
            operator,
            operator_span,
        ))),
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
        "^" => u32::try_from(right)
            .ok()
            .and_then(|exponent| left.checked_pow(exponent)),
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
            "this numeric operator is not supported by the current evaluator",
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

    let equal = match (&left, &right) {
        (Value::Int(left), Value::Int(right)) => left == right,
        (Value::Float(left), Value::Float(right)) => float_eq(*left, *right),
        (Value::Int(left), Value::Float(right)) => float_eq(*left as f64, *right),
        (Value::Float(left), Value::Int(right)) => float_eq(*left, *right as f64),
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
        (Value::Float(left), Value::Float(right)) => Some(float_total_cmp(*left, *right)),
        (Value::Int(left), Value::Float(right)) => Some(float_total_cmp(*left as f64, *right)),
        (Value::Float(left), Value::Int(right)) => Some(float_total_cmp(*left, *right as f64)),
        _ => None,
    }
}

/// Aven Float equality: NaN equals itself; `-0.0` equals `0.0` (IEEE).
fn float_eq(left: f64, right: f64) -> bool {
    (left.is_nan() && right.is_nan()) || left == right
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
    if value.is_nan() {
        write!(f, "NaN")
    } else if value == f64::INFINITY {
        write!(f, "Infinity")
    } else if value == f64::NEG_INFINITY {
        write!(f, "-Infinity")
    } else {
        write!(f, "{value}")
    }
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

fn dynamic_import(span: Span) -> Diagnostic {
    Diagnostic::error("dynamic import is not supported yet")
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
        .with_label(Label::primary(
            span,
            "this import root is not supported in this milestone",
        ))
        .with_note("use a local relative specifier or a root prefix provided by the host")
        .with_note("bare libraries and packages remain unsupported until package resolution lands")
}

/// A static relative import evaluated without an injected imports map: this
/// evaluation entered through bare `eval_module_with_globals` (an embedding)
/// instead of the module-graph driver, so no module was loaded. Unlike the
/// checker's warning, evaluation cannot produce a value here, so this is an
/// error.
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
