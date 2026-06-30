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

use aven_eval::Value;

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value, read_all_value, read_line_value};

impl Host {
    /// Register the `Http` platform namespace (currently just `Http.get`).
    pub fn register_http(&mut self) {
        self.register("Http", http_value(), crate::http_type());
    }
}

type BodyReader = Rc<RefCell<BufReader<Box<dyn Read>>>>;

#[derive(Debug, Clone, Copy)]
struct QueryArg<'a> {
    name: &'a str,
    value: &'a str,
}

#[derive(Debug, Clone)]
struct HeaderArg<'a> {
    name: &'a str,
    value: Cow<'a, str>,
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
}

fn http_value() -> Value {
    Value::record(vec![("get".to_owned(), http_get_native())])
}

fn http_get_native() -> Value {
    Value::native(|args| {
        let args = http_get_args(args)?;
        let mut request = ureq::get(args.url);
        for header in args.headers {
            request = request.set(header.name, header.value.as_ref());
        }
        for param in args.params {
            request = request.query(param.name, param.value);
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

    let (headers, params) = match args.get(1) {
        None => (Vec::new(), Vec::new()),
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
    })
}

fn parse_options(
    fields: &[(String, Value)],
) -> Result<(Vec<HeaderArg<'_>>, Vec<QueryArg<'_>>), String> {
    Ok((parse_header_options(fields)?, parse_param_options(fields)?))
}

fn parse_header_options(fields: &[(String, Value)]) -> Result<Vec<HeaderArg<'_>>, String> {
    let values = option_text_values(fields, "headers")?;
    let mut headers = Vec::with_capacity(values.len());
    for (name, value) in values {
        let value = match value {
            OptionTextValue::Single(value) => Cow::Borrowed(value),
            // RFC 7230 treats repeated header fields as comma-equivalent for most
            // headers. Cookie uses "; "; callers needing that pass a pre-joined string.
            OptionTextValue::Multiple(values) => Cow::Owned(values.join(", ")),
        };
        headers.push(HeaderArg { name, value });
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
    let headers = response_headers_value(&response);
    let reader: Box<dyn Read> = response.into_reader();
    Value::record(vec![
        ("status".to_owned(), Value::Int(i64::from(status))),
        ("headers".to_owned(), headers),
        ("body".to_owned(), body_handle_value(reader)),
    ])
}

fn response_headers_value(response: &ureq::Response) -> Value {
    let mut seen = Vec::new();
    let mut headers = Vec::new();

    for name in response.headers_names() {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        headers.extend(
            response
                .all(&name)
                .into_iter()
                .map(|value| http_header_value(&name, value)),
        );
    }

    Value::Array(Rc::new(headers))
}

fn http_header_value(name: &str, value: &str) -> Value {
    Value::record(vec![
        ("name".to_owned(), Value::Text(name.to_owned())),
        ("value".to_owned(), Value::Text(value.to_owned())),
    ])
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
            crate::build::array(crate::http_header_type())
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
            "res = Http.get(\"u\", { headers: { [\"Content-Type\"]: \"x\" }, params: { page: \"2\" } })\n",
        );
    }

    #[test]
    fn http_get_checks_with_empty_options() {
        assert_checks("res = Http.get(\"u\", {})\n");
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

    fn record_field_type(ty: &Type, name: &str) -> Type {
        record_fields(ty)
            .unwrap_or_else(|| panic!("expected a record type"))
            .into_iter()
            .find_map(|field| (field.name == name).then_some(field.ty))
            .unwrap_or_else(|| panic!("expected record field `{name}`"))
    }
}
