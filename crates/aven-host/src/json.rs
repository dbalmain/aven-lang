//! JSON codec host namespace.
//!
//! `Json.encode` is an ordinary native returning `Text`. `Json.decode` returns
//! an Aven `Result`, using the checker's host-comptime resolver to refine the
//! result type from the optional trailing target type argument.

use std::fmt;
use std::rc::Rc;

use aven_check::{ComptimeArg, ComptimeError, HostComptimeFn, Type};
use aven_eval::{RuntimeType, Value};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value};

impl Host {
    /// Register the `Json` namespace and the named `Json`/`JsonError` types.
    pub fn register_json(&mut self) {
        self.register("Json", json_value(), crate::json_type());
        self.register_type_definition("Json", crate::json_dynamic_type());
        self.register_type_definition("JsonError", crate::json_error_type());
        self.register_comptime_resolver("Json.decode", vec![1], decode_comptime_resolver());
    }
}

#[derive(Debug, Clone)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

#[derive(Debug, Clone, Copy)]
enum JsonNumber {
    Int(i64),
    Float(f64),
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonValueVisitor)
    }
}

struct JsonValueVisitor;

impl<'de> Visitor<'de> for JsonValueVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::Number(JsonNumber::Int(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let number = i64::try_from(value)
            .map(JsonNumber::Int)
            .unwrap_or_else(|_| JsonNumber::Float(value as f64));
        Ok(JsonValue::Number(number))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::Number(JsonNumber::Float(value)))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::String(value))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element()? {
            values.push(value);
        }
        Ok(JsonValue::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        while let Some(key) = map.next_key()? {
            entries.push((key, map.next_value()?));
        }
        Ok(JsonValue::Object(entries))
    }
}

struct DecodeComptimeResolver;

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
            crate::build::named("JsonError"),
        ))
    }
}

pub(crate) fn decode_comptime_resolver() -> Rc<dyn HostComptimeFn> {
    Rc::new(DecodeComptimeResolver)
}

fn json_value() -> Value {
    Value::record(vec![
        ("encode".to_owned(), encode_native()),
        ("decode".to_owned(), decode_native()),
    ])
}

fn encode_native() -> Value {
    Value::native(|args| {
        let [value] = args else {
            return Err(format!(
                "Json.encode expects 1 argument, got {}",
                args.len()
            ));
        };

        encode_to_text(value).map(Value::Text)
    })
}

pub(crate) fn encode_to_text(value: &Value) -> Result<String, String> {
    let mut output = String::new();
    encode_value(value, EncodePosition::TopLevel, &mut output)?;
    Ok(output)
}

fn decode_native() -> Value {
    Value::native(|args| {
        if args.len() > 2 {
            return Err(format!(
                "Json.decode expects 1 or 2 arguments, got {}",
                args.len()
            ));
        }
        let (text, target) = match args {
            [Value::Text(text)] => (text, None),
            [Value::Text(text), target] => (text, Some(target)),
            [other] | [other, ..] => {
                return Err(format!(
                    "Json.decode expects Text input, got {}",
                    aven_value_type_name(other)
                ));
            }
            [] => {
                return Err("Json.decode expects at least 1 argument, got 0".to_owned());
            }
        };

        let parsed = match serde_json::from_str::<JsonValue>(text) {
            Ok(value) => value,
            Err(error) => return Ok(err_value(parse_error_value(error.to_string()))),
        };

        let default_target = Value::named_type("Json");
        let target = target.unwrap_or(&default_target);
        match decode_value(&parsed, target, &JsonPath::root()) {
            Ok(value) => Ok(ok_value(value)),
            Err(DecodeError::Shape(error)) => Ok(err_value(shape_error_value(error))),
            Err(DecodeError::InvalidTarget(message)) => Err(message),
        }
    })
}

#[derive(Clone, Copy)]
enum EncodePosition {
    TopLevel,
    RecordField,
    ArrayElement,
}

