use std::{collections::HashSet, rc::Rc};

use aven_check::{ComptimeArg, ComptimeError, HostComptimeFn, RowEntry, Type};
use aven_eval::{RuntimeType, RuntimeTypeDescriptor, RuntimeTypeGraph, RuntimeTypeId, Value};

use crate::io::aven_value_type_name;
use crate::temporal::{
    Date, DateTime, Duration, Instant, Time, date_value, datetime_value, duration_value,
    instant_value, time_value,
};

#[derive(Debug, Clone)]
pub(crate) enum FormatValue {
    Null,
    Bool(bool),
    Number(FormatNumber),
    Text(String),
    Array(Vec<FormatValue>),
    Object(Vec<(String, FormatValue)>),
    /// Host-internal datetime arm. Untyped decode renders ISO `Text`; typed
    /// decode maps each kind to the matching temporal type.
    Temporal(FormatTemporal),
}

/// The four calendar kinds TOML can express natively (and that codecs carry
/// without pre-stringifying). `Duration` is not a native TOML kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatTemporal {
    /// Offset date-time, already normalized to UTC epoch nanos.
    Instant(Instant),
    DateTime(DateTime),
    Date(Date),
    Time(Time),
}

impl FormatTemporal {
    pub(crate) fn iso_text(self) -> String {
        match self {
            Self::Instant(value) => value.format(),
            Self::DateTime(value) => value.format(),
            Self::Date(value) => value.format(),
            Self::Time(value) => value.format(),
        }
    }

    pub(crate) fn kind_name(self) -> &'static str {
        match self {
            Self::Instant(_) => "Instant",
            Self::DateTime(_) => "DateTime",
            Self::Date(_) => "Date",
            Self::Time(_) => "Time",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum FormatNumber {
    Int(i64),
    Float(f64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShapeError {
    path: String,
    expected: String,
    found: String,
}

pub(crate) enum DecodeError {
    Shape(ShapeError),
    InvalidTarget(String),
}

struct DecodeComptimeResolver {
    error_type: &'static str,
}

impl HostComptimeFn for DecodeComptimeResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        let target = match args {
            [] => crate::build::named("Data"),
            [target] => target
                .as_type()
                .cloned()
                .ok_or_else(|| ComptimeError::new("decode target must be a compile-time type"))?,
            _ => {
                return Err(ComptimeError::new(format!(
                    "decode resolver expects at most one compile-time target type argument, got {}",
                    args.len()
                )));
            }
        };
        if let Some(name) = deprecated_dynamic_target_name(&target) {
            return Err(ComptimeError::new(format!(
                "`{name}` is a format type, not the dynamic decode target; use `Data`"
            )));
        }

        Ok(crate::build::result(
            target,
            crate::build::named(self.error_type),
        ))
    }
}

pub(crate) fn decode_comptime_resolver(error_type: &'static str) -> Rc<dyn HostComptimeFn> {
    Rc::new(DecodeComptimeResolver { error_type })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormatPath(String);

impl FormatPath {
    fn root() -> Self {
        Self("$".to_owned())
    }

    fn field(&self, name: &str) -> Self {
        Self(format!("{}.{name}", self.0))
    }

    fn index(&self, index: usize) -> Self {
        Self(format!("{}[{index}]", self.0))
    }
}

pub(crate) fn decode_value(
    value: &FormatValue,
    target: &Value,
    format_name: &str,
) -> Result<Value, DecodeError> {
    decode_at(value, target, &FormatPath::root(), format_name)
}

fn decode_at(
    value: &FormatValue,
    target: &Value,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    match target {
        Value::Type(RuntimeType::Named(name)) => decode_named(value, name, path, format_name),
        Value::Type(RuntimeType::Optional(inner)) => decode_at(value, inner, path, format_name),
        Value::Type(RuntimeType::Nullable(inner)) => {
            if matches!(value, FormatValue::Null) {
                Ok(Value::Null)
            } else {
                decode_at(value, inner, path, format_name)
            }
        }
        Value::Type(RuntimeType::Array(inner)) => decode_array(value, inner, path, format_name),
        Value::Type(RuntimeType::Recursive(reference)) => {
            decode_recursive_at(value, reference.id, &reference.graph, path, format_name)
        }
        Value::Record(fields) if runtime_type_target(target) => {
            decode_record(value, fields, path, format_name)
        }
        other => Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode target must be a type value or record of type values, got {}",
            aven_value_type_name(other)
        ))),
    }
}

