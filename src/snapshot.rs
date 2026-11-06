//! Immutable views of a coherent map version.

use hashbrown::HashMap;
use rustc_hash::FxBuildHasher;
use std::sync::Arc;

/// An immutable view whose entries can be borrowed without cloning keys or values.
///
/// In [`crate::SnapshotMode::Cow`], capturing this view shares shard allocations.
/// A subsequent write copies only a shard still retained by a view. In other
/// modes, capture clones initialized shards. Holding a view can therefore retain
/// old shard allocations after writes, migration, or destruction of the map.
///
/// The view preserves the mutation version and active shard count from the same
/// coherent capture. Its iteration order is unspecified.
pub struct SnapshotView<K, V, S = FxBuildHasher> {
    shards: Arc<Vec<Arc<HashMap<K, V, S>>>>,
    len: usize,
    version: u64,
    shard_count: usize,
}

impl<K, V, S> SnapshotView<K, V, S> {
    pub(crate) fn new(
        shards: Vec<Arc<HashMap<K, V, S>>>,
        version: u64,
        shard_count: usize,
    ) -> Self {
        let len = shards.iter().map(|shard| shard.len()).sum();
        Self {
            shards: Arc::new(shards),
            len,
            version,
            shard_count,
        }
    }

    /// Borrow all captured keys and values without acquiring map locks.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.shards.iter().flat_map(|shard| shard.iter())
    }

    /// Number of entries in this view.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the captured map is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Mutation version captured with these entries.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Active shard count captured with these entries.
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }
}

impl<K, V, S> Clone for SnapshotView<K, V, S> {
    fn clone(&self) -> Self {
        Self {
            shards: self.shards.clone(),
            len: self.len,
            version: self.version,
            shard_count: self.shard_count,
        }
    }
}
