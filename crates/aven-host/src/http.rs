//! HTTP platform capability.
//!
//! `Http` is a plain host record value, matching the existing stream/file
//! handle shape: fields are native functions and the crate root owns the
//! matching record type.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::rc::Rc;
use std::time::{Duration, Instant};

use aven_check::{ComptimeArg, ComptimeError, HostComptimeFn, RowEntry, Type, type_fits_boundary};
use aven_eval::Value;

use crate::Host;
use crate::io::{aven_value_type_name, err_value, ok_value, read_all_value, read_line_value};
use crate::json;

impl Host {
    /// Register the `Http` platform namespace.
    pub fn register_http(&mut self) {
        self.register("Http", http_value(), crate::http_type());
        for method in HttpMethod::ALL {
            self.register_comptime_type_resolver(
                method.resolver_key(),
                vec![1],
                comptime_resolver(method),
            );
        }
    }
}

type BodyReader = Rc<RefCell<BufReader<Box<dyn Read>>>>;
type HttpResponse = ureq::http::Response<ureq::Body>;

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

#[derive(Debug, Clone, Copy)]
pub(crate) enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl HttpMethod {
    const ALL: [Self; 5] = [Self::Get, Self::Post, Self::Put, Self::Delete, Self::Patch];

    fn api_name(self) -> &'static str {
        match self {
            Self::Get => "Http.get",
            Self::Post => "Http.post",
            Self::Put => "Http.put",
            Self::Delete => "Http.delete",
            Self::Patch => "Http.patch",
        }
    }

    fn field_name(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Put => "put",
            Self::Delete => "delete",
            Self::Patch => "patch",
        }
    }

    fn resolver_key(self) -> &'static str {
        self.api_name()
    }

    fn allows_body_options(self) -> bool {
        !matches!(self, Self::Get)
    }
}

#[derive(Debug, Clone)]
struct HttpArgs<'a> {
    method: HttpMethod,
    url: &'a str,
    options: HttpOptions<'a>,
}

#[derive(Debug, Clone)]
struct HttpOptions<'a> {
    headers: Vec<HeaderArg<'a>>,
    params: Vec<QueryArg<'a>>,
    timeout: Option<Duration>,
    body: Option<RequestBody<'a>>,
}

impl<'a> HttpOptions<'a> {
    fn empty() -> Self {
        Self {
            headers: Vec::new(),
            params: Vec::new(),
            timeout: None,
            body: None,
        }
    }
}

#[derive(Debug, Clone)]
enum RequestBody<'a> {
    Text(&'a str),
    Json(String),
}

struct HttpTypeResolver {
    method: HttpMethod,
}

impl HostComptimeFn for HttpTypeResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        match args {
            [] => Ok(http_result_type()),
            [ComptimeArg::Type(options)] => {
                validate_options_type(self.method, options)?;
                Ok(http_result_type())
            }
            [_] => Err(ComptimeError::new(format!(
                "{} options must be a record type",
                self.method.api_name()
            ))),
            _ => Err(ComptimeError::new(format!(
                "{} expects at most one compile-time options type, got {}",
                self.method.api_name(),
                args.len()
            ))),
        }
    }
}

pub(crate) fn comptime_resolver(method: HttpMethod) -> Rc<dyn HostComptimeFn> {
    Rc::new(HttpTypeResolver { method })
}

fn http_result_type() -> Type {
    crate::build::result(crate::http_response_type(), crate::http_error_type())
}

fn validate_options_type(method: HttpMethod, options: &Type) -> Result<(), ComptimeError> {
    let Type::Record(row) = options else {
        return Err(ComptimeError::new(format!(
            "{} options must be a record type, found `{}`",
            method.api_name(),
            options.render()
        )));
    };

    let mut has_body = false;
    let mut has_json = false;
    for entry in &row.entries {
        let RowEntry::Field { name, ty } = entry else {
            return Ok(());
        };
        match name.as_str() {
            "headers" => validate_text_values_record_type(method, "header", ty)?,
            "params" => validate_text_values_record_type(method, "param", ty)?,
            "timeout" => validate_timeout_type(ty)?,
            "body" if method.allows_body_options() => {
                has_body = true;
                validate_body_type(ty)?;
            }
            "json" if method.allows_body_options() => {
                has_json = true;
            }
            other => {
                return Err(ComptimeError::new(format!("unknown Http option `{other}`")));
            }
        }
    }

    if has_body && has_json {
        return Err(ComptimeError::new(
            "options cannot have both `body` and `json`",
        ));
    }

    Ok(())
}

