//! Host registry binding runtime values to their Aven types.
//!
//! A [`Host`] is the single place where a Rust-implemented library or platform
//! declares a name once and feeds both halves of the toolchain: the runtime
//! [`aven_eval::Value`] flows to the evaluator and the [`aven_check::Type`] flows
//! to the checker, so the two can never drift. Libraries and platforms use the
//! same [`Host::register`] entry point; required capabilities (e.g. logging) are
//! Rust traits the platform implements, while the statically-known value+type is
//! registered through helpers like [`Host::register_logger`].

mod http;
mod io;
mod json;
mod marshal;

use std::rc::Rc;

use aven_check::{HostComptimeFn, HostComptimeFnSpec, HostGlobals, Type};

pub use marshal::{AvenMarshal, IntoHostFn};

/// Re-exported Aven type builders so hosts spell types without depending on
/// `aven-check` directly (the registration/typing vocabulary lives here).
pub use aven_check::build;
use aven_eval::Value;
use aven_eval::logging::{LogSink, TraceContext, logger};

/// A name registered with both a runtime value and an Aven type.
struct TypedEntry {
    name: String,
    value: Value,
    ty: Type,
}

/// A name registered with a runtime value but no expressible type yet. It runs
/// but is not type-checked.
struct RuntimeOnlyEntry {
    name: String,
    value: Value,
}

/// A checker-visible named type alias with no runtime value.
struct TypeDefinitionEntry {
    name: String,
    ty: Type,
}

/// A name registered with a runtime value, a base checker type, and a
/// host-side comptime resolver that can refine call result types.
struct ComptimeEntry {
    name: String,
    value: Value,
    ty: Type,
    resolver: Rc<dyn HostComptimeFn>,
    comptime_params: Vec<usize>,
}

/// A host-side comptime resolver keyed independently from runtime value/type
/// registration, for resolvers that live under fields of typed host records.
struct ComptimeResolverEntry {
    key: String,
    resolver: Rc<dyn HostComptimeFn>,
    comptime_params: Vec<usize>,
}

/// Registry of host/library globals seeded into the evaluator and the checker.
#[derive(Default)]
pub struct Host {
    typed: Vec<TypedEntry>,
    runtime_only: Vec<RuntimeOnlyEntry>,
    type_definitions: Vec<TypeDefinitionEntry>,
    comptime: Vec<ComptimeEntry>,
    comptime_resolvers: Vec<ComptimeResolverEntry>,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a name with its runtime value AND its Aven type (the normal path
    /// for both libraries and platforms). Free [`build::var`] variables in `ty`
    /// are generalized by the checker and instantiated fresh at each use site.
    pub fn register(&mut self, name: impl Into<String>, value: Value, ty: Type) {
        self.typed.push(TypedEntry {
            name: name.into(),
            value,
            ty,
        });
    }

    /// Escape hatch for a value whose type is not registered yet. Runs but is NOT
    /// type-checked.
    pub fn register_runtime_only(&mut self, name: impl Into<String>, value: Value) {
        self.runtime_only.push(RuntimeOnlyEntry {
            name: name.into(),
            value,
        });
    }

    /// Register a named type alias visible to the checker, without adding a
    /// runtime value to the host prelude.
    pub fn register_type_definition(&mut self, name: impl Into<String>, ty: Type) {
        self.type_definitions.push(TypeDefinitionEntry {
            name: name.into(),
            ty,
        });
    }

    /// Register a host function whose ordinary base type binds the name and
    /// checks arguments, while a Rust resolver computes the call result type
    /// from the listed compile-time argument indexes.
    pub fn register_comptime_fn(
        &mut self,
        name: impl Into<String>,
        value: Value,
        ty: Type,
        comptime_params: Vec<usize>,
        resolver: Rc<dyn HostComptimeFn>,
    ) {
        self.comptime.push(ComptimeEntry {
            name: name.into(),
            value,
            ty,
            resolver,
            comptime_params,
        });
    }

    /// Register a comptime resolver under an arbitrary checker key, without a
    /// matching runtime value/type entry.
    pub fn register_comptime_resolver(
        &mut self,
        key: impl Into<String>,
        comptime_params: Vec<usize>,
        resolver: Rc<dyn HostComptimeFn>,
    ) {
        self.comptime_resolvers.push(ComptimeResolverEntry {
            key: key.into(),
            resolver,
            comptime_params,
        });
    }

    /// Register a typed Rust closure: derive both its Aven [`Type`] and a
    /// marshalling [`Value::native`] from the signature so the value and type
    /// can't drift, then register them through the normal [`Host::register`]
    /// path. Monomorphic primitives only — see [`AvenMarshal`].
    pub fn register_fn<F, Args>(&mut self, name: impl Into<String>, f: F)
    where
        F: IntoHostFn<Args>,
    {
        let (ty, value) = f.into_host_fn();
        self.register(name, value, ty);
    }

    /// Register the `logger` required capability: build the logger value from the
    /// platform's [`LogSink`] implementation and register it under `"logger"` with
    /// the statically-known [`logger_type`].
    pub fn register_logger(&mut self, sink: Rc<dyn LogSink>, trace: TraceContext) {
        self.register("logger", logger(sink, trace), logger_type());
    }

    /// Globals for the evaluator (all registered values).
    pub fn eval_globals(&self) -> Vec<(String, Value)> {
        self.typed
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .chain(
                self.comptime
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.value.clone())),
            )
            .chain(
                self.runtime_only
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.value.clone())),
            )
            .collect()
    }

    /// Globals for the checker (only typed registrations).
    pub fn check_globals(&self) -> Vec<(String, Type)> {
        self.typed
            .iter()
            .map(|entry| (entry.name.clone(), entry.ty.clone()))
            .chain(
                self.comptime
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.ty.clone())),
            )
            .collect()
    }

    /// Globals for the checker, including host comptime resolvers.
    pub fn check_host_globals(&self) -> HostGlobals {
        HostGlobals::new(
            self.check_globals(),
            self.comptime
                .iter()
                .map(|entry| {
                    (
                        entry.name.clone(),
                        HostComptimeFnSpec::new(
                            Rc::clone(&entry.resolver),
                            entry.comptime_params.clone(),
                        ),
                    )
                })
                .chain(self.comptime_resolvers.iter().map(|entry| {
                    (
                        entry.key.clone(),
                        HostComptimeFnSpec::new(
                            Rc::clone(&entry.resolver),
                            entry.comptime_params.clone(),
                        ),
                    )
                }))
                .collect(),
        )
        .with_type_definitions(
            self.type_definitions
                .iter()
                .map(|entry| (entry.name.clone(), entry.ty.clone()))
                .collect(),
        )
    }
}

