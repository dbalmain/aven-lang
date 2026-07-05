use aven_eval::Value;

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value};
use crate::text_format::{
    DecodeError, FormatNumber, FormatValue, decode_value, parse_error_value, shape_error_value,
};

impl Host {
    /// Register the `Yaml` type artifact carrying `encode`/`decode` statics.
    pub fn register_yaml(&mut self) {
        self.register_type_with_statics(
            "Yaml",
            crate::json_dynamic_type(),
            vec![
                (
                    "encode".to_owned(),
                    crate::yaml_encode_type(),
                    encode_native(),
                ),
                (
                    "decode".to_owned(),
                    crate::yaml_decode_base_type(),
                    decode_native(),
                ),
            ],
        );
        self.register_type_definition("YamlError", crate::yaml_error_type());
        self.register_comptime_resolver("Yaml.decode", vec![1], decode_comptime_resolver());
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

        let default_target = Value::named_type("Json");
        let target = target.unwrap_or(&default_target);
        match decode_value(&parsed, target, "Yaml") {
            Ok(value) => Ok(ok_value(value)),
            Err(DecodeError::Shape(error)) => Ok(err_value(shape_error_value(error))),
            Err(DecodeError::InvalidTarget(message)) => Err(message),
        }
    })
}

#[derive(Debug, Clone)]
struct YamlLine {
    number: usize,
    indent: usize,
    text: String,
}

fn parse_yaml(text: &str) -> Result<FormatValue, String> {
    let lines = yaml_lines(text)?;
    if lines.is_empty() {
        return Ok(FormatValue::Null);
    }

    let mut parser = YamlParser { lines, index: 0 };
    let indent = parser.lines[0].indent;
    let value = parser.parse_block(indent)?;
    if parser.index < parser.lines.len() {
        let line = &parser.lines[parser.index];
        return Err(format!("unexpected YAML content on line {}", line.number));
    }
    Ok(value)
}

fn yaml_lines(text: &str) -> Result<Vec<YamlLine>, String> {
    let mut lines = Vec::new();
    let mut saw_doc_start = false;
    let mut saw_content = false;
    let mut saw_doc_end = false;

    for (index, raw_line) in text.lines().enumerate() {
        let number = index + 1;
        let without_comment = strip_yaml_comment(raw_line);
        let trimmed_end = without_comment.trim_end();
        let trimmed = trimmed_end.trim_start();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed == "---" {
            if saw_doc_start || saw_content {
                return Err("Yaml.decode expects a single YAML document".to_owned());
            }
            saw_doc_start = true;
            continue;
        }
        if trimmed == "..." {
            saw_doc_end = true;
            continue;
        }
        if saw_doc_end {
            return Err("Yaml.decode expects a single YAML document".to_owned());
        }

        if raw_line
            .chars()
            .take_while(|ch| ch.is_whitespace())
            .any(|ch| ch == '\t')
        {
            return Err(format!(
                "YAML indentation cannot contain tabs on line {number}"
            ));
        }

        saw_content = true;
        lines.push(YamlLine {
            number,
            indent: trimmed_end.len() - trimmed.len(),
            text: trimmed.to_owned(),
        });
    }

    Ok(lines)
}

fn strip_yaml_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut output = String::new();

    for ch in line.chars() {
        if escaped {
            output.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if in_double => {
                output.push(ch);
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
                output.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                output.push(ch);
            }
            '#' if !in_single && !in_double => break,
            _ => output.push(ch),
        }
    }

    output
}

struct YamlParser {
    lines: Vec<YamlLine>,
    index: usize,
}

impl YamlParser {
    fn parse_block(&mut self, indent: usize) -> Result<FormatValue, String> {
        let Some(line) = self.lines.get(self.index) else {
            return Ok(FormatValue::Null);
        };
        if line.indent < indent {
            return Ok(FormatValue::Null);
        }
        if line.indent > indent {
            return Err(format!("unexpected indentation on line {}", line.number));
        }

        if line.text.starts_with("- ") || line.text == "-" {
            self.parse_sequence(indent)
        } else if split_key_value(&line.text).is_some() {
            self.parse_mapping(indent).map(FormatValue::Object)
        } else {
            let value = parse_inline_value(&line.text)
                .map_err(|error| format!("{error} on line {}", line.number))?;
            self.index += 1;
            Ok(value)
        }
    }

