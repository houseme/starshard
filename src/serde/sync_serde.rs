//! `Serialize` and `Deserialize` implementations for `ShardedHashMap`.
//!
//! The serialized form is `{ shard_count, entries: [(K, V), ...] }`.
//! Serialization borrows entries from a coherent immutable view.
//! Hasher state is not preserved; deserialization rebuilds with `S::default()`.
//! A `shard_count` of zero is normalized to [`DEFAULT_SHARDS`].

use super::DEFAULT_SHARDS;
use super::ShardedHashMap;
use ::serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeStruct};
use std::hash::{BuildHasher, Hash};

/// Owned transit representation used for deserialization.
#[derive(Deserialize)]
struct ShardedMapRepr<K, V> {
    shard_count: usize,
    entries: Vec<(K, V)>,
}

// Serialize
impl<K, V, S> Serialize for ShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + Serialize,
    V: Clone + Send + Sync + Serialize,
    S: BuildHasher + Clone + Send + Sync + Default,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: Serializer,
    {
        // Capture topology and data together, and release map locks before any
        // user serializer runs. Cow views share shards without cloning entries.
        let view = self.snapshot_view();
        let mut repr = serializer.serialize_struct("ShardedMapRepr", 2)?;
        repr.serialize_field("shard_count", &view.shard_count())?;
        repr.serialize_field("entries", &view)?;
        repr.end()
    }
}

// Deserialize
impl<'de, K, V, S> Deserialize<'de> for ShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + Deserialize<'de>,
    V: Clone + Send + Sync + Deserialize<'de>,
    S: BuildHasher + Clone + Send + Sync + Default,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let repr = ShardedMapRepr::<K, V>::deserialize(deserializer)?;
        let sc = if repr.shard_count == 0 {
            DEFAULT_SHARDS
        } else {
            repr.shard_count
        };
        let map = ShardedHashMap::<K, V, S>::with_shards_and_hasher(sc, S::default());
        for (k, v) in repr.entries {
            map.insert(k, v);
        }
        Ok(map)
    }
}
