//! HTTP platform capability.
//!
//! `Http` is a plain host record value, matching the existing stream/file
//! handle shape: fields are native functions and the crate root owns the
//! matching record type.

use std::borrow::Cow;
use std::cell::RefCell;
use std::error::Error as StdError;
use std::io::{self, BufRead, BufReader, Read};
use std::rc::Rc;
use std::time::Duration;

use aven_check::{ComptimeArg, ComptimeError, HostComptimeFn, RowEntry, Type, type_fits_boundary};
use aven_eval::Value;

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value, read_all_value, read_line_value};

impl Host {
    /// Register the `Http` platform namespace (currently just `Http.get`).
    pub fn register_http(&mut self) {
        self.register("Http", http_value(), crate::http_type());
        self.register_comptime_type_resolver("Http.get", vec![1], get_comptime_resolver());
    }
}

type BodyReader = Rc<RefCell<BufReader<Box<dyn Read>>>>;
type ParsedOptions<'a> = (Vec<HeaderArg<'a>>, Vec<QueryArg<'a>>, Option<Duration>);

#[derive(Debug, Clone, Copy)]
struct QueryArg<'a> {
    name: &'a str,
    value: &'a str,
}

#[derive(Debug, Clone)]
struct HeaderArg<'a> {
    name: Cow<'a, str>,
    value: &'a str,
}

#[derive(Debug, Clone)]
enum OptionTextValue<'a> {
    Single(&'a str),
    Multiple(Vec<&'a str>),
}

#[derive(Debug, Clone)]
struct HttpGetArgs<'a> {
    url: &'a str,
    headers: Vec<HeaderArg<'a>>,
    params: Vec<QueryArg<'a>>,
    timeout: Option<Duration>,
}

struct HttpGetTypeResolver;

impl HostComptimeFn for HttpGetTypeResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        match args {
            [] => Ok(http_get_result_type()),
            [ComptimeArg::Type(options)] => {
                validate_options_type(options)?;
                Ok(http_get_result_type())
            }
            [_] => Err(ComptimeError::new("Http.get options must be a record type")),
            _ => Err(ComptimeError::new(format!(
                "Http.get expects at most one compile-time options type, got {}",
                args.len()
            ))),
        }
    }
}

pub(crate) fn get_comptime_resolver() -> Rc<dyn HostComptimeFn> {
    Rc::new(HttpGetTypeResolver)
}

fn http_get_result_type() -> Type {
    crate::build::result(crate::http_response_type(), crate::http_error_type())
}

fn validate_options_type(options: &Type) -> Result<(), ComptimeError> {
    let Type::Record(row) = options else {
        return Err(ComptimeError::new(format!(
            "Http.get options must be a record type, found `{}`",
            options.render()
        )));
    };
    for entry in &row.entries {
        let RowEntry::Field { name, ty } = entry else {
            return Ok(());
        };
        match name.as_str() {
            "headers" => validate_text_values_record_type("header", ty)?,
            "params" => validate_text_values_record_type("param", ty)?,
            "timeout" => validate_timeout_type(ty)?,
            other => {
                return Err(ComptimeError::new(format!("unknown Http option `{other}`")));
            }
        }
    }

    Ok(())
}

fn validate_text_values_record_type(kind: &str, ty: &Type) -> Result<(), ComptimeError> {
    let Type::Record(row) = ty else {
        return Err(ComptimeError::new(format!(
            "Http.get `{kind}s` option must be a record type, found `{}`",
            ty.render()
        )));
    };
    for entry in &row.entries {
        let RowEntry::Field { name, ty } = entry else {
            return Ok(());
        };
        validate_text_value_field_type(kind, name, ty)?;
    }

    Ok(())
}

fn validate_text_value_field_type(kind: &str, name: &str, ty: &Type) -> Result<(), ComptimeError> {
    if type_fits_boundary(&crate::build::text(), ty)
        || type_fits_boundary(&crate::build::array(crate::build::text()), ty)
    {
        return Ok(());
    }

    let optional = matches!(ty, Type::Optional(_) | Type::Nullable(_));
    let guard_note = if optional {
        "; optional header/param fields are not accepted yet, guard or default the value before passing it"
    } else {
        ""
    };
    Err(ComptimeError::new(format!(
        "{kind} `{name}` must be `Text` or `Array[Text]`, found `{}`{guard_note}",
        ty.render()
    )))
}

