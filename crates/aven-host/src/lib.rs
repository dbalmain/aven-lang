//! Host registry binding runtime values to their Aven types.
//!
//! A [`Host`] is the single place where a Rust-implemented library or platform
//! declares a name once and feeds both halves of the toolchain: the runtime
//! [`aven_eval::Value`] flows to the evaluator and the [`aven_check::Type`] flows
//! to the checker, so the two can never drift. Libraries and platforms use the
//! same [`Host::register`] entry point; required capabilities (e.g. logging) are
//! Rust traits the platform implements, while the statically-known value+type is
//! registered through helpers like [`Host::register_logger`].

mod marshal;

use std::rc::Rc;

use aven_check::Type;

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

/// Registry of host/library globals seeded into the evaluator and the checker.
#[derive(Default)]
pub struct Host {
    typed: Vec<TypedEntry>,
    runtime_only: Vec<RuntimeOnlyEntry>,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a name with its runtime value AND its Aven type (the normal path
    /// for both libraries and platforms).
    pub fn register(&mut self, name: impl Into<String>, value: Value, ty: Type) {
        self.typed.push(TypedEntry {
            name: name.into(),
            value,
            ty,
        });
    }

    /// Escape hatch for a value whose type isn't expressible yet (generics need
    /// scheme support — see the P2 typed-fn adapter). Runs but is NOT type-checked.
    pub fn register_runtime_only(&mut self, name: impl Into<String>, value: Value) {
        self.runtime_only.push(RuntimeOnlyEntry {
            name: name.into(),
            value,
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
            .collect()
    }
}

/// The Aven type of the standard logger value.
///
/// Approximate: the `child` method returns an open record rather than a named
/// recursive `Logger`. A precise recursive type is deferred until a named
/// `Logger` type / typed-fn adapter exists. Single source of truth so the CLI's
/// `Platform.Log` field shares it without reconstructing the type.
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

#[cfg(test)]
mod tests {
    use super::*;

    use aven_check::{function_required_arity, function_signature, record_fields};

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
        host.register_runtime_only("debug", Value::Int(7));

        assert_eq!(
            host.eval_globals(),
            vec![("debug".to_owned(), Value::Int(7))]
        );
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
}
