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
mod temporal;
mod text_format;
mod toml_format;
mod yaml;

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use aven_check::{HostComptimeFn, HostComptimeFnSpec, HostComptimeParam, HostGlobals, Type};

pub use aven_parser::{OperatorAssociativity, OperatorPrecedence};
pub use marshal::{AvenMarshal, IntoHostFn};
/// The Aven type of the platform `now` value: `() -> Instant`.
pub use temporal::now_type;
/// The Aven type of the platform `zone` value: `(Text) -> Result(Zone, Text)`.
pub use temporal::zone_type;

/// Re-exported Aven type builders so hosts spell types without depending on
/// `aven-check` directly (the registration/typing vocabulary lives here).
pub use aven_check::build;
use aven_eval::Value;
use aven_eval::logging::{LogSink, TraceContext, logger};
use aven_parser::{is_custom_operator_token, is_reserved_or_fixed_operator};

/// Why a platform operator-fixity registration was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorRegistrationError {
    InvalidToken { token: String },
    ReservedToken { token: String },
    Duplicate { token: String },
}

impl fmt::Display for OperatorRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken { token } => {
                write!(formatter, "invalid custom operator token `{token}`")
            }
            Self::ReservedToken { token } => {
                write!(
                    formatter,
                    "cannot register fixity for reserved operator `{token}`"
                )
            }
            Self::Duplicate { token } => {
                write!(
                    formatter,
                    "operator `{token}` is already registered by this host"
                )
            }
        }
    }
}

impl std::error::Error for OperatorRegistrationError {}

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

/// A static carried by a named type: bound both as a runtime native (under the
/// `"Type.name"` eval key the evaluator resolves on `Value::Type` field access)
/// and as a checker type (looked up through [`HostGlobals::statics`]).
struct StaticMember {
    name: String,
    value: Value,
    ty: Type,
}

/// A named type together with the statics it carries.
struct StaticsEntry {
    type_name: String,
    members: Vec<StaticMember>,
}

/// A name registered with a runtime value, a base checker type, and a
/// host-side comptime resolver that can refine call result types.
struct ComptimeEntry {
    name: String,
    value: Value,
    ty: Type,
    resolver: Rc<dyn HostComptimeFn>,
    comptime_params: Vec<HostComptimeParam>,
}

/// A host-side comptime resolver keyed independently from runtime value/type
/// registration, for resolvers that live under fields of typed host records.
struct ComptimeResolverEntry {
    key: String,
    resolver: Rc<dyn HostComptimeFn>,
    comptime_params: Vec<HostComptimeParam>,
}

/// A custom operator fixity registered by the embedding platform.
struct OperatorFixityRegistration {
    token: String,
    precedence: OperatorPrecedence,
    associativity: OperatorAssociativity,
}

/// Registry of host/library globals seeded into the evaluator and the checker.
#[derive(Default)]
pub struct Host {
    typed: Vec<TypedEntry>,
    runtime_only: Vec<RuntimeOnlyEntry>,
    type_definitions: Vec<TypeDefinitionEntry>,
    statics: Vec<StaticsEntry>,
    comptime: Vec<ComptimeEntry>,
    comptime_resolvers: Vec<ComptimeResolverEntry>,
    operator_fixities: Vec<OperatorFixityRegistration>,
    clock_registered: bool,
    zones_registered: bool,
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

    /// Register custom infix fixity supplied by this platform. This affects
    /// parsing only; it does not add an Aven value or checker declaration.
    pub fn register_operator(
        &mut self,
        token: impl Into<String>,
        precedence: OperatorPrecedence,
        associativity: OperatorAssociativity,
    ) -> Result<(), OperatorRegistrationError> {
        let token = token.into();
        if !is_custom_operator_token(&token) {
            return Err(if is_reserved_or_fixed_operator(&token) {
                OperatorRegistrationError::ReservedToken { token }
            } else {
                OperatorRegistrationError::InvalidToken { token }
            });
        }
        if self
            .operator_fixities
            .iter()
            .any(|registration| registration.token == token)
        {
            return Err(OperatorRegistrationError::Duplicate { token });
        }

        self.operator_fixities.push(OperatorFixityRegistration {
            token,
            precedence,
            associativity,
        });
        Ok(())
    }