fn validate_timeout_type(ty: &Type) -> Result<(), ComptimeError> {
    if type_fits_boundary(&crate::build::int(), ty) {
        Ok(())
    } else {
        Err(ComptimeError::new(format!(
            "Http option `timeout` must be `Int`, found `{}`",
            ty.render()
        )))
    }
}

fn http_value() -> Value {
    Value::record(vec![("get".to_owned(), http_get_native())])
}

fn http_get_native() -> Value {
    Value::native(|args| {
        let args = http_get_args(args)?;
        let mut request = ureq::get(args.url);
        for header in args.headers {
            request = request.set(header.name.as_ref(), header.value);
        }
        for param in args.params {
            request = request.query(param.name, param.value);
        }
        if let Some(timeout) = args.timeout {
            request = request.timeout(timeout);
        }

        Ok(match request.call() {
            Ok(response) | Err(ureq::Error::Status(_, response)) => {
                ok_value(http_response_value(response))
            }
            Err(ureq::Error::Transport(error)) => err_value(http_transport_error_value(&error)),
        })
    })
}

fn http_get_args(args: &[Value]) -> Result<HttpGetArgs<'_>, String> {
    if !(1..=2).contains(&args.len()) {
        return Err(format!(
            "Http.get expects 1 or 2 arguments, got {}",
            args.len()
        ));
    }

    let Value::Text(url) = &args[0] else {
        return Err(format!(
            "Http.get expects a Text URL, got {}",
            aven_value_type_name(&args[0])
        ));
    };

    let (headers, params, timeout) = match args.get(1) {
        None => (Vec::new(), Vec::new(), None),
        Some(Value::Record(fields)) => parse_options(fields.as_ref())?,
        Some(other) => {
            return Err(format!(
                "Http.get expects options Record, got {}",
                aven_value_type_name(other)
            ));
        }
    };

    Ok(HttpGetArgs {
        url,
        headers,
        params,
        timeout,
    })
}

fn parse_options(fields: &[(String, Value)]) -> Result<ParsedOptions<'_>, String> {
    validate_option_keys(fields)?;
    Ok((
        parse_header_options(fields)?,
        parse_param_options(fields)?,
        parse_timeout_option(fields)?,
    ))
}

fn validate_option_keys(fields: &[(String, Value)]) -> Result<(), String> {
    for (name, _) in fields {
        if !matches!(name.as_str(), "headers" | "params" | "timeout") {
            return Err(format!("unknown Http option `{name}`"));
        }
    }
    Ok(())
}

fn parse_header_options(fields: &[(String, Value)]) -> Result<Vec<HeaderArg<'_>>, String> {
    let values = option_text_values(fields, "headers")?;
    let mut headers = Vec::with_capacity(values.len());
    for (name, value) in values {
        match value {
            OptionTextValue::Single(value) => headers.push(HeaderArg {
                name: Cow::Borrowed(name),
                value,
            }),
            OptionTextValue::Multiple(values) => {
                let total = values.len();
                for (index, value) in values.into_iter().enumerate() {
                    headers.push(HeaderArg {
                        name: repeated_header_name(name, index, total)?,
                        value,
                    });
                }
            }
        }
    }
    Ok(headers)
}

fn parse_param_options(fields: &[(String, Value)]) -> Result<Vec<QueryArg<'_>>, String> {
    let values = option_text_values(fields, "params")?;
    let mut params = Vec::new();
    for (name, value) in values {
        match value {
            OptionTextValue::Single(value) => params.push(QueryArg { name, value }),
            OptionTextValue::Multiple(values) => {
                params.extend(values.into_iter().map(|value| QueryArg { name, value }))
            }
        }
    }
    Ok(params)
}

fn repeated_header_name(name: &str, index: usize, total: usize) -> Result<Cow<'_, str>, String> {
    if index == 0 || name.starts_with("x-") || name.starts_with("X-") {
        return Ok(Cow::Borrowed(name));
    }

    header_name_case_variant(name, index).map(Cow::Owned).ok_or_else(|| {
        format!(
            "Http.get options.headers.{name} has {total} values, but this header name cannot be emitted repeatedly"
        )
    })
}