    fn parse_sequence(&mut self, indent: usize) -> Result<FormatValue, String> {
        let mut values = Vec::new();
        while let Some(line) = self.lines.get(self.index) {
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return Err(format!("unexpected indentation on line {}", line.number));
            }
            if !(line.text.starts_with("- ") || line.text == "-") {
                break;
            }

            let number = line.number;
            let item = line
                .text
                .strip_prefix('-')
                .unwrap_or("")
                .trim_start()
                .to_owned();
            self.index += 1;
            if item.is_empty() {
                values.push(self.parse_nested_or_null(indent, number)?);
            } else if let Some((key, rest)) = split_key_value(&item) {
                let mut entries = Vec::new();
                entries.push((
                    parse_key(key, number)?,
                    self.parse_mapping_value(rest, indent, number)?,
                ));
                if self
                    .lines
                    .get(self.index)
                    .is_some_and(|next| next.indent > indent)
                {
                    let nested_indent = self.lines[self.index].indent;
                    entries.append(&mut self.parse_mapping(nested_indent)?);
                }
                values.push(FormatValue::Object(entries));
            } else {
                values.push(
                    parse_inline_value(&item)
                        .map_err(|error| format!("{error} on line {number}"))?,
                );
            }
        }

        Ok(FormatValue::Array(values))
    }

    fn parse_mapping(&mut self, indent: usize) -> Result<Vec<(String, FormatValue)>, String> {
        let mut entries = Vec::new();
        while let Some(line) = self.lines.get(self.index) {
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return Err(format!("unexpected indentation on line {}", line.number));
            }
            if line.text.starts_with("- ") || line.text == "-" {
                break;
            }

            let line_text = line.text.clone();
            let line_number = line.number;
            let Some((key, rest)) = split_key_value(&line_text) else {
                return Err(format!("expected `key: value` on line {}", line.number));
            };
            let key = parse_key(key, line_number)?;
            self.index += 1;
            let value = self.parse_mapping_value(rest, indent, line_number)?;
            entries.push((key, value));
        }
        Ok(entries)
    }

    fn parse_mapping_value(
        &mut self,
        rest: &str,
        indent: usize,
        line_number: usize,
    ) -> Result<FormatValue, String> {
        if rest.trim().is_empty() {
            self.parse_nested_or_null(indent, line_number)
        } else {
            parse_inline_value(rest.trim())
                .map_err(|error| format!("{error} on line {line_number}"))
        }
    }

    fn parse_nested_or_null(
        &mut self,
        indent: usize,
        line_number: usize,
    ) -> Result<FormatValue, String> {
        let Some(next) = self.lines.get(self.index) else {
            return Ok(FormatValue::Null);
        };
        if next.indent <= indent {
            Ok(FormatValue::Null)
        } else {
            self.parse_block(next.indent)
                .map_err(|error| format!("{error} after line {line_number}"))
        }
    }
}

fn split_key_value(text: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut escaped = false;

    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '[' if !in_single && !in_double => bracket_depth += 1,
            ']' if !in_single && !in_double => bracket_depth = bracket_depth.saturating_sub(1),
            '{' if !in_single && !in_double => brace_depth += 1,
            '}' if !in_single && !in_double => brace_depth = brace_depth.saturating_sub(1),
            ':' if !in_single && !in_double && bracket_depth == 0 && brace_depth == 0 => {
                return Some((&text[..index], &text[index + 1..]));
            }
            _ => {}
        }
    }

    None
}

fn parse_key(key: &str, line_number: usize) -> Result<String, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("empty YAML mapping key on line {line_number}"));
    }
    match parse_quoted_text(key) {
        Some(text) => Ok(text),
        None => Ok(key.to_owned()),
    }
}

fn parse_inline_value(text: &str) -> Result<FormatValue, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(FormatValue::Null);
    }
    if text.starts_with('[') || text.ends_with(']') {
        return parse_inline_array(text);
    }
    if text.starts_with('{') || text.ends_with('}') {
        return parse_inline_object(text);
    }
    if let Some(text) = parse_quoted_text(text) {
        return Ok(FormatValue::Text(text));
    }

    match text {
        "null" | "Null" | "NULL" | "~" => Ok(FormatValue::Null),
        "true" | "True" | "TRUE" => Ok(FormatValue::Bool(true)),
        "false" | "False" | "FALSE" => Ok(FormatValue::Bool(false)),
        _ => parse_number_or_text(text),
    }
}

