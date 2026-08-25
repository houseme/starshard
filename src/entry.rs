//! Entry-style API for [`ShardedHashMap`].

use crate::ShardedHashMap;
use std::hash::{BuildHasher, Hash};

/// Entry handle for a key in a [`ShardedHashMap`].
///
/// The variant reflects the state observed when [`ShardedHashMap::entry`] was
/// called. Mutating methods re-enter the target shard and perform the requested
/// operation atomically for that method.
pub enum Entry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// The key was present when the entry was created.
    Occupied(OccupiedEntry<'a, K, V, S>),
    /// The key was absent when the entry was created.
    Vacant(VacantEntry<'a, K, V, S>),
}

impl<'a, K, V, S> Entry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    pub(crate) fn occupied(map: &'a ShardedHashMap<K, V, S>, key: K) -> Self {
        Self::Occupied(OccupiedEntry { map, key })
    }

    pub(crate) fn vacant(map: &'a ShardedHashMap<K, V, S>, key: K) -> Self {
        Self::Vacant(VacantEntry { map, key })
    }

    /// Returns a reference to this entry's key.
    pub fn key(&self) -> &K {
        match self {
            Self::Occupied(entry) => entry.key(),
            Self::Vacant(entry) => entry.key(),
        }
    }

    /// Returns true when this handle was created from an occupied key.
    pub fn is_occupied(&self) -> bool {
        matches!(self, Self::Occupied(_))
    }

    /// Returns true when this handle was created from a vacant key.
    pub fn is_vacant(&self) -> bool {
        matches!(self, Self::Vacant(_))
    }

    /// Ensures a value exists for the key and returns the existing or inserted value.
    pub fn or_insert(self, default: V) -> V {
        self.or_insert_with(|| default)
    }

    /// Ensures a value exists for the key, computing the value only when absent.
    pub fn or_insert_with<F>(self, default: F) -> V
    where
        F: FnOnce() -> V,
    {
        let (map, key) = self.into_parts();
        map.compute_if_absent(key, default)
    }

    /// Runs `f` on the value when present, then returns a fresh entry handle.
    pub fn and_modify<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut V),
    {
        let (map, key) = self.into_parts();
        let modified = map
            .compute_if_present(&key, |old| {
                let mut next = old.clone();
                f(&mut next);
                Some(next)
            })
            .is_some();
        if modified {
            Self::occupied(map, key)
        } else {
            Self::vacant(map, key)
        }
    }

    /// Inserts a value for the key, returning the previous value if any.
    pub fn insert(self, value: V) -> Option<V> {
        let (map, key) = self.into_parts();
        map.insert(key, value)
    }

    /// Removes the key, returning the previous value if any.
    pub fn remove(self) -> Option<V> {
        let (map, key) = self.into_parts();
        map.remove(&key)
    }

    fn into_parts(self) -> (&'a ShardedHashMap<K, V, S>, K) {
        match self {
            Self::Occupied(entry) => (entry.map, entry.key),
            Self::Vacant(entry) => (entry.map, entry.key),
        }
    }
}

/// Occupied entry handle for [`ShardedHashMap`].
pub struct OccupiedEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    map: &'a ShardedHashMap<K, V, S>,
    key: K,
}

impl<'a, K, V, S> OccupiedEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Returns a reference to the entry key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Returns the current value, or `None` if it was removed concurrently.
    pub fn get(&self) -> Option<V> {
        self.map.get(&self.key)
    }

    /// Replaces the current value, returning the previous value if any.
    pub fn insert(self, value: V) -> Option<V> {
        self.map.insert(self.key, value)
    }

    /// Removes this entry and returns the value if it still exists.
    pub fn remove(self) -> Option<V> {
        self.map.remove(&self.key)
    }

    /// Removes this entry and returns the key/value pair if it still exists.
    pub fn remove_entry(self) -> Option<(K, V)> {
        let value = self.map.remove(&self.key)?;
        Some((self.key, value))
    }
}

/// Vacant entry handle for [`ShardedHashMap`].
pub struct VacantEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    map: &'a ShardedHashMap<K, V, S>,
    key: K,
}

impl<'a, K, V, S> VacantEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Returns a reference to the entry key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Consumes the handle and returns the owned key.
    pub fn into_key(self) -> K {
        self.key
    }

    /// Inserts the value if the key is still absent and returns the final value.
    pub fn insert(self, value: V) -> V {
        self.map.compute_if_absent(self.key, || value)
    }
}