fn header_name_case_variant(name: &str, ordinal: usize) -> Option<String> {
    let alpha_count = name
        .bytes()
        .filter(|byte| byte.is_ascii_alphabetic())
        .count();
    if alpha_count == 0 {
        return None;
    }
    let variant_count = 1usize.checked_shl(alpha_count as u32)?;
    if ordinal >= variant_count {
        return None;
    }

    let mut remaining = ordinal;
    for mask in 0..variant_count {
        let mut alpha_index = 0;
        let variant = name
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphabetic() {
                    let upper = (mask & (1 << alpha_index)) != 0;
                    alpha_index += 1;
                    if upper {
                        byte.to_ascii_uppercase()
                    } else {
                        byte.to_ascii_lowercase()
                    }
                } else {
                    byte
                }
            })
            .map(char::from)
            .collect::<String>();

        if variant == name {
            continue;
        }
        if remaining == 1 {
            return Some(variant);
        }
        remaining -= 1;
    }

    None
}

fn parse_timeout_option(fields: &[(String, Value)]) -> Result<Option<Duration>, String> {
    match record_field(fields, "timeout") {
        None | Some(Value::Undefined) => Ok(None),
        Some(Value::Int(ms)) if *ms >= 0 => Ok(Some(Duration::from_millis(*ms as u64))),
        Some(Value::Int(ms)) => Err(format!(
            "Http.get options.timeout expects non-negative Int milliseconds, got {ms}"
        )),
        Some(other) => Err(format!(
            "Http.get options.timeout expects Int milliseconds, got {}",
            aven_value_type_name(other)
        )),
    }
}

fn option_text_values<'a>(
    fields: &'a [(String, Value)],
    option_name: &str,
) -> Result<Vec<(&'a str, OptionTextValue<'a>)>, String> {
    match record_field(fields, option_name) {
        None | Some(Value::Undefined) => Ok(Vec::new()),
        Some(Value::Record(fields)) => parse_text_value_record(option_name, fields.as_ref()),
        Some(other) => Err(format!(
            "Http.get options.{option_name} expects Record, got {}",
            aven_value_type_name(other)
        )),
    }
}

fn parse_text_value_record<'a>(
    option_name: &str,
    fields: &'a [(String, Value)],
) -> Result<Vec<(&'a str, OptionTextValue<'a>)>, String> {
    fields
        .iter()
        .map(|(name, value)| {
            parse_option_text_value(option_name, name, value).map(|value| (name.as_str(), value))
        })
        .collect()
}

fn parse_option_text_value<'a>(
    option_name: &str,
    field_name: &str,
    value: &'a Value,
) -> Result<OptionTextValue<'a>, String> {
    match value {
        Value::Text(text) => Ok(OptionTextValue::Single(text)),
        Value::Array(values) => {
            let mut texts = Vec::with_capacity(values.len());
            for (index, value) in values.iter().enumerate() {
                let Value::Text(text) = value else {
                    return Err(format!(
                        "Http.get options.{option_name}.{field_name} expects Text or Array Text, got Array with {} at index {index}",
                        aven_value_type_name(value)
                    ));
                };
                texts.push(text.as_str());
            }
            Ok(OptionTextValue::Multiple(texts))
        }
        other => Err(format!(
            "Http.get options.{option_name}.{field_name} expects Text or Array Text, got {}",
            aven_value_type_name(other)
        )),
    }
}

fn record_field<'a>(fields: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(field_name, value)| (field_name == name).then_some(value))
}

fn http_response_value(response: ureq::Response) -> Value {
    let status = response.status();
    let headers = response_headers(&response);
    let reader: Box<dyn Read> = response.into_reader();
    Value::record(vec![
        ("status".to_owned(), Value::Int(i64::from(status))),
        ("headers".to_owned(), Value::Map(Rc::clone(&headers))),
        ("first".to_owned(), first_header_native(Rc::clone(&headers))),
        ("body".to_owned(), body_handle_value(reader)),
    ])
}

fn response_headers(response: &ureq::Response) -> Rc<Vec<(Value, Value)>> {
    let mut seen = Vec::new();
    let mut headers = Vec::new();

    for name in response.headers_names() {
        let lower = name.to_ascii_lowercase();
        if seen.contains(&lower) {
            continue;
        }
        seen.push(lower.clone());
        let values = response
            .all(&name)
            .into_iter()
            .map(|value| Value::Text(value.to_owned()))
            .collect::<Vec<_>>();
        headers.push((Value::Text(lower), Value::Array(Rc::new(values))));
    }

    Rc::new(headers)
}

