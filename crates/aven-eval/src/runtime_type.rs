use std::{collections::HashMap, fmt, rc::Rc};

use aven_parser::Literal;

use crate::Value;

/// An artifact-local key for a recursive runtime type descriptor.
///
/// The checker-to-runtime adapter assigns these compact keys while copying a
/// checked unfolding table. They are meaningful only together with the
/// [`RuntimeTypeGraph`] carried by a [`RuntimeType`].
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
    Apply {
        callee: Box<Self>,
        args: Vec<Self>,
    },
    Function {
        params: Vec<Self>,
        result: Box<Self>,
        required: usize,
    },
    Optional(Box<Self>),
    Nullable(Box<Self>),
    Tuple(Vec<Self>),
    Record(Vec<(String, Self)>),
    SlotRecord {
        data: Vec<(String, Self)>,
        slots: Vec<(String, Self)>,
    },
    Variant(Vec<RuntimeVariantDescriptor>),
    Recursive {
        id: RuntimeTypeId,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeVariantDescriptor {
    Tag {
        name: String,
        payload: Vec<RuntimeTypeDescriptor>,
    },
    Literal(Literal),
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

/// The canonical runtime representation of a reified type.
///
/// Every shape, including non-recursive compound types, uses the same
/// descriptor tree. Recursive nodes resolve through the finite graph shared by
/// the root. Keeping the graph out of child nodes makes recursive values finite
/// without maintaining a second, graph-less `Value` representation.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeType {
    descriptor: RuntimeTypeDescriptor,
    graph: Rc<RuntimeTypeGraph>,
}

impl RuntimeType {
    pub fn new(descriptor: RuntimeTypeDescriptor) -> Self {
        Self::with_graph(descriptor, Rc::new(RuntimeTypeGraph::default()))
    }

    pub fn with_graph(descriptor: RuntimeTypeDescriptor, graph: Rc<RuntimeTypeGraph>) -> Self {
        Self { descriptor, graph }
    }

    pub fn named(name: impl Into<String>) -> Self {
        Self::new(RuntimeTypeDescriptor::Named(name.into()))
    }

    pub fn recursive(
        id: RuntimeTypeId,
        name: impl Into<String>,
        graph: Rc<RuntimeTypeGraph>,
    ) -> Self {
        Self::with_graph(
            RuntimeTypeDescriptor::Recursive {
                id,
                name: name.into(),
            },
            graph,
        )
    }

    pub fn descriptor(&self) -> &RuntimeTypeDescriptor {
        &self.descriptor
    }

    pub fn graph(&self) -> &RuntimeTypeGraph {
        &self.graph
    }

    pub fn named_name(&self) -> Option<&str> {
        match &self.descriptor {
            RuntimeTypeDescriptor::Named(name) => Some(name),
            _ => None,
        }
    }

    pub fn with_descriptor(&self, descriptor: RuntimeTypeDescriptor) -> Self {
        Self::with_graph(descriptor, Rc::clone(&self.graph))
    }

    fn wrap(self, wrap: impl FnOnce(Box<RuntimeTypeDescriptor>) -> RuntimeTypeDescriptor) -> Self {
        Self::with_graph(wrap(Box::new(self.descriptor)), self.graph)
    }

    pub fn optional(self) -> Self {
        self.wrap(RuntimeTypeDescriptor::Optional)
    }

    pub fn nullable(self) -> Self {
        self.wrap(RuntimeTypeDescriptor::Nullable)
    }

    /// Build a type application while preserving its recursive-type graph.
    ///
    /// # Errors
    ///
    /// Returns an error when two inputs carry different non-empty graphs. Graph
    /// node IDs are artifact-local, so combining those graphs would require
    /// remapping every recursive reference rather than simply joining the maps.
    pub fn apply(callee: Self, args: Vec<Self>) -> Result<Self, String> {
        let graph = common_runtime_type_graph(std::iter::once(&callee).chain(args.iter()))?;
        Ok(Self::with_graph(
            RuntimeTypeDescriptor::Apply {
                callee: Box::new(callee.descriptor),
                args: args.into_iter().map(|arg| arg.descriptor).collect(),
            },
            graph,
        ))
    }

    /// Build a record type while preserving its recursive-type graph.
    ///
    /// # Errors
    ///
    /// Returns an error when two fields carry different non-empty graphs. Graph
    /// node IDs are artifact-local, so the graphs cannot be safely merged
    /// without remapping their recursive references.
    pub fn record(fields: Vec<(String, Self)>) -> Result<Self, String> {
        let graph = common_runtime_type_graph(fields.iter().map(|(_, ty)| ty))?;
        Ok(Self::with_graph(
            RuntimeTypeDescriptor::Record(
                fields
                    .into_iter()
                    .map(|(name, ty)| (name, ty.descriptor))
                    .collect(),
            ),
            graph,
        ))
    }

    /// Canonicalize an evaluator value used in type position. Record-shaped
    /// source expressions are accepted for compatibility, but consumers only
    /// receive the descriptor representation.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Type(ty) => Ok(ty.clone()),
            Value::Record(fields) | Value::NamedRecord { fields, .. } => Self::record(
                fields
                    .iter()
                    .map(|(name, field)| Ok((name.clone(), Self::from_value(field)?)))
                    .collect::<Result<_, String>>()?,
            ),
            _ => Err(format!("expected Type, got {}", value.type_name())),
        }
    }
}