/// The Aven type of the standard logger value.
///
/// Approximate: the `child` method returns an open record rather than a named
/// recursive `Logger`. A precise recursive type is deferred until a named
/// `Logger` type / typed-fn adapter exists.
pub fn logger_type() -> Type {
    // `(Text, ?{..}) -> Unit`: one required message, an optional trailing fields
    // record, so both `logger.info("msg")` and `logger.info("msg", { .. })` check.
    let level_method = || {
        build::function_opt(
            vec![build::text()],
            vec![build::open_record(vec![])],
            build::unit(),
        )
    };
    // `({..}) -> {..}` — open-record return approximates the recursive `Logger`.
    let child = build::function(vec![build::open_record(vec![])], build::open_record(vec![]));

    build::record(vec![
        ("trace", level_method()),
        ("debug", level_method()),
        ("info", level_method()),
        ("warn", level_method()),
        ("error", level_method()),
        ("fatal", level_method()),
        ("child", child),
    ])
}

/// The Aven type of the standard `dbg` value.
pub fn dbg_type() -> Type {
    build::function(vec![build::var("a")], build::var("a"))
}

/// The Aven type of the standard `write` value.
pub fn io_write_type() -> Type {
    build::function(vec![build::text()], build::empty_record())
}

/// The Aven type of the standard `writeLine` value.
pub fn io_write_line_type() -> Type {
    build::function(vec![build::text()], build::empty_record())
}

/// The Aven type of the standard `readLine` value.
pub fn io_read_line_type() -> Type {
    build::function(vec![], build::optional(build::text()))
}

/// The Aven type of the standard `readAll` value.
pub fn io_read_all_type() -> Type {
    build::function(vec![], build::text())
}

/// The closed `WriteError` variant: write-side IO failures, each tag carrying a
/// `Text` message. Tags mirror the `std::io::ErrorKind`s the CLI distinguishes.
pub fn write_error_type() -> Type {
    build::variant(vec![
        ("BrokenPipe", vec![build::text()]),
        ("PermissionDenied", vec![build::text()]),
        ("Other", vec![build::text()]),
    ])
}