fn validate_text_values_record_type(
    method: HttpMethod,
    kind: &str,
    ty: &Type,
) -> Result<(), ComptimeError> {
    let Type::Record(row) = ty else {
        return Err(ComptimeError::new(format!(
            "{} `{kind}s` option must be a record type, found `{}`",
            method.api_name(),
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
        "{kind} `{name}` must be `Text` or `Array(Text)`, found `{}`{guard_note}",
        ty.render()
    )))
}

fn validate_body_type(ty: &Type) -> Result<(), ComptimeError> {
    if type_fits_boundary(&crate::build::text(), ty) {
        Ok(())
    } else {
        Err(ComptimeError::new(format!(
            "body option must be `Text`, found `{}`",
            ty.render()
        )))
    }
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
    Value::record(
        HttpMethod::ALL
            .into_iter()
            .map(|method| (method.field_name().to_owned(), http_native(method)))
            .collect(),
    )
}

fn http_native(method: HttpMethod) -> Value {
    Value::native(move |args| {
        let args = http_args(method, args)?;
        Ok(http_result_value(send_http_request(&args)))
    })
}

fn http_args(method: HttpMethod, args: &[Value]) -> Result<HttpArgs<'_>, String> {
    if !(1..=2).contains(&args.len()) {
        return Err(format!(
            "{} expects 1 or 2 arguments, got {}",
            method.api_name(),
            args.len()
        ));
    }

    let Value::Text(url) = &args[0] else {
        return Err(format!(
            "{} expects a Text URL, got {}",
            method.api_name(),
            aven_value_type_name(&args[0])
        ));
    };

    let options = match args.get(1) {
        None => HttpOptions::empty(),
        Some(Value::Record(fields)) => parse_options(method, fields.as_ref())?,
        Some(other) => {
            return Err(format!(
                "{} expects options Record, got {}",
                method.api_name(),
                aven_value_type_name(other)
            ));
        }
    };

    Ok(HttpArgs {
        method,
        url,
        options,
    })
}

fn send_http_request(args: &HttpArgs<'_>) -> Result<HttpResponse, ureq::Error> {
    let agent = http_agent(args.options.timeout);
    let started = Instant::now();
    let result = match args.method {
        HttpMethod::Get => send_without_body(agent.get(args.url), &args.options),
        HttpMethod::Post => send_with_optional_body(agent.post(args.url), &args.options),
        HttpMethod::Put => send_with_optional_body(agent.put(args.url), &args.options),
        HttpMethod::Delete => send_without_body(agent.delete(args.url), &args.options),
        HttpMethod::Patch => send_with_optional_body(agent.patch(args.url), &args.options),
    };

    // ureq 3.3 can still return a delayed local response after the configured
    // receive timeout; preserve Aven's single timeout contract at this boundary.
    if args
        .options
        .timeout
        .is_some_and(|timeout| started.elapsed() >= timeout)
    {
        Err(ureq::Error::Timeout(ureq::Timeout::Global))
    } else {
        result
    }
}

fn http_agent(timeout: Option<Duration>) -> ureq::Agent {
    let mut config = ureq::Agent::config_builder().http_status_as_error(false);
    if let Some(timeout) = timeout {
        config = config
            .timeout_global(Some(timeout))
            .timeout_resolve(Some(timeout))
            .timeout_connect(Some(timeout))
            .timeout_send_request(Some(timeout))
            .timeout_await_100(Some(timeout))
            .timeout_send_body(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout));
    }
    ureq::Agent::new_with_config(config.build())
}

fn send_without_body(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    options: &HttpOptions<'_>,
) -> Result<HttpResponse, ureq::Error> {
    let request = apply_common_options(request, options);
    match &options.body {
        Some(body) => send_body(request.force_send_body(), body),
        None => request.call(),
    }
}