fn common_runtime_type_graph<'a>(
    types: impl IntoIterator<Item = &'a RuntimeType>,
) -> Result<Rc<RuntimeTypeGraph>, String> {
    // Checked reification gives every recursive type in one artifact the same
    // graph, while types assembled directly from builtins carry an empty graph.
    // Consequently normal source evaluation cannot reach the mismatch below.
    // The guard protects the public Rust construction API (and future
    // cross-artifact composition), where identical numeric IDs from independent
    // graphs need not identify the same recursive type.
    let mut graph: Option<Rc<RuntimeTypeGraph>> = None;
    for ty in types {
        if ty.graph.is_empty() {
            continue;
        }
        match &graph {
            None => graph = Some(Rc::clone(&ty.graph)),
            Some(current) if current.as_ref() == ty.graph.as_ref() => {}
            Some(_) => {
                return Err("cannot combine runtime types from different recursive graphs".into());
            }
        }
    }
    Ok(graph.unwrap_or_else(|| Rc::new(RuntimeTypeGraph::default())))
}

/// Checked runtime type bindings which replace evaluation of reifiable source
/// type expressions with their canonical descriptor-backed values. This is
/// essential for recursive bindings, whose source expressions cannot be
/// evaluated eagerly without trying to build an infinite value.
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

    pub(super) fn get(&self, name: &str) -> Option<Value> {
        self.values.get(name).cloned()
    }
}

impl fmt::Display for RuntimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.descriptor)
    }
}

impl fmt::Display for RuntimeTypeDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => write!(f, "{name}"),
            Self::Apply { callee, args } => {
                write!(f, "{callee}(")?;
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                write!(f, ")")
            }
            Self::Function {
                params,
                result,
                required,
            } => {
                if params.len() == 1 && *required == 1 {
                    write!(f, "{} -> {result}", params[0])
                } else {
                    write!(f, "(")?;
                    for (index, param) in params.iter().enumerate() {
                        if index > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{param}")?;
                        if index >= *required {
                            write!(f, " = _")?;
                        }
                    }
                    write!(f, ") -> {result}")
                }
            }
            Self::Optional(inner) => write!(f, "?{inner}"),
            Self::Nullable(inner) => write!(f, "{inner}?"),
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
                    if index == 0 {
                        write!(f, " ")?;
                    } else {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                if !fields.is_empty() {
                    write!(f, " ")?;
                }
                write!(f, "}}")
            }
            Self::SlotRecord { data, slots } => {
                write!(f, "{{")?;
                for (index, (name, ty)) in data.iter().chain(slots).enumerate() {
                    if index == 0 {
                        write!(f, " ")?;
                    } else {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                if !data.is_empty() || !slots.is_empty() {
                    write!(f, " ")?;
                }
                write!(f, "}}")
            }
            Self::Variant(entries) => {
                write!(f, "@{{")?;
                for (index, entry) in entries.iter().enumerate() {
                    if index == 0 {
                        write!(f, " ")?;
                    } else {
                        write!(f, ", ")?;
                    }
                    write!(f, "{entry}")?;
                }
                if !entries.is_empty() {
                    write!(f, " ")?;
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
            Self::Literal(value) => match value {
                Literal::Bool(value) => write!(f, "{value}"),
                Literal::Number(value) | Literal::String(value) => {
                    write!(f, "{value}")
                }
            },
        }
    }
}