/// The closed `ReadError` variant: read-side IO failures, each tag carrying a
/// `Text` message. EOF is reported as `@Ok(undefined)`, not a `ReadError`.
pub fn read_error_type() -> Type {
    build::variant(vec![
        ("UnexpectedEof", vec![build::text()]),
        ("Other", vec![build::text()]),
    ])
}

/// The closed `IoError` variant: generic stream/file failures (used by `flush`
/// and `open`), each tag carrying a `Text` message. The `open`-side kinds
/// (`NotFound`/`PermissionDenied`/`AlreadyExists`) join the stream-side
/// `BrokenPipe`/`Other` so one error type covers both surfaces.
pub fn io_error_type() -> Type {
    build::variant(vec![
        ("NotFound", vec![build::text()]),
        ("PermissionDenied", vec![build::text()]),
        ("AlreadyExists", vec![build::text()]),
        ("BrokenPipe", vec![build::text()]),
        ("Other", vec![build::text()]),
    ])
}

/// An HTTP header as surfaced on requests and responses.
pub fn http_header_type() -> Type {
    build::record(vec![("name", build::text()), ("value", build::text())])
}

/// The closed `HttpError` variant: transport failures from `Http.get`, each tag
/// carrying a `Text` message. HTTP status codes are response data, not errors.
pub fn http_error_type() -> Type {
    build::variant(vec![
        ("Timeout", vec![build::text()]),
        ("ConnectionFailed", vec![build::text()]),
        ("InvalidUrl", vec![build::text()]),
        ("Other", vec![build::text()]),
    ])
}

/// The `Http.get` response record.
pub fn http_response_type() -> Type {
    build::record(vec![
        ("status", build::int()),
        ("headers", build::array(http_header_type())),
        ("body", stdin_handle_type()),
    ])
}

/// `(Text, ?{ headers: ?{..}, params: ?{..} }) -> Result[Response, HttpError]`.
pub fn http_get_type() -> Type {
    let text_value_record = || build::optional(build::open_record(vec![]));
    let options = build::record(vec![
        ("headers", text_value_record()),
        ("params", text_value_record()),
    ]);
    build::function_opt(
        vec![build::text()],
        vec![options],
        build::result(http_response_type(), http_error_type()),
    )
}

/// The `Http` platform namespace record.
pub fn http_type() -> Type {
    build::record(vec![("get", http_get_type())])
}

/// The closed `JsonError` variant returned by `Json.decode`.
pub fn json_error_type() -> Type {
    build::variant(vec![
        (
            "Parse",
            vec![build::record(vec![("message", build::text())])],
        ),
        (
            "Shape",
            vec![build::record(vec![
                ("path", build::text()),
                ("expected", build::text()),
                ("found", build::text()),
            ])],
        ),
    ])
}

/// `(a) -> Text` — `Json.encode` accepts any checked value and validates JSON
/// encodability at runtime.
pub fn json_encode_type() -> Type {
    build::function(vec![build::var("a")], build::text())
}

/// The base `Json.decode` type: `(Text, ?) -> ?`. The checker uses it for
/// arity and the input text argument; the host comptime resolver refines the
/// result from the trailing type argument.
pub fn json_decode_base_type() -> Type {
    build::function(vec![build::text(), Type::Deferred], Type::Deferred)
}

/// The `Json` platform namespace record.
pub fn json_type() -> Type {
    build::record(vec![
        ("encode", json_encode_type()),
        ("decode", json_decode_base_type()),
    ])
}

/// The base `open` type: `(Text, "r" | "w" | "a" | "rw") -> ?`. The checker
/// uses it for name binding, arity, and argument validation; the host comptime
/// resolver refines the result from the second argument's mode string.
pub fn open_base_type() -> Type {
    build::function(
        vec![build::text(), build::text_literals(&["r", "w", "a", "rw"])],
        Type::Deferred,
    )
}

/// The `File` platform namespace record.
pub fn file_type() -> Type {
    build::record(vec![("open", open_base_type())])
}

/// `(Text) -> Result[{}, WriteError]` — a handle `write`/`writeLine` method.
fn handle_write_type() -> Type {
    build::function(
        vec![build::text()],
        build::result(build::empty_record(), write_error_type()),
    )
}

/// `() -> Result[?Text, ReadError]` — a handle `readLine` method (EOF is
/// `@Ok(undefined)`).
fn handle_read_line_type() -> Type {
    build::function(
        vec![],
        build::result(build::optional(build::text()), read_error_type()),
    )
}

