use std::{collections::HashSet, rc::Rc};

use aven_check::{
    ComptimeArg, ComptimeError, ComptimeTypeContext, HostComptimeFn, RowEntry, Type,
    might_contain_float,
};
use aven_core::BuiltinType;
use aven_eval::{Int, RuntimeType, RuntimeTypeDescriptor, RuntimeTypeGraph, RuntimeTypeId, Value};

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

#[derive(Debug, Clone)]
pub(crate) enum FormatNumber {
    Int(Int),
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

struct EncodeTextComptimeResolver {
    format: &'static str,
}

impl HostComptimeFn for EncodeTextComptimeResolver {
    fn resolve(&self, _args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        unreachable!("encodeText resolver always receives checker type analysis context")
    }

    fn resolve_with_type_context(
        &self,
        args: &[ComptimeArg],
        context: &ComptimeTypeContext<'_>,
    ) -> Result<Type, ComptimeError> {
        let [ComptimeArg::Type(argument)] = args else {
            return Err(ComptimeError::new(format!(
                "{}.encodeText resolver expects one inferred argument type",
                self.format
            )));
        };
        if might_contain_float(argument, context) {
            return Err(ComptimeError::new(format!(
                "{}.encodeText requires a type that cannot contain a non-finite Float; this type may — use {}.encode instead",
                self.format, self.format
            )));
        }
        Ok(crate::build::text())
    }
}

pub(crate) fn encode_text_comptime_resolver(format: &'static str) -> Rc<dyn HostComptimeFn> {
    Rc::new(EncodeTextComptimeResolver { format })
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
    let target = RuntimeType::from_value(target).map_err(|_| {
        DecodeError::InvalidTarget(format!(
            "{format_name}.decode target must be a type value or record of type values, got {}",
            aven_value_type_name(target)
        ))
    })?;
    decode_descriptor_at(
        value,
        target.descriptor(),
        target.graph(),
        path,
        format_name,
    )
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
        RuntimeTypeDescriptor::Apply { callee, args }
            if matches!(callee.as_ref(), RuntimeTypeDescriptor::Named(name) if BuiltinType::from_name(name) == Some(BuiltinType::Array))
                && args.len() == 1 =>
        {
            decode_descriptor_array(value, &args[0], graph, path, format_name, false)
        }
        RuntimeTypeDescriptor::Apply { callee, args }
            if matches!(callee.as_ref(), RuntimeTypeDescriptor::Named(name) if BuiltinType::from_name(name) == Some(BuiltinType::Set))
                && args.len() == 1 =>
        {
            decode_descriptor_array(value, &args[0], graph, path, format_name, true)
        }
        RuntimeTypeDescriptor::Apply { callee, args }
            if matches!(callee.as_ref(), RuntimeTypeDescriptor::Named(name) if BuiltinType::from_name(name) == Some(BuiltinType::Map))
                && args.len() == 2 =>
        {
            decode_descriptor_map(value, &args[0], &args[1], graph, path, format_name)
        }
        RuntimeTypeDescriptor::Tuple(items) => {
            decode_descriptor_tuple(value, items, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Record(fields) => {
            decode_descriptor_record(value, fields, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Recursive { id, .. } => {
            decode_recursive_at(value, *id, graph, path, format_name)
        }
        RuntimeTypeDescriptor::Apply { .. }
        | RuntimeTypeDescriptor::Function { .. }
        | RuntimeTypeDescriptor::SlotRecord { .. }
        | RuntimeTypeDescriptor::Variant(_) => Err(DecodeError::InvalidTarget(format!(
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
    match BuiltinType::from_name(name) {
        Some(BuiltinType::Data) => return Ok(decode_dynamic_data(value)),
        Some(BuiltinType::Json | BuiltinType::Yaml | BuiltinType::Toml) => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target {name} is a format type; use Data for dynamic values"
            )));
        }
        Some(BuiltinType::Text) => match value {
            FormatValue::Text(text) => Some(Value::Text(text.clone())),
            FormatValue::Temporal(temporal) => Some(Value::Text(temporal.iso_text())),
            _ => None,
        },
        Some(BuiltinType::Int) => match value {
            FormatValue::Number(FormatNumber::Int(value)) => Some(Value::Int(value.clone())),
            _ => None,
        },
        Some(BuiltinType::Float) => match value {
            FormatValue::Number(FormatNumber::Int(value)) => value.to_f64().map(Value::Float),
            FormatValue::Number(FormatNumber::Float(value)) => Some(Value::Float(*value)),
            _ => None,
        },
        Some(BuiltinType::Bool) => match value {
            FormatValue::Bool(value) => Some(Value::Bool(*value)),
            _ => None,
        },
        Some(BuiltinType::Null) if matches!(value, FormatValue::Null) => Some(Value::Null),
        Some(BuiltinType::Null | BuiltinType::Undefined) => None,
        Some(BuiltinType::Array) => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target Array must be written as Array(T)"
            )));
        }
        None if name == "Instant" => {
            return decode_temporal_target(value, TemporalTarget::Instant, path);
        }
        None if name == "DateTime" => {
            return decode_temporal_target(value, TemporalTarget::DateTime, path);
        }
        None if name == "Date" => return decode_temporal_target(value, TemporalTarget::Date, path),
        None if name == "Time" => return decode_temporal_target(value, TemporalTarget::Time, path),
        None if name == "Duration" => return decode_duration_target(value, path),
        _ => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode cannot decode target type {name}"
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
        FormatValue::Number(FormatNumber::Int(value)) => {
            data_tag("Int", vec![Value::Int(value.clone())])
        }
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
        Type::Named(name)
            if matches!(
                BuiltinType::from_name(name),
                Some(BuiltinType::Json | BuiltinType::Yaml | BuiltinType::Toml)
            ) =>
        {
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
        Type::Error
        | Type::Deferred
        | Type::Named(_)
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Recursive(_) => None,
    }
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

fn decode_descriptor_array(
    value: &FormatValue,
    target: &RuntimeTypeDescriptor,
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
    set: bool,
) -> Result<Value, DecodeError> {
    let FormatValue::Array(items) = value else {
        let constructor = if set { "Set" } else { "Array" };
        return Err(shape_error(
            path,
            &format!("{constructor}({target})"),
            value,
        ));
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

    if set {
        let mut unique = Vec::with_capacity(output.len());
        for value in output {
            if !unique.contains(&value) {
                unique.push(value);
            }
        }
        Ok(Value::Set(Rc::new(unique)))
    } else {
        Ok(Value::Array(Rc::new(output)))
    }
}

fn decode_descriptor_tuple(
    value: &FormatValue,
    targets: &[RuntimeTypeDescriptor],
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let FormatValue::Array(items) = value else {
        return Err(shape_error(path, "Tuple", value));
    };
    if !targets
        .iter()
        .all(|target| runtime_descriptor_target(target, graph))
    {
        return Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode Tuple target contains a non-decodable type"
        )));
    }
    if items.len() != targets.len() {
        return Err(DecodeError::Shape(ShapeError {
            path: path.0.clone(),
            expected: format!("Tuple with {} items", targets.len()),
            found: format!("Array with {} items", items.len()),
        }));
    }
    items
        .iter()
        .zip(targets)
        .enumerate()
        .map(|(index, (item, target))| {
            decode_descriptor_at(item, target, graph, &path.index(index), format_name)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|values| Value::Tuple(Rc::new(values)))
}

fn decode_descriptor_map(
    value: &FormatValue,
    key_target: &RuntimeTypeDescriptor,
    value_target: &RuntimeTypeDescriptor,
    graph: &RuntimeTypeGraph,
    path: &FormatPath,
    format_name: &str,
) -> Result<Value, DecodeError> {
    let FormatValue::Object(object) = value else {
        return Err(shape_error(path, "Map", value));
    };
    if !runtime_descriptor_target(key_target, graph)
        || !runtime_descriptor_target(value_target, graph)
    {
        return Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode Map target contains a non-decodable type"
        )));
    }
    object
        .iter()
        .map(|(key, value)| {
            let entry_path = path.field(key);
            Ok((
                decode_descriptor_at(
                    &FormatValue::Text(key.clone()),
                    key_target,
                    graph,
                    &entry_path,
                    format_name,
                )?,
                decode_descriptor_at(value, value_target, graph, &entry_path, format_name)?,
            ))
        })
        .collect::<Result<Vec<_>, DecodeError>>()
        .map(|entries| Value::Map(Rc::new(entries)))
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
        RuntimeTypeDescriptor::Optional(inner) | RuntimeTypeDescriptor::Nullable(inner) => {
            runtime_descriptor_target_inner(inner, graph, visited)
        }
        RuntimeTypeDescriptor::Apply { callee, args }
            if matches!(callee.as_ref(), RuntimeTypeDescriptor::Named(name) if matches!(BuiltinType::from_name(name), Some(BuiltinType::Array | BuiltinType::Set)))
                && args.len() == 1 =>
        {
            runtime_descriptor_target_inner(&args[0], graph, visited)
        }
        RuntimeTypeDescriptor::Apply { callee, args }
            if matches!(callee.as_ref(), RuntimeTypeDescriptor::Named(name) if BuiltinType::from_name(name) == Some(BuiltinType::Map))
                && args.len() == 2 =>
        {
            args.iter()
                .all(|arg| runtime_descriptor_target_inner(arg, graph, visited))
        }
        RuntimeTypeDescriptor::Tuple(items) => items
            .iter()
            .all(|item| runtime_descriptor_target_inner(item, graph, visited)),
        RuntimeTypeDescriptor::Record(fields) => fields
            .iter()
            .all(|(_, field)| runtime_descriptor_target_inner(field, graph, visited)),
        RuntimeTypeDescriptor::Recursive { id, .. } if visited.insert(*id) => graph
            .unfolding(*id)
            .is_some_and(|head| runtime_descriptor_target_inner(head, graph, visited)),
        RuntimeTypeDescriptor::Recursive { .. } => true,
        RuntimeTypeDescriptor::Apply { .. }
        | RuntimeTypeDescriptor::Function { .. }
        | RuntimeTypeDescriptor::SlotRecord { .. }
        | RuntimeTypeDescriptor::Variant(_) => false,
    }
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

