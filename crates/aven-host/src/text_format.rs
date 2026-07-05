use std::rc::Rc;

use aven_check::{ComptimeArg, ComptimeError, HostComptimeFn, Type};
use aven_eval::{RuntimeType, Value};

use crate::io::aven_value_type_name;

#[derive(Debug, Clone)]
pub(crate) enum FormatValue {
    Null,
    Bool(bool),
    Number(FormatNumber),
    Text(String),
    Array(Vec<FormatValue>),
    Object(Vec<(String, FormatValue)>),
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
            [] => crate::build::named("Json"),
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
        Value::Record(fields) if runtime_type_target(target) => {
            decode_record(value, fields, path, format_name)
        }
        other => Err(DecodeError::InvalidTarget(format!(
            "{format_name}.decode target must be a type value or record of type values, got {}",
            aven_value_type_name(other)
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
        "Json" | "Yaml" | "Toml" => return Ok(decode_dynamic_json(value)),
        "Text" => match value {
            FormatValue::Text(text) => Some(Value::Text(text.clone())),
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
        "Array" => {
            return Err(DecodeError::InvalidTarget(format!(
                "{format_name}.decode target Array must be written as Array[T]"
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

fn decode_dynamic_json(value: &FormatValue) -> Value {
    match value {
        FormatValue::Null => json_tag("Null", Vec::new()),
        FormatValue::Bool(value) => json_tag("Bool", vec![Value::Bool(*value)]),
        FormatValue::Number(FormatNumber::Int(value)) => json_tag("Int", vec![Value::Int(*value)]),
        FormatValue::Number(FormatNumber::Float(value)) => {
            json_tag("Float", vec![Value::Float(*value)])
        }
        FormatValue::Text(value) => json_tag("Text", vec![Value::Text(value.clone())]),
        FormatValue::Array(values) => {
            let values = values.iter().map(decode_dynamic_json).collect();
            json_tag("Array", vec![Value::Array(Rc::new(values))])
        }
        FormatValue::Object(entries) => {
            let entries = entries
                .iter()
                .map(|(key, value)| (Value::Text(key.clone()), decode_dynamic_json(value)))
                .collect();
            json_tag("Object", vec![Value::Map(Rc::new(entries))])
        }
    }
}

fn json_tag(name: &str, payload: Vec<Value>) -> Value {
    Value::Tag {
        name: name.to_owned(),
        payload,
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

fn target_is_optional(target: &Value) -> bool {
    matches!(target, Value::Type(RuntimeType::Optional(_)))
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
    format!("Array[{}]", target_display(target))
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
        FormatValue::Null => "Null",
        FormatValue::Bool(_) => "Bool",
        FormatValue::Number(FormatNumber::Int(_)) => "Int",
        FormatValue::Number(FormatNumber::Float(_)) => "Float",
        FormatValue::Text(_) => "Text",
        FormatValue::Array(_) => "Array",
        FormatValue::Object(_) => "Record",
    }
    .to_owned()
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
    fn dynamic_decode_accepts_registered_format_names() {
        let value = FormatValue::Object(vec![(
            "name".to_owned(),
            FormatValue::Text("Ada".to_owned()),
        )]);

        for target_name in ["Json", "Yaml", "Toml"] {
            let target = Value::named_type(target_name);
            let decoded = match decode_value(&value, &target, target_name) {
                Ok(decoded) => decoded,
                Err(DecodeError::Shape(_)) => panic!("{target_name} dynamic decode shaped"),
                Err(DecodeError::InvalidTarget(message)) => panic!("{message}"),
            };

            let Value::Tag { name, payload } = decoded else {
                panic!("expected dynamic object tag, got {decoded:?}");
            };
            assert_eq!(name, "Object");
            assert_eq!(payload.len(), 1);
        }
    }
}