    /// Platform fixity declarations in stable registration order, suitable for
    /// passing to `ProjectConfig::operator_fixity_table`.
    pub fn operator_fixities(&self) -> Vec<(String, OperatorPrecedence, OperatorAssociativity)> {
        self.operator_fixities
            .iter()
            .map(|registration| {
                (
                    registration.token.clone(),
                    registration.precedence,
                    registration.associativity,
                )
            })
            .collect()
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

    /// Register a named type together with the statics it carries. The type is a
    /// checker-visible definition (like [`Host::register_type_definition`]); each
    /// static binds a runtime native the evaluator resolves on `Type.name` field
    /// access and a checker type the inference path resolves the same way.
    pub fn register_type_with_statics(
        &mut self,
        name: impl Into<String>,
        ty: Type,
        statics: Vec<(String, Type, Value)>,
    ) {
        let type_name = name.into();
        self.register_type_definition(type_name.clone(), ty);
        self.statics.push(StaticsEntry {
            type_name,
            members: statics
                .into_iter()
                .map(|(name, ty, value)| StaticMember { name, value, ty })
                .collect(),
        });
    }

    fn register_type_statics(
        &mut self,
        name: impl Into<String>,
        statics: Vec<(String, Type, Value)>,
    ) {
        let type_name = name.into();
        self.statics.push(StaticsEntry {
            type_name,
            members: statics
                .into_iter()
                .map(|(name, ty, value)| StaticMember { name, value, ty })
                .collect(),
        });
    }

    fn register_data_type(&mut self) {
        if self
            .type_definitions
            .iter()
            .any(|entry| entry.name == "Data")
        {
            return;
        }

        self.register_type_definition("Data", data_type());
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
            comptime_params: comptime_params
                .into_iter()
                .map(HostComptimeParam::Value)
                .collect(),
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
            comptime_params: comptime_params
                .into_iter()
                .map(HostComptimeParam::Value)
                .collect(),
        });
    }

    /// Register a comptime resolver whose arguments are the inferred static
    /// types of runtime call arguments.
    pub fn register_comptime_type_resolver(
        &mut self,
        key: impl Into<String>,
        comptime_params: Vec<usize>,
        resolver: Rc<dyn HostComptimeFn>,
    ) {
        self.comptime_resolvers.push(ComptimeResolverEntry {
            key: key.into(),
            resolver,
            comptime_params: comptime_params
                .into_iter()
                .map(HostComptimeParam::TypeOf)
                .collect(),
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
            .chain(self.statics.iter().flat_map(|entry| {
                // The type name binds a type value; each static binds under the
                // `"Type.name"` key the evaluator resolves on field access.
                std::iter::once((entry.type_name.clone(), Value::named_type(&entry.type_name)))
                    .chain(entry.members.iter().map(|member| {
                        (
                            format!("{}.{}", entry.type_name, member.name),
                            member.value.clone(),
                        )
                    }))
            }))
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
                        HostComptimeFnSpec::with_params(
                            Rc::clone(&entry.resolver),
                            entry.comptime_params.clone(),
                        ),
                    )
                })
                .chain(self.comptime_resolvers.iter().map(|entry| {
                    (
                        entry.key.clone(),
                        HostComptimeFnSpec::with_params(
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
        .with_statics(
            self.statics
                .iter()
                .map(|entry| {
                    (
                        entry.type_name.clone(),
                        entry
                            .members
                            .iter()
                            .map(|member| (member.name.clone(), member.ty.clone()))
                            .collect(),
                    )
                })
                .collect(),
        )
    }

    /// Embedded standard-library modules available from this host's registered
    /// capabilities. The pure modules are always present.
    pub fn std_library(&self) -> HashMap<String, &'static str> {
        let mut library = std_library();
        if self.clock_registered {
            library.insert("std/clock".to_owned(), include_str!("../std/clock.av"));
        }
        if self.zones_registered {
            library.insert("std/zones".to_owned(), include_str!("../std/zones.av"));
        }
        library
    }

    /// Names embedded capability modules may pun but user modules receive only
    /// through their public module import.
    pub fn library_only_global_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.clock_registered {
            names.push("now".to_owned());
        }
        if self.zones_registered {
            names.push("zone".to_owned());
        }
        names
    }

    /// Capability modules known to the standard library but unavailable from
    /// this host, for actionable import diagnostics.
    pub fn disabled_capability_modules(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let mut modules = Vec::new();
        if !self.clock_registered {
            modules.push(("std/clock", "clock", "register_clock"));
        }
        if !self.zones_registered {
            modules.push(("std/zones", "zones", "register_zones"));
        }
        modules
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

/// The closed `HttpError` variant: transport failures from `Http` calls, each
/// tag carrying a `Text` message. HTTP status codes are response data, not
/// errors.
pub fn http_error_type() -> Type {
    build::variant(vec![
        ("Timeout", vec![build::text()]),
        ("ConnectionFailed", vec![build::text()]),
        ("InvalidUrl", vec![build::text()]),
        ("Other", vec![build::text()]),
    ])
}

/// The `Http` response record.
pub fn http_response_type() -> Type {
    build::record(vec![
        ("status", build::int()),
        (
            "headers",
            build::map(build::text(), build::array(build::text())),
        ),
        (
            "first",
            build::function(vec![build::text()], build::optional(build::text())),
        ),
        ("body", stdin_handle_type()),
    ])
}

/// `(Text, ?{..}) -> Result(Response, HttpError)`.
pub fn http_method_type() -> Type {
    build::function_opt(
        vec![build::text()],
        vec![build::open_record(vec![])],
        build::result(http_response_type(), http_error_type()),
    )
}

/// The `Http` platform namespace record.
pub fn http_type() -> Type {
    build::record(vec![
        ("get", http_method_type()),
        ("post", http_method_type()),
        ("put", http_method_type()),
        ("delete", http_method_type()),
        ("patch", http_method_type()),
    ])
}

fn format_error_type() -> Type {
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

fn format_encode_error_type() -> Type {
    build::variant(vec![(
        "Encode",
        vec![build::record(vec![("message", build::text())])],
    )])
}

/// The closed `JsonError` variant returned by `Json.decode`.
pub fn json_error_type() -> Type {
    format_error_type()
}

/// The closed `JsonEncodeError` variant returned by `Json.encode`.
pub fn json_encode_error_type() -> Type {
    format_encode_error_type()
}

/// The closed `YamlError` variant returned by `Yaml.decode`.
pub fn yaml_error_type() -> Type {
    format_error_type()
}

/// The closed `YamlEncodeError` variant returned by `Yaml.encode`.
pub fn yaml_encode_error_type() -> Type {
    format_encode_error_type()
}

/// The closed `TomlError` variant returned by `Toml.decode`.
pub fn toml_error_type() -> Type {
    format_error_type()
}

/// The closed `TomlEncodeError` variant returned by `Toml.encode`.
pub fn toml_encode_error_type() -> Type {
    format_encode_error_type()
}

/// The recursive dynamic data value shape returned by one-argument format
/// decodes.
pub fn data_type() -> Type {
    build::variant(vec![
        ("Null", vec![]),
        ("Bool", vec![build::bool()]),
        ("Int", vec![build::int()]),
        ("Float", vec![build::float()]),
        ("Text", vec![build::text()]),
        ("Array", vec![build::array(build::named("Data"))]),
        (
            "Object",
            vec![build::map(build::text(), build::named("Data"))],
        ),
    ])
}

fn format_decode_base_type() -> Type {
    build::function_opt(vec![build::text()], vec![Type::Deferred], Type::Deferred)
}

/// `(a) -> Result(Text, JsonEncodeError)` — `Json.encode` accepts any checked
/// value and validates JSON encodability at runtime.
pub fn json_encode_type() -> Type {
    build::function(
        vec![build::var("a")],
        build::result(build::text(), build::named("JsonEncodeError")),
    )
}

/// `(a) -> Text` — `Json.encodeText` is additionally gated by a host comptime
/// resolver which proves `a` cannot contain a `Float`.
pub fn json_encode_text_type() -> Type {
    build::function(vec![build::var("a")], build::text())
}

/// The base `Json.decode` type: `(Text, ? = _) -> ?`. The checker uses it for
/// arity and the input text argument; the host comptime resolver refines the
/// result from the optional trailing type argument, defaulting to `Data`.
pub fn json_decode_base_type() -> Type {
    format_decode_base_type()
}

/// `(a) -> Result(Text, YamlEncodeError)` — `Yaml.encode` mirrors
/// `Json.encode`'s type shape.
pub fn yaml_encode_type() -> Type {
    build::function(
        vec![build::var("a")],
        build::result(build::text(), build::named("YamlEncodeError")),
    )
}

/// `(a) -> Text`; the comptime resolver rejects types that may contain Float.
pub fn yaml_encode_text_type() -> Type {
    build::function(vec![build::var("a")], build::text())
}

/// The base `Yaml.decode` type: `(Text, ? = _) -> ?`.
pub fn yaml_decode_base_type() -> Type {
    format_decode_base_type()
}

/// `(a) -> Result(Text, TomlEncodeError)` — `Toml.encode` mirrors
/// `Json.encode`'s type shape.
pub fn toml_encode_type() -> Type {
    build::function(
        vec![build::var("a")],
        build::result(build::text(), build::named("TomlEncodeError")),
    )
}

/// `(a) -> Text`; the comptime resolver rejects types that may contain Float.
pub fn toml_encode_text_type() -> Type {
    build::function(vec![build::var("a")], build::text())
}

/// The base `Toml.decode` type: `(Text, ? = _) -> ?`.
pub fn toml_decode_base_type() -> Type {
    format_decode_base_type()
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

/// `(Text) -> Result({}, WriteError)` — a handle `write`/`writeLine` method.
fn handle_write_type() -> Type {
    build::function(
        vec![build::text()],
        build::result(build::empty_record(), write_error_type()),
    )
}

/// `() -> Result(?Text, ReadError)` — a handle `readLine` method (EOF is
/// `@Ok(undefined)`).
fn handle_read_line_type() -> Type {
    build::function(
        vec![],
        build::result(build::optional(build::text()), read_error_type()),
    )
}

/// `() -> Result(Text, ReadError)` — a handle `readAll` method.
fn handle_read_all_type() -> Type {
    build::function(vec![], build::result(build::text(), read_error_type()))
}

/// `() -> Result({}, IoError)` — a handle `flush` method.
fn handle_flush_type() -> Type {
    build::function(
        vec![],
        build::result(build::empty_record(), io_error_type()),
    )
}

/// The `stdout` handle type: a closed record of write-side methods. `stderr`
/// shares this shape. Callers annotate parameters as open records (e.g.
/// `{ write : (Text) -> Result({}, WriteError) | r }`), so width subtyping lets
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

/// The library name the embedded standard library registers under.
pub const STD_LIBRARY_NAME: &str = "std";
pub const STD_AMBIENT_METHOD_MODULES: &[&str] = &["std/array"];

/// Embedded standard-library sources, keyed by module specifier. std is
/// written in Aven and only puns host-registered natives, so registering this
/// map as the `std` library on `ModuleRoots` needs no filesystem at runtime.
/// The CLI wires it in; embedded hosts opt in explicitly.
pub fn std_library() -> HashMap<String, &'static str> {
    HashMap::from([
        ("std".to_owned(), include_str!("../std/std.av")),
        ("std/array".to_owned(), include_str!("../std/array.av")),
        ("std/map".to_owned(), include_str!("../std/map.av")),
        ("std/time".to_owned(), include_str!("../std/time.av")),
        ("std/result".to_owned(), include_str!("../std/result.av")),
    ])
}

/// Embedded standard-library modules for the standard CLI and LSP host
/// surface, including the clock and named-zone capabilities they provide.
pub fn standard_std_library() -> HashMap<String, &'static str> {
    let mut library = std_library();
    library.insert("std/clock".to_owned(), include_str!("../std/clock.av"));
    library.insert("std/zones".to_owned(), include_str!("../std/zones.av"));
    library
}

/// Capability-internal globals for the standard CLI and LSP host surface.
pub fn standard_library_only_global_names() -> Vec<String> {
    vec!["now".to_owned(), "zone".to_owned()]
}

/// Public type globals for the standard host prelude. Capability internals are
/// available only while checking their embedded re-export modules.
pub fn standard_check_globals() -> Vec<(String, Type)> {
    standard_public_check_host_globals().types
}

/// Full standard check host globals with capability-internal names (`now`,
/// `zone`) stripped. Use for user-facing single-file check/LSP surfaces; the
/// module graph keeps the full set and filters per-node via
/// [`standard_library_only_global_names`].
pub fn standard_public_check_host_globals() -> HostGlobals {
    let mut globals = standard_check_host_globals();
    let library_only = standard_library_only_global_names();
    globals
        .types
        .retain(|(name, _)| !library_only.iter().any(|only| only == name));
    globals
}

/// Type globals plus host comptime resolvers for the standard host prelude.
///
/// Includes capability-internal value types (`now`, `zone`) so embedded
/// re-export modules can pun them. User modules must not see those names:
/// strip via [`standard_public_check_host_globals`] or
/// `ModuleRoots::library_only_global_names`.
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
        ("now".to_owned(), now_type()),
        ("zone".to_owned(), zone_type()),
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
            (
                "Json.encodeText".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    text_format::encode_text_comptime_resolver("Json"),
                    vec![0],
                ),
            ),
            (
                "Yaml.decode".to_owned(),
                HostComptimeFnSpec::new(yaml::decode_comptime_resolver(), vec![1]),
            ),
            (
                "Yaml.encodeText".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    text_format::encode_text_comptime_resolver("Yaml"),
                    vec![0],
                ),
            ),
            (
                "Toml.decode".to_owned(),
                HostComptimeFnSpec::new(toml_format::decode_comptime_resolver(), vec![1]),
            ),
            (
                "Toml.encodeText".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    text_format::encode_text_comptime_resolver("Toml"),
                    vec![0],
                ),
            ),
            (
                "Http.get".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    http::comptime_resolver(http::HttpMethod::Get),
                    vec![1],
                ),
            ),
            (
                "Http.post".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    http::comptime_resolver(http::HttpMethod::Post),
                    vec![1],
                ),
            ),
            (
                "Http.put".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    http::comptime_resolver(http::HttpMethod::Put),
                    vec![1],
                ),
            ),
            (
                "Http.delete".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    http::comptime_resolver(http::HttpMethod::Delete),
                    vec![1],
                ),
            ),
            (
                "Http.patch".to_owned(),
                HostComptimeFnSpec::new_type_of(
                    http::comptime_resolver(http::HttpMethod::Patch),
                    vec![1],
                ),
            ),
        ],
    )
    .with_type_definitions(
        std::iter::once(("Data".to_owned(), data_type()))
            .chain([
                ("JsonError".to_owned(), json_error_type()),
                ("JsonEncodeError".to_owned(), json_encode_error_type()),
                ("YamlError".to_owned(), yaml_error_type()),
                ("YamlEncodeError".to_owned(), yaml_encode_error_type()),
                ("TomlError".to_owned(), toml_error_type()),
                ("TomlEncodeError".to_owned(), toml_encode_error_type()),
            ])
            .chain(temporal::temporal_type_definitions())
            .collect(),
    )
    .with_statics(
        std::iter::once(("Json".to_owned(), json_statics()))
            .chain([
                ("Yaml".to_owned(), yaml_statics()),
                ("Toml".to_owned(), toml_statics()),
            ])
            .chain(temporal::temporal_statics_table())
            .collect(),
    )
}