fn decode_recursive_at(
    value: &FormatValue,
    id: RuntimeTypeId,
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let descriptor = graph.unfolding(id).ok_or_else(|| {
        DecodeError::InvalidTarget(format!(
            "{format_name}.decode recursive target has no descriptor head"
        ))
    })?;
    decode_descriptor_at(value, descriptor, graph, path, format_name)
}

fn decode_descriptor_at(
    value: &FormatValue,
    target: &RuntimeTypeDescriptor,
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    match target {
        RuntimeTypeDescriptor::Named(name) => decode_named(value, name, path, format_name),
        RuntimeTypeDescriptor::Optional(inner) => {
            decode_descriptor_at(value, inner, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Nullable(inner) => {
            if matches!(value, FormatValue::Null) {
                Ok(Value::Null)
            } else {
                decode_descriptor_at(value, inner, graph, path, format_name)
            }
        }
        RuntimeTypeDescriptor::Array(inner) => {
            decode_descriptor_array(value, inner, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Record(fields) => {
            decode_descriptor_record(value, fields, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Recursive { id, .. } => {
            decode_recursive_at(value, *id, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Map(_, _)
        | RuntimeTypeDescriptor::Tuple(_)
        | RuntimeTypeDescriptor::Variant(_)
        | RuntimeTypeDescriptor::Unsupported(_) => Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode cannot decode target type {target}"
        ))),
    }
}

fn decode_named(
    value: &FormatValue,
    name: &str,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    match name {
        "Data" => return Ok(decode_dynamic_data(value)),
        "Json" | "Yaml" | "Toml" => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target {name} is a format type; use Data for dynamic values"
            )));
        }
        "Text" => match value {
            FormatValue::Text(text) => Some(Value::Text(text.clone())),
            FormatValue::Temporal(temporal) => Some(Value::Text(temporal.iso_text())),
            _ => None,
        },
        "Int" => match value {
            FormatValue::Number(FormatNumber::Int(value)) => Some(Value::Int(*value)),
            _ => None,
        },
        "Float" => match value {
            FormatValue::Number(FormatNumber::Int(value)) => Some(Value::Float(*value as f64)),
            FormatValue::Number(FormatNumber::Float(value)) => Some(Value::Float(*value)),
            _ => None,
        },
        "Bool" => match value {
            FormatValue::Bool(value) => Some(Value::Bool(*value)),
            _ => None,
        },
        "Null" if matches!(value, FormatValue::Null) => Some(Value::Null),
        "Null" => None,
        "Undefined" => None,
        "Instant" => return decode_temporal_target(value, TemporalTarget::Instant, path),
        "DateTime" => return decode_temporal_target(value, TemporalTarget::DateTime, path),
        "Date" => return decode_temporal_target(value, TemporalTarget::Date, path),
        "Time" => return decode_temporal_target(value, TemporalTarget::Time, path),
        "Duration" => return decode_duration_target(value, path),
        "Array" => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target Array must be written as Array(T)"
            )));
        }
        unsupported => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode cannot decode target type {unsupported}"
            )));
        }
    }
    .ok_or_else(|| shape_error(path, name, value))
}

#[derive(Clone, Copy)]
enum TemporalTarget {
    Instant,
    DateTime,
    Date,
    Time,
}

impl TemporalTarget {
    fn name(self) -> &'static str {
        match self {
            Self::Instant => "Instant",
            Self::DateTime => "DateTime",
            Self::Date => "Date",
            Self::Time => "Time",
        }
    }
}

