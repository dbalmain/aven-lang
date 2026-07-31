use aven_eval::{Int, Value};

use crate::Host;
use crate::temporal::{
    format_temporal_from_toml, format_temporal_from_value, temporal_iso_text,
    toml_datetime_from_format_temporal,
};
use crate::text_format::{FormatNumber, FormatValue, TextFormat, register_text_format};

impl Host {
    /// Register the `Toml` type artifact carrying `encode`/`decode` statics.
    pub fn register_toml(&mut self) {
        register_text_format(self, TextFormat::Toml, parse_toml, encode_to_text);
    }
}

fn parse_toml(text: &str) -> Result<FormatValue, String> {
    text.parse::<::toml::Table>()
        .map(|table| {
            FormatValue::Object(
                table
                    .into_iter()
                    .map(|(key, value)| (key, toml_to_format_value(value)))
                    .collect(),
            )
        })
        .map_err(|error| error.to_string())
}

fn toml_to_format_value(value: ::toml::Value) -> FormatValue {
    match value {
        ::toml::Value::String(value) => FormatValue::Text(value),
        ::toml::Value::Integer(value) => FormatValue::Number(FormatNumber::Int(Int::from(value))),
        ::toml::Value::Float(value) => FormatValue::Number(FormatNumber::Float(value)),
        ::toml::Value::Boolean(value) => FormatValue::Bool(value),
        ::toml::Value::Datetime(value) => match format_temporal_from_toml(&value) {
            Ok(temporal) => FormatValue::Temporal(temporal),
            // Malformed TOML datetimes should not appear after a successful
            // parse; fall back to the textual form so decode still sees a value.
            Err(_) => FormatValue::Text(value.to_string()),
        },
        ::toml::Value::Array(values) => {
            FormatValue::Array(values.into_iter().map(toml_to_format_value).collect())
        }
        ::toml::Value::Table(entries) => FormatValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, toml_to_format_value(value)))
                .collect(),
        ),
    }
}

fn encode_to_text(value: &Value) -> Result<String, String> {
    let table = toml_table_from_top_level(value)?;
    ::toml::to_string(&table).map_err(|error| format!("Toml.encode failed: {error}"))
}

fn toml_table_from_top_level(value: &Value) -> Result<::toml::Table, String> {
    match toml_value(value, EncodePosition::TopLevel)? {
        ::toml::Value::Table(table) => Ok(table),
        _ => Err("Toml.encode expects a top-level record or @Object value".to_owned()),
    }
}

#[derive(Clone, Copy)]
enum EncodePosition {
    TopLevel,
    RecordField,
    ArrayElement,
}

fn toml_integer(value: &Int) -> Result<i64, String> {
    value.to_i64().ok_or_else(|| {
        format!("Toml.encode cannot represent Int {value}; TOML integers are signed 64-bit")
    })
}

fn toml_value(value: &Value, position: EncodePosition) -> Result<::toml::Value, String> {
    if let Some(temporal) = format_temporal_from_value(value) {
        return toml_datetime_from_format_temporal(temporal).map(::toml::Value::Datetime);
    }
    // Duration (and any other temporal without a TOML native kind) as ISO text.
    if let Some(text) = temporal_iso_text(value) {
        return Ok(::toml::Value::String(text));
    }

    match value {
        Value::Int(value) => toml_integer(value).map(::toml::Value::Integer),
        Value::Float(value) => Ok(::toml::Value::Float(*value)),
        Value::Text(value) => Ok(::toml::Value::String(value.clone())),
        Value::Bool(value) => Ok(::toml::Value::Boolean(*value)),
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => values
            .iter()
            .map(|value| toml_value(value, EncodePosition::ArrayElement))
            .collect::<Result<Vec<_>, _>>()
            .map(::toml::Value::Array),
        Value::Map(entries) => toml_table_from_map(entries).map(::toml::Value::Table),
        Value::Record(fields) | Value::NamedRecord { fields, .. } => {
            toml_table_from_record(fields).map(::toml::Value::Table)
        }
        Value::SlotRecord { fields, .. } => {
            toml_table_from_record(fields).map(::toml::Value::Table)
        }
        Value::BrandedPrimitive { payload, .. } => toml_value(&payload.to_value(), position),
        Value::Tag { name, payload } => toml_value_from_json_constructor(name, payload),
        Value::Undefined => match position {
            EncodePosition::RecordField => Err("Toml.encode cannot encode undefined".to_owned()),
            EncodePosition::TopLevel => {
                Err("Toml.encode cannot encode top-level undefined".to_owned())
            }
            EncodePosition::ArrayElement => {
                Err("Toml.encode cannot encode undefined array elements".to_owned())
            }
        },
        Value::Null => Err("Toml.encode cannot encode Null because TOML has no null".to_owned()),
        Value::ResultMethod { .. } => Err("Toml.encode cannot encode Function".to_owned()),
        Value::NamedMethod { .. } | Value::UnboundNamedMethod { .. } => {
            Err("Toml.encode cannot encode Function".to_owned())
        }
        Value::Closure(_) => Err("Toml.encode cannot encode Function".to_owned()),
        Value::Native(_) => Err("Toml.encode cannot encode Native".to_owned()),
        Value::Type(_) => Err("Toml.encode cannot encode Type".to_owned()),
        Value::NamedFamily(_) => Err("Toml.encode cannot encode Type".to_owned()),
    }
}

