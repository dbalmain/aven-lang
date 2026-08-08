//! Typed-fn adapter: derive an Aven [`Type`] and a marshalling
//! [`Value::native`] from a monomorphic Rust closure so the value and type
//! can't drift.
//!
//! [`AvenMarshal`] is the single source pairing a Rust type with its Aven type
//! and the value conversions in both directions. [`IntoHostFn`] lifts a
//! `Fn(A0, ..) -> R` (arities 0..=4, every argument [`AvenMarshal`]) into the
//! `(Type, Value)` pair [`crate::Host::register_fn`] feeds to `register`.
//! Ordinary returns marshal as Aven values; [`HostResult`] preserves a native
//! runtime failure without changing the function's Aven result type.
//!
//! Deferred: generic host fns (e.g. `dbg : (a) -> a`, needing a `Value`
//! passthrough mapped to a type variable plus scheme support), a derive/helper
//! for records↔structs, optional params via the adapter, and arities above 4.

use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use aven_check::{Type, build};
use aven_eval::{Int, Value};

/// A Rust type that marshals to/from an Aven [`Value`] and knows its Aven
/// [`Type`]. Implemented for the primitive scalars, arbitrary-precision
/// [`Int`], unit, and the standard compound containers;
/// [`from_value`](AvenMarshal::from_value) reports a clear shape mismatch on the
/// wrong runtime shape.
pub trait AvenMarshal: Sized {
    fn aven_type() -> Type;
    fn to_value(self) -> Value;
    fn from_value(value: &Value) -> Result<Self, String>;
}

/// A typed host function result which may abort evaluation with a platform
/// error. Unlike Rust's ordinary [`Result`], which marshals to Aven's
/// `Result(ok, err)` value, this wrapper keeps `T` as the Aven return type and
/// propagates `Err(String)` through the evaluator's native-call boundary.
#[derive(Debug)]
pub struct HostResult<T>(Result<T, String>);

impl<T> From<Result<T, String>> for HostResult<T> {
    fn from(result: Result<T, String>) -> Self {
        Self(result)
    }
}

/// Return-side conversion used by [`IntoHostFn`].
pub trait HostFnReturn: Sized {
    fn return_type() -> Type;
    fn into_host_value(self) -> Result<Value, String>;
}

impl<T: AvenMarshal> HostFnReturn for T {
    fn return_type() -> Type {
        T::aven_type()
    }

    fn into_host_value(self) -> Result<Value, String> {
        Ok(self.to_value())
    }
}

impl<T: AvenMarshal> HostFnReturn for HostResult<T> {
    fn return_type() -> Type {
        T::aven_type()
    }

    fn into_host_value(self) -> Result<Value, String> {
        self.0.map(AvenMarshal::to_value)
    }
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
        Value::Stream(_) => "Stream",
        Value::Map(_) => "Map",
        Value::Record(_) | Value::SlotRecord { .. } | Value::NamedRecord { .. } => "Record",
        Value::BrandedPrimitive { payload, .. } => payload.type_name(),
        Value::Tag { .. } => "Tag",
        Value::ResultMethod { .. }
        | Value::NamedMethod { .. }
        | Value::UnboundNamedMethod { .. }
        | Value::Closure(_)
        | Value::Native(_)
        | Value::RangeConstructor { .. } => "Function",
        Value::Type(_) | Value::NamedFamily(_) => "Type",
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
        Value::int(self)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Int(int) => int
                .to_i64()
                .ok_or_else(|| format!("Int value {int} does not fit Rust i64")),
            other => Err(mismatch("Int", other)),
        }
    }
}

impl AvenMarshal for Int {
    fn aven_type() -> Type {
        build::int()
    }

    fn to_value(self) -> Value {
        Value::Int(self)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Int(int) => Ok(int.clone()),
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

impl<T: AvenMarshal> AvenMarshal for Vec<T> {
    fn aven_type() -> Type {
        build::array(T::aven_type())
    }

    fn to_value(self) -> Value {
        Value::Array(Rc::new(
            self.into_iter().map(AvenMarshal::to_value).collect(),
        ))
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Array(values) = value else {
            return Err(mismatch("Array", value));
        };
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                T::from_value(value).map_err(|error| format!("at Array[{index}]: {error}"))
            })
            .collect()
    }
}