fn decode_temporal_target(
    value: &FormatValue,
    target: TemporalTarget,
    path: &FormatPath,
) -> Result<Value, DecodeError> {
    match value {
        FormatValue::Temporal(temporal) => match (target, *temporal) {
            (TemporalTarget::Instant, FormatTemporal::Instant(instant)) => {
                Ok(instant_value(instant))
            }
            (TemporalTarget::DateTime, FormatTemporal::DateTime(datetime)) => {
                Ok(datetime_value(datetime))
            }
            (TemporalTarget::Date, FormatTemporal::Date(date)) => Ok(date_value(date)),
            (TemporalTarget::Time, FormatTemporal::Time(time)) => Ok(time_value(time)),
            // Local date-time into Instant is a shape error: no offset to anchor.
            _ => Err(shape_error(path, target.name(), value)),
        },
        FormatValue::Text(text) => match target {
            TemporalTarget::Instant => Instant::parse(text)
                .map(instant_value)
                .map_err(|_| shape_error(path, target.name(), value)),
            TemporalTarget::DateTime => DateTime::parse(text)
                .map(datetime_value)
                .map_err(|_| shape_error(path, target.name(), value)),
            TemporalTarget::Date => Date::parse(text)
                .map(date_value)
                .map_err(|_| shape_error(path, target.name(), value)),
            TemporalTarget::Time => Time::parse(text)
                .map(time_value)
                .map_err(|_| shape_error(path, target.name(), value)),
        },
        _ => Err(shape_error(path, target.name(), value)),
    }
}

fn decode_duration_target(value: &FormatValue, path: &FormatPath) -> Result<Value, DecodeError> {
    match value {
        FormatValue::Text(text) => Duration::parse(text)
            .map(duration_value)
            .map_err(|_| shape_error(path, "Duration", value)),
        _ => Err(shape_error(path, "Duration", value)),
    }
}

fn decode_dynamic_data(value: &FormatValue) -> Value {
    match value {
        FormatValue::Null => data_tag("Null", Vec::new()),
        FormatValue::Bool(value) => data_tag("Bool", vec![Value::Bool(*value)]),
        FormatValue::Number(FormatNumber::Int(value)) => data_tag("Int", vec![Value::Int(*value)]),
        FormatValue::Number(FormatNumber::Float(value)) => {
            data_tag("Float", vec![Value::Float(*value)])
        }
        FormatValue::Text(value) => data_tag("Text", vec![Value::Text(value.clone())]),
        // Data stays temporal-free: untyped decode yields ISO Text.
        FormatValue::Temporal(temporal) => data_tag("Text", vec![Value::Text(temporal.iso_text())]),
        FormatValue::Array(values) => {
            let values = values.iter().map(decode_dynamic_data).collect();
            data_tag("Array", vec![Value::Array(Rc::new(values))])
        }
        FormatValue::Object(entries) => {
            let entries = entries
                .iter()
                .map(|(key, value)| (Value::Text(key.clone()), decode_dynamic_data(value)))
                .collect();
            data_tag("Object", vec![Value::Map(Rc::new(entries))])
        }
    }
}

fn data_tag(name: &str, payload: Vec<Value>) -> Value {
    Value::Tag {
        name: name.to_owned(),
        payload,
    }
}

fn deprecated_dynamic_target_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) if matches!(name.as_str(), "Json" | "Yaml" | "Toml") => {
            Some(name.as_str())
        }
        Type::Apply { callee, args } => deprecated_dynamic_target_name(callee)
            .or_else(|| args.iter().find_map(deprecated_dynamic_target_name)),
        Type::Function { params, result, .. } => params
            .iter()
            .find_map(deprecated_dynamic_target_name)
            .or_else(|| deprecated_dynamic_target_name(result)),
        Type::Optional(inner) | Type::Nullable(inner) => deprecated_dynamic_target_name(inner),
        Type::Tuple(items) => items.iter().find_map(deprecated_dynamic_target_name),
        Type::Record(row) | Type::Variant(row) => {
            row.entries.iter().find_map(|entry| match entry {
                RowEntry::Field { ty, .. } => deprecated_dynamic_target_name(ty),
                RowEntry::Tag { payload, .. } => {
                    payload.iter().find_map(deprecated_dynamic_target_name)
                }
                RowEntry::Literal { .. } => None,
            })
        }
        Type::SlotRecord { data, slots } => [data, slots].into_iter().find_map(|row| {
            row.entries.iter().find_map(|entry| match entry {
                RowEntry::Field { ty, .. } => deprecated_dynamic_target_name(ty),
                RowEntry::Tag { payload, .. } => {
                    payload.iter().find_map(deprecated_dynamic_target_name)
                }
                RowEntry::Literal { .. } => None,
            })
        }),
        Type::Deferred
        | Type::Named(_)
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Recursive(_) => None,
    }
}

