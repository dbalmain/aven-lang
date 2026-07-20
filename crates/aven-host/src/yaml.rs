use std::fmt;

use aven_eval::Value;
use serde::de::{self, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value};
use crate::temporal::temporal_iso_text;
use crate::text_format::{
    DecodeError, FormatNumber, FormatValue, decode_value, encode_error_value, parse_error_value,
    shape_error_value,
};

impl Host {
    /// Register the `Yaml` type artifact carrying `encode`/`decode` statics.
    pub fn register_yaml(&mut self) {
        self.register_data_type();
        self.register_type_statics(
            "Yaml",
            vec![
                (
                    "encode".to_owned(),
                    crate::yaml_encode_type(),
                    encode_native(),
                ),
                (
                    "encodeText".to_owned(),
                    crate::yaml_encode_text_type(),
                    encode_text_native(),
                ),
                (
                    "decode".to_owned(),
                    crate::yaml_decode_base_type(),
                    decode_native(),
                ),
            ],
        );
        self.register_type_definition("YamlError", crate::yaml_error_type());
        self.register_type_definition("YamlEncodeError", crate::yaml_encode_error_type());
        self.register_comptime_resolver("Yaml.decode", vec![1], decode_comptime_resolver());
        self.register_comptime_type_resolver(
            "Yaml.encodeText",
            vec![0],
            crate::text_format::encode_text_comptime_resolver("Yaml"),
        );
    }
}

pub(crate) fn decode_comptime_resolver() -> std::rc::Rc<dyn aven_check::HostComptimeFn> {
    crate::text_format::decode_comptime_resolver("YamlError")
}

fn decode_native() -> Value {
    Value::native(|args| {
        if args.len() > 2 {
            return Err(format!(
                "Yaml.decode expects 1 or 2 arguments, got {}",
                args.len()
            ));
        }
        let (text, target) = match args {
            [Value::Text(text)] => (text, None),
            [Value::Text(text), target] => (text, Some(target)),
            [other] | [other, ..] => {
                return Err(format!(
                    "Yaml.decode expects Text input, got {}",
                    aven_value_type_name(other)
                ));
            }
            [] => {
                return Err("Yaml.decode expects at least 1 argument, got 0".to_owned());
            }
        };

        let parsed = match parse_yaml(text) {
            Ok(value) => value,
            Err(error) => return Ok(err_value(parse_error_value(error))),
        };

        let default_target = Value::named_type("Data");
        let target = target.unwrap_or(&default_target);
        match decode_value(&parsed, target, "Yaml") {
            Ok(value) => Ok(ok_value(value)),
            Err(DecodeError::Shape(error)) => Ok(err_value(shape_error_value(error))),
            Err(DecodeError::InvalidTarget(message)) => Err(message),
        }
    })
}

struct YamlValue(FormatValue);

impl<'de> Deserialize<'de> for YamlValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(YamlValueVisitor)
    }
}

struct YamlValueVisitor;