/// The statics the `Json` type carries: `encode`/`encodeText`/`decode`. Shared by the
/// hand-built [`standard_check_host_globals`] and [`Host::register_json`] so the
/// two registration paths can't drift.
pub(crate) fn json_statics() -> Vec<(String, Type)> {
    vec![
        ("encode".to_owned(), json_encode_type()),
        ("encodeText".to_owned(), json_encode_text_type()),
        ("decode".to_owned(), json_decode_base_type()),
    ]
}

pub(crate) fn yaml_statics() -> Vec<(String, Type)> {
    vec![
        ("encode".to_owned(), yaml_encode_type()),
        ("encodeText".to_owned(), yaml_encode_text_type()),
        ("decode".to_owned(), yaml_decode_base_type()),
    ]
}

pub(crate) fn toml_statics() -> Vec<(String, Type)> {
    vec![
        ("encode".to_owned(), toml_encode_type()),
        ("encodeText".to_owned(), toml_encode_text_type()),
        ("decode".to_owned(), toml_decode_base_type()),
    ]
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
    fn std_library_modules_are_keyed_under_std_and_parse_cleanly() {
        let library = std_library();

        let mut specifiers: Vec<_> = library.keys().cloned().collect();
        specifiers.sort();
        assert_eq!(
            specifiers,
            ["std", "std/array", "std/map", "std/result", "std/time"]
        );

        for (specifier, source) in &library {
            assert!(
                specifier == STD_LIBRARY_NAME
                    || specifier.starts_with(&format!("{STD_LIBRARY_NAME}/")),
                "module `{specifier}` is not under the std library name"
            );
            let file =
                aven_core::SourceFile::new(aven_core::FileId(0), specifier.clone(), None, *source);
            let parsed = aven_parser::parse_source(&file);
            assert!(
                parsed.diagnostics.is_empty(),
                "std module `{specifier}` has parse diagnostics: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn std_library_capability_modules_follow_host_registration() {
        let pure = std_library();
        assert!(!pure.contains_key("std/clock"));
        assert!(!pure.contains_key("std/zones"));

        let mut clock_host = Host::new();
        clock_host.register_clock();
        let clock = clock_host.std_library();
        assert!(clock.contains_key("std/clock"));
        assert!(!clock.contains_key("std/zones"));
        assert_eq!(clock_host.library_only_global_names(), ["now"]);

        let mut zones_host = Host::new();
        zones_host.register_zones_with_dirs(vec![]);
        let zones = zones_host.std_library();
        assert!(!zones.contains_key("std/clock"));
        assert!(zones.contains_key("std/zones"));
        assert_eq!(zones_host.library_only_global_names(), ["zone"]);
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
    fn register_operator_preserves_fixity_registration_order() {
        let mut host = Host::new();
        host.register_operator(
            "**",
            OperatorPrecedence::Exponentiation,
            OperatorAssociativity::Right,
        )
        .expect("custom operator should register");
        host.register_operator(
            "$$",
            OperatorPrecedence::Multiplicative,
            OperatorAssociativity::Left,
        )
        .expect("second custom operator should register");

        assert_eq!(
            host.operator_fixities(),
            vec![
                (
                    "**".to_owned(),
                    OperatorPrecedence::Exponentiation,
                    OperatorAssociativity::Right,
                ),
                (
                    "$$".to_owned(),
                    OperatorPrecedence::Multiplicative,
                    OperatorAssociativity::Left,
                ),
            ]
        );
    }

    #[test]
    fn register_operator_rejects_invalid_reserved_and_duplicate_tokens() {
        let mut host = Host::new();

        assert_eq!(
            host.register_operator(
                "word",
                OperatorPrecedence::Additive,
                OperatorAssociativity::Left,
            ),
            Err(OperatorRegistrationError::InvalidToken {
                token: "word".to_owned(),
            })
        );
        for token in ["+", "==", "|>"] {
            assert_eq!(
                host.register_operator(
                    token,
                    OperatorPrecedence::Additive,
                    OperatorAssociativity::Left,
                ),
                Err(OperatorRegistrationError::ReservedToken {
                    token: token.to_owned(),
                })
            );
        }

        host.register_operator(
            "**",
            OperatorPrecedence::Exponentiation,
            OperatorAssociativity::Right,
        )
        .expect("custom operator should register");
        assert_eq!(
            host.register_operator(
                "**",
                OperatorPrecedence::Exponentiation,
                OperatorAssociativity::Right,
            ),
            Err(OperatorRegistrationError::Duplicate {
                token: "**".to_owned(),
            })
        );
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

        let http = global_type(&globals, "Http");
        let http_fields = record_fields(http).expect("Http is a record");
        let http_field_names = http_fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            http_field_names,
            vec!["get", "post", "put", "delete", "patch"]
        );
        for method in http_field_names {
            let method_type = record_field_type(http, method);
            let (params, result) =
                function_signature(&method_type).expect("Http method is a function");
            assert_eq!(function_required_arity(&method_type), Some(1));
            assert_eq!(params, vec![build::text(), build::open_record(vec![])]);
            assert_eq!(
                result,
                build::result(http_response_type(), http_error_type())
            );
        }

        // Format names are type artifacts carrying statics, not namespace
        // records: they are absent from `types` and their codec statics
        // members live in the statics table.
        let host_globals = standard_check_host_globals();
        let data = host_globals
            .type_definitions
            .iter()
            .find_map(|(name, ty)| (name == "Data").then_some(ty))
            .expect("Data type definition is registered");
        assert_eq!(
            variant_tags(data),
            Some(vec![
                "Null".to_owned(),
                "Bool".to_owned(),
                "Int".to_owned(),
                "Float".to_owned(),
                "Text".to_owned(),
                "Array".to_owned(),
                "Object".to_owned(),
            ])
        );

        for (format, error) in [
            ("Json", "JsonEncodeError"),
            ("Yaml", "YamlEncodeError"),
            ("Toml", "TomlEncodeError"),
        ] {
            assert!(global_type_opt(&globals, format).is_none());

            let statics = aven_check::type_statics(&host_globals, format)
                .unwrap_or_else(|| panic!("{format} carries statics"));
            let static_names = statics
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(static_names, vec!["encode", "encodeText", "decode"]);

            let encode = static_type(&statics, "encode");
            let (encode_params, encode_result) = function_signature(&encode)
                .unwrap_or_else(|| panic!("{format}.encode is a function"));
            assert_eq!(function_required_arity(&encode), Some(1));
            assert_eq!(encode_params, vec![build::var("a")]);
            assert_eq!(
                encode_result,
                build::result(build::text(), build::named(error))
            );

            let encode_text = static_type(&statics, "encodeText");
            let (encode_text_params, encode_text_result) = function_signature(&encode_text)
                .unwrap_or_else(|| panic!("{format}.encodeText is a function"));
            assert_eq!(function_required_arity(&encode_text), Some(1));
            assert_eq!(encode_text_params, vec![build::var("a")]);
            assert_eq!(encode_text_result, build::text());

            let decode = static_type(&statics, "decode");
            let (decode_params, decode_result) = function_signature(&decode)
                .unwrap_or_else(|| panic!("{format}.decode is a function"));
            assert_eq!(function_required_arity(&decode), Some(1));
            assert_eq!(decode_params, vec![build::text(), Type::Deferred]);
            assert_eq!(decode_result, Type::Deferred);
        }
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
        for error_type in [json_error_type(), yaml_error_type(), toml_error_type()] {
            assert_eq!(
                variant_tags(&error_type),
                Some(vec!["Parse".to_owned(), "Shape".to_owned()])
            );
        }
        for error_type in [
            json_encode_error_type(),
            yaml_encode_error_type(),
            toml_encode_error_type(),
        ] {
            assert_eq!(variant_tags(&error_type), Some(vec!["Encode".to_owned()]));
        }
        assert_eq!(
            variant_tags(&data_type()),
            Some(vec![
                "Null".to_owned(),
                "Bool".to_owned(),
                "Int".to_owned(),
                "Float".to_owned(),
                "Text".to_owned(),
                "Array".to_owned(),
                "Object".to_owned()
            ])
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
        global_type_opt(globals, name).unwrap_or_else(|| panic!("expected global `{name}`"))
    }

    fn global_type_opt<'a>(globals: &'a [(String, Type)], name: &str) -> Option<&'a Type> {
        globals
            .iter()
            .find_map(|(global_name, ty)| (global_name == name).then_some(ty))
    }

    fn static_type(statics: &[aven_check::RecordField], name: &str) -> Type {
        statics
            .iter()
            .find_map(|field| (field.name == name).then_some(field.ty.clone()))
            .unwrap_or_else(|| panic!("expected static `{name}`"))
    }

    fn record_field_type(ty: &Type, name: &str) -> Type {
        record_fields(ty)
            .expect("expected a record type")
            .into_iter()
            .find_map(|field| (field.name == name).then_some(field.ty))
            .unwrap_or_else(|| panic!("expected record field `{name}`"))
    }
}