fn send_with_optional_body(
    request: ureq::RequestBuilder<ureq::typestate::WithBody>,
    options: &HttpOptions<'_>,
) -> Result<HttpResponse, ureq::Error> {
    let request = apply_common_options(request, options);
    match &options.body {
        Some(body) => send_body(request, body),
        None => request.send_empty(),
    }
}

fn apply_common_options<BodyState>(
    mut request: ureq::RequestBuilder<BodyState>,
    options: &HttpOptions<'_>,
) -> ureq::RequestBuilder<BodyState> {
    for header in &options.headers {
        // ureq 3 appends repeated header names here; ureq 2 replaced them.
        request = request.header(header.name.as_ref(), header.value);
    }
    for param in &options.params {
        request = request.query(param.name, param.value);
    }
    request
}

fn send_body(
    request: ureq::RequestBuilder<ureq::typestate::WithBody>,
    body: &RequestBody<'_>,
) -> Result<HttpResponse, ureq::Error> {
    match body {
        RequestBody::Text(text) => request.send(*text),
        RequestBody::Json(text) => request.send(text.as_str()),
    }
}

fn http_result_value(result: Result<HttpResponse, ureq::Error>) -> Value {
    match result {
        Ok(response) => ok_value(http_response_value(response)),
        Err(error) => err_value(http_transport_error_value(&error)),
    }
}

fn parse_options(
    method: HttpMethod,
    fields: &[(String, Value)],
) -> Result<HttpOptions<'_>, String> {
    validate_option_keys(method, fields)?;
    let mut options = HttpOptions {
        headers: parse_header_options(method, fields)?,
        params: parse_param_options(method, fields)?,
        timeout: parse_timeout_option(method, fields)?,
        body: parse_body_option(method, fields)?,
    };
    if matches!(options.body, Some(RequestBody::Json(_))) && !has_content_type_header(&options) {
        options.headers.push(HeaderArg {
            name: Cow::Borrowed("content-type"),
            value: "application/json",
        });
    }
    Ok(options)
}

fn validate_option_keys(method: HttpMethod, fields: &[(String, Value)]) -> Result<(), String> {
    for (name, _) in fields {
        let valid = matches!(name.as_str(), "headers" | "params" | "timeout")
            || (method.allows_body_options() && matches!(name.as_str(), "body" | "json"));
        if !valid {
            return Err(format!("unknown Http option `{name}`"));
        }
    }
    Ok(())
}

fn parse_header_options(
    method: HttpMethod,
    fields: &[(String, Value)],
) -> Result<Vec<HeaderArg<'_>>, String> {
    let values = option_text_values(method, fields, "headers")?;
    let mut headers = Vec::with_capacity(values.len());
    for (name, value) in values {
        match value {
            OptionTextValue::Single(value) => headers.push(HeaderArg {
                name: Cow::Borrowed(name),
                value,
            }),
            OptionTextValue::Multiple(values) => {
                for value in values {
                    headers.push(HeaderArg {
                        name: Cow::Borrowed(name),
                        value,
                    });
                }
            }
        }
    }
    Ok(headers)
}

fn parse_param_options(
    method: HttpMethod,
    fields: &[(String, Value)],
) -> Result<Vec<QueryArg<'_>>, String> {
    let values = option_text_values(method, fields, "params")?;
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

fn parse_timeout_option(
    method: HttpMethod,
    fields: &[(String, Value)],
) -> Result<Option<Duration>, String> {
    match record_field(fields, "timeout") {
        None | Some(Value::Undefined) => Ok(None),
        Some(Value::Int(ms)) if *ms >= 0 => Ok(Some(Duration::from_millis(*ms as u64))),
        Some(Value::Int(ms)) => Err(format!(
            "{} options.timeout expects non-negative Int milliseconds, got {ms}",
            method.api_name()
        )),
        Some(other) => Err(format!(
            "{} options.timeout expects Int milliseconds, got {}",
            method.api_name(),
            aven_value_type_name(other)
        )),
    }
}