impl<T: AvenMarshal> AvenMarshal for Option<T> {
    fn aven_type() -> Type {
        build::optional(T::aven_type())
    }

    fn to_value(self) -> Value {
        self.map_or(Value::Undefined, AvenMarshal::to_value)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        if matches!(value, Value::Undefined) {
            Ok(None)
        } else {
            T::from_value(value).map(Some)
        }
    }
}

impl<T: AvenMarshal, E: AvenMarshal> AvenMarshal for Result<T, E> {
    fn aven_type() -> Type {
        build::result(T::aven_type(), E::aven_type())
    }

    fn to_value(self) -> Value {
        let (name, payload) = match self {
            Ok(value) => ("Ok", value.to_value()),
            Err(error) => ("Err", error.to_value()),
        };
        Value::Tag {
            name: name.to_owned(),
            payload: vec![payload],
        }
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Tag { name, payload } = value else {
            return Err(mismatch("Result", value));
        };
        let [payload] = payload.as_slice() else {
            return Err(format!(
                "expected Result tag with 1 payload, got @{name} with {} payloads",
                payload.len()
            ));
        };
        match name.as_str() {
            "Ok" => T::from_value(payload)
                .map(Ok)
                .map_err(|error| format!("at @Ok payload: {error}")),
            "Err" => E::from_value(payload)
                .map(Err)
                .map_err(|error| format!("at @Err payload: {error}")),
            _ => Err(format!("expected @Ok or @Err, got @{name}")),
        }
    }
}

impl<K, V> AvenMarshal for BTreeMap<K, V>
where
    K: AvenMarshal + Ord,
    V: AvenMarshal,
{
    fn aven_type() -> Type {
        build::map(K::aven_type(), V::aven_type())
    }

    fn to_value(self) -> Value {
        Value::Map(Rc::new(
            self.into_iter()
                .map(|(key, value)| (key.to_value(), value.to_value()))
                .collect(),
        ))
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Map(entries) = value else {
            return Err(mismatch("Map", value));
        };
        entries
            .iter()
            .enumerate()
            .map(|(index, (key, value))| {
                Ok((
                    K::from_value(key)
                        .map_err(|error| format!("at Map entry {index} key: {error}"))?,
                    V::from_value(value)
                        .map_err(|error| format!("at Map entry {index} value: {error}"))?,
                ))
            })
            .collect()
    }
}

impl<T> AvenMarshal for BTreeSet<T>
where
    T: AvenMarshal + Ord,
{
    fn aven_type() -> Type {
        build::set(T::aven_type())
    }

    fn to_value(self) -> Value {
        Value::Set(Rc::new(
            self.into_iter().map(AvenMarshal::to_value).collect(),
        ))
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Set(values) = value else {
            return Err(mismatch("Set", value));
        };
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                T::from_value(value).map_err(|error| format!("at Set[{index}]: {error}"))
            })
            .collect()
    }
}

macro_rules! impl_tuple_marshal {
    ($length:expr; $($index:tt => $item:ident),+) => {
        impl<$($item: AvenMarshal),+> AvenMarshal for ($($item,)+) {
            fn aven_type() -> Type {
                Type::Tuple(vec![$($item::aven_type()),+])
            }

            fn to_value(self) -> Value {
                Value::Tuple(Rc::new(vec![$(self.$index.to_value()),+]))
            }

            fn from_value(value: &Value) -> Result<Self, String> {
                let Value::Tuple(values) = value else {
                    return Err(mismatch("Tuple", value));
                };
                if values.len() != $length {
                    return Err(format!(
                        "expected Tuple with {} items, got Tuple with {} items",
                        $length,
                        values.len()
                    ));
                }
                Ok(($(
                    $item::from_value(&values[$index])
                        .map_err(|error| format!("at Tuple[{}]: {error}", $index))?,
                )+))
            }
        }
    };
}