fn parse_inline_array(text: &str) -> Result<FormatValue, String> {
    let Some(inner) = text
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Err("invalid inline YAML array".to_owned());
    };
    let values = split_top_level(inner, ',')
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(parse_inline_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FormatValue::Array(values))
}

fn parse_inline_object(text: &str) -> Result<FormatValue, String> {
    let Some(inner) = text
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Err("invalid inline YAML object".to_owned());
    };
    let mut entries = Vec::new();
    for item in split_top_level(inner, ',') {
        if item.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = split_key_value(item) else {
            return Err("invalid inline YAML object entry".to_owned());
        };
        entries.push((parse_key(key, 0)?, parse_inline_value(value)?));
    }
    Ok(FormatValue::Object(entries))
}

fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut escaped = false;

    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '[' if !in_single && !in_double => bracket_depth += 1,
            ']' if !in_single && !in_double => bracket_depth = bracket_depth.saturating_sub(1),
            '{' if !in_single && !in_double => brace_depth += 1,
            '}' if !in_single && !in_double => brace_depth = brace_depth.saturating_sub(1),
            ch if ch == separator
                && !in_single
                && !in_double
                && bracket_depth == 0
                && brace_depth == 0 =>
            {
                parts.push(&text[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn parse_quoted_text(text: &str) -> Option<String> {
    if let Some(inner) = text
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return Some(unescape_double_quoted(inner));
    }
    text.strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .map(|inner| inner.replace("''", "'"))
}

fn unescape_double_quoted(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('"') => output.push('"'),
            Some('\\') => output.push('\\'),
            Some(other) => output.push(other),
            None => output.push('\\'),
        }
    }
    output
}

fn parse_number_or_text(text: &str) -> Result<FormatValue, String> {
    let normalized = text.replace('_', "");
    if is_integer_like(&normalized) {
        if let Ok(value) = normalized.parse::<i64>() {
            return Ok(FormatValue::Number(FormatNumber::Int(value)));
        }
        if let Ok(value) = normalized.parse::<f64>() {
            return Ok(FormatValue::Number(FormatNumber::Float(value)));
        }
    }
    if is_float_like(&normalized)
        && let Ok(value) = normalized.parse::<f64>()
    {
        return Ok(FormatValue::Number(FormatNumber::Float(value)));
    }
    Ok(FormatValue::Text(text.to_owned()))
}

fn is_integer_like(text: &str) -> bool {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn is_float_like(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) && text.parse::<f64>().is_ok()
}

fn encode_native() -> Value {
    Value::native(|args| {
        let [value] = args else {
            return Err(format!(
                "Yaml.encode expects 1 argument, got {}",
                args.len()
            ));
        };

        encode_to_text(value).map(Value::Text)
    })
}