fn toml_value_from_json_constructor(
    name: &str,
    payload: &[Value],
) -> Result<::toml::Value, String> {
    match name {
        "Null" => {
            let [] = payload else {
                return Err(json_constructor_shape_error(name, "no payload"));
            };
            Err("Toml.encode cannot encode @Null because TOML has no null".to_owned())
        }
        "Bool" => {
            let [Value::Bool(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Bool"));
            };
            Ok(::toml::Value::Boolean(*value))
        }
        "Int" => {
            let [Value::Int(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Int"));
            };
            toml_integer(value).map(::toml::Value::Integer)
        }
        "Float" => {
            let [Value::Float(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Float"));
            };
            Ok(::toml::Value::Float(*value))
        }
        "Text" => {
            let [Value::Text(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Text"));
            };
            Ok(::toml::Value::String(value.clone()))
        }
        "Array" => {
            let [Value::Array(values)] = payload else {
                return Err(json_constructor_shape_error(name, "Array(Data)"));
            };
            values
                .iter()
                .map(|value| toml_value(value, EncodePosition::ArrayElement))
                .collect::<Result<Vec<_>, _>>()
                .map(::toml::Value::Array)
        }
        "Object" => {
            let [Value::Map(entries)] = payload else {
                return Err(json_constructor_shape_error("Object", "Map(Text, Data)"));
            };
            toml_table_from_map(entries).map(::toml::Value::Table)
        }
        _ if payload.is_empty() => Err(format!(
            "Toml.encode cannot encode nullary tag @{name}; TOML tag wire form is not decided"
        )),
        _ => Err(format!(
            "Toml.encode cannot encode tag @{name} with payload"
        )),
    }
}

fn toml_table_from_record(fields: &[(String, Value)]) -> Result<::toml::Table, String> {
    let mut table = ::toml::Table::new();
    for (name, value) in fields {
        if matches!(value, Value::Undefined) {
            continue;
        }

        table.insert(
            name.clone(),
            toml_value(value, EncodePosition::RecordField)?,
        );
    }
    Ok(table)
}

fn toml_table_from_map(entries: &[(Value, Value)]) -> Result<::toml::Table, String> {
    let mut table = ::toml::Table::new();
    for (key, value) in entries {
        let Value::Text(key) = key else {
            return Err("Toml.encode expected Map(Text, _) keys".to_owned());
        };
        table.insert(key.clone(), toml_value(value, EncodePosition::RecordField)?);
    }
    Ok(table)
}

fn json_constructor_shape_error(name: &str, expected: &str) -> String {
    format!("Toml.encode expected @{name} payload shape {expected}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use aven_core::Span;
    use aven_parser::parse_module;

    fn toml_host() -> Host {
        let mut host = Host::new();
        host.register_toml();
        host.register_json();
        host.register_yaml();
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
        let outcome = aven_eval::eval_module_with_options(
            &parsed.module,
            aven_eval::EvalModuleOptions::default().with_globals(toml_host().eval_globals()),
        );
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
            &toml_host().check_host_globals(),
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
            panic!("expected TomlError tag, got {error:?}");
        };
        let [payload] = payload.as_slice() else {
            panic!("TomlError carries one payload record");
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

    fn map_entries(value: &Value) -> &[(Value, Value)] {
        let [Value::Map(entries)] = tag_payload(value, "Object") else {
            panic!("expected @Object(Map), got {value:?}");
        };
        entries.as_ref()
    }

    fn map_value<'a>(entries: &'a [(Value, Value)], key: &str) -> &'a Value {
        entries
            .iter()
            .find_map(|(entry_key, value)| {
                (entry_key == &Value::Text(key.to_owned())).then_some(value)
            })
            .unwrap_or_else(|| panic!("map has key `{key}`"))
    }

    #[test]
    fn typed_decode_builds_record() {
        let value = run("Config = { name: Text, count: Int, enabled: Bool }\n\
             Toml.decode(\"name = 'Ada'\\ncount = 3\\nenabled = true\\n\", Config)?!\n");

        assert_eq!(text(field(&value, "name")), "Ada");
        assert_eq!(field(&value, "count"), &Value::int(3));
        assert_eq!(field(&value, "enabled"), &Value::Bool(true));
    }

    #[test]
    fn dynamic_decode_uses_data_constructor_tree() {
        let value =
            run("Toml.decode(\"name = 'Ada'\\ncount = 3\\nwhen = 1979-05-27T07:32:00Z\\n\")?!\n");
        let entries = map_entries(&value);

        assert_eq!(
            tag_payload(map_value(entries, "name"), "Text"),
            &[Value::Text("Ada".to_owned())]
        );
        assert_eq!(
            tag_payload(map_value(entries, "count"), "Int"),
            &[Value::int(3)]
        );
        assert_eq!(
            tag_payload(map_value(entries, "when"), "Text"),
            &[Value::Text("1979-05-27T07:32:00Z".to_owned())]
        );
    }

    #[test]
    fn encode_round_trip_preserves_typed_record() {
        let value = run("Config = { name: Text, count: Int }\n\
             encoded = Toml.encode({ name: \"Ada\", count: 3 })?!\n\
             decoded = Toml.decode(encoded, Config)?!\n\
             { encoded: encoded, decoded: decoded }\n");

        assert!(text(field(&value, "encoded")).contains("name = \"Ada\""));
        assert_eq!(field(field(&value, "decoded"), "count"), &Value::int(3));
    }

    #[test]
    fn parse_error_returns_structured_result_error() {
        let value = run("Toml.decode(\"= nope\")\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Parse");
        assert!(!text(field(payload, "message")).is_empty());
    }

    #[test]
    fn shape_error_reports_path() {
        let value = run("Toml.decode(\"name = 1\", { name: Text })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.name");
        assert_eq!(text(field(payload, "found")), "Int");
    }

    #[test]
    fn encode_returns_structured_error_for_null() {
        let value = run("Toml.encode({ name: null })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert_eq!(
            text(field(payload, "message")),
            "Toml.encode cannot encode Null because TOML has no null"
        );
    }

    #[test]
    fn encode_rejects_integers_beyond_toml_signed_range() {
        let value = run("Toml.encode({ value: 18446744073709551615 })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Encode");
        assert!(
            text(field(payload, "message")).contains("TOML integers are signed 64-bit"),
            "{payload:?}"
        );
    }

    #[test]
    fn encode_non_finite_floats_natively() {
        let value =
            run("Toml.encode({ nan: 0.0 / 0.0, positive: 1.0 / 0.0, negative: -1.0 / 0.0 })?!\n");
        let encoded = text(&value);

        assert!(encoded.contains("nan = nan"), "expected nan: {encoded}");
        assert!(
            encoded.contains("positive = inf"),
            "expected inf: {encoded}"
        );
        assert!(
            encoded.contains("negative = -inf"),
            "expected -inf: {encoded}"
        );
    }

    #[test]
    fn checker_resolves_one_arg_decode_to_dynamic_data_result() {
        let source = "text = \"name = 'Ada'\"\ndecoded = Toml.decode(text)\n";
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

        assert_eq!(ty.render(), "Result(Data, TomlError)");
    }

    #[test]
    fn typed_decode_maps_four_toml_datetime_kinds() {
        let value = run("Cfg = {\n\
               when: Instant,\n\
               local: DateTime,\n\
               day: Date,\n\
               clock: Time\n\
             }\n\
             Toml.decode(\"when = 1979-05-27T07:32:00-08:00\\n\
local = 1979-05-27T07:32:00\\n\
day = 1979-05-27\\n\
clock = 07:32:00\\n\", Cfg)?!\n");

        assert_eq!(
            text(&run_field_call(&value, "when", "format")),
            "1979-05-27T15:32:00Z"
        );
        assert_eq!(
            text(&run_field_call(&value, "local", "format")),
            "1979-05-27T07:32:00"
        );
        assert_eq!(field(field(&value, "day"), "year"), &Value::int(1979));
        assert_eq!(field(field(&value, "day"), "month"), &Value::int(5));
        assert_eq!(field(field(&value, "day"), "day"), &Value::int(27));
        assert_eq!(field(field(&value, "clock"), "hour"), &Value::int(7));
        assert_eq!(field(field(&value, "clock"), "minute"), &Value::int(32));
    }

    #[test]
    fn typed_decode_local_datetime_into_instant_is_shape_error() {
        let value = run("Toml.decode(\"when = 1979-05-27T07:32:00\\n\", { when: Instant })\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Shape");
        assert_eq!(text(field(payload, "path")), "$.when");
        assert_eq!(text(field(payload, "expected")), "Instant");
        assert_eq!(text(field(payload, "found")), "DateTime");
    }

    #[test]
    fn typed_decode_accepts_string_field_as_temporal() {
        let value = run("Toml.decode(\"day = \\\"1979-05-27\\\"\\n\", { day: Date })?!\n");
        assert_eq!(
            crate::temporal::temporal_kind(field(&value, "day")),
            Some("Date")
        );
        assert_eq!(text(&run_field_call(&value, "day", "format")), "1979-05-27");
    }

    #[test]
    fn untyped_decode_still_yields_iso_text() {
        let value = run("Toml.decode(\"when = 1979-05-27T07:32:00Z\\n\")?!\n");
        let entries = map_entries(&value);
        assert_eq!(
            tag_payload(map_value(entries, "when"), "Text"),
            &[Value::Text("1979-05-27T07:32:00Z".to_owned())]
        );
    }

    #[test]
    fn toml_encode_emits_native_unquoted_datetimes() {
        let value = run("d = Date.parse(\"1979-05-27\")?!\n\
             t = Time.parse(\"07:32:00\")?!\n\
             dt = DateTime.of(d, t)\n\
             i = Instant.parse(\"1979-05-27T07:32:00Z\")?!\n\
             Toml.encode({ day: d, clock: t, local: dt, when: i })?!\n");
        let encoded = text(&value);
        assert!(
            encoded.contains("day = 1979-05-27"),
            "expected native date, got {encoded}"
        );
        assert!(
            encoded.contains("clock = 07:32:00"),
            "expected native time, got {encoded}"
        );
        assert!(
            encoded.contains("local = 1979-05-27T07:32:00"),
            "expected native local date-time, got {encoded}"
        );
        assert!(
            encoded.contains("when = 1979-05-27T07:32:00Z")
                || encoded.contains("when = 1979-05-27T07:32:00+00:00"),
            "expected native offset date-time, got {encoded}"
        );
        // Must not quote the native forms.
        assert!(
            !encoded.contains("day = \"1979-05-27\""),
            "date should be unquoted: {encoded}"
        );
    }

    #[test]
    fn json_and_yaml_encode_emit_iso_scalars() {
        let value = run("d = Date.parse(\"1979-05-27\")?!\n\
             {\n\
               json: Json.encode({ day: d })?!,\n\
               yaml: Yaml.encode({ day: d })?!\n\
             }\n");
        let json = text(field(&value, "json"));
        let yaml = text(field(&value, "yaml"));
        assert!(
            json.contains("\"1979-05-27\""),
            "JSON should quote ISO text: {json}"
        );
        assert!(
            yaml.contains("1979-05-27"),
            "YAML should contain ISO text: {yaml}"
        );
    }

    #[test]
    fn typed_toml_round_trip_preserves_temporal_kinds() {
        let value = run("Cfg = { day: Date, when: Instant }\n\
             original = {\n\
               day: Date.parse(\"1979-05-27\")?!,\n\
               when: Instant.parse(\"1979-05-27T07:32:00Z\")?!\n\
             }\n\
             encoded = Toml.encode(original)?!\n\
             decoded = Toml.decode(encoded, Cfg)?!\n\
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
        native(
            &[],
            aven_eval::NativeContext::without_source(aven_core::Span::new(0, 0)),
        )
        .unwrap_or_else(|error| panic!("method failed: {error}"))
    }
}