fn parse_body_option<'a>(
    method: HttpMethod,
    fields: &'a [(String, Value)],
) -> Result<Option<RequestBody<'a>>, String> {
    if !method.allows_body_options() {
        return Ok(None);
    }

    let body = record_field(fields, "body");
    let json = record_field(fields, "json");
    if body.is_some() && json.is_some() {
        return Err("options cannot have both `body` and `json`".to_owned());
    }

    match (body, json) {
        (Some(Value::Text(text)), None) => Ok(Some(RequestBody::Text(text))),
        (Some(other), None) => Err(format!(
            "{} options.body expects Text, got {}",
            method.api_name(),
            aven_value_type_name(other)
        )),
        (None, Some(value)) => {
            json::encode_to_text(value).map(|text| Some(RequestBody::Json(text)))
        }
        (None, None) => Ok(None),
        (Some(_), Some(_)) => unreachable!("body/json conflict returned earlier"),
    }
}

fn has_content_type_header(options: &HttpOptions<'_>) -> bool {
    options
        .headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case("content-type"))
}

fn option_text_values<'a>(
    method: HttpMethod,
    fields: &'a [(String, Value)],
    option_name: &str,
) -> Result<Vec<(&'a str, OptionTextValue<'a>)>, String> {
    match record_field(fields, option_name) {
        None | Some(Value::Undefined) => Ok(Vec::new()),
        Some(Value::Record(fields)) => {
            parse_text_value_record(method, option_name, fields.as_ref())
        }
        Some(other) => Err(format!(
            "{} options.{option_name} expects Record, got {}",
            method.api_name(),
            aven_value_type_name(other)
        )),
    }
}

fn parse_text_value_record<'a>(
    method: HttpMethod,
    option_name: &str,
    fields: &'a [(String, Value)],
) -> Result<Vec<(&'a str, OptionTextValue<'a>)>, String> {
    fields
        .iter()
        .map(|(name, value)| {
            parse_option_text_value(method, option_name, name, value)
                .map(|value| (name.as_str(), value))
        })
        .collect()
}

fn parse_option_text_value<'a>(
    method: HttpMethod,
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
                        "{} options.{option_name}.{field_name} expects Text or Array Text, got Array with {} at index {index}",
                        method.api_name(),
                        aven_value_type_name(value)
                    ));
                };
                texts.push(text.as_str());
            }
            Ok(OptionTextValue::Multiple(texts))
        }
        other => Err(format!(
            "{} options.{option_name}.{field_name} expects Text or Array Text, got {}",
            method.api_name(),
            aven_value_type_name(other)
        )),
    }
}

fn record_field<'a>(fields: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(field_name, value)| (field_name == name).then_some(value))
}

fn http_response_value(response: HttpResponse) -> Value {
    let status = response.status().as_u16();
    let headers = response_headers(&response);
    let (_, body) = response.into_parts();
    let reader: Box<dyn Read> = Box::new(body.into_reader());
    Value::record(vec![
        ("status".to_owned(), Value::Int(i64::from(status))),
        ("headers".to_owned(), Value::Map(Rc::clone(&headers))),
        ("first".to_owned(), first_header_native(Rc::clone(&headers))),
        ("body".to_owned(), body_handle_value(reader)),
    ])
}