fn decode_record(
    value: &FormatValue,
    fields: &[(String, Value)],
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let FormatValue::Object(object) = value else {
        return Err(shape_error(path, "Record", value));
    };

    let mut output = Vec::with_capacity(fields.len());
    for (name, target) in fields {
        if !runtime_type_target(target) {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target field `{name}` must be a type value, got {}",
                aven_value_type_name(target)
            )));
        }

        let field_path = path.field(name);
        let field = match object
            .iter()
            .find_map(|(field_name, field_value)| (field_name == name).then_some(field_value))
        {
            Some(field_value) => decode_at(field_value, target, &field_path, format_name)?,
            None if target_is_optional(target) => Value::Undefined,
            None => {
                return Err(DecodeError::Shape(ShapeError {
                    path: field_path.0,
                    expected: target_display(target),
                    found: "Undefined".to_owned(),
                }));
            }
        };
        output.push((name.clone(), field));
    }

    Ok(Value::record(output))
}

fn decode_descriptor_record(
    value: &FormatValue,
    fields: &[(String, RuntimeTypeDescriptor)],
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let FormatValue::Object(object) = value else {
        return Err(shape_error(path, "Record", value));
    };

    let mut output = Vec::with_capacity(fields.len());
    for (name, target) in fields {
        if !runtime_descriptor_target(target, graph) {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target field `{name}` must be a decodable type, got {target}"
            )));
        }

        let field_path = path.field(name);
        let field = match object
            .iter()
            .find_map(|(field_name, field_value)| (field_name == name).then_some(field_value))
        {
            Some(field_value) => {
                decode_descriptor_at(field_value, target, graph, &field_path, format_name)?
            }
            None if descriptor_is_optional(target, graph) => Value::Undefined,
            None => {
                return Err(DecodeError::Shape(ShapeError {
                    path: field_path.0,
                    expected: target.to_string(),
                    found: "Undefined".to_owned(),
                }));
            }
        };
        output.push((name.clone(), field));
    }

    Ok(Value::record(output))
}

fn decode_array(
    value: &FormatValue,
    target: &Value,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let FormatValue::Array(items) = value else {
        return Err(shape_error(path, &target_display_array(target), value));
    };
    if !runtime_type_target(target) {
        return Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode Array target must be a type value, got {}",
            aven_value_type_name(target)
        )));
    }

    let mut output = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        output.push(decode_at(item, target, &path.index(index), format_name)?);
    }

    Ok(Value::Array(Rc::new(output)))
}

fn decode_descriptor_array(
    value: &FormatValue,
    target: &RuntimeTypeDescriptor,
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let FormatValue::Array(items) = value else {
        return Err(shape_error(path, &format!("Array({target})"), value));
    };
    if !runtime_descriptor_target(target, graph) {
        return Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode Array target must be a decodable type, got {target}"
        )));
    }

    let mut output = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        output.push(decode_descriptor_at(
            item,
            target,
            graph,
            &path.index(index),
            format_name,
        )?);
    }

    Ok(Value::Array(Rc::new(output)))
}

fn descriptor_is_optional(target: &RuntimeTypeDescriptor, graph: &RuntimeTypeGraph) -> bool {
    descriptor_is_optional_inner(target, graph, &mut HashSet::new())
}

