//! Typed-fn adapter: derive an Aven [`Type`] and a marshalling
//! [`Value::native`] from a monomorphic Rust closure so the value and type
//! can't drift.
//!
//! [`AvenMarshal`] is the single source pairing a Rust type with its Aven type
//! and the value conversions in both directions. [`IntoHostFn`] lifts a
//! `Fn(A0, ..) -> R` (arities 0..=4, every type [`AvenMarshal`]) into the
//! `(Type, Value)` pair [`crate::Host::register_fn`] feeds to `register`.
//!
//! Deferred: generic host fns (e.g. `dbg : (a) -> a`, needing a `Value`
//! passthrough mapped to a type variable plus scheme support), compound
//! marshalling (records↔structs, `Vec`↔Array, `Option`↔`?T`, `Result`↔Aven
//! `Result`), optional params via the adapter, and arities above 4.

use aven_check::{Type, build};
use aven_eval::Value;

/// A Rust type that marshals to/from an Aven [`Value`] and knows its Aven
/// [`Type`]. Implemented for the primitive scalars and unit only;
/// [`from_value`](AvenMarshal::from_value) reports a clear shape mismatch on the
/// wrong runtime shape.
pub trait AvenMarshal: Sized {
    fn aven_type() -> Type;
    fn to_value(self) -> Value;
    fn from_value(value: &Value) -> Result<Self, String>;
}

/// Error for a `from_value` shape mismatch, e.g. "expected Int, got Text".
fn mismatch(expected: &str, got: &Value) -> String {
    let got = match got {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Text(_) => "Text",
        Value::Bool(_) => "Bool",
        Value::Tuple(values) if values.is_empty() => "Unit",
        Value::Array(_) => "Array",
        Value::Tuple(_) => "Tuple",
        Value::Set(_) => "Set",
        Value::Record(_) => "Record",
        Value::Tag { .. } => "Tag",
        Value::Closure(_) | Value::Native(_) => "Function",
        Value::Type(_) => "Type",
        Value::Undefined => "Undefined",
        Value::Null => "Null",
    };
    format!("expected {expected}, got {got}")
}

impl AvenMarshal for i64 {
    fn aven_type() -> Type {
        build::int()
    }

    fn to_value(self) -> Value {
        Value::Int(self)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Int(int) => Ok(*int),
            other => Err(mismatch("Int", other)),
        }
    }
}

impl AvenMarshal for f64 {
    fn aven_type() -> Type {
        build::float()
    }

    fn to_value(self) -> Value {
        Value::Float(self)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Float(float) => Ok(*float),
            other => Err(mismatch("Float", other)),
        }
    }
}

impl AvenMarshal for String {
    fn aven_type() -> Type {
        build::text()
    }

    fn to_value(self) -> Value {
        Value::Text(self)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Text(text) => Ok(text.clone()),
            other => Err(mismatch("Text", other)),
        }
    }
}

impl AvenMarshal for bool {
    fn aven_type() -> Type {
        build::bool()
    }

    fn to_value(self) -> Value {
        Value::Bool(self)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Bool(boolean) => Ok(*boolean),
            other => Err(mismatch("Bool", other)),
        }
    }
}

impl AvenMarshal for () {
    fn aven_type() -> Type {
        build::unit()
    }

    fn to_value(self) -> Value {
        Value::unit()
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        if value.is_unit() {
            Ok(())
        } else {
            Err(mismatch("Unit", value))
        }
    }
}

mod sealed {
    pub trait Sealed<Args> {}
}

/// A Rust closure that lifts into the `(Type, Value)` pair
/// [`crate::Host::register_fn`] registers. Sealed: implemented (via a macro)
/// only for `Fn(A0, ..) -> R + 'static` where every `Ai: AvenMarshal` and
/// `R: AvenMarshal`, arities 0..=4.
pub trait IntoHostFn<Args>: sealed::Sealed<Args> {
    /// Derive the function [`Type`] (all params required) and a
    /// [`Value::native`] that arity-checks, unmarshals each argument, calls the
    /// closure, and marshals the result.
    fn into_host_fn(self) -> (Type, Value);
}

/// Expand to the second token, discarding the first — lets the arity macro
/// build a `[(); N]`-shaped array to count params at compile time.
macro_rules! replace_expr {
    ($_t:tt $sub:expr) => {
        $sub
    };
}