fn encode_to_text(value: &Value) -> Result<String, String> {
    let mut output = String::new();
    encode_yaml_value(value, EncodePosition::TopLevel, 0, &mut output)?;
    if !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

#[derive(Clone, Copy)]
enum EncodePosition {
    TopLevel,
    RecordField,
    ArrayElement,
}

fn encode_yaml_value(
    value: &Value,
    position: EncodePosition,
    indent: usize,
    output: &mut String,
) -> Result<(), String> {
    if let Some(scalar) = yaml_scalar(value)? {
        output.push_str(&scalar);
        return Ok(());
    }
    if let Some(values) = yaml_sequence(value)? {
        encode_yaml_sequence(values, indent, output)
    } else if let Some(fields) = yaml_record_fields(value)? {
        encode_yaml_record(fields, indent, output)
    } else if let Some(entries) = yaml_map_entries(value)? {
        encode_yaml_map(entries, indent, output)
    } else {
        match value {
            Value::Undefined => match position {
                EncodePosition::RecordField => {
                    Err("Yaml.encode cannot encode undefined".to_owned())
                }
                EncodePosition::TopLevel => {
                    Err("Yaml.encode cannot encode top-level undefined".to_owned())
                }
                EncodePosition::ArrayElement => {
                    Err("Yaml.encode cannot encode undefined array elements".to_owned())
                }
            },
            Value::Closure(_) => Err("Yaml.encode cannot encode Function".to_owned()),
            Value::Native(_) => Err("Yaml.encode cannot encode Native".to_owned()),
            Value::Type(_) => Err("Yaml.encode cannot encode Type".to_owned()),
            Value::Tag { name, payload } if payload.is_empty() => Err(format!(
                "Yaml.encode cannot encode nullary tag @{name}; YAML tag wire form is not decided"
            )),
            Value::Tag { name, .. } => Err(format!(
                "Yaml.encode cannot encode tag @{name} with payload"
            )),
            _ => Err(format!(
                "Yaml.encode cannot encode {}",
                aven_value_type_name(value)
            )),
        }
    }
}

fn yaml_scalar(value: &Value) -> Result<Option<String>, String> {
    match value {
        Value::Int(value) => Ok(Some(value.to_string())),
        Value::Float(value) if value.is_finite() => Ok(Some(value.to_string())),
        Value::Float(_) => Err("Yaml.encode cannot encode NaN or infinite Float".to_owned()),
        Value::Text(value) => Ok(Some(quote_yaml_string(value))),
        Value::Bool(true) => Ok(Some("true".to_owned())),
        Value::Bool(false) => Ok(Some("false".to_owned())),
        Value::Null => Ok(Some("null".to_owned())),
        Value::Tag { name, payload } => yaml_scalar_from_json_constructor(name, payload),
        _ => Ok(None),
    }
}

fn yaml_scalar_from_json_constructor(
    name: &str,
    payload: &[Value],
) -> Result<Option<String>, String> {
    match name {
        "Null" => {
            let [] = payload else {
                return Err(json_constructor_shape_error(name, "no payload"));
            };
            Ok(Some("null".to_owned()))
        }
        "Bool" => {
            let [Value::Bool(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Bool"));
            };
            Ok(Some(if *value { "true" } else { "false" }.to_owned()))
        }
        "Int" => {
            let [Value::Int(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Int"));
            };
            Ok(Some(value.to_string()))
        }
        "Float" => {
            let [Value::Float(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Float"));
            };
            if !value.is_finite() {
                return Err("Yaml.encode cannot encode NaN or infinite Float".to_owned());
            }
            Ok(Some(value.to_string()))
        }
        "Text" => {
            let [Value::Text(value)] = payload else {
                return Err(json_constructor_shape_error(name, "Text"));
            };
            Ok(Some(quote_yaml_string(value)))
        }
        "Array" | "Object" => Ok(None),
        _ => Ok(None),
    }
}

fn yaml_sequence(value: &Value) -> Result<Option<&[Value]>, String> {
    match value {
        Value::Array(values) | Value::Tuple(values) | Value::Set(values) => Ok(Some(values)),
        Value::Tag { name, payload } if name == "Array" => {
            let [Value::Array(values)] = payload.as_slice() else {
                return Err(json_constructor_shape_error(name, "Array[Json]"));
            };
            Ok(Some(values))
        }
        _ => Ok(None),
    }
}

fn yaml_record_fields(value: &Value) -> Result<Option<&[(String, Value)]>, String> {
    match value {
        Value::Record(fields) => Ok(Some(fields)),
        _ => Ok(None),
    }
}

fn yaml_map_entries(value: &Value) -> Result<Option<&[(Value, Value)]>, String> {
    match value {
        Value::Map(entries) => Ok(Some(entries)),
        Value::Tag { name, payload } if name == "Object" => {
            let [Value::Map(entries)] = payload.as_slice() else {
                return Err(json_constructor_shape_error("Object", "Map[Text, Json]"));
            };
            Ok(Some(entries))
        }
        _ => Ok(None),
    }
}

fn encode_yaml_sequence(
    values: &[Value],
    indent: usize,
    output: &mut String,
) -> Result<(), String> {
    if values.is_empty() {
        output.push_str("[]");
        return Ok(());
    }

    for value in values {
        push_indent(output, indent);
        output.push_str("- ");
        if let Some(scalar) = yaml_scalar(value)? {
            output.push_str(&scalar);
            output.push('\n');
        } else {
            output.push('\n');
            encode_yaml_value(value, EncodePosition::ArrayElement, indent + 2, output)?;
            ensure_trailing_newline(output);
        }
    }
    Ok(())
}

fn encode_yaml_record(
    fields: &[(String, Value)],
    indent: usize,
    output: &mut String,
) -> Result<(), String> {
    if fields.is_empty() {
        output.push_str("{}");
        return Ok(());
    }

    for (name, value) in fields {
        if matches!(value, Value::Undefined) {
            continue;
        }
        push_indent(output, indent);
        output.push_str(&yaml_key(name));
        output.push(':');
        encode_yaml_field_value(value, indent, output)?;
    }
    Ok(())
}

fn encode_yaml_map(
    entries: &[(Value, Value)],
    indent: usize,
    output: &mut String,
) -> Result<(), String> {
    if entries.is_empty() {
        output.push_str("{}");
        return Ok(());
    }

    for (key, value) in entries {
        let Value::Text(key) = key else {
            return Err("Yaml.encode expected Map[Text, _] keys".to_owned());
        };
        if matches!(value, Value::Undefined) {
            continue;
        }
        push_indent(output, indent);
        output.push_str(&yaml_key(key));
        output.push(':');
        encode_yaml_field_value(value, indent, output)?;
    }
    Ok(())
}

fn encode_yaml_field_value(
    value: &Value,
    indent: usize,
    output: &mut String,
) -> Result<(), String> {
    if let Some(scalar) = yaml_scalar(value)? {
        output.push(' ');
        output.push_str(&scalar);
        output.push('\n');
    } else {
        output.push('\n');
        encode_yaml_value(value, EncodePosition::RecordField, indent + 2, output)?;
        ensure_trailing_newline(output);
    }
    Ok(())
}

fn push_indent(output: &mut String, indent: usize) {
    for _ in 0..indent {
        output.push(' ');
    }
}

fn ensure_trailing_newline(output: &mut String) {
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn yaml_key(key: &str) -> String {
    if key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        && !key.is_empty()
    {
        key.to_owned()
    } else {
        quote_yaml_string(key)
    }
}

fn quote_yaml_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn json_constructor_shape_error(name: &str, expected: &str) -> String {
    format!("Yaml.encode expected @{name} payload shape {expected}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use aven_core::{Span, codes};
    use aven_parser::parse_module;

    fn yaml_host() -> Host {
        let mut host = Host::new();
        host.register_yaml();
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

    fn run_diagnostics(source: &str) -> Vec<aven_core::Diagnostic> {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        aven_eval::eval_module_with_globals(&parsed.module, yaml_host().eval_globals()).diagnostics
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
    fn typed_decode_builds_record() {
        let value = run("Config = { name: Text, count: Int, enabled: Bool }\n\
             Yaml.decode(\"name: Ada\\ncount: 3\\nenabled: true\\n\", Config)?!\n");

        assert_eq!(text(field(&value, "name")), "Ada");
        assert_eq!(field(&value, "count"), &Value::Int(3));
        assert_eq!(field(&value, "enabled"), &Value::Bool(true));
    }

    #[test]
    fn dynamic_decode_uses_json_constructor_tree() {
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
             encoded = Yaml.encode({ name: \"Ada\", count: 3 })\n\
             decoded = Yaml.decode(encoded, Config)?!\n\
             { encoded: encoded, decoded: decoded }\n");

        assert_eq!(text(field(&value, "encoded")), "name: \"Ada\"\ncount: 3\n");
        assert_eq!(field(field(&value, "decoded"), "count"), &Value::Int(3));
    }

    #[test]
    fn multi_document_input_returns_parse_error() {
        let value = run("Yaml.decode(\"---\\nname: Ada\\n---\\nname: Grace\\n\")\n");
        let (kind, payload) = err_payload(&value);

        assert_eq!(kind, "Parse");
        assert!(
            text(field(payload, "message")).contains("single YAML document"),
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
    fn encode_rejects_unknown_tags() {
        let diagnostics = run_diagnostics("Yaml.encode(@Red)\n");

        assert_platform_error_contains(&diagnostics, "Yaml.encode cannot encode nullary tag @Red");
    }

    #[test]
    fn checker_resolves_one_arg_decode_to_dynamic_json_result() {
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

        assert_eq!(ty.render(), "Result[Json, YamlError]");
    }
}
