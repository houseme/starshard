//! Internal type aliases for shard storage.
//!
//! Defines the concrete container types used by both sync and async maps:
//! `StdShard`, `StdShardVec`, `StdShardVecArc` (and their `Async*` mirrors),
//! plus the `ReplicaList` alias used by the advanced replication feature.

use super::*;
use std::ops::{Deref, DerefMut};

/// Mutable shard storage. Only Cow mode pays for a shared map allocation.
pub(crate) enum ShardData<K, V, S> {
    Owned(HashMap<K, V, S>),
    Shared(Arc<HashMap<K, V, S>>),
}

impl<K, V, S> ShardData<K, V, S> {
    pub(crate) fn new(map: HashMap<K, V, S>, mode: SnapshotMode) -> Self {
        if mode == SnapshotMode::Cow {
            Self::Shared(Arc::new(map))
        } else {
            Self::Owned(map)
        }
    }

    pub(crate) fn as_map(&self) -> &HashMap<K, V, S> {
        match self {
            Self::Owned(map) => map,
            Self::Shared(map) => map,
        }
    }
}

impl<K: Clone, V: Clone, S: Clone> ShardData<K, V, S> {
    /// Share Cow storage; materialize an independent map for other modes.
    pub(crate) fn snapshot(&self) -> Arc<HashMap<K, V, S>> {
        match self {
            Self::Owned(map) => Arc::new(map.clone()),
            Self::Shared(map) => map.clone(),
        }
    }
}

impl<K, V, S> Deref for ShardData<K, V, S> {
    type Target = HashMap<K, V, S>;

    fn deref(&self) -> &Self::Target {
        self.as_map()
    }
}

impl<K: Clone, V: Clone, S: Clone> DerefMut for ShardData<K, V, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Owned(map) => map,
            Self::Shared(map) => Arc::make_mut(map),
        }
    }
}

pub(crate) type StdShardMap<K, V, S> = ShardData<K, V, S>;
pub(crate) type StdShard<K, V, S> = Arc<StdRwLock<StdShardMap<K, V, S>>>;
pub(crate) type StdShardVec<K, V, S> = Vec<Option<StdShard<K, V, S>>>;
pub(crate) type StdShardVecArc<K, V, S> = Arc<StdRwLock<StdShardVec<K, V, S>>>;

#[cfg(feature = "async")]
pub(crate) type AsyncShardMap<K, V, S> = ShardData<K, V, S>;
#[cfg(feature = "async")]
pub(crate) type AsyncShard<K, V, S> = Arc<TokioRwLock<AsyncShardMap<K, V, S>>>;
#[cfg(feature = "async")]
pub(crate) type AsyncShardVec<K, V, S> = Vec<Option<AsyncShard<K, V, S>>>;
#[cfg(feature = "async")]
pub(crate) type AsyncShardVecArc<K, V, S> = Arc<TokioRwLock<AsyncShardVec<K, V, S>>>;

/// Type alias for replica list to reduce type complexity
#[cfg(all(feature = "async", feature = "advanced"))]
pub(crate) type ReplicaList<K, V> = Arc<StdRwLock<Vec<Arc<dyn advanced::Replica<K, V>>>>>;