impl<'de> Visitor<'de> for YamlValueVisitor {
    type Value = YamlValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a YAML value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Number(FormatNumber::Int(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let number = i64::try_from(value)
            .map(FormatNumber::Int)
            .unwrap_or_else(|_| FormatNumber::Float(value as f64));
        Ok(YamlValue(FormatValue::Number(number)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Number(FormatNumber::Float(value))))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Text(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlValue(FormatValue::Text(value)))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(YamlValue(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(YamlValue(FormatValue::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        while let Some(YamlKey(key)) = map.next_key()? {
            let YamlValue(value) = map.next_value()?;
            entries.push((key, value));
        }
        Ok(YamlValue(FormatValue::Object(entries)))
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: EnumAccess<'de>,
    {
        let (_tag, contents) = data.variant::<String>()?;
        contents.newtype_variant()
    }
}

struct YamlKey(String);

impl<'de> Deserialize<'de> for YamlKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(YamlKeyVisitor)
    }
}

struct YamlKeyVisitor;

impl<'de> Visitor<'de> for YamlKeyVisitor {
    type Value = YamlKey;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("YAML keys must be text")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlKey(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(YamlKey(value))
    }
}

fn parse_yaml(text: &str) -> Result<FormatValue, String> {
    serde_norway::from_str::<YamlValue>(text)
        .map(|YamlValue(value)| value)
        .map_err(|error| error.to_string())
}

fn encode_native() -> Value {
    Value::native(|args| {
        let [value] = args else {
            return Err(format!(
                "Yaml.encode expects 1 argument, got {}",
                args.len()
            ));
        };

        Ok(match encode_to_text(value) {
            Ok(text) => ok_value(Value::Text(text)),
            Err(error) => err_value(encode_error_value(error)),
        })
    })
}

fn encode_text_native() -> Value {
    Value::native(|args| {
        let [value] = args else {
            return Err(format!(
                "Yaml.encodeText expects 1 argument, got {}",
                args.len()
            ));
        };
        let text = encode_to_text(value).unwrap_or_else(|error| {
            panic!("Yaml.encodeText Float-free type invariant failed: {error}")
        });
        Ok(Value::Text(text))
    })
}

fn encode_to_text(value: &Value) -> Result<String, String> {
    let value = yaml_value(value, EncodePosition::TopLevel)?;
    serde_norway::to_string(&value).map_err(|error| format!("Yaml.encode failed: {error}"))
}

#[derive(Clone, Copy)]
enum EncodePosition {
    TopLevel,
    RecordField,
    ArrayElement,
}

fn yaml_value(value: &Value, position: EncodePosition) -> Result<serde_norway::Value, String> {
    // Temporal values encode as plain ISO scalars (YAML 1.2 core has no timestamp).
    if let Some(text) = temporal_iso_text(value) {
        return Ok(serde_norway::Value::String(text));
    }

    match value {
        Value::Int(value) => Ok(serde_norway::Value::Number((*value).into())),
        Value::Float(value) => Ok(serde_norway::Value::Number((*value).into())),
        Value::Text(value) => Ok(serde_norway::Value::String(value.clone())),
        Value::Bool(value) => Ok(serde_norway::Value::Bool(*value)),
        Value::Null => Ok(serde_norway::Value::Null),
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => values
            .iter()
            .map(|value| yaml_value(value, EncodePosition::ArrayElement))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_norway::Value::Sequence),
        Value::Map(entries) => yaml_mapping_from_map(entries).map(serde_norway::Value::Mapping),
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
            yaml_mapping_from_record(fields).map(serde_norway::Value::Mapping)
        }
        Value::SlotRecord { fields, .. } => {
            yaml_mapping_from_record(fields).map(serde_norway::Value::Mapping)
        }
        Value::BrandedPrimitive { payload, .. } => yaml_value(&payload.to_value(), position),
        Value::Tag { name, payload } => yaml_value_from_json_constructor(name, payload),
        Value::Undefined => match position {
            EncodePosition::RecordField => Err("Yaml.encode cannot encode undefined".to_owned()),
            EncodePosition::TopLevel => {
                Err("Yaml.encode cannot encode top-level undefined".to_owned())
            }
            EncodePosition::ArrayElement => {
                Err("Yaml.encode cannot encode undefined array elements".to_owned())
            }
        },
        Value::ResultMethod { .. } => Err("Yaml.encode cannot encode Function".to_owned()),
        Value::NamedMethod { .. } | Value::UnboundNamedMethod { .. } => {
            Err("Yaml.encode cannot encode Function".to_owned())
        }
        Value::Closure(_) => Err("Yaml.encode cannot encode Function".to_owned()),
        Value::Native(_) => Err("Yaml.encode cannot encode Native".to_owned()),
        Value::Type(_) => Err("Yaml.encode cannot encode Type".to_owned()),
        Value::NamedFamily(_) => Err("Yaml.encode cannot encode Type".to_owned()),
    }
}

fn yaml_value_from_json_constructor(
    name: &str,
    payload: &[Value],
) -> Result<serde_norway::Value, String> {
    match name {
        "Null" => {
            let [] = payload else {
                return Err(json_constructor_shape_error(name, "no payload"));
            };
            Ok(serde_norway::Value::Null)
        }
        "Bool" => {
            let [Value::Bool(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Bool"));
            };
            Ok(serde_norway::Value::Bool(*value))
        }
        "Int" => {
            let [Value::Int(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Int"));
            };
            Ok(serde_norway::Value::Number((*value).into()))
        }
        "Float" => {
            let [Value::Float(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Float"));
            };
            Ok(serde_norway::Value::Number((*value).into()))
        }
        "Text" => {
            let [Value::Text(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Text"));
            };
            Ok(serde_norway::Value::String(value.clone()))
        }
        "Array" => {
            let [Value::Array(values)] = payload else {
                return Err(json_constructor_shape_error(name, "Array(Data)"));
            };
            values
                .iter()
                .map(|value| yaml_value(value, EncodePosition::ArrayElement))
                .collect::<Result<Vec<_>, _>>()
                .map(serde_norway::Value::Sequence)
        }
        "Object" => {
            let [Value::Map(entries)] = payload else {
                return Err(json_constructor_shape_error("Object", "Map(Text, Data)"));
            };
            yaml_mapping_from_map(entries).map(serde_norway::Value::Mapping)
        }
        _ if payload.is_empty() => Err(format!(
            "Yaml.encode cannot encode nullary tag @{name}; YAML tag wire form is not decided"
        )),
        _ => Err(format!(
            "Yaml.encode cannot encode tag @{name} with payload"
        )),
    }
}

fn yaml_mapping_from_record(fields: &[(String, Value)]) -> Result<serde_norway::Mapping, String> {
    let mut mapping = serde_norway::Mapping::new();
    for (name, value) in fields {
        if matches!(value, Value::Undefined) {
            continue;
        }

        mapping.insert(
            serde_norway::Value::String(name.clone()),
            yaml_value(value, EncodePosition::RecordField)?,
        );
    }
    Ok(mapping)
}

fn yaml_mapping_from_map(entries: &[(Value, Value)]) -> Result<serde_norway::Mapping, String> {
    let mut mapping = serde_norway::Mapping::new();
    for (key, value) in entries {
        let Value::Text(key) = key else {
            return Err("Yaml.encode expected Map(Text, _) keys".to_owned());
        };
        if matches!(value, Value::Undefined) {
            continue;
        }

        mapping.insert(
            serde_norway::Value::String(key.clone()),
            yaml_value(value, EncodePosition::RecordField)?,
        );
    }
    Ok(mapping)
}

fn json_constructor_shape_error(name: &str, expected: &str) -> String {
    format!("Yaml.encode expected @{name} payload shape {expected}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use aven_core::Span;
    use aven_parser::parse_module;

    fn yaml_host() -> Host {
        let mut host = Host::new();
        host.register_yaml();
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
            aven_eval::eval_module_with_globals(&parsed.module, yaml_host().eval_globals());
        assert!(
            outcome.diagnostics.is_empty(),
            "program runs: {:?}",
            outcome.diagnostics
        );
        outcome
            .value
            .unwrap_or_else(|| panic!("program yields a value"))
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
            &yaml_host().check_host_globals(),
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
            panic!("expected YamlError tag, got {error:?}");
        };
        let [payload] = payload.as_slice() else {
            panic!("YamlError carries one payload record");
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

    fn tag_array_payload<'a>(value: &'a Value, expected: &str) -> &'a [Value] {
        let [Value::Array(values)] = tag_payload(value, expected) else {
            panic!("expected @{expected}(Array), got {value:?}");
        };
        values.as_ref()
    }

    #[test]
    fn typed_decode_builds_record() {
        let value = run("Config = { name: Text, count: Int, enabled: Bool }\n\
             Yaml.decode(\"name: Ada\\ncount: 3\\nenabled: true\\n\", Config)?!\n");

        assert_eq!(text(field(&value, "name")), "Ada");
        assert_eq!(field(&value, "count"), &Value::Int(3));
        assert_eq!(field(&value, "enabled"), &Value::Bool(true));
    }

    #[test]
    fn dynamic_decode_uses_data_constructor_tree() {
        let value = run("Yaml.decode(\"[1, 1.5, true, null, Ada]\")?!\n");
        let items = tag_array_payload(&value, "Array");
        let names = items
            .iter()
            .map(|value| {
                let Value::Tag { name, .. } = value else {
                    panic!("expected tag, got {value:?}");
                };
                name.as_str()
            })
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["Int", "Float", "Bool", "Null", "Text"]);
    }

    #[test]
    fn encode_round_trip_preserves_typed_record() {
        let value = run("Config = { name: Text, count: Int }\n\
             encoded = Yaml.encode({ name: \"Ada\", count: 3 })?!\n\
             decoded = Yaml.decode(encoded, Config)?!\n\
             { encoded: encoded, decoded: decoded }\n");

        assert_eq!(text(field(&value, "encoded")), "name: Ada\ncount: 3\n");
        assert_eq!(field(field(&value, "decoded"), "count"), &Value::Int(3));
    }

    #[test]
    fn multi_document_input_returns_parse_error() {
        let value = run("Yaml.decode(\"---\\nname: Ada\\n---\\nname: Grace\\n\")\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Parse");
        assert!(
            text(field(payload, "message")).contains("more than one document"),
            "{payload:?}"
        );
    }

    #[test]
    fn non_text_mapping_key_returns_parse_error() {
        let value = run("Yaml.decode(\"1: one\")\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Parse");
        assert!(
            text(field(payload, "message")).contains("YAML keys must be text"),
            "{payload:?}"
        );
    }

    #[test]
    fn shape_error_reports_path() {
        let value = run("Yaml.decode(\"name: 1\", { name: Text })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.name");
        assert_eq!(text(field(payload, "found")), "Int");
    }

    #[test]
    fn encode_returns_structured_error_for_unknown_tags() {
        let value = run("Yaml.encode(@Red)\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert!(
            text(field(payload, "message")).contains("Yaml.encode cannot encode nullary tag @Red"),
            "{payload:?}"
        );
    }

    #[test]
    fn encode_non_finite_floats_natively() {
        let value =
            run("Yaml.encode({ nan: 0.0 / 0.0, positive: 1.0 / 0.0, negative: -1.0 / 0.0 })?!\n");
        let encoded = text(&value);

        assert!(encoded.contains("nan: .nan"), "expected .nan: {encoded}");
        assert!(
            encoded.contains("positive: .inf"),
            "expected .inf: {encoded}"
        );
        assert!(
            encoded.contains("negative: -.inf"),
            "expected -.inf: {encoded}"
        );
    }

    #[test]
    fn checker_resolves_one_arg_decode_to_dynamic_data_result() {
        let source = "text = \"name: Ada\"\ndecoded = Yaml.decode(text)\n";
        let checked = check(source);

        assert!(
            checked.diagnostics.is_empty(),
            "one-arg dynamic decode checks: {:?}",
            checked.diagnostics
        );
        let offset = source
            .find("decoded")
            .unwrap_or_else(|| panic!("source mentions decoded"));
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .unwrap_or_else(|| panic!("decoded has an inferred type"));

        assert_eq!(ty.render(), "Result(Data, YamlError)");
    }

    #[test]
    fn checker_text_decode_method_matches_static_form() {
        let source =
            "Config = { name: Text }\ntext = \"name: Ada\"\ndecoded = text.decode(Yaml, Config)\n";
        let checked = check(source);

        assert!(
            checked.diagnostics.is_empty(),
            "method form checks: {:?}",
            checked.diagnostics
        );
        let offset = source
            .find("decoded")
            .unwrap_or_else(|| panic!("source mentions decoded"));
        let ty = checked
            .type_at(Span::new(offset, offset + "decoded".len()))
            .unwrap_or_else(|| panic!("decoded has an inferred type"));

        assert_eq!(ty.render(), "Result(Config, YamlError)");
    }

    #[test]
    fn eval_text_decode_method_matches_static_form() {
        let value = run("Config = { name: Text, count: Int }\n\
             text = \"name: Ada\\ncount: 3\\n\"\n\
             method = text.decode(Yaml, Config)?!\n\
             direct = Yaml.decode(text, Config)?!\n\
             { method: method, direct: direct }\n");

        assert_eq!(text(field(field(&value, "method"), "name")), "Ada");
        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn eval_encode_method_accepts_named_annotation_receiver() {
        let value = run("Y = { y: Int }\n\
             y: Y = { y: 2 }\n\
             method = y.encode(Yaml)?!\n\
             direct = Yaml.encode(y)?!\n\
             { method: method, direct: direct }\n");

        assert_eq!(text(field(&value, "method")), "y: 2\n");
        assert_eq!(field(&value, "method"), field(&value, "direct"));
    }

    #[test]
    fn typed_decode_accepts_plain_iso_scalars_as_temporals() {
        let value = run("Cfg = { day: Date, when: Instant }\n\
             Yaml.decode(\
               \"day: 1979-05-27\\nwhen: 1979-05-27T09:00:00+10:00\\n\",\
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
        assert_eq!(
            text(&run_field_call(&value, "when", "format")),
            "1979-05-26T23:00:00Z"
        );
    }

    #[test]
    fn typed_decode_accepts_quoted_iso_scalars_as_temporals() {
        let value = run("Cfg = { day: Date, when: Instant }\n\
             Yaml.decode(\
               \"day: \\\"1979-05-27\\\"\\nwhen: \\\"1979-05-27T09:00:00+10:00\\\"\\n\",\
               Cfg\
             )?!\n");
        assert_eq!(text(&run_field_call(&value, "day", "format")), "1979-05-27");
        assert_eq!(
            text(&run_field_call(&value, "when", "format")),
            "1979-05-26T23:00:00Z"
        );
    }

    #[test]
    fn typed_decode_malformed_temporal_string_is_shape_error() {
        let value = run("Yaml.decode(\"when: not-a-date\\n\", { when: Instant })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.when");
        assert_eq!(text(field(payload, "expected")), "Instant");
        assert_eq!(text(field(payload, "found")), "Text");
    }

    #[test]
    fn typed_yaml_round_trip_preserves_date_and_instant() {
        let value = run("Cfg = { day: Date, when: Instant }\n\
             original = {\n\
               day: Date.parse(\"1979-05-27\")?!,\n\
               when: Instant.parse(\"1979-05-27T07:32:00Z\")?!\n\
             }\n\
             encoded = Yaml.encode(original)?!\n\
             decoded = Yaml.decode(encoded, Cfg)?!\n\
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