/// `() -> Result[Text, ReadError]` — a handle `readAll` method.
fn handle_read_all_type() -> Type {
    build::function(vec![], build::result(build::text(), read_error_type()))
}

/// `() -> Result[{}, IoError]` — a handle `flush` method.
fn handle_flush_type() -> Type {
    build::function(
        vec![],
        build::result(build::empty_record(), io_error_type()),
    )
}

/// The `stdout` handle type: a closed record of write-side methods. `stderr`
/// shares this shape. Callers annotate parameters as open records (e.g.
/// `{ write : (Text) -> Result[{}, WriteError] | r }`), so width subtyping lets
/// a function needing only `write` accept any of these handles.
pub fn stdout_handle_type() -> Type {
    build::record(vec![
        ("write", handle_write_type()),
        ("writeLine", handle_write_type()),
        ("flush", handle_flush_type()),
    ])
}

/// The `stderr` handle type (identical shape to [`stdout_handle_type`]).
pub fn stderr_handle_type() -> Type {
    stdout_handle_type()
}

/// The `stdin` handle type: a closed record of read-side methods.
pub fn stdin_handle_type() -> Type {
    build::record(vec![
        ("readLine", handle_read_line_type()),
        ("readAll", handle_read_all_type()),
    ])
}

/// The `stdio` handle type: the union of read- and write-side methods.
pub fn stdio_handle_type() -> Type {
    build::record(vec![
        ("write", handle_write_type()),
        ("writeLine", handle_write_type()),
        ("readLine", handle_read_line_type()),
        ("readAll", handle_read_all_type()),
        ("flush", handle_flush_type()),
    ])
}

/// Type globals for the standard host prelude used by `aven check` and the LSP.
pub fn standard_check_globals() -> Vec<(String, Type)> {
    standard_check_host_globals().types
}

/// Type globals plus host comptime resolvers for the standard host prelude.
pub fn standard_check_host_globals() -> HostGlobals {
    let types = vec![
        ("logger".to_owned(), logger_type()),
        ("dbg".to_owned(), dbg_type()),
        ("write".to_owned(), io_write_type()),
        ("writeLine".to_owned(), io_write_line_type()),
        ("readLine".to_owned(), io_read_line_type()),
        ("readAll".to_owned(), io_read_all_type()),
        ("stdout".to_owned(), stdout_handle_type()),
        ("stderr".to_owned(), stderr_handle_type()),
        ("stdin".to_owned(), stdin_handle_type()),
        ("stdio".to_owned(), stdio_handle_type()),
        ("File".to_owned(), file_type()),
        ("Http".to_owned(), http_type()),
        ("Json".to_owned(), json_type()),
    ];

    HostGlobals::new(
        types,
        vec![
            (
                "File.open".to_owned(),
                HostComptimeFnSpec::new(io::open_comptime_resolver(), vec![1]),
            ),
            (
                "Json.decode".to_owned(),
                HostComptimeFnSpec::new(json::decode_comptime_resolver(), vec![1]),
            ),
        ],
    )
    .with_type_definitions(vec![("JsonError".to_owned(), json_error_type())])
}

#[cfg(test)]
mod tests {
    use super::*;

    use aven_check::{function_required_arity, function_signature, record_fields, variant_tags};

    struct NullSink;

    impl LogSink for NullSink {
        fn emit(&self, _record: &aven_eval::logging::LogRecord<'_>) {}
    }

    fn trace_context() -> TraceContext {
        TraceContext {
            trace_id: "0".repeat(32),
            span_id: "0".repeat(16),
            trace_flags: "01".to_owned(),
            trace_state: String::new(),
        }
    }

    #[test]
    fn register_round_trips_into_both_globals() {
        let mut host = Host::new();
        host.register("answer", Value::Int(42), build::int());

        let eval = host.eval_globals();
        assert_eq!(eval, vec![("answer".to_owned(), Value::Int(42))]);

        let check = host.check_globals();
        assert_eq!(check, vec![("answer".to_owned(), build::int())]);
    }

    #[test]
    fn runtime_only_is_evaluated_but_not_checked() {
        let mut host = Host::new();
        host.register_runtime_only("dbg", Value::Int(7));

        assert_eq!(host.eval_globals(), vec![("dbg".to_owned(), Value::Int(7))]);
        assert!(host.check_globals().is_empty());
    }

