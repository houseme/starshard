//! Serializable snapshot wrapper for `AsyncShardedHashMap`.
//!
//! Since async maps require `.await` to read locks, they cannot directly
//! implement `Serialize`.  Instead, call `async_snapshot_serializable().await`
//! to obtain an [`AsyncShardedHashMapSnapshot`] that implements `Serialize`.

use super::*;
use ::serde::{Serialize, Serializer, ser::SerializeSeq};

#[cfg(all(feature = "async", feature = "serde"))]
impl<K, V> AsyncShardedHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + Serialize + 'static,
    V: Clone + Send + Sync + Serialize + 'static,
{
    /// Obtain a serializable snapshot wrapper (for `serde`).
    pub async fn async_snapshot_serializable(&self) -> AsyncShardedHashMapSnapshot<K, V> {
        AsyncShardedHashMapSnapshot(self.iter().await)
    }
}

/* -------- Serializable Snapshot Wrapper for Async Map -------- */
/// A serializable snapshot of an `AsyncShardedHashMap`.
#[cfg(all(feature = "async", feature = "serde"))]
pub struct AsyncShardedHashMapSnapshot<K, V>(Vec<(K, V)>);

#[cfg(all(feature = "async", feature = "serde"))]
impl<K, V> Serialize for AsyncShardedHashMapSnapshot<K, V>
where
    K: Serialize,
    V: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for kv in &self.0 {
            seq.serialize_element(kv)?;
        }
        seq.end()
    }
}