fn descriptor_is_optional_inner(
    target: &RuntimeTypeDescriptor,
    graph: &RuntimeTypeGraph,
    visited: &mut HashSet<RuntimeTypeId>,
) -> bool {
    match target {
        RuntimeTypeDescriptor::Optional(_) => true,
        RuntimeTypeDescriptor::Recursive { id, .. } if visited.insert(*id) => graph
            .unfolding(*id)
            .is_some_and(|head| descriptor_is_optional_inner(head, graph, visited)),
        _ => false,
    }
}

fn runtime_descriptor_target(target: &RuntimeTypeDescriptor, graph: &RuntimeTypeGraph) -> bool {
    runtime_descriptor_target_inner(target, graph, &mut HashSet::new())
}

fn runtime_descriptor_target_inner(
    target: &RuntimeTypeDescriptor,
    graph: &RuntimeTypeGraph,
    visited: &mut HashSet<RuntimeTypeId>,
) -> bool {
    match target {
        RuntimeTypeDescriptor::Named(_) => true,
        RuntimeTypeDescriptor::Optional(inner)
        | RuntimeTypeDescriptor::Nullable(inner)
        | RuntimeTypeDescriptor::Array(inner) => {
            runtime_descriptor_target_inner(inner, graph, visited)
        }
        RuntimeTypeDescriptor::Record(fields) => fields
            .iter()
            .all(|(_, field)| runtime_descriptor_target_inner(field, graph, visited)),
        RuntimeTypeDescriptor::Recursive { id, .. } if visited.insert(*id) => graph
            .unfolding(*id)
            .is_some_and(|head| runtime_descriptor_target_inner(head, graph, visited)),
        RuntimeTypeDescriptor::Recursive { .. } => true,
        RuntimeTypeDescriptor::Map(_, _)
        | RuntimeTypeDescriptor::Tuple(_)
        | RuntimeTypeDescriptor::Variant(_)
        | RuntimeTypeDescriptor::Unsupported(_) => false,
    }
}

fn target_is_optional(target: &Value) -> bool {
    match target {
        Value::Type(RuntimeType::Optional(_)) => true,
        Value::Type(RuntimeType::Recursive(reference)) => reference
            .graph
            .unfolding(reference.id)
            .is_some_and(|head| descriptor_is_optional(head, &reference.graph)),
        _ => false,
    }
}

fn runtime_type_target(value: &Value) -> bool {
    match value {
        Value::Type(_) => true,
        Value::Record(fields) => fields
            .iter()
            .all(|(_, field_value)| runtime_type_target(field_value)),
        _ => false,
    }
}

fn target_display(target: &Value) -> String {
    target.to_string()
}

fn target_display_array(target: &Value) -> String {
    format!("Array({})", target_display(target))
}

fn shape_error(path: &FormatPath, expected: &str, found: &FormatValue) -> DecodeError {
    DecodeError::Shape(ShapeError {
        path: path.0.clone(),
        expected: expected.to_owned(),
        found: found_kind(found),
    })
}

fn found_kind(value: &FormatValue) -> String {
    match value {
        FormatValue::Null => "Null".to_owned(),
        FormatValue::Bool(_) => "Bool".to_owned(),
        FormatValue::Number(FormatNumber::Int(_)) => "Int".to_owned(),
        FormatValue::Number(FormatNumber::Float(_)) => "Float".to_owned(),
        FormatValue::Text(_) => "Text".to_owned(),
        FormatValue::Array(_) => "Array".to_owned(),
        FormatValue::Object(_) => "Record".to_owned(),
        FormatValue::Temporal(temporal) => temporal.kind_name().to_owned(),
    }
}

pub(crate) fn parse_error_value(message: impl Into<String>) -> Value {
    Value::Tag {
        name: "Parse".to_owned(),
        payload: vec![Value::record(vec![(
            "message".to_owned(),
            Value::Text(message.into()),
        )])],
    }
}

