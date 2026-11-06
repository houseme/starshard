//! Serde support for Starshard maps.
//!
//! - [`sync_serde`] — `Serialize` / `Deserialize` impls for `ShardedHashMap`.
//! - [`async_snapshot`] — serializable snapshot wrapper for
//!   `AsyncShardedHashMap` (behind `async` + `serde` features).
//! - [`SnapshotView`] — borrowed serialization as an array of key/value pairs.

use super::*;
use ::serde::{Serialize, Serializer, ser::SerializeSeq};

pub(crate) mod sync_serde;

#[cfg(feature = "async")]
pub(crate) mod async_snapshot;

#[cfg(all(feature = "async", feature = "serde"))]
pub use async_snapshot::AsyncShardedHashMapSnapshot;

impl<K, V, S> Serialize for SnapshotView<K, V, S>
where
    K: Serialize,
    V: Serialize,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for entry in self.iter() {
            sequence.serialize_element(&entry)?;
        }
        sequence.end()
    }
}
