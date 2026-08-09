use std::{
    collections::hash_map::DefaultHasher,
    fmt,
    hash::{Hash, Hasher},
    rc::Rc,
};

use imbl::{HashMap, OrdMap, Vector};

use crate::{PrimitivePayload, Value, int_to_f64};

/// Persistent map storage with stable insertion-order identifiers.
///
/// The hash index stores collision lists rather than `Value` keys because Aven
/// numeric equality intentionally crosses the Int/Float boundary and is not a
/// Rust `Eq` relation. Every candidate is confirmed with Aven equality.
#[derive(Clone, Default)]
pub struct MapValue {
    entries: OrdMap<u64, (Value, Value)>,
    index: HashMap<u64, Vector<u64>>,
    next_id: u64,
}

impl MapValue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: impl IntoIterator<Item = (Value, Value)>) -> Self {
        let mut map = Self::new();
        for (key, value) in entries {
            map.insert(key, value);
        }
        map
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Value, Value)> {
        self.entries.iter().map(|(_, entry)| entry)
    }

    pub fn get(&self, key: &Value) -> Option<&Value> {
        let id = self.entry_id(key)?;
        self.entries.get(&id).map(|(_, value)| value)
    }

    pub fn insert(&mut self, key: Value, value: Value) {
        let fingerprint = key_fingerprint(&key);
        if let Some(id) = self.entry_id_with_fingerprint(&key, fingerprint) {
            self.entries.insert(id, (key, value));
            return;
        }

        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("a Map cannot contain more than u64::MAX lifetime insertions");
        self.entries.insert(id, (key, value));

        let mut ids = self.index.get(&fingerprint).cloned().unwrap_or_default();
        ids.push_back(id);
        self.index.insert(fingerprint, ids);
    }

    pub fn remove(&mut self, key: &Value) {
        let fingerprint = key_fingerprint(key);
        let Some(id) = self.entry_id_with_fingerprint(key, fingerprint) else {
            return;
        };

        self.entries.remove(&id);
        let Some(mut ids) = self.index.get(&fingerprint).cloned() else {
            return;
        };
        let Some(position) = ids.iter().position(|candidate| *candidate == id) else {
            return;
        };
        ids.remove(position);
        if ids.is_empty() {
            self.index.remove(&fingerprint);
        } else {
            self.index.insert(fingerprint, ids);
        }
    }

    fn entry_id(&self, key: &Value) -> Option<u64> {
        self.entry_id_with_fingerprint(key, key_fingerprint(key))
    }

    fn entry_id_with_fingerprint(&self, key: &Value, fingerprint: u64) -> Option<u64> {
        self.index.get(&fingerprint)?.iter().copied().find(|id| {
            self.entries
                .get(id)
                .is_some_and(|(candidate, _)| candidate == key)
        })
    }
}

impl PartialEq for MapValue {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.iter().all(|(key, value)| {
                other
                    .get(key)
                    .is_some_and(|other_value| value == other_value)
            })
    }
}

impl fmt::Debug for MapValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl FromIterator<(Value, Value)> for MapValue {
    fn from_iter<T: IntoIterator<Item = (Value, Value)>>(iter: T) -> Self {
        Self::from_entries(iter)
    }
}

fn key_fingerprint(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_value(value, &mut hasher);
    hasher.finish()
}

fn hash_value(value: &Value, state: &mut impl Hasher) {
    match value {
        Value::Int(value) => hash_number(int_to_f64(value), state),
        Value::Float(value) => hash_number(*value, state),
        Value::Text(value) => hash_tagged(1, value, state),
        Value::Bool(value) => hash_tagged(2, value, state),
        Value::Array(values) => hash_sequence(3, values, state),
        Value::Tuple(values) => hash_sequence(4, values, state),
        Value::Set(values) => hash_unordered_values(5, values, state),
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
        | Value::RangeConstructor { .. } => 255_u8.hash(state),
    }
}

fn hash_primitive_payload(payload: &PrimitivePayload, state: &mut impl Hasher) {
    match payload {
        PrimitivePayload::Int(value) => hash_number(int_to_f64(value), state),
        PrimitivePayload::Float(value) => hash_number(*value, state),
        PrimitivePayload::Text(value) => hash_tagged(1, value, state),
        PrimitivePayload::Bool(value) => hash_tagged(2, value, state),
        PrimitivePayload::Array(values) => hash_sequence(3, values, state),
        PrimitivePayload::Set(values) => hash_unordered_values(5, values, state),
        PrimitivePayload::Map(entries) => hash_map(6, entries, state),
    }
}

fn hash_number(value: f64, state: &mut impl Hasher) {
    0_u8.hash(state);
    let bits = if value.is_nan() {
        f64::NAN.to_bits()
    } else if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    };
    bits.hash(state);
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

fn hash_unordered_values(tag: u8, values: &[Value], state: &mut impl Hasher) {
    tag.hash(state);
    let mut fingerprints: Vec<_> = values.iter().map(key_fingerprint).collect();
    fingerprints.sort_unstable();
    fingerprints.hash(state);
}

fn hash_map(tag: u8, entries: &MapValue, state: &mut impl Hasher) {
    tag.hash(state);
    let mut fingerprints: Vec<_> = entries
        .iter()
        .map(|(key, value)| {
            let mut pair = DefaultHasher::new();
            hash_value(key, &mut pair);
            hash_value(value, &mut pair);
            pair.finish()
        })
        .collect();
    fingerprints.sort_unstable();
    fingerprints.hash(state);
}

fn hash_record(tag: u8, fields: &[(String, Value)], state: &mut impl Hasher) {
    tag.hash(state);
    let mut fingerprints: Vec<_> = fields
        .iter()
        .map(|(name, value)| {
            let mut field = DefaultHasher::new();
            name.hash(&mut field);
            hash_value(value, &mut field);
            field.finish()
        })
        .collect();
    fingerprints.sort_unstable();
    fingerprints.hash(state);
}