    #[test]
    fn register_fn_derives_type_and_native_in_globals() {
        let mut host = Host::new();
        host.register_fn("add", |a: i64, b: i64| a + b);

        let check = host.check_globals();
        assert_eq!(check.len(), 1);
        assert_eq!(check[0].0, "add");
        let (params, result) = function_signature(&check[0].1).expect("add is a function");
        assert_eq!(params, vec![build::int(), build::int()]);
        assert_eq!(result, build::int());
        assert_eq!(function_required_arity(&check[0].1), Some(2));

        let eval = host.eval_globals();
        assert_eq!(eval.len(), 1);
        let Value::Native(native) = &eval[0].1 else {
            panic!("add is a native value");
        };
        assert_eq!(native(&[Value::Int(2), Value::Int(3)]), Ok(Value::Int(5)));
        assert_eq!(
            native(&[Value::Text("x".to_owned()), Value::Int(3)]),
            Err("expected Int, got Text".to_owned())
        );
        assert_eq!(
            native(&[Value::Int(2)]),
            Err("expected 2 arguments, got 1".to_owned())
        );
    }

    #[test]
    fn register_fn_nullary() {
        let mut host = Host::new();
        host.register_fn("answer", || 42_i64);

        let check = host.check_globals();
        assert_eq!(check[0].1, build::function(vec![], build::int()));

        let eval = host.eval_globals();
        let Value::Native(native) = &eval[0].1 else {
            panic!("answer is a native value");
        };
        assert_eq!(native(&[]), Ok(Value::Int(42)));
    }

    #[test]
    fn register_fn_checks_and_evaluates_end_to_end() {
        use aven_parser::parse_module;

        let mut host = Host::new();
        host.register_fn("add", |a: i64, b: i64| a + b);

        let ok = parse_module("add(2, 3)\n");
        assert!(
            ok.diagnostics.is_empty(),
            "program parses: {:?}",
            ok.diagnostics
        );
        let checked = aven_check::check_module_with_globals(&ok.module, &host.check_globals());
        assert!(
            checked.diagnostics.is_empty(),
            "add(2, 3) checks: {:?}",
            checked.diagnostics
        );
        let evaluated = aven_eval::eval_module_with_globals(&ok.module, host.eval_globals());
        assert_eq!(evaluated.value, Some(Value::Int(5)));

        let bad = parse_module("add(\"x\", 3)\n");
        let checked = aven_check::check_module_with_globals(&bad.module, &host.check_globals());
        assert!(
            !checked.diagnostics.is_empty(),
            "add(\"x\", 3) is a type error"
        );
    }

    #[test]
    fn register_logger_types_the_method_record() {
        let mut host = Host::new();
        host.register_logger(Rc::new(NullSink), trace_context());

        let eval = host.eval_globals();
        assert_eq!(eval.len(), 1);
        assert_eq!(eval[0].0, "logger");

        let check = host.check_globals();
        assert_eq!(check.len(), 1);
        assert_eq!(check[0].0, "logger");

        let fields = record_fields(&check[0].1).expect("logger type is a record");
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["trace", "debug", "info", "warn", "error", "fatal", "child"]
        );

        let info = fields
            .iter()
            .find(|field| field.name == "info")
            .expect("logger has an info method");
        let (params, result) = function_signature(&info.ty).expect("info is a function");
        assert_eq!(params.len(), 2);
        assert_eq!(
            function_required_arity(&info.ty),
            Some(1),
            "info takes one required message, fields optional"
        );
        assert_eq!(result, build::unit());
    }