fn response_headers(response: &HttpResponse) -> Rc<Vec<(Value, Value)>> {
    let mut seen = Vec::new();
    let mut headers = Vec::new();

    for name in response.headers().keys() {
        let lower = name.as_str().to_ascii_lowercase();
        if seen.contains(&lower) {
            continue;
        }
        seen.push(lower.clone());
        let values = response
            .headers()
            .get_all(name)
            .into_iter()
            .map(|value| Value::Text(String::from_utf8_lossy(value.as_bytes()).to_string()))
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

fn http_transport_error_value(error: &ureq::Error) -> Value {
    Value::Tag {
        name: http_transport_error_tag(error).to_owned(),
        payload: vec![Value::Text(error.to_string())],
    }
}

fn http_transport_error_tag(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::Timeout(_) => "Timeout",
        ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::InvalidProxyUrl => {
            "InvalidUrl"
        }
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_) => {
            "ConnectionFailed"
        }
        _ => "Other",
    }
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

    fn run_diagnostics(source: &str) -> Vec<aven_core::Diagnostic> {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        eval_module_with_globals(&parsed.module, http_host().eval_globals()).diagnostics
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
    fn http_mutating_methods_check_with_url_only() {
        for method in ["post", "put", "delete", "patch"] {
            let source = format!("res = Http.{method}(\"u\")\n");
            let ty = binding_type(&source, "res");
            assert_eq!(
                ty,
                crate::build::result(crate::http_response_type(), crate::http_error_type()),
                "Http.{method} result type"
            );
        }
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
            "header `accept` must be `Text` or `Array(Text)`",
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
    fn http_get_rejects_body_as_unknown_option() {
        let diagnostics = check_diagnostics("res = Http.get(\"u\", { body: \"x\" })\n");
        assert_host_error_contains(&diagnostics, "unknown Http option `body`");
    }

    #[test]
    fn http_post_rejects_bad_body_type() {
        let diagnostics = check_diagnostics("res = Http.post(\"u\", { body: 1 })\n");
        assert_host_error_contains(&diagnostics, "body option must be `Text`");
    }

    #[test]
    fn http_post_rejects_body_and_json_together() {
        let diagnostics =
            check_diagnostics("res = Http.post(\"u\", { body: \"x\", json: { ok: true } })\n");
        assert_host_error_contains(&diagnostics, "options cannot have both `body` and `json`");
    }

    #[test]
    fn http_post_json_accepts_concrete_record_type() {
        assert_checks(
            "patch = { name: \"Ada\", tags: [\"admin\"] }\nres = Http.post(\"u\", { json: patch })\n",
        );
    }

    #[test]
    fn http_post_json_non_concrete_value_type_defers_without_diagnostic() {
        let diagnostics =
            check_diagnostics("send = (value) => Http.post(\"u\", { json: value })\n");
        assert!(
            diagnostics.is_empty(),
            "non-concrete json value type defers quietly: {diagnostics:?}"
        );
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
        let Value::Native(get) = http_native(HttpMethod::Get) else {
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
        let Value::Native(get) = http_native(HttpMethod::Get) else {
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
        let Value::Native(get) = http_native(HttpMethod::Get) else {
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
    fn http_post_native_rejects_body_and_json_together() {
        let Value::Native(post) = http_native(HttpMethod::Post) else {
            panic!("Http.post is native");
        };
        let options = Value::record(vec![
            ("body".to_owned(), Value::Text("raw".to_owned())),
            (
                "json".to_owned(),
                Value::record(vec![("ok".to_owned(), Value::Bool(true))]),
            ),
        ]);
        let error = match post(&[Value::Text("https://example.com".to_owned()), options]) {
            Ok(value) => panic!("expected native arg error, got {value:?}"),
            Err(error) => error,
        };
        assert_eq!(error, "options cannot have both `body` and `json`");
    }

    #[test]
    fn http_post_body_json_conflict_is_runtime_platform_error() {
        let diagnostics = run_diagnostics(
            "Http.post(\"http://127.0.0.1:1\", { body: \"raw\", json: { ok: true } })\n",
        );
        assert_platform_error_contains(&diagnostics, "options cannot have both `body` and `json`");
    }

    #[test]
    fn http_post_non_encodable_json_surfaces_json_encode_error() {
        let diagnostics = run_diagnostics("Http.post(\"http://127.0.0.1:1\", { json: Http })\n");
        assert_platform_error_contains(&diagnostics, "Json.encode cannot encode Native");
    }

    #[test]
    fn http_get_args_parse_timeout_milliseconds() {
        let options = Value::record(vec![("timeout".to_owned(), Value::Int(25))]);
        let values = [Value::Text("https://example.com".to_owned()), options];
        let args = http_args(HttpMethod::Get, &values).expect("valid args");
        assert_eq!(args.options.timeout, Some(Duration::from_millis(25)));
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
                .any(|line| line == "accept: application/json"),
            "first repeated Accept header reaches the wire without case variation: {request:?}"
        );
        assert!(
            request.lines().any(|line| line == "accept: text/html"),
            "second repeated Accept header reaches the wire without case variation: {request:?}"
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

    #[test]
    fn http_post_json_end_to_end_sets_body_headers_and_builds_response() {
        let (url, handle) = spawn_one_request_server(
            "HTTP/1.1 201 Created\r\nLocation: /users/ada\r\nContent-Type: text/plain\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncreated",
        );
        let source = format!(
            "resp = Http.post(\"{url}/users\", {{ headers: {{ accept: [\"application/json\", \"text/plain\"], \"x-request-id\": \"req-1\" }}, json: {{ name: \"Ada\", tags: [\"admin\"] }} }})?^\n\
             {{ status: resp.status, location: resp.first(\"location\"), body: resp.body.readAll()?^ }}\n"
        );
        let value = run(&source);
        let request = handle.join().expect("server thread completes");

        assert!(
            request.starts_with("POST /users HTTP/1.1\r\n"),
            "POST request line reaches the wire: {request:?}"
        );
        assert_request_has_header(&request, "content-type: application/json");
        assert_eq!(
            request
                .lines()
                .filter(|line| *line == "accept: application/json")
                .count(),
            1,
            "first repeated header is appended without case variation: {request:?}"
        );
        assert_eq!(
            request
                .lines()
                .filter(|line| *line == "accept: text/plain")
                .count(),
            1,
            "second repeated header is appended without case variation: {request:?}"
        );
        assert_request_has_header(&request, "x-request-id: req-1");
        assert_eq!(
            request_body(&request),
            "{\"name\":\"Ada\",\"tags\":[\"admin\"]}"
        );

        let Value::Record(fields) = value else {
            panic!("program returns a record");
        };
        assert_eq!(value_record_field(&fields, "status"), &Value::Int(201));
        assert_eq!(
            value_record_field(&fields, "location"),
            &Value::Text("/users/ada".to_owned())
        );
        assert_eq!(
            value_record_field(&fields, "body"),
            &Value::Text("created".to_owned())
        );
    }

    #[test]
    fn http_mutating_methods_reach_wire() {
        for (method, call) in [
            ("PUT", "Http.put(\"{url}/item\", { body: \"raw\" })?^\n"),
            ("DELETE", "Http.delete(\"{url}/item\")?^\n"),
            ("PATCH", "Http.patch(\"{url}/item\", { body: \"raw\" })?^\n"),
        ] {
            let (url, handle) = spawn_one_request_server(
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let source = call.replace("{url}", &url);
            let _value = run(&source);
            let request = handle.join().expect("server thread completes");
            assert!(
                request.starts_with(&format!("{method} /item HTTP/1.1\r\n")),
                "{method} request line reaches the wire: {request:?}"
            );
        }
    }

    #[test]
    fn http_json_respects_explicit_content_type_header() {
        let (url, handle) = spawn_one_request_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let source = format!(
            "Http.patch(\"{url}/doc\", {{ headers: {{ \"content-type\": \"application/merge-patch+json\" }}, json: {{ active: true }} }})?^\n"
        );
        let _value = run(&source);
        let request = handle.join().expect("server thread completes");

        assert_request_has_header(&request, "content-type: application/merge-patch+json");
        assert!(
            !request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("content-type: application/json")),
            "implicit JSON content-type is not added when explicit header exists: {request:?}"
        );
        assert_eq!(request_body(&request), "{\"active\":true}");
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

    fn response_with_headers() -> HttpResponse {
        ureq::http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .header("Set-Cookie", "a=1")
            .header("set-cookie", "b=2")
            .body(ureq::Body::builder().data(""))
            .expect("response builds")
    }

    fn assert_request_has_header(request: &str, expected: &str) {
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case(expected)),
            "expected header {expected:?}: {request:?}"
        );
    }

    fn request_body(request: &str) -> &str {
        request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("")
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
                if request_is_complete(&request) {
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

    fn request_is_complete(request: &[u8]) -> bool {
        let Some(body_start) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        else {
            return false;
        };

        request.len() >= body_start + request_content_length(&request[..body_start])
    }

    fn request_content_length(headers: &[u8]) -> usize {
        let text = String::from_utf8_lossy(headers);
        text.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }
}
