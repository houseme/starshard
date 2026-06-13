#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![doc = r#"
Starshard: a high-performance, lazily sharded concurrent HashMap.

Features
---------
- `async`: enables `AsyncShardedHashMap` backed by `tokio::sync::RwLock`.
- `rayon`: enables parallel snapshot flattening inside iteration for large maps (sync + async).
- `serde`: (sync) serialize/deserialize via a stable map snapshot; async map via snapshot helper.

Serde Semantics
---------------
`ShardedHashMap`:
- Serialized form: { shard_count: usize, entries: Vec<(K,V)> }.
- Hasher state is *not* preserved; deserialization rebuilds with `S::default()`.
- Requires `K: Eq + Hash + Clone + Send + Sync + Serialize + Deserialize`, `V: Clone + Send + Sync + Serialize + Deserialize`,
  `S: BuildHasher + Clone + Send + Sync + Default`.

`AsyncShardedHashMap`:
- No direct `Serialize`/`Deserialize` (locks need `await`).
- Use `async_snapshot_serializable().await` to obtain a snapshot wrapper implementing `Serialize`.
- To rebuild: create a new async map, then bulk insert entries.

Design Goals
-------------
1. Minimize contention via sharding (coarse dynamic set of RwLocks).
2. Lazy shard materialization to reduce cold-start memory (slots are `None` until touched).
3. O(1) (amortized) length via atomic counter (fast cloning, no full scan).
4. Parallel iteration using `rayon` if enabled (snapshots each shard then flattens).
5. Async version mirrors sync semantics; attempts optimistic `try_read` first to reduce await points.
6. Predictable memory layout leveraging `hashbrown::HashMap` and user-supplied hasher.

Consistency Model
------------------
- Per-shard operations are linearizable with respect to that shard.
- Global iteration is *snapshot-per-shard* at the moment each shard lock is taken:
  You may see entries inserted/removed concurrently in other shards.
- `len()` is eventually consistent only in the trivial sense of atomic monotonic increments/decrements:
  It reflects completed inserts/removes; in-flight operations not yet applied are invisible.

Thread / Task Safety
---------------------
- Each shard guarded by a single RwLock (Std or Tokio).
- No nested acquisition of multiple shard locks (avoids lock order deadlocks).
- Atomic length update only after a structural insert/delete succeeds.
- `Clone` bounds on `K`,`V` needed for iteration snapshot flattening.

Performance Notes (Indicative, not guaranteed)
-----------------------------------------------
- Read-heavy sync workloads: sharding reduces write interference vs a single map + RwLock.
- `rayon` speeds large aggregate scans (e.g. metrics dump, checkpoint) 3-4x on >100k elements.
- Lazy shards: memory roughly proportional to number of distinct shard indices used.

Hasher Choice
--------------
- Default `FxBuildHasher`: speed oriented (non-cryptographic).
- For DoS / adversarial key defense use: `std::collections::hash_map::RandomState`.
  Example:
  ```
  use starshard::ShardedHashMap;
  use std::collections::hash_map::RandomState;
  let map: ShardedHashMap<String, u64, RandomState> =
      ShardedHashMap::with_shards_and_hasher(128, RandomState::default());
  ```

Limitations
------------
- Supports stop-the-world shard rebalancing via `rebalance_to(...)` and online incremental migration via
  `start_rebalance_online(...)` + `advance_rebalance(...)`.
- Lifecycle features currently provide introspection/utilities (e.g. per-shard load, drain, memory stats),
  but do not implement a built-in autonomous TTL eviction engine.
- Iteration allocates temporary vectors proportional to initialized shards (to snapshot).
- Not lock-free; large writer pressure can still cause convoying on hot shards.

Future Extension Ideas
-----------------------
- Optional background shard growth / rebalancing.
- Built-in configurable eviction scheduler integration (LRU per shard / clock / segmented queue).
- Metrics hooks (pre/post op).
- Batched mutation (multi-insert with single lock acquisition per target shard).
- Optional copy-on-write snapshots for near-zero iteration locking windows.

Examples
---------
Sync (default features):
```
use starshard::ShardedHashMap;
use rustc_hash::FxBuildHasher;

let map: ShardedHashMap<String, i32, FxBuildHasher> = ShardedHashMap::new(64);
map.insert("a".into(), 1);
assert_eq!(map.get(&"a".into()), Some(1));
assert_eq!(map.len(), 1);
```

Async (enable `async` feature):
```
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(64);
    map.insert("k".into(), 7).await;
    assert_eq!(map.get(&"k".into()).await, Some(7));
}
```

Parallel iteration (enable `rayon`):
```
use starshard::ShardedHashMap;
let map: ShardedHashMap<String, u32> = ShardedHashMap::new(32);
for i in 0..10_000 {
    map.insert(format!("k{i}"), i);
}
let count = map.iter().count(); // internally parallel if `rayon` feature active
assert_eq!(count, 10_000);
```

Async + Rayon (enable `async,rayon`):
```
#[cfg(all(feature="async", feature="rayon"))]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let m: AsyncShardedHashMap<u32, u32> = AsyncShardedHashMap::new(64);
    for i in 0..1000 { m.insert(i, i*i).await; }
    let items = m.iter().await; // flattens in parallel internally
    assert_eq!(items.len(), 1000);
}
```

Custom hasher (RandomState):
```
use starshard::ShardedHashMap;
use std::collections::hash_map::RandomState;
let secure: ShardedHashMap<String, i64, RandomState> =
    ShardedHashMap::with_shards_and_hasher(64, RandomState::default());
secure.insert("x".into(), 1);
```
"#]