fn encode_value(
    value: &Value,
    position: EncodePosition,
    output: &mut String,
) -> Result<(), String> {
    match value {
        Value::Int(value) => output.push_str(&value.to_string()),
        Value::Float(value) if value.is_finite() => output.push_str(&value.to_string()),
        Value::Float(_) => return Err("Json.encode cannot encode NaN or infinite Float".to_owned()),
        Value::Text(value) => encode_string(value, output),
        Value::Bool(true) => output.push_str("true"),
        Value::Bool(false) => output.push_str("false"),
        Value::Null => output.push_str("null"),
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => {
            encode_sequence(values, output)?;
        }
        Value::Map(_) => return Err("Json.encode cannot encode Map".to_owned()),
        Value::Record(fields) => encode_record(fields, output)?,
        Value::Undefined => match position {
            EncodePosition::RecordField => {}
            EncodePosition::TopLevel => {
                return Err("Json.encode cannot encode top-level undefined".to_owned());
            }
            EncodePosition::ArrayElement => {
                return Err("Json.encode cannot encode undefined array elements".to_owned());
            }
        },
        Value::Tag { name, payload } => {
            if encode_json_constructor(name, payload, output)? {
                return Ok(());
            }
            if payload.is_empty() {
                return Err(format!(
                    "Json.encode cannot encode nullary tag @{name}; JSON tag wire form is not decided"
                ));
            }
            return Err(format!(
                "Json.encode cannot encode tag @{name} with payload"
            ));
        }
        Value::Closure(_) => return Err("Json.encode cannot encode Function".to_owned()),
        Value::Native(_) => return Err("Json.encode cannot encode Native".to_owned()),
        Value::Type(_) => return Err("Json.encode cannot encode Type".to_owned()),
    }

    Ok(())
}