fn first_header_native(headers: Rc<Vec<(Value, Value)>>) -> Value {
    Value::native(move |args| {
        if args.len() != 1 {
            return Err(format!("first expects 1 argument, got {}", args.len()));
        }
        let Value::Text(name) = &args[0] else {
            return Err(format!(
                "first expects a Text header name, got {}",
                aven_value_type_name(&args[0])
            ));
        };
        let lower = name.to_ascii_lowercase();

        for (key, values) in headers.iter() {
            if !matches!(key, Value::Text(candidate) if candidate == &lower) {
                continue;
            }
            let Value::Array(values) = values else {
                return Ok(Value::Undefined);
            };
            return Ok(values.first().cloned().unwrap_or(Value::Undefined));
        }

        Ok(Value::Undefined)
    })
}

fn body_handle_value(reader: Box<dyn Read>) -> Value {
    let reader = Rc::new(RefCell::new(BufReader::new(reader)));
    Value::record(vec![
        ("readLine".to_owned(), body_read_line_native(&reader)),
        ("readAll".to_owned(), body_read_all_native(&reader)),
    ])
}

fn body_read_line_native(reader: &BodyReader) -> Value {
    let reader = Rc::clone(reader);
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("readLine expects 0 arguments, got {}", args.len()));
        }

        let mut line = String::new();
        let result = reader.borrow_mut().read_line(&mut line);
        Ok(read_line_value(result, line))
    })
}

fn body_read_all_native(reader: &BodyReader) -> Value {
    let reader = Rc::clone(reader);
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("readAll expects 0 arguments, got {}", args.len()));
        }

        let mut text = String::new();
        let result = reader.borrow_mut().read_to_string(&mut text);
        Ok(read_all_value(result, text))
    })
}

fn http_transport_error_value(error: &ureq::Transport) -> Value {
    Value::Tag {
        name: http_transport_error_tag(error).to_owned(),
        payload: vec![Value::Text(error.to_string())],
    }
}

fn http_transport_error_tag(error: &ureq::Transport) -> &'static str {
    if transport_timed_out(error) {
        return "Timeout";
    }

    match error.kind() {
        ureq::ErrorKind::InvalidUrl
        | ureq::ErrorKind::UnknownScheme
        | ureq::ErrorKind::InvalidProxyUrl => "InvalidUrl",
        ureq::ErrorKind::Dns
        | ureq::ErrorKind::ConnectionFailed
        | ureq::ErrorKind::ProxyConnect => "ConnectionFailed",
        _ => "Other",
    }
}