use hashbrown::HashMap;
#[cfg(feature = "rayon")]
use rayon::prelude::*;
use rustc_hash::FxBuildHasher;
use std::hash::{BuildHasher, Hash};
use std::sync::{
    Arc, Mutex as StdMutex, RwLock as StdRwLock, RwLockReadGuard as StdReadGuard,
    RwLockWriteGuard as StdWriteGuard,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

#[cfg(feature = "async")]
use tokio::sync::{
    Mutex as TokioMutex, RwLock as TokioRwLock, RwLockWriteGuard as TokioWriteGuard,
};

/* ======================== Module Declarations ======================== */

/// Version 0.9.0 features: TTL, eviction, metrics, and advanced iteration.
#[cfg(feature = "lifecycle")]
pub mod eviction;

/// Version 1.0.0 features: Transactions, CAS, replication, and diagnostics.
#[cfg(feature = "advanced")]
pub mod advanced;

// Re-export key v0.9.0 types
#[cfg(feature = "lifecycle")]
pub use eviction::{
    AtomicMetrics, DrainIterator, EvictionConfig, EvictionPolicy, IterBuilder, MemoryStats,
    MetricsStats, PerShardLoad,
};

// Re-export key v1.0.0 types
#[cfg(feature = "advanced")]
pub use advanced::{
    CasResult, CowSnapshot, IsolatedSnapshot, LockProfile, QuorumConfig, Replica, ReplicaError,
    ReplicationOp, Transaction, TransactionResult, TxnOp,
};

pub(crate) use crate::core::StdShardVecArc;
type StdCowShard<K, V, S> = Arc<StdRwLock<Arc<HashMap<K, V, S>>>>;
type StdCowShardVecArc<K, V, S> = Arc<StdRwLock<Vec<Option<StdCowShard<K, V, S>>>>>;
type SnapshotCache<K, V> = Arc<StdRwLock<Option<Arc<Vec<(K, V)>>>>>;

#[cfg(feature = "async")]
pub(crate) use crate::core::AsyncShardVecArc;
#[cfg(feature = "async")]
type AsyncCowShard<K, V, S> = Arc<TokioRwLock<Arc<HashMap<K, V, S>>>>;
#[cfg(feature = "async")]
type AsyncCowShardVecArc<K, V, S> = Arc<TokioRwLock<Vec<Option<AsyncCowShard<K, V, S>>>>>;
#[cfg(feature = "async")]
type AsyncSnapshotCache<K, V> = Arc<TokioRwLock<Option<Arc<Vec<(K, V)>>>>>;

#[cfg(all(feature = "async", feature = "advanced"))]
pub(crate) use crate::core::ReplicaList;

/* ======================== Extracted Modules ======================== */

mod types;
mod error;
mod rebalance;

pub use types::{DEFAULT_SHARDS, MAX_SHARDS, ShardStats, SnapshotMode};
pub use error::ShardCountError;
pub use rebalance::{RebalanceOptions, RebalanceReport, RebalanceStatus};
pub(crate) use rebalance::RebalanceTracker;

/* ============================== Sync Map =============================== */

/// Sharded concurrent HashMap (synchronous).
///
/// Cloning the map is cheap (`Arc` handles + atomic length).
#[derive(Clone)]
pub struct ShardedHashMap<K, V, S = FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    snapshot_mode: SnapshotMode,
    shards: StdShardVecArc<K, V, S>,
    cow_shards: StdCowShardVecArc<K, V, S>,
    previous_shards: Arc<StdRwLock<Option<crate::core::StdShardVec<K, V, S>>>>,
    hasher: S,
    shard_count: Arc<AtomicUsize>,
    previous_shard_count: Arc<AtomicUsize>,
    total_len: Arc<AtomicUsize>,
    write_epoch: Arc<AtomicU64>,
    snapshot_cache: SnapshotCache<K, V>,
    snapshot_cache_epoch: Arc<AtomicU64>,
    rebalance_lock: Arc<StdMutex<()>>,
    rebalance_tracker: Arc<RebalanceTracker>,
    #[cfg(feature = "advanced")]
    version: Arc<AtomicUsize>,
    #[cfg(feature = "advanced")]
    profiling_enabled: Arc<std::sync::atomic::AtomicBool>,
}

mod core;

/* ---------------------- Serde for ShardedHashMap ------------------------ */

#[cfg(feature = "serde")]
mod serde;

/* ============================== Async Map =============================== */

/// Asynchronous sharded concurrent HashMap (Tokio `RwLock`).
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
#[cfg(feature = "async")]
#[derive(Clone)]
pub struct AsyncShardedHashMap<K, V, S = FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    snapshot_mode: SnapshotMode,
    shards: AsyncShardVecArc<K, V, S>,
    cow_shards: AsyncCowShardVecArc<K, V, S>,
    previous_shards: Arc<TokioRwLock<Option<crate::core::AsyncShardVec<K, V, S>>>>,
    hasher: S,
    shard_count: Arc<AtomicUsize>,
    previous_shard_count: Arc<AtomicUsize>,
    total_len: Arc<AtomicUsize>,
    write_epoch: Arc<AtomicU64>,
    snapshot_cache: AsyncSnapshotCache<K, V>,
    snapshot_cache_epoch: Arc<AtomicU64>,
    rebalance_lock: Arc<TokioMutex<()>>,
    rebalance_tracker: Arc<RebalanceTracker>,
    #[cfg(feature = "advanced")]
    version: Arc<AtomicUsize>,
    #[cfg(feature = "advanced")]
    profiling_enabled: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "advanced")]
    replicas: ReplicaList<K, V>,
    #[cfg(feature = "advanced")]
    quorum_config: Arc<StdRwLock<Option<QuorumConfig>>>,
}

#[cfg(all(feature = "async", feature = "serde"))]
pub use serde::AsyncShardedHashMapSnapshot;

/* ================================ Tests ================================ */

#[cfg(test)]
mod tests;