impl_tuple_marshal!(1; 0 => T0);
impl_tuple_marshal!(2; 0 => T0, 1 => T1);
impl_tuple_marshal!(3; 0 => T0, 1 => T1, 2 => T2);
impl_tuple_marshal!(4; 0 => T0, 1 => T1, 2 => T2, 3 => T3);

mod sealed {
    pub trait Sealed<Args> {}
}

/// A Rust closure that lifts into the `(Type, Value)` pair
/// [`crate::Host::register_fn`] registers. Sealed: implemented (via a macro)
/// only for `Fn(A0, ..) -> R + 'static` where every `Ai: AvenMarshal` and
/// `R: HostFnReturn`, arities 0..=4.
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
            R: HostFnReturn,
        {}

        impl<F, R, $($arg),*> IntoHostFn<($($arg,)*)> for F
        where
            F: Fn($($arg),*) -> R + 'static,
            $($arg: AvenMarshal,)*
            R: HostFnReturn,
        {
            fn into_host_fn(self) -> (Type, Value) {
                let ty = build::function(vec![$($arg::aven_type()),*], R::return_type());
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
                    result.into_host_value()
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
impl_into_host_fn!(A0, A1, A2, A3, A4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        assert_eq!(i64::from_value(&42_i64.to_value()), Ok(42));
        let large: Int = "115132219018763992565095597973971522401"
            .parse()
            .expect("test integer literal is valid");
        assert_eq!(Int::from_value(&large.clone().to_value()), Ok(large));
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
    fn arrays_optionals_results_and_tuples_round_trip() {
        let array = vec![Some(1_i64), None, Some(3)];
        assert_eq!(
            Vec::<Option<i64>>::from_value(&array.clone().to_value()),
            Ok(array)
        );
        assert_eq!(
            Vec::<Option<i64>>::aven_type(),
            build::array(build::optional(build::int()))
        );

        let ok: Result<(i64, String), bool> = Ok((7, "seven".to_owned()));
        assert_eq!(
            Result::<(i64, String), bool>::from_value(&ok.clone().to_value()),
            Ok(ok)
        );
        assert_eq!(
            Result::<(i64, String), bool>::aven_type(),
            build::result(
                Type::Tuple(vec![build::int(), build::text()]),
                build::bool(),
            )
        );

        let err: Result<(i64, String), bool> = Err(true);
        assert_eq!(
            Result::<(i64, String), bool>::from_value(&err.clone().to_value()),
            Ok(err)
        );
    }

    #[test]
    fn ordered_maps_and_sets_round_trip() {
        let map = BTreeMap::from([("a".to_owned(), 1_i64), ("b".to_owned(), 2)]);
        assert_eq!(
            BTreeMap::<String, i64>::from_value(&map.clone().to_value()),
            Ok(map)
        );
        assert_eq!(
            BTreeMap::<String, i64>::aven_type(),
            build::map(build::text(), build::int())
        );

        let set = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        assert_eq!(
            BTreeSet::<String>::from_value(&set.clone().to_value()),
            Ok(set)
        );
        assert_eq!(BTreeSet::<String>::aven_type(), build::set(build::text()));
    }

    #[test]
    fn compound_mismatches_report_the_nested_location() {
        assert_eq!(
            Vec::<i64>::from_value(&Value::Array(Rc::new(vec![
                Value::int(1),
                Value::Text("x".to_owned()),
            ]))),
            Err("at Array[1]: expected Int, got Text".to_owned())
        );
        assert_eq!(
            Result::<i64, String>::from_value(&Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Bool(true)],
            }),
            Err("at @Ok payload: expected Int, got Bool".to_owned())
        );
        assert_eq!(
            <(i64, bool)>::from_value(&Value::Tuple(Rc::new(vec![Value::int(1)]))),
            Err("expected Tuple with 2 items, got Tuple with 1 items".to_owned())
        );
    }

    #[test]
    fn from_value_reports_shape_mismatch() {
        assert_eq!(
            i64::from_value(&Value::Text("x".to_owned())),
            Err("expected Int, got Text".to_owned())
        );
        assert_eq!(
            String::from_value(&Value::int(1)),
            Err("expected Text, got Int".to_owned())
        );
        assert_eq!(
            bool::from_value(&Value::unit()),
            Err("expected Bool, got Unit".to_owned())
        );
        assert!(<()>::from_value(&Value::int(1)).is_err());
    }

    #[test]
    fn i64_host_arguments_reject_out_of_range_aven_ints() {
        let large = Value::Int(
            "18446744073709551615"
                .parse()
                .expect("test integer literal is valid"),
        );
        assert_eq!(
            i64::from_value(&large),
            Err("Int value 18446744073709551615 does not fit Rust i64".to_owned())
        );

        let (_, native) = (|value: i64| value).into_host_fn();
        assert_eq!(
            call_native(&native, &[large]),
            Err("Int value 18446744073709551615 does not fit Rust i64".to_owned())
        );
    }

    fn call_native(value: &Value, args: &[Value]) -> Result<Value, String> {
        let Value::Native(native) = value else {
            panic!("expected a native value");
        };
        native(
            args,
            aven_eval::NativeContext::without_source(aven_core::Span::new(0, 0)),
        )
    }

    #[test]
    fn binary_fn_derives_type_and_marshalling_native() {
        let (ty, value) = (|a: i64, b: i64| a + b).into_host_fn();

        assert_eq!(
            ty,
            build::function(vec![build::int(), build::int()], build::int())
        );

        assert_eq!(
            call_native(&value, &[Value::int(2), Value::int(3)]),
            Ok(Value::int(5))
        );
        assert_eq!(
            call_native(&value, &[Value::Text("x".to_owned()), Value::int(3)]),
            Err("expected Int, got Text".to_owned())
        );
        assert_eq!(
            call_native(&value, &[Value::int(1)]),
            Err("expected 2 arguments, got 1".to_owned())
        );
    }

    #[test]
    fn nullary_fn_derives_type_and_native() {
        let (ty, value) = (|| 42_i64).into_host_fn();

        assert_eq!(ty, build::function(vec![], build::int()));
        assert_eq!(call_native(&value, &[]), Ok(Value::int(42)));
        assert_eq!(
            call_native(&value, &[Value::int(0)]),
            Err("expected 0 arguments, got 1".to_owned())
        );
    }

    #[test]
    fn host_result_keeps_the_success_type_and_propagates_native_errors() {
        let (ty, value) =
            (|| HostResult::<Option<String>>::from(Err("read failed".to_owned()))).into_host_fn();

        assert_eq!(ty, build::function(vec![], build::optional(build::text())));
        assert_eq!(call_native(&value, &[]), Err("read failed".to_owned()));
    }

    #[test]
    fn compound_and_five_argument_functions_derive_types_and_marshal() {
        let (ty, native) = (|values: Vec<i64>, fallback: Option<i64>| -> Result<i64, String> {
            values
                .into_iter()
                .next()
                .or(fallback)
                .ok_or("empty".to_owned())
        })
        .into_host_fn();
        assert_eq!(
            ty,
            build::function(
                vec![build::array(build::int()), build::optional(build::int())],
                build::result(build::int(), build::text()),
            )
        );
        assert_eq!(
            call_native(
                &native,
                &[Value::Array(Rc::new(vec![Value::int(4)])), Value::Undefined],
            ),
            Ok(Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::int(4)],
            })
        );

        let (ty, native) =
            (|a: i64, b: i64, c: i64, d: i64, e: i64| a + b + c + d + e).into_host_fn();
        assert_eq!(ty, build::function(vec![build::int(); 5], build::int()));
        assert_eq!(
            call_native(
                &native,
                &[
                    Value::int(1),
                    Value::int(2),
                    Value::int(3),
                    Value::int(4),
                    Value::int(5),
                ],
            ),
            Ok(Value::int(15))
        );
    }
}