fn transport_timed_out(error: &ureq::Transport) -> bool {
    StdError::source(error)
        .and_then(|source| source.downcast_ref::<io::Error>())
        .is_some_and(|source| source.kind() == io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    use aven_check::{RowEntry, RowTail, Type, record_fields, variant_tags};
    use aven_core::{Span, codes};
    use aven_eval::eval_module_with_globals;
    use aven_parser::parse_module;

    fn http_host() -> Host {
        let mut host = Host::new();
        host.register_http();
        host
    }

    fn check_module(source: &str) -> aven_check::CheckOutput {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        aven_check::check_module_with_host_globals(
            &parsed.module,
            &http_host().check_host_globals(),
        )
    }

    fn check_diagnostics(source: &str) -> Vec<aven_core::Diagnostic> {
        check_module(source).diagnostics
    }

    fn assert_checks(source: &str) {
        let checked = check_module(source);
        assert!(
            checked.diagnostics.is_empty(),
            "program checks: {:?}",
            checked.diagnostics
        );
    }

    fn binding_type(source: &str, name: &str) -> Type {
        let checked = check_module(source);
        assert!(
            checked.diagnostics.is_empty(),
            "program checks: {:?}",
            checked.diagnostics
        );
        let offset = source
            .find(name)
            .unwrap_or_else(|| panic!("source mentions `{name}`"));
        checked
            .type_at(Span::new(offset, offset + name.len()))
            .unwrap_or_else(|| panic!("`{name}` has an inferred type"))
            .clone()
    }

    fn run(source: &str) -> Value {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        let outcome = eval_module_with_globals(&parsed.module, http_host().eval_globals());
        assert!(
            outcome.diagnostics.is_empty(),
            "program runs: {:?}",
            outcome.diagnostics
        );
        outcome
            .value
            .unwrap_or_else(|| panic!("program yields a value"))
    }

    #[test]
    fn http_response_type_uses_status_headers_and_read_body() {
        assert_eq!(
            record_field_type(&crate::http_response_type(), "status"),
            crate::build::int()
        );
        assert_eq!(
            record_field_type(&crate::http_response_type(), "headers"),
            crate::build::map(
                crate::build::text(),
                crate::build::array(crate::build::text())
            )
        );
        assert_eq!(
            record_field_type(&crate::http_response_type(), "first"),
            crate::build::function(
                vec![crate::build::text()],
                crate::build::optional(crate::build::text())
            )
        );
        assert_eq!(
            record_field_type(&crate::http_response_type(), "body"),
            crate::stdin_handle_type()
        );
    }

    #[test]
    fn http_error_type_is_closed_variant_with_documented_tags() {
        assert_eq!(
            variant_tags(&crate::http_error_type()),
            Some(vec![
                "Timeout".to_owned(),
                "ConnectionFailed".to_owned(),
                "InvalidUrl".to_owned(),
                "Other".to_owned(),
            ])
        );

        let Type::Variant(row) = crate::http_error_type() else {
            panic!("http error type is a variant");
        };
        assert_eq!(row.tail, RowTail::Closed, "http error variant is closed");
        for entry in &row.entries {
            let RowEntry::Tag { payload, .. } = entry else {
                panic!("http error variant entry is a tag");
            };
            assert_eq!(
                payload,
                &vec![crate::build::text()],
                "each tag carries a Text message"
            );
        }
    }

    #[test]
    fn http_get_checks_with_url_only() {
        let ty = binding_type("res = Http.get(\"u\")\n", "res");
        assert_eq!(
            ty,
            crate::build::result(crate::http_response_type(), crate::http_error_type())
        );
    }

    #[test]
    fn http_get_checks_with_headers_options() {
        assert_checks("res = Http.get(\"u\", { headers: { Authorization: \"A\" } })\n");
    }

    #[test]
    fn http_get_checks_with_params_options() {
        assert_checks("res = Http.get(\"u\", { params: { tag: [\"a\", \"b\"] } })\n");
    }

    #[test]
    fn http_get_checks_with_computed_hyphenated_header_key() {
        assert_checks("res = Http.get(\"u\", { headers: { [\"Content-Type\"]: \"x\" } })\n");
    }

    #[test]
    fn http_get_checks_with_headers_and_params_options() {
        assert_checks(
            "id = \"req-1\"\nres = Http.get(\"u\", { headers: { accept: [\"application/json\", \"text/html\"], \"x-request-id\": id }, params: { tag: [\"a\", \"b\"], page: \"2\" }, timeout: 5000 })\n",
        );
    }

    #[test]
    fn http_get_checks_with_empty_options() {
        assert_checks("res = Http.get(\"u\", {})\n");
    }

    #[test]
    fn http_get_response_headers_and_first_check() {
        let source = "res = Http.get(\"u\")?^\ncookies = res.headers.get(\"set-cookie\")\ncontentType = res.first(\"content-type\")\n";
        assert_eq!(
            binding_type(source, "cookies"),
            crate::build::optional(crate::build::array(crate::build::text()))
        );
        assert_eq!(
            binding_type(source, "contentType"),
            crate::build::optional(crate::build::text())
        );
    }

    #[test]
    fn http_get_rejects_non_text_url() {
        let diagnostics = check_diagnostics("res = Http.get(5)\n");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::MISMATCH)),
            "non-Text URL is rejected: {diagnostics:?}"
        );
    }

    #[test]
    fn http_get_rejects_bad_header_field_type() {
        let diagnostics = check_diagnostics("res = Http.get(\"u\", { headers: { accept: 1 } })\n");
        assert_host_error_contains(
            &diagnostics,
            "header `accept` must be `Text` or `Array[Text]`",
        );
    }

    #[test]
    fn http_get_rejects_bad_timeout_type() {
        let diagnostics = check_diagnostics("res = Http.get(\"u\", { timeout: \"5\" })\n");
        assert_host_error_contains(&diagnostics, "Http option `timeout` must be `Int`");
    }

    #[test]
    fn http_get_rejects_unknown_option_key() {
        let diagnostics = check_diagnostics("res = Http.get(\"u\", { hedaers: {} })\n");
        assert_host_error_contains(&diagnostics, "unknown Http option `hedaers`");
    }

    #[test]
    fn http_get_rejects_optional_header_field_type() {
        let diagnostics = check_diagnostics(
            "maybe : ?Text = undefined\nres = Http.get(\"u\", { headers: { accept: maybe } })\n",
        );
        assert_host_error_contains(&diagnostics, "guard or default the value before passing it");
    }

    #[test]
    fn http_get_non_concrete_options_type_defers_without_diagnostic() {
        let diagnostics = check_diagnostics("fetch = (options) => Http.get(\"u\", options)\n");
        assert!(
            diagnostics.is_empty(),
            "non-concrete options type defers quietly: {diagnostics:?}"
        );
    }

    #[test]
    fn http_response_body_checks_as_read_handle() {
        assert_checks("res = Http.get(\"u\")?!\nres.body.readLine()\n");
    }

    #[test]
    fn http_response_body_lacks_write_method() {
        let diagnostics = check_diagnostics("res = Http.get(\"u\")?!\nres.body.write(\"x\")\n");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("type.missing-field")),
            "body read handle has no write: {diagnostics:?}"
        );
    }

    #[test]
    fn http_get_invalid_url_returns_invalid_url_error_offline() {
        let value = run("Http.get(\"broken/url\")\n");
        let Value::Tag { name, payload } = value else {
            panic!("expected a Result tag");
        };
        assert_eq!(name, "Err");

        let Some(error) = payload.first() else {
            panic!("Result error payload is present");
        };
        let Value::Tag {
            name: error_name,
            payload: error_payload,
        } = error
        else {
            panic!("expected an HttpError tag");
        };
        assert_eq!(error_name, "InvalidUrl");
        assert!(
            matches!(error_payload.as_slice(), [Value::Text(message)] if !message.is_empty()),
            "error carries a message: {error_payload:?}"
        );
    }

    #[test]
    fn http_get_native_rejects_non_text_url_arg() {
        let Value::Native(get) = http_get_native() else {
            panic!("Http.get is native");
        };
        let error = match get(&[Value::Int(5)]) {
            Ok(value) => panic!("expected native arg error, got {value:?}"),
            Err(error) => error,
        };
        assert_eq!(error, "Http.get expects a Text URL, got Int");
    }

    #[test]
    fn http_get_native_rejects_non_text_header_value() {
        let Value::Native(get) = http_get_native() else {
            panic!("Http.get is native");
        };
        let options = Value::record(vec![(
            "headers".to_owned(),
            Value::record(vec![("Authorization".to_owned(), Value::Int(5))]),
        )]);
        let error = match get(&[Value::Text("https://example.com".to_owned()), options]) {
            Ok(value) => panic!("expected native arg error, got {value:?}"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "Http.get options.headers.Authorization expects Text or Array Text, got Int"
        );
    }

    #[test]
    fn http_get_native_rejects_unknown_option_key() {
        let Value::Native(get) = http_get_native() else {
            panic!("Http.get is native");
        };
        let options = Value::record(vec![("hedaers".to_owned(), Value::record(vec![]))]);
        let error = match get(&[Value::Text("https://example.com".to_owned()), options]) {
            Ok(value) => panic!("expected native arg error, got {value:?}"),
            Err(error) => error,
        };
        assert_eq!(error, "unknown Http option `hedaers`");
    }

    #[test]
    fn http_get_args_parse_timeout_milliseconds() {
        let options = Value::record(vec![("timeout".to_owned(), Value::Int(25))]);
        let values = [Value::Text("https://example.com".to_owned()), options];
        let args = http_get_args(&values).expect("valid args");
        assert_eq!(args.timeout, Some(Duration::from_millis(25)));
    }

    #[test]
    fn http_get_timeout_plumbs_to_ureq() {
        let (url, handle) = spawn_one_request_server_with_delay(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            Duration::from_millis(100),
        );
        let value = run(&format!("Http.get(\"{url}/slow\", {{ timeout: 1 }})\n"));
        let _request = handle.join().expect("server thread completes");

        let Value::Tag { name, payload } = value else {
            panic!("expected a Result tag");
        };
        assert_eq!(name, "Err");
        let Some(Value::Tag {
            name: error_name, ..
        }) = payload.first()
        else {
            panic!("expected an HttpError payload");
        };
        assert_eq!(error_name, "Timeout");
    }

    #[test]
    fn response_headers_collect_repeated_values_lowercased() {
        let response = response_with_headers();
        let headers = response_headers(&response);

        assert!(headers.iter().any(|(key, value)| {
            matches!(
                (key, value),
                (Value::Text(name), Value::Array(values))
                    if name == "set-cookie"
                        && values.as_slice()
                            == [Value::Text("a=1".to_owned()), Value::Text("b=2".to_owned())]
            )
        }));
    }

    #[test]
    fn response_first_returns_first_value_or_undefined() {
        let response = response_with_headers();
        let headers = response_headers(&response);
        let Value::Native(first) = first_header_native(headers) else {
            panic!("first is native");
        };

        assert_eq!(
            first(&[Value::Text("SET-COOKIE".to_owned())]),
            Ok(Value::Text("a=1".to_owned()))
        );
        assert_eq!(
            first(&[Value::Text("missing".to_owned())]),
            Ok(Value::Undefined)
        );
    }

    #[test]
    fn http_get_end_to_end_repeats_request_and_response_headers() {
        let (url, handle) = spawn_one_request_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nset-cookie: b=2\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let source = format!(
            "resp = Http.get(\"{url}/demo\", {{ headers: {{ accept: [\"application/json\", \"text/html\"] }}, params: {{ tag: [\"a\", \"b\"], page: \"2\" }} }})?^\n\
             {{ status: resp.status, cookies: resp.headers.get(\"set-cookie\"), contentType: resp.first(\"content-type\"), body: resp.body.readAll()?^ }}\n"
        );
        let value = run(&source);
        let request = handle.join().expect("server thread completes");

        assert!(
            request.starts_with("GET /demo?tag=a&tag=b&page=2 HTTP/1.1\r\n"),
            "request line includes repeated query params: {request:?}"
        );
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("accept: application/json")),
            "first repeated Accept header reaches the wire: {request:?}"
        );
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("accept: text/html")),
            "second repeated Accept header reaches the wire: {request:?}"
        );

        let Value::Record(fields) = value else {
            panic!("program returns a record");
        };
        assert_eq!(value_record_field(&fields, "status"), &Value::Int(200));
        assert_eq!(
            value_record_field(&fields, "contentType"),
            &Value::Text("text/plain".to_owned())
        );
        assert_eq!(
            value_record_field(&fields, "body"),
            &Value::Text("ok".to_owned())
        );
        assert_eq!(
            value_record_field(&fields, "cookies"),
            &Value::Array(Rc::new(vec![
                Value::Text("a=1".to_owned()),
                Value::Text("b=2".to_owned())
            ]))
        );
    }

    fn record_field_type(ty: &Type, name: &str) -> Type {
        record_fields(ty)
            .unwrap_or_else(|| panic!("expected a record type"))
            .into_iter()
            .find_map(|field| (field.name == name).then_some(field.ty))
            .unwrap_or_else(|| panic!("expected record field `{name}`"))
    }

    fn assert_host_error_contains(diagnostics: &[aven_core::Diagnostic], message: &str) {
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some(codes::comptime::HOST_FUNCTION)
                    && diagnostic.message.contains(message)
            }),
            "expected host-comptime diagnostic containing {message:?}: {diagnostics:?}"
        );
    }

    fn response_with_headers() -> ureq::Response {
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nset-cookie: b=2\r\nContent-Length: 0\r\n\r\n"
            .parse()
            .expect("response parses")
    }

    fn spawn_one_request_server(response: &'static str) -> (String, thread::JoinHandle<String>) {
        spawn_one_request_server_with_delay(response, Duration::ZERO)
    }

    fn spawn_one_request_server_with_delay(
        response: &'static str,
        delay: Duration,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().expect("read local address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept one request");
            let mut request = Vec::new();
            let mut buffer = [0; 512];
            loop {
                let read = stream.read(&mut buffer).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            thread::sleep(delay);
            let _ = stream.write_all(response.as_bytes());
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://{addr}"), handle)
    }

    fn value_record_field<'a>(fields: &'a [(String, Value)], name: &str) -> &'a Value {
        fields
            .iter()
            .find_map(|(field_name, value)| (field_name == name).then_some(value))
            .unwrap_or_else(|| panic!("expected value field `{name}`"))
    }
}
