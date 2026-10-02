#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![doc = r#"
Starshard: a high-performance, lazily sharded concurrent HashMap.

Features
---------
- `async`: enables `AsyncShardedHashMap` backed by `tokio::sync::RwLock`.
- `rayon`: enables parallel large synchronous snapshots and optional iterator-builder filtering.
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
5. Async version mirrors sync semantics with cancellation-safe mutation accounting.
6. Predictable memory layout leveraging `hashbrown::HashMap` and user-supplied hasher.

Consistency Model
------------------
- Per-shard operations are linearizable with respect to that shard.
- Global iteration and shared snapshots pin routing exclusively while collecting data.
  Snapshot construction pauses map operations and includes both migration generations.
- `len()` reflects committed length changes. In-flight mutations may not yet be reflected;
  after all operations complete it matches the number of stored logical keys.

Thread / Task Safety
---------------------
- Each shard guarded by a single RwLock (Std or Tokio).
- A routing guard pins the shard directory for each operation.
- Multiple shard locks follow active-before-previous generation order, with ascending
  indices for transactions. Callbacks must not reenter the map.
- Atomic length update only after a structural insert/delete succeeds.
- `Clone` bounds on `K`,`V` needed for iteration snapshot flattening.

Performance Notes (Indicative, not guaranteed)
-----------------------------------------------
- Read-heavy sync workloads: sharding reduces write interference vs a single map + RwLock.
- Large synchronous scans may use Rayon; gains depend on workload and need measurement.
- Lazy shards allocate storage only for initialized slots; read misses do not initialize them.

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
- Entry/time-bounded migration and snapshot construction with shorter pauses.
- Cached and Cow modes currently share lazy immutable snapshots, not per-shard COW storage.

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
```ignore
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
```ignore
#[cfg(all(feature="async", feature="rayon"))]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let m: AsyncShardedHashMap<u32, u32> = AsyncShardedHashMap::new(64);
    for i in 0..1000 { m.insert(i, i*i).await; }
    let items = m.iter().await; // clones each value once without synchronously waiting for Rayon
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
    Arc, RwLock as StdRwLock, RwLockReadGuard as StdReadGuard, RwLockWriteGuard as StdWriteGuard,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

#[cfg(feature = "async")]
use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock};

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
type SnapshotCache<K, V> = Arc<StdRwLock<Option<(u64, Arc<Vec<(K, V)>>)>>>;

#[cfg(feature = "async")]
pub(crate) use crate::core::AsyncShardVecArc;

#[cfg(all(feature = "async", feature = "advanced"))]
pub(crate) use crate::core::ReplicaList;

/* ======================== Extracted Modules ======================== */

mod error;
mod rebalance;
mod types;

pub use error::ShardCountError;
pub(crate) use rebalance::RebalanceTracker;
pub use rebalance::{RebalanceOptions, RebalanceReport, RebalanceStatus};
pub use types::{DEFAULT_SHARDS, MAX_SHARDS, ShardStats, SnapshotMode};

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
    routing_lock: Arc<StdRwLock<()>>,
    previous_shards: Arc<StdRwLock<Option<crate::core::StdShardVec<K, V, S>>>>,
    hasher: S,
    shard_count: Arc<AtomicUsize>,
    previous_shard_count: Arc<AtomicUsize>,
    total_len: Arc<AtomicUsize>,
    write_epoch: Arc<AtomicU64>,
    snapshot_cache: SnapshotCache<K, V>,
    rebalance_tracker: Arc<RebalanceTracker>,
    #[cfg(feature = "advanced")]
    profiling_enabled: Arc<std::sync::atomic::AtomicBool>,
}

mod core;
mod entry;

pub use entry::{Entry, OccupiedEntry, VacantEntry};

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
    routing_lock: Arc<TokioRwLock<()>>,
    previous_shards: Arc<TokioRwLock<Option<crate::core::AsyncShardVec<K, V, S>>>>,
    hasher: S,
    shard_count: Arc<AtomicUsize>,
    previous_shard_count: Arc<AtomicUsize>,
    total_len: Arc<AtomicUsize>,
    write_epoch: Arc<AtomicU64>,
    snapshot_cache: SnapshotCache<K, V>,
    rebalance_lock: Arc<TokioMutex<()>>,
    rebalance_tracker: Arc<RebalanceTracker>,
    #[cfg(feature = "advanced")]
    profiling_enabled: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "advanced")]
    replicas: ReplicaList<K, V>,
    #[cfg(feature = "advanced")]
    replication_state: Arc<crate::core::replication::ReplicationState>,
    #[cfg(feature = "advanced")]
    quorum_config: Arc<StdRwLock<Option<QuorumConfig>>>,
}

#[cfg(all(feature = "async", feature = "serde"))]
pub use serde::AsyncShardedHashMapSnapshot;

#[cfg(feature = "async")]
mod async_entry;

#[cfg(feature = "async")]
pub use async_entry::{AsyncEntry, AsyncOccupiedEntry, AsyncVacantEntry};

/* ================================ Tests ================================ */

#[cfg(test)]
mod tests;
