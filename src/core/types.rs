//! Internal type aliases for shard storage.
//!
//! Defines the concrete container types used by both sync and async maps:
//! `StdShard`, `StdShardVec`, `StdShardVecArc` (and their `Async*` mirrors),
//! plus the `ReplicaList` alias used by the advanced replication feature.

use super::*;

pub(crate) type StdShardMap<K, V, S> = HashMap<K, V, S>;
pub(crate) type StdShard<K, V, S> = Arc<StdRwLock<StdShardMap<K, V, S>>>;
pub(crate) type StdShardVec<K, V, S> = Vec<Option<StdShard<K, V, S>>>;
pub(crate) type StdShardVecArc<K, V, S> = Arc<StdRwLock<StdShardVec<K, V, S>>>;

#[cfg(feature = "async")]
pub(crate) type AsyncShardMap<K, V, S> = HashMap<K, V, S>;
#[cfg(feature = "async")]
pub(crate) type AsyncShard<K, V, S> = Arc<TokioRwLock<AsyncShardMap<K, V, S>>>;
#[cfg(feature = "async")]
pub(crate) type AsyncShardVec<K, V, S> = Vec<Option<AsyncShard<K, V, S>>>;
#[cfg(feature = "async")]
pub(crate) type AsyncShardVecArc<K, V, S> = Arc<TokioRwLock<AsyncShardVec<K, V, S>>>;

/// Type alias for replica list to reduce type complexity
#[cfg(all(feature = "async", feature = "advanced"))]
pub(crate) type ReplicaList<K, V> = Arc<StdRwLock<Vec<Arc<dyn advanced::Replica<K, V>>>>>;