pub(crate) fn shape_error_value(error: ShapeError) -> Value {
    Value::Tag {
        name: "Shape".to_owned(),
        payload: vec![Value::record(vec![
            ("path".to_owned(), Value::Text(error.path)),
            ("expected".to_owned(), Value::Text(error.expected)),
            ("found".to_owned(), Value::Text(error.found)),
        ])],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_decode_accepts_data_target() {
        let value = FormatValue::Object(vec![(
            "name".to_owned(),
            FormatValue::Text("Ada".to_owned()),
        )]);

        let decoded = match decode_value(&value, &Value::named_type("Data"), "Json") {
            Ok(decoded) => decoded,
            Err(DecodeError::Shape(_)) => panic!("Data dynamic decode shaped"),
            Err(DecodeError::InvalidTarget(message)) => panic!("{message}"),
        };

        let Value::Tag { name, payload } = decoded else {
            panic!("expected dynamic object tag, got {decoded:?}");
        };
        assert_eq!(name, "Object");
        assert_eq!(payload.len(), 1);
    }

    #[test]
    fn dynamic_decode_rejects_format_targets() {
        let value = FormatValue::Null;

        for target_name in ["Json", "Yaml", "Toml"] {
            let target = Value::named_type(target_name);
            let error = match decode_value(&value, &target, "Json") {
                Ok(decoded) => panic!("{target_name} target decoded as {decoded:?}"),
                Err(DecodeError::Shape(_)) => panic!("{target_name} target shaped"),
                Err(DecodeError::InvalidTarget(message)) => message,
            };

            assert!(error.contains("use Data"));
        }
    }

    #[test]
    fn recursive_descriptor_decodes_one_hundred_finite_levels() {
        let id = RuntimeTypeId(0);
        let graph = Rc::new(RuntimeTypeGraph::new([(
            id,
            RuntimeTypeDescriptor::Record(vec![
                (
                    "value".to_owned(),
                    RuntimeTypeDescriptor::Named("Int".to_owned()),
                ),
                (
                    "children".to_owned(),
                    RuntimeTypeDescriptor::Array(Box::new(RuntimeTypeDescriptor::Recursive {
                        id,
                        name: "Tree".to_owned(),
                    })),
                ),
            ]),
        )]));
        let target = Value::recursive_type(id, "Tree", graph);
        let mut input = FormatValue::Object(vec![
            (
                "value".to_owned(),
                FormatValue::Number(FormatNumber::Int(100)),
            ),
            ("children".to_owned(), FormatValue::Array(Vec::new())),
        ]);
        for value in (0..100).rev() {
            input = FormatValue::Object(vec![
                (
                    "value".to_owned(),
                    FormatValue::Number(FormatNumber::Int(value)),
                ),
                ("children".to_owned(), FormatValue::Array(vec![input])),
            ]);
        }

        let decoded = match decode_value(&input, &target, "Json") {
            Ok(decoded) => decoded,
            Err(DecodeError::Shape(error)) => panic!("recursive input shaped: {error:?}"),
            Err(DecodeError::InvalidTarget(message)) => panic!("{message}"),
        };
        let mut node = &decoded;
        for expected in 0..100 {
            let Value::Record(fields) = node else {
                panic!("level {expected} is a record: {node:?}");
            };
            assert_eq!(record_field(fields, "value"), &Value::Int(expected));
            let Value::Array(children) = record_field(fields, "children") else {
                panic!("level {expected} has children");
            };
            let [child] = children.as_slice() else {
                panic!("level {expected} has one child");
            };
            node = child;
        }
        let Value::Record(fields) = node else {
            panic!("leaf is a record: {node:?}");
        };
        assert_eq!(record_field(fields, "value"), &Value::Int(100));
        assert_eq!(
            record_field(fields, "children"),
            &Value::Array(Rc::new(Vec::new()))
        );
    }

    fn record_field<'a>(fields: &'a [(String, Value)], name: &str) -> &'a Value {
        fields
            .iter()
            .find_map(|(field_name, value)| (field_name == name).then_some(value))
            .unwrap_or_else(|| panic!("record has field `{name}`"))
    }
}
