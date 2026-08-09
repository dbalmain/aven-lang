use std::fmt;

use imbl::{HashMap, OrdMap, Vector};

use crate::{Value, fingerprint::value_fingerprint};

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
        let fingerprint = value_fingerprint(&key);
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
        let fingerprint = value_fingerprint(key);
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
        self.entry_id_with_fingerprint(key, value_fingerprint(key))
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
