//! Entry-style API for [`AsyncShardedHashMap`].

use crate::AsyncShardedHashMap;
use std::hash::{BuildHasher, Hash};

/// Entry handle for a key in an [`AsyncShardedHashMap`].
///
/// The variant reflects the state observed when [`AsyncShardedHashMap::entry`]
/// was called. Mutating methods re-enter the target shard and perform the
/// requested operation atomically for that method.
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub enum AsyncEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// The key was present when the entry was created.
    Occupied(AsyncOccupiedEntry<'a, K, V, S>),
    /// The key was absent when the entry was created.
    Vacant(AsyncVacantEntry<'a, K, V, S>),
}

impl<'a, K, V, S> AsyncEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    pub(crate) fn occupied(map: &'a AsyncShardedHashMap<K, V, S>, key: K) -> Self {
        Self::Occupied(AsyncOccupiedEntry { map, key })
    }

    pub(crate) fn vacant(map: &'a AsyncShardedHashMap<K, V, S>, key: K) -> Self {
        Self::Vacant(AsyncVacantEntry { map, key })
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
    pub async fn or_insert(self, default: V) -> V {
        self.or_insert_with(|| default).await
    }

    /// Ensures a value exists for the key, computing the value only when absent.
    pub async fn or_insert_with<F>(self, default: F) -> V
    where
        F: FnOnce() -> V,
    {
        let (map, key) = self.into_parts();
        map.compute_if_absent(key, default).await
    }

    /// Runs `f` on the value when present, then returns a fresh entry handle.
    pub async fn and_modify<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut V),
    {
        let (map, key) = self.into_parts();
        let modified = map
            .compute_if_present(&key, |old| {
                let mut next = old;
                f(&mut next);
                Some(next)
            })
            .await
            .is_some();
        if modified {
            Self::occupied(map, key)
        } else {
            Self::vacant(map, key)
        }
    }

    /// Inserts a value for the key, returning the previous value if any.
    pub async fn insert(self, value: V) -> Option<V> {
        let (map, key) = self.into_parts();
        map.insert(key, value).await
    }

    /// Removes the key, returning the previous value if any.
    pub async fn remove(self) -> Option<V> {
        let (map, key) = self.into_parts();
        map.remove(&key).await
    }

    fn into_parts(self) -> (&'a AsyncShardedHashMap<K, V, S>, K) {
        match self {
            Self::Occupied(entry) => (entry.map, entry.key),
            Self::Vacant(entry) => (entry.map, entry.key),
        }
    }
}

/// Occupied entry handle for [`AsyncShardedHashMap`].
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub struct AsyncOccupiedEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    map: &'a AsyncShardedHashMap<K, V, S>,
    key: K,
}

impl<'a, K, V, S> AsyncOccupiedEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Returns a reference to the entry key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Returns the current value, or `None` if it was removed concurrently.
    pub async fn get(&self) -> Option<V> {
        self.map.get(&self.key).await
    }

    /// Replaces the current value, returning the previous value if any.
    pub async fn insert(self, value: V) -> Option<V> {
        self.map.insert(self.key, value).await
    }

    /// Removes this entry and returns the value if it still exists.
    pub async fn remove(self) -> Option<V> {
        self.map.remove(&self.key).await
    }

    /// Removes this entry and returns the key/value pair if it still exists.
    pub async fn remove_entry(self) -> Option<(K, V)> {
        let value = self.map.remove(&self.key).await?;
        Some((self.key, value))
    }
}

/// Vacant entry handle for [`AsyncShardedHashMap`].
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub struct AsyncVacantEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    map: &'a AsyncShardedHashMap<K, V, S>,
    key: K,
}

impl<'a, K, V, S> AsyncVacantEntry<'a, K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
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
    pub async fn insert(self, value: V) -> V {
        self.map.compute_if_absent(self.key, || value).await
    }
}
