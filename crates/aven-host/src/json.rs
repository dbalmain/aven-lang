//! JSON codec host namespace.
//!
//! Both JSON codec natives return Aven `Result` values. `Json.decode` uses the
//! checker's host-comptime resolver to refine its result type from the optional
//! trailing target type argument.

use std::fmt;

use aven_eval::Value;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value};
use crate::temporal::temporal_iso_text;
use crate::text_format::{
    DecodeError, FormatNumber, FormatValue, decode_value, encode_error_value, parse_error_value,
    shape_error_value,
};

type JsonValue = FormatValue;
type JsonNumber = FormatNumber;

impl Host {
    /// Register the `Json` type artifact (carrying `encode`/`decode` statics),
    /// the shared `Data` dynamic type, and the named `JsonError` type.
    pub fn register_json(&mut self) {
        self.register_data_type();
        self.register_type_statics(
            "Json",
            vec![
                (
                    "encode".to_owned(),
                    crate::json_encode_type(),
                    encode_native(),
                ),
                (
                    "decode".to_owned(),
                    crate::json_decode_base_type(),
                    decode_native(),
                ),
            ],
        );
        self.register_type_definition("JsonError", crate::json_error_type());
        self.register_type_definition("JsonEncodeError", crate::json_encode_error_type());
        self.register_comptime_resolver("Json.decode", vec![1], decode_comptime_resolver());
    }
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
        Ok(JsonValue::Text(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::Text(value))
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

pub(crate) fn decode_comptime_resolver() -> std::rc::Rc<dyn aven_check::HostComptimeFn> {
    crate::text_format::decode_comptime_resolver("JsonError")
}

fn encode_native() -> Value {
    Value::native(|args| {
        let [value] = args else {
            return Err(format!(
                "Json.encode expects 1 argument, got {}",
                args.len()
            ));
        };

        Ok(match encode_to_text(value) {
            Ok(text) => ok_value(Value::Text(text)),
            Err(error) => err_value(encode_error_value(error)),
        })
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

        let default_target = Value::named_type("Data");
        let target = target.unwrap_or(&default_target);
        match decode_value(&parsed, target, "Json") {
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
    if let Some(text) = temporal_iso_text(value) {
        encode_string(&text, output);
        return Ok(());
    }

    match value {
        Value::Int(value) => output.push_str(&value.to_string()),
        Value::Float(value) if value.is_finite() => output.push_str(&value.to_string()),
        Value::Float(value) => return Err(json_non_finite_float_error(*value)),
        Value::Text(value) => encode_string(value, output),
        Value::Bool(true) => output.push_str("true"),
        Value::Bool(false) => output.push_str("false"),
        Value::Null => output.push_str("null"),
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => {
            encode_sequence(values, output)?;
        }
        Value::Map(_) => return Err("Json.encode cannot encode Map".to_owned()),
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
            encode_record(fields, output)?;
        }
        Value::SlotRecord { fields, .. } => encode_record(fields, output)?,
        Value::BrandedPrimitive { payload, .. } => {
            return encode_value(&payload.to_value(), position, output);
        }
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
        Value::ResultMethod { .. } => return Err("Json.encode cannot encode Function".to_owned()),
        Value::NamedMethod { .. } | Value::UnboundNamedMethod { .. } => {
            return Err("Json.encode cannot encode Function".to_owned());
        }
        Value::Closure(_) => return Err("Json.encode cannot encode Function".to_owned()),
        Value::Native(_) => return Err("Json.encode cannot encode Native".to_owned()),
        Value::Type(_) => return Err("Json.encode cannot encode Type".to_owned()),
        Value::NamedFamily(_) => return Err("Json.encode cannot encode Type".to_owned()),
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
                return Err(json_non_finite_float_error(*value));
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
                return Err(json_constructor_shape_error(name, "Array(Data)"));
            };
            encode_json_array(values, output)?;
        }
        "Object" => {
            let [Value::Map(entries)] = payload else {
                return Err(json_constructor_shape_error(name, "Map(Text, Data)"));
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
            return Err(json_constructor_shape_error("Object", "Map(Text, Data)"));
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
            "Json.encode expected Data constructor tag, got {}",
            aven_value_type_name(value)
        ));
    };
    if encode_json_constructor(name, payload, output)? {
        Ok(())
    } else {
        Err(format!(
            "Json.encode expected Data constructor tag, got @{name}"
        ))
    }
}

fn json_constructor_shape_error(name: &str, expected: &str) -> String {
    format!("Json.encode expected @{name} payload shape {expected}")
}

fn json_non_finite_float_error(value: f64) -> String {
    let kind = if value.is_nan() {
        "NaN"
    } else if value.is_sign_positive() {
        "Infinity"
    } else {
        "-Infinity"
    };
    format!("Json.encode cannot encode non-finite Float {kind}")
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::rc::Rc;

    use aven_core::{Span, codes};
    use aven_parser::parse_module;

    fn json_host() -> Host {
        let mut host = Host::new();
        host.register_json();
        host.register_temporals();
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
             text = Json.encode({ name: \"Ada\", phone: undefined, nick: null })?!\n\
             decoded = Json.decode(text, User)?!\n\
             { text: text, decoded: decoded }\n");

        assert_eq!(text(field(&value, "text")), r#"{"name":"Ada","nick":null}"#);
        let decoded = field(&value, "decoded");
        assert_eq!(text(field(decoded, "name")), "Ada");
        assert_eq!(field(decoded, "phone"), &Value::Undefined);
        assert_eq!(field(decoded, "nick"), &Value::Null);
    }

    #[test]
    fn dynamic_decode_one_arg_builds_data_constructor_tree() {
        let value = run("Json.decode(\"[1,1.5,1e10,9223372036854775808,true,null]\")?!\n");
        let items = tag_array_payload(&value, "Array");
        let names = items.iter().map(tag_name).collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["Int", "Float", "Float", "Float", "Bool", "Null"]
        );
    }

    #[test]
    fn dynamic_decode_explicit_data_target_preserves_order() {
        let value = run(
            "parsed = Json.decode(\"{\\\"b\\\":1,\\\"a\\\":2}\", Data)?!\n\
             Json.encode(parsed)?!\n",
        );

        assert_eq!(text(&value), r#"{"b":1,"a":2}"#);
    }

    #[test]
    fn dynamic_decode_rejects_old_json_target_name() {
        let checked = check("text = \"{}\"\ndecoded = Json.decode(text, Json)\n");
        assert!(
            checked
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("use `Data`")),
            "old Json target is a check-time error: {:?}",
            checked.diagnostics
        );

        let diagnostics = run_diagnostics("Json.decode(\"{}\", Json)\n");
        assert_platform_error_contains(
            &diagnostics,
            "Json.decode target Json is a format type; use Data",
        );
    }

    #[test]
    fn encode_accepts_structural_json_constructor_tags() {
        let value = run(
            "Json.encode(@Object(Map.from([(\"b\", @Int(1)), (\"a\", @Array([@Bool(true), @Null]))])))?!\n",
        );

        assert_eq!(text(&value), r#"{"b":1,"a":[true,null]}"#);
    }

    #[test]
    fn eval_encode_method_matches_static_form_for_record() {
        let value = run("user = { name: \"Ada\", count: 3 }\n\
             method = user.encode(Json)?!\n\
             direct = Json.encode(user)?!\n\
             { method: method, direct: direct }\n");

        assert_eq!(text(field(&value, "method")), r#"{"name":"Ada","count":3}"#);
        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn eval_encode_method_matches_static_form_for_dynamic_data() {
        let value = run(
            "parsed = Json.decode(\"{\\\"name\\\":\\\"Ada\\\",\\\"ok\\\":true}\")?!\n\
             method = parsed.encode(Json)?!\n\
             direct = Json.encode(parsed)?!\n\
             { method: method, direct: direct }\n",
        );

        assert_eq!(text(field(&value, "method")), r#"{"name":"Ada","ok":true}"#);
        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn eval_encode_method_keeps_receiver_encode_field() {
        let value = run("user = { name: \"Ada\", encode: (format) => \"own\" }\n\
             user.encode(Json)\n");

        assert_eq!(text(&value), "own");
    }

    #[test]
    fn encode_returns_structured_error_for_wrong_constructor_payload_shape() {
        let value = run("Json.encode(@Int(\"no\"))\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert_eq!(
            text(field(payload, "message")),
            "Json.encode expected @Int payload shape Int"
        );
    }

    #[test]
    fn encode_rejects_undefined_outside_record_fields() {
        let value = run("Json.encode([undefined])\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert_eq!(
            text(field(payload, "message")),
            "Json.encode cannot encode undefined array elements"
        );
    }

    #[test]
    fn encode_rejects_tags_until_a_wire_form_is_decided() {
        let value = run("Json.encode(@Red)\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert!(
            text(field(payload, "message")).contains("Json.encode cannot encode nullary tag @Red"),
            "{payload:?}"
        );
    }

    #[test]
    fn encode_rejects_closures_without_invoking_them() {
        let value = run("fail = () => Json.decode(\"not json\")?!\n\
             Json.encode({ child: fail })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert_eq!(
            text(field(payload, "message")),
            "Json.encode cannot encode Function"
        );
    }

    #[test]
    fn encode_returns_non_finite_float_kind_error() {
        let value = run("Json.encode({ value: 0.0 / 0.0 })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert_eq!(
            text(field(payload, "message")),
            "Json.encode cannot encode non-finite Float NaN"
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
        let value = run("Target = { a: Array({ b: Text }) }\n\
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

        assert_eq!(ty.render(), "Result(User, JsonError)");
    }

    #[test]
    fn recursive_decode_target_uses_a_finite_runtime_descriptor() {
        let source = "Node = { value: Int, next: ?Node }\n\
                      Json.decode(\"{\\\"value\\\": 1}\", Node)?!\n";
        let checked = check(source);
        assert!(
            checked.diagnostics.is_empty(),
            "recursive target checks: {:?}",
            checked.diagnostics
        );

        let id = aven_eval::RuntimeTypeId(0);
        let graph = Rc::new(aven_eval::RuntimeTypeGraph::new([(
            id,
            aven_eval::RuntimeTypeDescriptor::Record(vec![
                (
                    "value".to_owned(),
                    aven_eval::RuntimeTypeDescriptor::Named("Int".to_owned()),
                ),
                (
                    "next".to_owned(),
                    aven_eval::RuntimeTypeDescriptor::Optional(Box::new(
                        aven_eval::RuntimeTypeDescriptor::Recursive {
                            id,
                            name: "Node".to_owned(),
                        },
                    )),
                ),
            ]),
        )]));
        let runtime_types = aven_eval::RuntimeTypeBindings::new([(
            "Node".to_owned(),
            Value::recursive_type(id, "Node", graph),
        )]);
        let parsed = parse_module(source);
        let outcome = aven_eval::eval_module_with_globals_imports_and_runtime_types(
            &parsed.module,
            json_host().eval_globals(),
            &aven_eval::ModuleImports::default(),
            &runtime_types,
        );
        assert!(
            outcome.diagnostics.is_empty(),
            "recursive target evaluates without diagnostics: {:?}",
            outcome.diagnostics
        );
        let value = outcome.value.expect("recursive decode returns a value");
        assert_eq!(field(&value, "value"), &Value::Int(1));
        assert_eq!(field(&value, "next"), &Value::Undefined);
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
    fn checker_resolves_one_arg_decode_to_dynamic_data_result() {
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

        assert_eq!(ty.render(), "Result(Data, JsonError)");
    }

    #[test]
    fn checker_resolves_explicit_data_decode_to_dynamic_data_result() {
        let source = "text = \"{}\"\ndecoded = Json.decode(text, Data)\n";
        let checked = check(source);

        assert!(
            checked.diagnostics.is_empty(),
            "explicit Data target checks: {:?}",
            checked.diagnostics
        );
        let offset = source.find("decoded").expect("source mentions decoded");
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .expect("decoded has an inferred type");

        assert_eq!(ty.render(), "Result(Data, JsonError)");
    }

    #[test]
    fn match_binder_over_decoded_data_records_hover_type() {
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
        assert_eq!(parsed_ty.render(), "Data");

        let fields_offset = source.find("fields").expect("source mentions fields");
        let fields_ty = checked
            .type_at(Span::new(fields_offset, fields_offset + "fields".len()))
            .expect("the match binder has an inferred type");
        assert_eq!(fields_ty.render(), "Map(Text, Data)");
    }

    #[test]
    fn checker_text_decode_method_matches_static_form() {
        let method = "User = { name: Text }\ntext = \"{}\"\ndecoded = text.decode(Json, User)\n";
        let static_form =
            "User = { name: Text }\ntext = \"{}\"\ndecoded = Json.decode(text, User)\n";

        let method_checked = check(method);
        assert!(
            method_checked.diagnostics.is_empty(),
            "method form checks: {:?}",
            method_checked.diagnostics
        );

        let offset = method.find("decoded").expect("source mentions decoded");
        let span = Span::new(offset, offset + "decoded".len());
        let method_ty = method_checked
            .type_at(span)
            .expect("method decode has an inferred type");
        assert_eq!(method_ty.render(), "Result(User, JsonError)");

        // The method spelling infers exactly as the format-owned static call.
        let static_checked = check(static_form);
        let static_ty = static_checked
            .type_at(span)
            .expect("static decode has an inferred type");
        assert_eq!(method_ty, static_ty);

        // Hover on the `.decode` access itself shows the resolved call type
        // (structurally expanded, as expression hovers are).
        let offset = method
            .find("text.decode")
            .expect("source mentions text.decode");
        let access_ty = method_checked
            .type_at(Span::new(offset, offset + "text.decode".len()))
            .expect(".decode access has an inferred type");
        assert!(
            access_ty.render().starts_with("Result("),
            "hover on .decode shows the Result type, got {}",
            access_ty.render()
        );
    }

    #[test]
    fn checker_resolves_one_arg_text_decode_to_dynamic_data_result() {
        let source = "text = \"{}\"\ndecoded = text.decode(Json)\n";
        let checked = check(source);

        assert!(
            checked.diagnostics.is_empty(),
            "one-arg method decode checks: {:?}",
            checked.diagnostics
        );
        let offset = source.find("decoded").expect("source mentions decoded");
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .expect("decoded has an inferred type");

        assert_eq!(ty.render(), "Result(Data, JsonError)");
    }

    #[test]
    fn checker_rejects_text_decode_without_format() {
        let checked = check("text = \"{}\"\ndecoded = text.decode()\n");

        assert!(
            checked
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::DECODE_FORMAT)),
            "missing format is a decode-format diagnostic: {:?}",
            checked.diagnostics
        );
    }

    #[test]
    fn checker_rejects_text_decode_non_format_first_arg() {
        let checked = check("text = \"{}\"\ndecoded = text.decode(text)\n");

        assert!(
            checked
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::DECODE_FORMAT)),
            "a non-format first argument is a decode-format diagnostic: {:?}",
            checked.diagnostics
        );
    }

    #[test]
    fn eval_text_decode_method_matches_static_form() {
        let value = run("User = { name: Text }\n\
             text = \"{\\\"name\\\":\\\"Ada\\\"}\"\n\
             method = text.decode(Json, User)?!\n\
             direct = Json.decode(text, User)?!\n\
             { method: method, direct: direct }\n");

        assert_eq!(text(field(field(&value, "method"), "name")), "Ada");
        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn eval_one_arg_text_decode_matches_dynamic_static_form() {
        let value = run("text = \"{\\\"name\\\":\\\"Ada\\\"}\"\n\
             method = text.decode(Json)?!\n\
             direct = Json.decode(text)?!\n\
             { method: method, direct: direct }\n");

        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn eval_text_decode_method_with_inline_record_target() {
        let value = run("text = \"{\\\"name\\\":\\\"Ada\\\"}\"\n\
             method = text.decode(Json, { name: Text })?!\n\
             direct = Json.decode(text, { name: Text })?!\n\
             { method: method, direct: direct }\n");

        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn typed_decode_accepts_iso_string_fields_as_temporals() {
        let value = run("Cfg = { day: Date, when: Instant }\n\
             Json.decode(\
               \"{\\\"day\\\":\\\"1979-05-27\\\",\\\"when\\\":\\\"1979-05-27T09:00:00+10:00\\\"}\",\
               Cfg\
             )?!\n");
        assert_eq!(
            crate::temporal::temporal_kind(field(&value, "day")),
            Some("Date")
        );
        assert_eq!(
            crate::temporal::temporal_kind(field(&value, "when")),
            Some("Instant")
        );
        assert_eq!(text(&run_field_call(&value, "day", "format")), "1979-05-27");
        // Offset form normalizes to UTC.
        assert_eq!(
            text(&run_field_call(&value, "when", "format")),
            "1979-05-26T23:00:00Z"
        );
    }

    #[test]
    fn typed_decode_malformed_temporal_string_is_shape_error() {
        let value = run("Json.decode(\"{\\\"when\\\":\\\"not-a-date\\\"}\", { when: Instant })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.when");
        assert_eq!(text(field(payload, "expected")), "Instant");
        assert_eq!(text(field(payload, "found")), "Text");
    }

    #[test]
    fn typed_json_round_trip_preserves_date_and_instant() {
        let value = run("Cfg = { day: Date, when: Instant }\n\
             original = {\n\
               day: Date.parse(\"1979-05-27\")?!,\n\
               when: Instant.parse(\"1979-05-27T07:32:00Z\")?!\n\
             }\n\
             encoded = Json.encode(original)?!\n\
             decoded = Json.decode(encoded, Cfg)?!\n\
             {\n\
               day: decoded.day.format(),\n\
               when: decoded.when.format()\n\
             }\n");
        assert_eq!(text(field(&value, "day")), "1979-05-27");
        assert_eq!(text(field(&value, "when")), "1979-05-27T07:32:00Z");
    }

    /// Invoke a nullary method field on a nested record field of `value`.
    fn run_field_call(value: &Value, field_name: &str, method: &str) -> Value {
        let receiver = field(value, field_name);
        let Value::Record(fields) = receiver else {
            panic!("expected record field `{field_name}`");
        };
        let Value::Native(native) = fields
            .iter()
            .find_map(|(name, value)| (name == method).then_some(value))
            .unwrap_or_else(|| panic!("record has method `{method}`"))
        else {
            panic!("method `{method}` is native");
        };
        native(&[]).unwrap_or_else(|error| panic!("method failed: {error}"))
    }
}
