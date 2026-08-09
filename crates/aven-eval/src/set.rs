use std::fmt;

use imbl::{HashMap, OrdMap, Vector};

use crate::{Value, fingerprint::value_fingerprint};

/// Persistent set storage with stable insertion-order identifiers.
///
/// Members live in an ordered tree keyed by an insertion counter so iteration
/// and rendering stay in insertion order, and a hash index over structural
/// fingerprints answers membership. The index stores collision lists rather
/// than `Value` members because Aven equality intentionally crosses the
/// Int/Float boundary and is not a Rust `Eq` relation; every candidate is
/// confirmed with Aven equality.
#[derive(Clone, Default)]
pub struct SetValue {
    members: OrdMap<u64, Value>,
    index: HashMap<u64, Vector<u64>>,
    next_id: u64,
}

impl SetValue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Collect members, keeping the first occurrence of each: a later duplicate
    /// neither moves the original nor grows the set.
    pub fn from_values(values: impl IntoIterator<Item = Value>) -> Self {
        let mut set = Self::new();
        for value in values {
            set.insert(value);
        }
        set
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Value> {
        self.members.iter().map(|(_, member)| member)
    }

    pub fn contains(&self, value: &Value) -> bool {
        self.member_id(value).is_some()
    }

    /// Add a member. A member already present keeps its original position and
    /// its original value, so re-adding is observably a no-op.
    pub fn insert(&mut self, value: Value) {
        let fingerprint = value_fingerprint(&value);
        if self
            .member_id_with_fingerprint(&value, fingerprint)
            .is_some()
        {
            return;
        }

        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("a Set cannot contain more than u64::MAX lifetime insertions");
        self.members.insert(id, value);

        let mut ids = self.index.get(&fingerprint).cloned().unwrap_or_default();
        ids.push_back(id);
        self.index.insert(fingerprint, ids);
    }

    fn member_id(&self, value: &Value) -> Option<u64> {
        self.member_id_with_fingerprint(value, value_fingerprint(value))
    }

    fn member_id_with_fingerprint(&self, value: &Value, fingerprint: u64) -> Option<u64> {
        self.index
            .get(&fingerprint)?
            .iter()
            .copied()
            .find(|id| self.members.get(id).is_some_and(|member| member == value))
    }
}

impl PartialEq for SetValue {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|member| other.contains(member))
    }
}

impl fmt::Debug for SetValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl FromIterator<Value> for SetValue {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        Self::from_values(iter)
    }
}

impl Extend<Value> for SetValue {
    fn extend<T: IntoIterator<Item = Value>>(&mut self, iter: T) {
        for value in iter {
            self.insert(value);
        }
    }
}