    #[test]
    fn standard_check_globals_have_expected_shapes() {
        let globals = standard_check_globals();
        let names = globals
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "logger",
                "dbg",
                "write",
                "writeLine",
                "readLine",
                "readAll",
                "stdout",
                "stderr",
                "stdin",
                "stdio",
                "File",
                "Http",
                "Json"
            ]
        );

        let logger = global_type(&globals, "logger");
        let logger_fields = record_fields(logger).expect("logger is a record");
        let logger_field_names = logger_fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            logger_field_names,
            vec!["trace", "debug", "info", "warn", "error", "fatal", "child"]
        );

        let logger_info = record_field_type(logger, "info");
        let (info_params, info_result) =
            function_signature(&logger_info).expect("logger.info is a function");
        assert_eq!(function_required_arity(&logger_info), Some(1));
        assert_eq!(info_params.len(), 2);
        assert_eq!(info_params[0], build::text());
        assert!(record_fields(&info_params[1]).is_some());
        assert_eq!(info_result, build::unit());

        let dbg = global_type(&globals, "dbg");
        let (dbg_params, dbg_result) = function_signature(dbg).expect("dbg is a function");
        assert_eq!(function_required_arity(dbg), Some(1));
        assert_eq!(dbg_params, vec![build::var("a")]);
        assert_eq!(dbg_result, build::var("a"));

        let write = global_type(&globals, "write");
        let (write_params, write_result) = function_signature(write).expect("write is a function");
        assert_eq!(function_required_arity(write), Some(1));
        assert_eq!(write_params, vec![build::text()]);
        assert_eq!(write_result, build::empty_record());

        let write_line = global_type(&globals, "writeLine");
        let (write_line_params, write_line_result) =
            function_signature(write_line).expect("writeLine is a function");
        assert_eq!(function_required_arity(write_line), Some(1));
        assert_eq!(write_line_params, vec![build::text()]);
        assert_eq!(write_line_result, build::empty_record());

        let read_line = global_type(&globals, "readLine");
        let (read_line_params, read_line_result) =
            function_signature(read_line).expect("readLine is a function");
        assert_eq!(function_required_arity(read_line), Some(0));
        assert!(read_line_params.is_empty());
        assert_eq!(read_line_result, build::optional(build::text()));

        let read_all = global_type(&globals, "readAll");
        let (read_all_params, read_all_result) =
            function_signature(read_all).expect("readAll is a function");
        assert_eq!(function_required_arity(read_all), Some(0));
        assert!(read_all_params.is_empty());
        assert_eq!(read_all_result, build::text());

        let file = global_type(&globals, "File");
        let file_fields = record_fields(file).expect("File is a record");
        let file_field_names = file_fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(file_field_names, vec!["open"]);

        let open = record_field_type(file, "open");
        let (open_params, open_result) =
            function_signature(&open).expect("File.open is a function");
        assert_eq!(function_required_arity(&open), Some(2));
        assert_eq!(
            open_params,
            vec![build::text(), build::text_literals(&["r", "w", "a", "rw"])]
        );
        assert_eq!(open_result, Type::Deferred);

        let json = global_type(&globals, "Json");
        let json_fields = record_fields(json).expect("Json is a record");
        let json_field_names = json_fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(json_field_names, vec!["encode", "decode"]);

        let encode = record_field_type(json, "encode");
        let (encode_params, encode_result) =
            function_signature(&encode).expect("Json.encode is a function");
        assert_eq!(function_required_arity(&encode), Some(1));
        assert_eq!(encode_params, vec![build::var("a")]);
        assert_eq!(encode_result, build::text());

        let decode = record_field_type(json, "decode");
        let (decode_params, decode_result) =
            function_signature(&decode).expect("Json.decode is a function");
        assert_eq!(function_required_arity(&decode), Some(2));
        assert_eq!(decode_params, vec![build::text(), Type::Deferred]);
        assert_eq!(decode_result, Type::Deferred);
    }

    #[test]
    fn standard_check_globals_list_handle_record_shapes() {
        let globals = standard_check_globals();

        for (handle, expected) in [
            ("stdout", vec!["write", "writeLine", "flush"] as Vec<&str>),
            ("stderr", vec!["write", "writeLine", "flush"]),
            ("stdin", vec!["readLine", "readAll"]),
            (
                "stdio",
                vec!["write", "writeLine", "readLine", "readAll", "flush"],
            ),
        ] {
            let ty = global_type(&globals, handle);
            let fields = record_fields(ty).unwrap_or_else(|| panic!("{handle} is a record"));
            let names = fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names, expected, "{handle} method record");
        }

        // The methods return `Result`, not the bare `{}` the top-level `write`
        // returns — this is the boundary the handle tier introduces.
        let stdout = global_type(&globals, "stdout");
        let (write_params, write_result) =
            function_signature(&record_field_type(stdout, "write")).expect("write is a function");
        assert_eq!(write_params, vec![build::text()]);
        assert_eq!(
            write_result,
            build::result(build::empty_record(), write_error_type())
        );

        let stdin = global_type(&globals, "stdin");
        let (_, read_line_result) =
            function_signature(&record_field_type(stdin, "readLine")).expect("readLine is a fn");
        assert_eq!(
            read_line_result,
            build::result(build::optional(build::text()), read_error_type())
        );

        let (_, flush_result) =
            function_signature(&record_field_type(stdout, "flush")).expect("flush is a function");
        assert_eq!(
            flush_result,
            build::result(build::empty_record(), io_error_type())
        );
    }

    #[test]
    fn error_types_are_closed_variants_with_documented_tags() {
        assert_eq!(
            variant_tags(&write_error_type()),
            Some(vec![
                "BrokenPipe".to_owned(),
                "PermissionDenied".to_owned(),
                "Other".to_owned()
            ])
        );
        assert_eq!(
            variant_tags(&read_error_type()),
            Some(vec!["UnexpectedEof".to_owned(), "Other".to_owned()])
        );
        assert_eq!(
            variant_tags(&io_error_type()),
            Some(vec![
                "NotFound".to_owned(),
                "PermissionDenied".to_owned(),
                "AlreadyExists".to_owned(),
                "BrokenPipe".to_owned(),
                "Other".to_owned()
            ])
        );
        assert_eq!(
            variant_tags(&json_error_type()),
            Some(vec!["Parse".to_owned(), "Shape".to_owned()])
        );

        for ty in [write_error_type(), read_error_type(), io_error_type()] {
            let Type::Variant(row) = ty else {
                panic!("error type is a variant");
            };
            assert_eq!(
                row.tail,
                aven_check::RowTail::Closed,
                "error variant is closed"
            );
            for entry in &row.entries {
                let aven_check::RowEntry::Tag { payload, .. } = entry else {
                    panic!("error variant entry is a tag");
                };
                assert_eq!(
                    payload,
                    &vec![build::text()],
                    "each tag carries a Text message"
                );
            }
        }
    }

    #[test]
    fn bare_write_and_handle_write_lock_the_result_boundary() {
        // Bare `write` returns `{}`; `stdout.write` returns `Result` — the two
        // shapes pinned together so the boundary can't silently drift.
        let (_, bare_result) = function_signature(&io_write_type()).expect("write is a function");
        assert_eq!(bare_result, build::empty_record());

        let handle_write = record_field_type(&stdout_handle_type(), "write");
        let (_, handle_result) =
            function_signature(&handle_write).expect("stdout.write is a function");
        assert_eq!(
            handle_result,
            build::result(build::empty_record(), write_error_type())
        );
        assert_ne!(bare_result, handle_result);
    }

    #[test]
    fn open_write_record_param_accepts_write_handles_and_rejects_stdin() {
        use aven_parser::parse_module;

        // A function typed on an open `{ write | r }` record — the row-poly
        // surface that handle width subtyping is meant to serve.
        let globals = vec![
            (
                "needsWrite".to_owned(),
                build::function(
                    vec![build::open_record(vec![("write", handle_write_type())])],
                    build::empty_record(),
                ),
            ),
            ("stdout".to_owned(), stdout_handle_type()),
            ("stdio".to_owned(), stdio_handle_type()),
            ("stdin".to_owned(), stdin_handle_type()),
        ];

        for accepted in ["needsWrite(stdout)\n", "needsWrite(stdio)\n"] {
            let module = parse_module(accepted);
            assert!(module.diagnostics.is_empty(), "{accepted} parses");
            let checked = aven_check::check_module_with_globals(&module.module, &globals);
            assert!(
                checked.diagnostics.is_empty(),
                "{accepted} type-checks: {:?}",
                checked.diagnostics
            );
        }

        let module = parse_module("needsWrite(stdin)\n");
        assert!(module.diagnostics.is_empty(), "stdin program parses");
        let checked = aven_check::check_module_with_globals(&module.module, &globals);
        assert!(
            checked
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("type.missing-field")),
            "stdin lacks `write`: {:?}",
            checked.diagnostics
        );
    }

    fn global_type<'a>(globals: &'a [(String, Type)], name: &str) -> &'a Type {
        globals
            .iter()
            .find_map(|(global_name, ty)| (global_name == name).then_some(ty))
            .unwrap_or_else(|| panic!("expected global `{name}`"))
    }

    fn record_field_type(ty: &Type, name: &str) -> Type {
        record_fields(ty)
            .expect("expected a record type")
            .into_iter()
            .find_map(|field| (field.name == name).then_some(field.ty))
            .unwrap_or_else(|| panic!("expected record field `{name}`"))
    }
}