pub(crate) fn encode_error_value(message: impl Into<String>) -> Value {
    Value::Tag {
        name: "Encode".to_owned(),
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
    fn canonical_compound_descriptors_decode_without_value_shape_walkers() {
        let named = |name: &str| RuntimeTypeDescriptor::Named(name.to_owned());
        let apply = |name: &str, args| RuntimeTypeDescriptor::Apply {
            callee: Box::new(named(name)),
            args,
        };
        let target = Value::Type(RuntimeType::new(RuntimeTypeDescriptor::Tuple(vec![
            apply("Set", vec![named("Text")]),
            apply("Map", vec![named("Text"), named("Int")]),
        ])));
        let value = FormatValue::Array(vec![
            FormatValue::Array(vec![
                FormatValue::Text("a".to_owned()),
                FormatValue::Text("a".to_owned()),
            ]),
            FormatValue::Object(vec![(
                "answer".to_owned(),
                FormatValue::Number(FormatNumber::Int(Int::from(42))),
            )]),
        ]);

        let decoded = match decode_value(&value, &target, "Json") {
            Ok(decoded) => decoded,
            Err(DecodeError::Shape(error)) => panic!("unexpected shape error: {error:?}"),
            Err(DecodeError::InvalidTarget(message)) => panic!("{message}"),
        };

        assert_eq!(
            decoded,
            Value::Tuple(Rc::new(vec![
                Value::Set(Rc::new(vec![Value::Text("a".to_owned())])),
                Value::Map(Rc::new(vec![(
                    Value::Text("answer".to_owned()),
                    Value::int(42),
                )])),
            ]))
        );
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
                    RuntimeTypeDescriptor::Apply {
                        callee: Box::new(RuntimeTypeDescriptor::Named("Array".to_owned())),
                        args: vec![RuntimeTypeDescriptor::Recursive {
                            id,
                            name: "Tree".to_owned(),
                        }],
                    },
                ),
            ]),
        )]));
        let target = Value::recursive_type(id, "Tree", graph);
        let mut input = FormatValue::Object(vec![
            (
                "value".to_owned(),
                FormatValue::Number(FormatNumber::Int(Int::from(100))),
            ),
            ("children".to_owned(), FormatValue::Array(Vec::new())),
        ]);
        for value in (0..100).rev() {
            input = FormatValue::Object(vec![
                (
                    "value".to_owned(),
                    FormatValue::Number(FormatNumber::Int(Int::from(value))),
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
            assert_eq!(record_field(fields, "value"), &Value::int(expected));
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
        assert_eq!(record_field(fields, "value"), &Value::int(100));
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