fn encode_json_constructor(
    name: &str,
    payload: &[Value],
    output: &mut String,
) -> Result<bool, String> {
    match name {
        "Null" => {
            let [] = payload else {
                return Err(json_constructor_shape_error(name, "no payload"));
            };
            output.push_str("null");
        }
        "Bool" => {
            let [Value::Bool(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Bool"));
            };
            output.push_str(if *value { "true" } else { "false" });
        }
        "Int" => {
            let [Value::Int(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Int"));
            };
            output.push_str(&value.to_string());
        }
        "Float" => {
            let [Value::Float(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Float"));
            };
            if !value.is_finite() {
                return Err("Json.encode cannot encode NaN or infinite Float".to_owned());
            }
            output.push_str(&value.to_string());
        }
        "Text" => {
            let [Value::Text(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Text"));
            };
            encode_string(value, output);
        }
        "Array" => {
            let [Value::Array(values)] = payload else {
                return Err(json_constructor_shape_error(name, "Array[Json]"));
            };
            encode_json_array(values, output)?;
        }
        "Object" => {
            let [Value::Map(entries)] = payload else {
                return Err(json_constructor_shape_error(name, "Map[Text, Json]"));
            };
            encode_json_object(entries, output)?;
        }
        _ => return Ok(false),
    }

    Ok(true)
}

fn encode_json_array(values: &[Value], output: &mut String) -> Result<(), String> {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        encode_json_value(value, output)?;
    }
    output.push(']');
    Ok(())
}

fn encode_json_object(entries: &[(Value, Value)], output: &mut String) -> Result<(), String> {
    output.push('{');
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        let Value::Text(key) = key else {
            return Err(json_constructor_shape_error("Object", "Map[Text, Json]"));
        };
        encode_string(key, output);
        output.push(':');
        encode_json_value(value, output)?;
    }
    output.push('}');
    Ok(())
}

fn encode_json_value(value: &Value, output: &mut String) -> Result<(), String> {
    let Value::Tag { name, payload } = value else {
        return Err(format!(
            "Json.encode expected Json constructor tag, got {}",
            aven_value_type_name(value)
        ));
    };
    if encode_json_constructor(name, payload, output)? {
        Ok(())
    } else {
        Err(format!(
            "Json.encode expected Json constructor tag, got @{name}"
        ))
    }
}

fn json_constructor_shape_error(name: &str, expected: &str) -> String {
    format!("Json.encode expected @{name} payload shape {expected}")
}

fn encode_record(fields: &[(String, Value)], output: &mut String) -> Result<(), String> {
    output.push('{');
    let mut first = true;
    for (name, value) in fields {
        if matches!(value, Value::Undefined) {
            continue;
        }

        if first {
            first = false;
        } else {
            output.push(',');
        }

        encode_string(name, output);
        output.push(':');
        encode_value(value, EncodePosition::RecordField, output)?;
    }
    output.push('}');
    Ok(())
}

fn encode_sequence(values: &[Value], output: &mut String) -> Result<(), String> {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        encode_value(value, EncodePosition::ArrayElement, output)?;
    }
    output.push(']');
    Ok(())
}

fn encode_string(value: &str, output: &mut String) {
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                output.push_str("\\u");
                push_hex4(ch as u32, output);
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
}

fn push_hex4(value: u32, output: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for shift in [12, 8, 4, 0] {
        let digit = ((value >> shift) & 0xF) as usize;
        output.push(char::from(HEX[digit]));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShapeError {
    path: String,
    expected: String,
    found: String,
}

enum DecodeError {
    Shape(ShapeError),
    InvalidTarget(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsonPath(String);

impl JsonPath {
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

fn decode_value(value: &JsonValue, target: &Value, path: &JsonPath) -> Result<Value, DecodeError> {
    if json_namespace_target(target) {
        return decode_named(value, "Json", path);
    }

    match target {
        Value::Type(RuntimeType::Named(name)) => decode_named(value, name, path),
        Value::Type(RuntimeType::Optional(inner)) => decode_value(value, inner, path),
        Value::Type(RuntimeType::Nullable(inner)) => {
            if matches!(value, JsonValue::Null) {
                Ok(Value::Null)
            } else {
                decode_value(value, inner, path)
            }
        }
        Value::Type(RuntimeType::Array(inner)) => decode_array(value, inner, path),
        Value::Record(fields) if runtime_type_target(target) => decode_record(value, fields, path),
        other => Err(DecodeError::InvalidTarget(format!(
            "Json.decode target must be a type value or record of type values, got {}",
            aven_value_type_name(other)
        ))),
    }
}

fn decode_named(value: &JsonValue, name: &str, path: &JsonPath) -> Result<Value, DecodeError> {
    match name {
        "Json" => return Ok(decode_dynamic_json(value)),
        "Text" => match value {
            JsonValue::String(text) => Some(Value::Text(text.clone())),
            _ => None,
        },
        "Int" => match value {
            JsonValue::Number(JsonNumber::Int(value)) => Some(Value::Int(*value)),
            _ => None,
        },
        "Float" => match value {
            JsonValue::Number(JsonNumber::Int(value)) => Some(Value::Float(*value as f64)),
            JsonValue::Number(JsonNumber::Float(value)) => Some(Value::Float(*value)),
            _ => None,
        },
        "Bool" => match value {
            JsonValue::Bool(value) => Some(Value::Bool(*value)),
            _ => None,
        },
        "Null" if matches!(value, JsonValue::Null) => Some(Value::Null),
        "Null" => None,
        "Undefined" => None,
        "Array" => {
            return Err(DecodeError::InvalidTarget(
                "Json.decode target Array must be written as Array[T]".to_owned(),
            ));
        }
        unsupported => {
            return Err(DecodeError::InvalidTarget(format!(
                "Json.decode cannot decode target type {unsupported}"
            )));
        }
    }
    .ok_or_else(|| shape_error(path, name, value))
}

fn decode_dynamic_json(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => json_tag("Null", Vec::new()),
        JsonValue::Bool(value) => json_tag("Bool", vec![Value::Bool(*value)]),
        JsonValue::Number(JsonNumber::Int(value)) => json_tag("Int", vec![Value::Int(*value)]),
        JsonValue::Number(JsonNumber::Float(value)) => {
            json_tag("Float", vec![Value::Float(*value)])
        }
        JsonValue::String(value) => json_tag("Text", vec![Value::Text(value.clone())]),
        JsonValue::Array(values) => {
            let values = values.iter().map(decode_dynamic_json).collect();
            json_tag("Array", vec![Value::Array(Rc::new(values))])
        }
        JsonValue::Object(entries) => {
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
    value: &JsonValue,
    fields: &[(String, Value)],
    path: &JsonPath,
) -> Result<Value, DecodeError> {
    let JsonValue::Object(object) = value else {
        return Err(shape_error(path, "Record", value));
    };

    let mut output = Vec::with_capacity(fields.len());
    for (name, target) in fields {
        if !runtime_type_target(target) {
            return Err(DecodeError::InvalidTarget(format!(
                "Json.decode target field `{name}` must be a type value, got {}",
                aven_value_type_name(target)
            )));
        }

        let field_path = path.field(name);
        let field = match object
            .iter()
            .find_map(|(field_name, field_value)| (field_name == name).then_some(field_value))
        {
            Some(field_value) => decode_value(field_value, target, &field_path)?,
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

fn decode_array(value: &JsonValue, target: &Value, path: &JsonPath) -> Result<Value, DecodeError> {
    let JsonValue::Array(items) = value else {
        return Err(shape_error(path, &target_display_array(target), value));
    };
    if !runtime_type_target(target) {
        return Err(DecodeError::InvalidTarget(format!(
            "Json.decode Array target must be a type value, got {}",
            aven_value_type_name(target)
        )));
    }

    let mut output = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        output.push(decode_value(item, target, &path.index(index))?);
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

/// In checker-free runs, `Json.decode(text, Json)` passes the `Json`
/// namespace record itself as the target (the global name is the namespace,
/// not a type value). This shape test must track `json_value`'s field list.
fn json_namespace_target(value: &Value) -> bool {
    let Value::Record(fields) = value else {
        return false;
    };
    let [encode, decode] = fields.as_slice() else {
        return false;
    };
    matches!(
        (encode, decode),
        ((encode_name, Value::Native(_)), (decode_name, Value::Native(_)))
            if encode_name == "encode" && decode_name == "decode"
    )
}

fn target_display(target: &Value) -> String {
    target.to_string()
}

fn target_display_array(target: &Value) -> String {
    format!("Array[{}]", target_display(target))
}

fn shape_error(path: &JsonPath, expected: &str, found: &JsonValue) -> DecodeError {
    DecodeError::Shape(ShapeError {
        path: path.0.clone(),
        expected: expected.to_owned(),
        found: found_kind(found),
    })
}

fn found_kind(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "Null",
        JsonValue::Bool(_) => "Bool",
        JsonValue::Number(JsonNumber::Int(_)) => "Int",
        JsonValue::Number(JsonNumber::Float(_)) => "Float",
        JsonValue::String(_) => "Text",
        JsonValue::Array(_) => "Array",
        JsonValue::Object(_) => "Record",
    }
    .to_owned()
}

fn parse_error_value(message: String) -> Value {
    Value::Tag {
        name: "Parse".to_owned(),
        payload: vec![Value::record(vec![(
            "message".to_owned(),
            Value::Text(message),
        )])],
    }
}

fn shape_error_value(error: ShapeError) -> Value {
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

    use aven_core::{Span, codes};
    use aven_parser::parse_module;

    fn json_host() -> Host {
        let mut host = Host::new();
        host.register_json();
        host
    }

    fn run(source: &str) -> Value {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        let outcome =
            aven_eval::eval_module_with_globals(&parsed.module, json_host().eval_globals());
        assert!(
            outcome.diagnostics.is_empty(),
            "program runs: {:?}",
            outcome.diagnostics
        );
        outcome.value.expect("program yields a value")
    }

    fn run_diagnostics(source: &str) -> Vec<aven_core::Diagnostic> {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        aven_eval::eval_module_with_globals(&parsed.module, json_host().eval_globals()).diagnostics
    }

    fn check(source: &str) -> aven_check::CheckOutput {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        aven_check::check_module_with_host_globals(
            &parsed.module,
            &json_host().check_host_globals(),
        )
    }

    fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
        let Value::Record(fields) = value else {
            panic!("expected a record, got {value:?}");
        };
        fields
            .iter()
            .find_map(|(field_name, field_value)| (field_name == name).then_some(field_value))
            .unwrap_or_else(|| panic!("record has field `{name}`"))
    }

    fn text(value: &Value) -> &str {
        let Value::Text(text) = value else {
            panic!("expected Text, got {value:?}");
        };
        text
    }

    fn err_payload(value: &Value) -> (&str, &Value) {
        let Value::Tag { name, payload } = value else {
            panic!("expected Result tag, got {value:?}");
        };
        assert_eq!(name, "Err");
        let [error] = payload.as_slice() else {
            panic!("Err carries one payload");
        };
        let Value::Tag { name, payload } = error else {
            panic!("expected JsonError tag, got {error:?}");
        };
        let [payload] = payload.as_slice() else {
            panic!("JsonError carries one payload record");
        };
        (name.as_str(), payload)
    }

    fn tag_payload<'a>(value: &'a Value, expected: &str) -> &'a [Value] {
        let Value::Tag { name, payload } = value else {
            panic!("expected @{expected}, got {value:?}");
        };
        assert_eq!(name, expected);
        payload
    }

    fn tag_name(value: &Value) -> &str {
        let Value::Tag { name, .. } = value else {
            panic!("expected tag, got {value:?}");
        };
        name
    }

    fn tag_array_payload<'a>(value: &'a Value, expected: &str) -> &'a [Value] {
        let [Value::Array(values)] = tag_payload(value, expected) else {
            panic!("expected @{expected}(Array), got {value:?}");
        };
        values.as_ref()
    }

    fn assert_platform_error_contains(diagnostics: &[aven_core::Diagnostic], message: &str) {
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some(codes::runtime::PLATFORM_ERROR)
                    && diagnostic
                        .labels
                        .iter()
                        .any(|label| label.message.contains(message))
            }),
            "expected runtime platform diagnostic containing {message:?}: {diagnostics:?}"
        );
    }

    #[test]
    fn encode_decode_round_trip_preserves_omitted_and_null_fields() {
        let value = run("User = { name: Text, phone: ?Text, nick: Text? }\n\
             text = Json.encode({ name: \"Ada\", phone: undefined, nick: null })\n\
             decoded = Json.decode(text, User)?!\n\
             { text: text, decoded: decoded }\n");

        assert_eq!(text(field(&value, "text")), r#"{"name":"Ada","nick":null}"#);
        let decoded = field(&value, "decoded");
        assert_eq!(text(field(decoded, "name")), "Ada");
        assert_eq!(field(decoded, "phone"), &Value::Undefined);
        assert_eq!(field(decoded, "nick"), &Value::Null);
    }

    #[test]
    fn dynamic_decode_one_arg_builds_json_constructor_tree() {
        let value = run("Json.decode(\"[1,1.5,1e10,9223372036854775808,true,null]\")?!\n");
        let items = tag_array_payload(&value, "Array");
        let names = items.iter().map(tag_name).collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["Int", "Float", "Float", "Float", "Bool", "Null"]
        );
    }

    #[test]
    fn dynamic_decode_explicit_json_target_uses_namespace_and_preserves_order() {
        let value = run(
            "parsed = Json.decode(\"{\\\"b\\\":1,\\\"a\\\":2}\", Json)?!\n\
             Json.encode(parsed)\n",
        );

        assert_eq!(text(&value), r#"{"b":1,"a":2}"#);
    }

    #[test]
    fn encode_accepts_structural_json_constructor_tags() {
        let value = run(
            "Json.encode(@Object(Map.from([(\"b\", @Int(1)), (\"a\", @Array([@Bool(true), @Null]))])))\n",
        );

        assert_eq!(text(&value), r#"{"b":1,"a":[true,null]}"#);
    }

    #[test]
    fn encode_rejects_json_constructor_with_wrong_payload_shape() {
        let diagnostics = run_diagnostics("Json.encode(@Int(\"no\"))\n");

        assert_platform_error_contains(&diagnostics, "Json.encode expected @Int payload shape Int");
    }

    #[test]
    fn encode_rejects_undefined_outside_record_fields() {
        let parsed = parse_module("Json.encode([undefined])\n");
        assert!(parsed.diagnostics.is_empty(), "program parses");
        let outcome =
            aven_eval::eval_module_with_globals(&parsed.module, json_host().eval_globals());
        assert!(
            outcome.diagnostics.iter().any(
                |diagnostic| diagnostic.code.as_deref() == Some(codes::runtime::PLATFORM_ERROR)
            ),
            "undefined array element is a platform error: {:?}",
            outcome.diagnostics
        );
    }

    #[test]
    fn encode_rejects_tags_until_a_wire_form_is_decided() {
        let parsed = parse_module("Json.encode(@Red)\n");
        assert!(parsed.diagnostics.is_empty(), "program parses");
        let outcome =
            aven_eval::eval_module_with_globals(&parsed.module, json_host().eval_globals());
        assert!(
            outcome.diagnostics.iter().any(
                |diagnostic| diagnostic.code.as_deref() == Some(codes::runtime::PLATFORM_ERROR)
            ),
            "nullary tags are a platform error: {:?}",
            outcome.diagnostics
        );
    }

    #[test]
    fn decode_parse_error_returns_structured_result_error() {
        let value = run("Json.decode(\"{\", { name: Text })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Parse");
        assert!(text(field(payload, "message")).contains("EOF"));
    }

    #[test]
    fn decode_missing_required_field_reports_shape_path() {
        let value = run("Json.decode(\"{}\", { name: Text })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.name");
        assert_eq!(text(field(payload, "expected")), "Text");
        assert_eq!(text(field(payload, "found")), "Undefined");
    }

    #[test]
    fn decode_wrong_scalar_kind_reports_shape() {
        let value = run("Json.decode(\"{\\\"name\\\":1}\", { name: Text })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.name");
        assert_eq!(text(field(payload, "expected")), "Text");
        assert_eq!(text(field(payload, "found")), "Int");
    }

    #[test]
    fn decode_null_into_non_nullable_reports_shape() {
        let value = run("Json.decode(\"{\\\"name\\\":null}\", { name: Text })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.name");
        assert_eq!(text(field(payload, "found")), "Null");
    }

    #[test]
    fn decode_nested_shape_error_reports_precise_path() {
        let value = run("Target = { a: Array[{ b: Text }] }\n\
             Json.decode(\"{\\\"a\\\":[{\\\"b\\\":\\\"ok\\\"},{\\\"b\\\":1}]}\", Target)\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.a[1].b");
        assert_eq!(text(field(payload, "expected")), "Text");
        assert_eq!(text(field(payload, "found")), "Int");
    }

    #[test]
    fn checker_resolves_decode_result_from_type_argument() {
        let source = "User = { name: Text }\ntext = \"{}\"\ndecoded = Json.decode(text, User)\n";
        let checked = check(source);
        assert!(
            checked.diagnostics.is_empty(),
            "program checks: {:?}",
            checked.diagnostics
        );
        let offset = source.find("decoded").expect("source mentions decoded");
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .expect("decoded has an inferred type");

        assert_eq!(ty.render(), "Result[User, JsonError]");
    }

    #[test]
    fn checker_defers_decode_with_runtime_type_argument() {
        let source =
            "text = \"{}\"\ntarget = { name: \"Text\" }\ndecoded = Json.decode(text, target)\n";
        let checked = check(source);
        assert!(
            checked.diagnostics.is_empty(),
            "runtime target defers without diagnostics: {:?}",
            checked.diagnostics
        );
        let offset = source.find("decoded").expect("source mentions decoded");
        assert_eq!(
            checked.type_at(Span::new(offset, offset + "decoded".len())),
            None
        );
    }

    #[test]
    fn checker_resolves_one_arg_decode_to_dynamic_json_result() {
        let checked = check("text = \"{}\"\ndecoded = Json.decode(text)\n");

        assert!(
            checked.diagnostics.is_empty(),
            "one-arg dynamic decode checks: {:?}",
            checked.diagnostics
        );
        let source = "text = \"{}\"\ndecoded = Json.decode(text)\n";
        let offset = source.find("decoded").expect("source mentions decoded");
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .expect("decoded has an inferred type");

        assert_eq!(ty.render(), "Result[Json, JsonError]");
    }

    #[test]
    fn checker_resolves_explicit_json_decode_to_dynamic_json_result() {
        let source = "text = \"{}\"\ndecoded = Json.decode(text, Json)\n";
        let checked = check(source);

        assert!(
            checked.diagnostics.is_empty(),
            "explicit Json target checks: {:?}",
            checked.diagnostics
        );
        let offset = source.find("decoded").expect("source mentions decoded");
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .expect("decoded has an inferred type");

        assert_eq!(ty.render(), "Result[Json, JsonError]");
    }

    #[test]
    fn match_binder_over_decoded_json_records_hover_type() {
        let source = "parsed = Json.decode(\"{\\\"x\\\": 2}\")?^\n\
                      parsed ?>\n  @Object(fields) => 1\n  _ => 3\n";
        let checked = check(source);

        assert!(
            checked.diagnostics.is_empty(),
            "decoded match checks: {:?}",
            checked.diagnostics
        );
        let parsed_offset = source.find("parsed").expect("source mentions parsed");
        let parsed_ty = checked
            .type_at(Span::new(parsed_offset, parsed_offset + "parsed".len()))
            .expect("parsed has an inferred type");
        assert_eq!(parsed_ty.render(), "Json");

        let fields_offset = source.find("fields").expect("source mentions fields");
        let fields_ty = checked
            .type_at(Span::new(fields_offset, fields_offset + "fields".len()))
            .expect("the match binder has an inferred type");
        assert_eq!(fields_ty.render(), "Map[Text, Json]");
    }
}
