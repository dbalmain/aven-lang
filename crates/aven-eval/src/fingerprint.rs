//! Structural fingerprints for `Value`, shared by the persistent collections.
//!
//! `Value` carries `Rc` collections, streams and closures, so it has neither
//! `Hash` nor `Ord`, and Aven equality is not a Rust `Eq` relation — `1` and
//! `1.0` compare equal across kinds. A fingerprint hashes a normalised view of
//! the value instead: numbers collapse to their exact value with signed zero
//! and NaN folded together, and containers whose order does not affect
//! equality hash their members order-independently.
//!
//! Fingerprints are a pre-filter only. They may collide, so every candidate a
//! lookup finds is confirmed with Aven's own equality.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    rc::Rc,
};

use crate::{Int, MapValue, PrimitivePayload, SetValue, Value};

pub(crate) fn value_fingerprint(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_value(value, &mut hasher);
    hasher.finish()
}

fn hash_value(value: &Value, state: &mut impl Hasher) {
    match value {
        Value::Int(value) => hash_int(value, state),
        Value::Float(value) => hash_float(*value, state),
        Value::Text(value) => hash_tagged(1, value, state),
        Value::Bool(value) => hash_tagged(2, value, state),
        Value::Array(values) => hash_sequence(3, values, state),
        Value::Tuple(values) => hash_sequence(4, values, state),
        Value::Set(members) => hash_set(5, members, state),
        Value::Map(entries) => hash_map(6, entries, state),
        Value::Record(fields) => hash_record(7, fields, state),
        Value::SlotRecord { fields, slots } => {
            8_u8.hash(state);
            hash_record(0, fields, state);
            hash_record(0, slots, state);
        }
        Value::NamedRecord { descriptor, fields } => {
            9_u8.hash(state);
            Rc::as_ptr(descriptor).hash(state);
            hash_record(0, fields, state);
        }
        Value::BrandedPrimitive { payload, .. } => hash_primitive_payload(payload, state),
        Value::Tag { name, payload } => {
            10_u8.hash(state);
            name.hash(state);
            hash_sequence(0, payload, state);
        }
        Value::Type(ty) => hash_tagged(11, ty, state),
        Value::Undefined => 12_u8.hash(state),
        Value::Null => 13_u8.hash(state),
        Value::Stream(_)
        | Value::NamedFamily(_)
        | Value::NamedMethod { .. }
        | Value::UnboundNamedMethod { .. }
        | Value::ResultMethod { .. }
        | Value::StreamMethod { .. }
        | Value::ArrayFlatMapMethod(_)
        | Value::ArrayFoldMethod(_)
        | Value::Closure(_)
        | Value::Native(_)
        | Value::RangeConstructor { .. }
        | Value::CollectConstructor(_) => 255_u8.hash(state),
    }
}

fn hash_primitive_payload(payload: &PrimitivePayload, state: &mut impl Hasher) {
    match payload {
        PrimitivePayload::Int(value) => hash_int(value, state),
        PrimitivePayload::Float(value) => hash_float(*value, state),
        PrimitivePayload::Text(value) => hash_tagged(1, value, state),
        PrimitivePayload::Bool(value) => hash_tagged(2, value, state),
        PrimitivePayload::Array(values) => hash_sequence(3, values, state),
        PrimitivePayload::Set(members) => hash_set(5, members, state),
        PrimitivePayload::Map(entries) => hash_map(6, entries, state),
    }
}

/// Numbers hash by exact value rather than by `f64` bits, matching the exact
/// Int/Float comparison: an integer equals a float only when the float holds
/// that integer, so every whole number hashes through this one path whichever
/// kind carries it. Narrowing the integer here instead would hand distinct
/// integers past 2^53 a single shared slot.
fn hash_int(value: &Int, state: &mut impl Hasher) {
    hash_tagged(0, value, state);
}

fn hash_float(value: f64, state: &mut impl Hasher) {
    // `-0.0` reads as the integer zero, which folds the signed zeroes.
    if let Some(value) = Int::from_f64_exact(value) {
        hash_int(&value, state);
        return;
    }

    // NaN equals itself in Aven, so every payload folds onto one.
    let bits = if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    };
    hash_tagged(14, &bits, state);
}

fn hash_tagged(tag: u8, value: &impl Hash, state: &mut impl Hasher) {
    tag.hash(state);
    value.hash(state);
}

fn hash_sequence(tag: u8, values: &[Value], state: &mut impl Hasher) {
    tag.hash(state);
    values.len().hash(state);
    for value in values {
        hash_value(value, state);
    }
}

fn hash_set(tag: u8, members: &SetValue, state: &mut impl Hasher) {
    hash_unordered(tag, members.iter().map(value_fingerprint), state);
}

fn hash_map(tag: u8, entries: &MapValue, state: &mut impl Hasher) {
    hash_unordered(
        tag,
        entries.iter().map(|(key, value)| {
            let mut pair = DefaultHasher::new();
            hash_value(key, &mut pair);
            hash_value(value, &mut pair);
            pair.finish()
        }),
        state,
    );
}

fn hash_record(tag: u8, fields: &[(String, Value)], state: &mut impl Hasher) {
    hash_unordered(
        tag,
        fields.iter().map(|(name, value)| {
            let mut field = DefaultHasher::new();
            name.hash(&mut field);
            hash_value(value, &mut field);
            field.finish()
        }),
        state,
    );
}

/// Hash member fingerprints so the result does not depend on their order, for
/// the containers whose equality ignores it.
fn hash_unordered(tag: u8, fingerprints: impl Iterator<Item = u64>, state: &mut impl Hasher) {
    tag.hash(state);
    let mut fingerprints: Vec<_> = fingerprints.collect();
    fingerprints.sort_unstable();
    fingerprints.hash(state);
}