macro_rules! impl_into_host_fn {
    ($($arg:ident),*) => {
        impl<F, R, $($arg),*> sealed::Sealed<($($arg,)*)> for F
        where
            F: Fn($($arg),*) -> R + 'static,
            $($arg: AvenMarshal,)*
            R: AvenMarshal,
        {}

        impl<F, R, $($arg),*> IntoHostFn<($($arg,)*)> for F
        where
            F: Fn($($arg),*) -> R + 'static,
            $($arg: AvenMarshal,)*
            R: AvenMarshal,
        {
            fn into_host_fn(self) -> (Type, Value) {
                let ty = build::function(vec![$($arg::aven_type()),*], R::aven_type());
                #[allow(unused_variables, unused_mut)]
                let native = Value::native(move |args| {
                    const ARITY: usize = <[()]>::len(&[$(replace_expr!($arg ())),*]);
                    if args.len() != ARITY {
                        return Err(format!(
                            "expected {ARITY} arguments, got {}",
                            args.len()
                        ));
                    }
                    let mut iter = args.iter();
                    let result = self(
                        $($arg::from_value(iter.next().expect("arity checked above"))?,)*
                    );
                    Ok(result.to_value())
                });
                (ty, native)
            }
        }
    };
}

impl_into_host_fn!();
impl_into_host_fn!(A0);
impl_into_host_fn!(A0, A1);
impl_into_host_fn!(A0, A1, A2);
impl_into_host_fn!(A0, A1, A2, A3);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        assert_eq!(i64::from_value(&42_i64.to_value()), Ok(42));
        assert_eq!(f64::from_value(&1.5_f64.to_value()), Ok(1.5));
        assert_eq!(
            String::from_value(&"hi".to_owned().to_value()),
            Ok("hi".to_owned())
        );
        assert_eq!(bool::from_value(&true.to_value()), Ok(true));
        assert_eq!(<()>::from_value(&().to_value()), Ok(()));
    }

    #[test]
    fn primitive_types_are_their_named_types() {
        assert_eq!(i64::aven_type(), build::int());
        assert_eq!(f64::aven_type(), build::float());
        assert_eq!(String::aven_type(), build::text());
        assert_eq!(bool::aven_type(), build::bool());
        assert_eq!(<()>::aven_type(), build::unit());
    }

    #[test]
    fn from_value_reports_shape_mismatch() {
        assert_eq!(
            i64::from_value(&Value::Text("x".to_owned())),
            Err("expected Int, got Text".to_owned())
        );
        assert_eq!(
            String::from_value(&Value::Int(1)),
            Err("expected Text, got Int".to_owned())
        );
        assert_eq!(
            bool::from_value(&Value::unit()),
            Err("expected Bool, got Unit".to_owned())
        );
        assert!(<()>::from_value(&Value::Int(1)).is_err());
    }

    fn call_native(value: &Value, args: &[Value]) -> Result<Value, String> {
        let Value::Native(native) = value else {
            panic!("expected a native value");
        };
        native(args)
    }

    #[test]
    fn binary_fn_derives_type_and_marshalling_native() {
        let (ty, value) = (|a: i64, b: i64| a + b).into_host_fn();

        assert_eq!(
            ty,
            build::function(vec![build::int(), build::int()], build::int())
        );

        assert_eq!(
            call_native(&value, &[Value::Int(2), Value::Int(3)]),
            Ok(Value::Int(5))
        );
        assert_eq!(
            call_native(&value, &[Value::Text("x".to_owned()), Value::Int(3)]),
            Err("expected Int, got Text".to_owned())
        );
        assert_eq!(
            call_native(&value, &[Value::Int(1)]),
            Err("expected 2 arguments, got 1".to_owned())
        );
    }

    #[test]
    fn nullary_fn_derives_type_and_native() {
        let (ty, value) = (|| 42_i64).into_host_fn();

        assert_eq!(ty, build::function(vec![], build::int()));
        assert_eq!(call_native(&value, &[]), Ok(Value::Int(42)));
        assert_eq!(
            call_native(&value, &[Value::Int(0)]),
            Err("expected 0 arguments, got 1".to_owned())
        );
    }
}
