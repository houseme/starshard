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
- Supports stop-the-world shard rebalancing via `rebalance_to(...)`; online incremental migration is not implemented yet.
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
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::sync::{
    Arc, Mutex as StdMutex, RwLock as StdRwLock, RwLockReadGuard as StdReadGuard,
    RwLockWriteGuard as StdWriteGuard,
    atomic::{AtomicUsize, Ordering},
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

#[cfg(feature = "async")]
pub(crate) use crate::core::AsyncShardVecArc;

#[cfg(all(feature = "async", feature = "advanced"))]
pub(crate) use crate::core::ReplicaList;

/// Default shard count (power-of-two not required; hashing modulo used).
pub const DEFAULT_SHARDS: usize = 64;

/// Default hard cap for shard slots in infallible constructors.
///
/// This guards against accidental or attacker-influenced oversized allocations.
/// If you need a different cap, use `with_shards_and_hasher_capped(...)` or
/// `try_with_shards_and_hasher_capped(...)`.
pub const MAX_SHARDS: usize = 262_144;

/// Error returned by strict shard-count constructors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardCountError {
    requested: usize,
    max_allowed: usize,
}

impl ShardCountError {
    /// Returns the requested effective shard count (after zero-normalization).
    pub fn requested(&self) -> usize {
        self.requested
    }

    /// Returns the maximum allowed shard count used for validation.
    pub fn max_allowed(&self) -> usize {
        self.max_allowed
    }
}

impl fmt::Display for ShardCountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requested shard_count {} exceeds max allowed {}",
            self.requested, self.max_allowed
        )
    }
}

impl std::error::Error for ShardCountError {}

/// Statistics about shard distribution and utilization.
///
/// Fields:
/// - `initialized`: number of shards that have been allocated
/// - `total`: total configured shard slots
/// - `empty`: number of initialized shards with zero entries
/// - `avg_load`: average load across initialized shards
/// - `max_load`: maximum load in any shard
#[derive(Clone, Debug)]
pub struct ShardStats {
    /// Number of shards that have been allocated.
    pub initialized: usize,
    /// Total configured shard slots.
    pub total: usize,
    /// Number of initialized shards with zero entries.
    pub empty: usize,
    /// Average load across initialized shards.
    pub avg_load: f64,
    /// Maximum load in any shard.
    pub max_load: usize,
}

impl ShardStats {
    /// Returns shard utilization as a percentage (0-100).
    ///
    /// # Returns
    /// - `f64`: percentage of shards that have been initialized
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn utilization_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.initialized as f64 / self.total as f64) * 100.0
        }
    }
}

/// Rebalance execution options.
#[derive(Clone, Debug)]
pub struct RebalanceOptions {
    /// Reserved for future online/background mode.
    pub background: bool,
    /// Reserved for future batched migration mode.
    pub batch_size: usize,
    /// Reserved for future pause-budget mode.
    pub max_pause_ns: u64,
}

impl Default for RebalanceOptions {
    fn default() -> Self {
        Self {
            background: false,
            batch_size: 1024,
            max_pause_ns: 0,
        }
    }
}

/// Rebalance execution report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebalanceReport {
    /// Previous shard slot count.
    pub from_shards: usize,
    /// New shard slot count.
    pub to_shards: usize,
    /// Number of moved entries.
    pub moved_entries: usize,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u128,
}

/// Rebalance runtime status snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct RebalanceStatus {
    /// Current state label: `idle` or `migrating`.
    pub state: &'static str,
    /// Progress in range `[0.0, 1.0]`.
    pub progress: f64,
    /// Number of completed shard migrations.
    pub moved_shards: usize,
    /// Number of total source shards for current migration.
    pub total_shards: usize,
}

const REBALANCE_STATE_IDLE: usize = 0;
const REBALANCE_STATE_MIGRATING: usize = 1;

#[derive(Debug)]
struct RebalanceTracker {
    state: AtomicUsize,
    moved_shards: AtomicUsize,
    total_shards: AtomicUsize,
}

impl RebalanceTracker {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(REBALANCE_STATE_IDLE),
            moved_shards: AtomicUsize::new(0),
            total_shards: AtomicUsize::new(0),
        }
    }

    fn begin(&self, total_shards: usize) {
        self.total_shards.store(total_shards, Ordering::Relaxed);
        self.moved_shards.store(0, Ordering::Relaxed);
        self.state.store(REBALANCE_STATE_MIGRATING, Ordering::Relaxed);
    }

    fn step(&self) {
        self.moved_shards.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(&self) {
        self.state.store(REBALANCE_STATE_IDLE, Ordering::Relaxed);
        self.total_shards.store(0, Ordering::Relaxed);
        self.moved_shards.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> RebalanceStatus {
        let state_num = self.state.load(Ordering::Relaxed);
        let moved = self.moved_shards.load(Ordering::Relaxed);
        let total = self.total_shards.load(Ordering::Relaxed);
        let progress = if total == 0 {
            0.0
        } else {
            moved as f64 / total as f64
        };
        RebalanceStatus {
            state: if state_num == REBALANCE_STATE_MIGRATING {
                "migrating"
            } else {
                "idle"
            },
            progress,
            moved_shards: moved,
            total_shards: total,
        }
    }
}

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
    shards: StdShardVecArc<K, V, S>,
    previous_shards: Arc<StdRwLock<Option<crate::core::StdShardVec<K, V, S>>>>,
    hasher: S,
    shard_count: Arc<AtomicUsize>,
    previous_shard_count: Arc<AtomicUsize>,
    total_len: Arc<AtomicUsize>,
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
    shards: AsyncShardVecArc<K, V, S>,
    previous_shards: Arc<TokioRwLock<Option<crate::core::AsyncShardVec<K, V, S>>>>,
    hasher: S,
    shard_count: Arc<AtomicUsize>,
    previous_shard_count: Arc<AtomicUsize>,
    total_len: Arc<AtomicUsize>,
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
mod tests {
    use super::*;

    #[test]
    fn sync_basic() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        assert!(m.insert("a".into(), 1).is_none());
        assert_eq!(m.get(&"a".into()), Some(1));
        assert_eq!(m.len(), 1);
        assert_eq!(m.remove(&"a".into()), Some(1));
        assert!(m.is_empty());
    }

    #[test]
    fn sync_constructor_clamps_to_default_cap() {
        let m: ShardedHashMap<String, i32> =
            ShardedHashMap::with_shards_and_hasher(usize::MAX, FxBuildHasher);
        assert_eq!(m.shard_count(), MAX_SHARDS);
    }

    #[test]
    fn sync_constructor_supports_custom_cap() {
        let m: ShardedHashMap<String, i32> =
            ShardedHashMap::with_shards_and_hasher_capped(10_000, FxBuildHasher, 128);
        assert_eq!(m.shard_count(), 128);
    }

    #[test]
    fn sync_try_constructor_rejects_oversized_shards() {
        let err = match ShardedHashMap::<String, i32>::try_with_shards_and_hasher(
            MAX_SHARDS + 1,
            FxBuildHasher,
        ) {
            Ok(_) => panic!("expected oversized shard_count to return error"),
            Err(err) => err,
        };
        assert_eq!(err.requested(), MAX_SHARDS + 1);
        assert_eq!(err.max_allowed(), MAX_SHARDS);
    }

    #[test]
    fn sync_try_constructor_custom_cap_success_and_rejection() {
        let ok = ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            32,
            FxBuildHasher,
            64,
        )
        .expect("expected shard_count within custom cap to succeed");
        assert_eq!(ok.shard_count(), 32);

        let err = match ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            65,
            FxBuildHasher,
            64,
        ) {
            Ok(_) => panic!("expected shard_count above custom cap to fail"),
            Err(err) => err,
        };
        assert_eq!(err.requested(), 65);
        assert_eq!(err.max_allowed(), 64);
    }

    #[test]
    fn sync_try_constructor_custom_cap_zero_normalizes_to_one() {
        let ok = ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            1,
            FxBuildHasher,
            0,
        )
        .expect("expected shard_count=1 to pass when cap=0 normalizes to 1");
        assert_eq!(ok.shard_count(), 1);

        let err = match ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            2,
            FxBuildHasher,
            0,
        ) {
            Ok(_) => panic!("expected shard_count=2 to fail when cap=0 normalizes to 1"),
            Err(err) => err,
        };
        assert_eq!(err.requested(), 2);
        assert_eq!(err.max_allowed(), 1);
    }

    #[test]
    fn sync_contains() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        assert!(!m.contains(&"a".into()));
        m.insert("a".into(), 1);
        assert!(m.contains(&"a".into()));
        m.insert("b".into(), 2);
        assert!(m.contains(&"b".into()));
        m.remove(&"a".into());
        assert!(!m.contains(&"a".into()));
        assert!(m.contains(&"b".into()));
    }

    #[test]
    fn sync_iteration() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("x".into(), 10);
        m.insert("y".into(), 20);
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn sync_rebalance_stop_the_world() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        for i in 0..200 {
            m.insert(format!("k{i}"), i);
        }
        let before_len = m.len();
        let report = m
            .rebalance_to(32, RebalanceOptions::default())
            .expect("sync rebalance should succeed");
        assert_eq!(report.from_shards, 4);
        assert_eq!(report.to_shards, 32);
        assert_eq!(report.moved_entries, before_len);
        assert_eq!(m.shard_count(), 32);
        assert_eq!(m.len(), before_len);
        for i in 0..200 {
            assert_eq!(m.get(&format!("k{i}")), Some(i));
        }
    }

    #[test]
    fn sync_rebalance_rejects_oversized_target() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        let err = m
            .rebalance_to(MAX_SHARDS + 1, RebalanceOptions::default())
            .expect_err("oversized rebalance target should fail");
        assert_eq!(err.requested(), MAX_SHARDS + 1);
        assert_eq!(err.max_allowed(), MAX_SHARDS);
    }

    #[test]
    fn sync_rebalance_status_is_idle_after_rebalance() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("k".into(), 1);
        let status_before = m.rebalance_status();
        assert_eq!(status_before.state, "idle");
        m.rebalance_to(8, RebalanceOptions::default())
            .expect("rebalance should succeed");
        let status_after = m.rebalance_status();
        assert_eq!(status_after.state, "idle");
        assert_eq!(status_after.total_shards, 0);
        assert_eq!(status_after.moved_shards, 0);
    }

    #[test]
    fn sync_online_rebalance_incremental_migration() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        for i in 0..120 {
            m.insert(format!("k{i}"), i);
        }
        assert_eq!(m.len(), 120);
        m.start_rebalance_online(16)
            .expect("online rebalance start should succeed");
        assert_eq!(m.rebalance_status().state, "migrating");
        assert_eq!(m.get(&"k3".to_string()), Some(3));

        assert_eq!(m.remove(&"k42".to_string()), Some(42));
        assert_eq!(m.insert("hot".to_string(), 777), None);

        while m.rebalance_status().state == "migrating" {
            let advanced = m.advance_rebalance(2);
            assert!(advanced <= 2);
            if advanced == 0 {
                break;
            }
        }

        let status = m.rebalance_status();
        assert_eq!(status.state, "idle");
        assert_eq!(m.shard_count(), 16);
        assert_eq!(m.get(&"k42".to_string()), None);
        assert_eq!(m.get(&"hot".to_string()), Some(777));
        assert_eq!(m.len(), 120);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_basic() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
        assert!(m.insert("a".into(), 1).await.is_none());
        assert_eq!(m.get(&"a".into()).await, Some(1));
        assert_eq!(m.len().await, 1);
        assert_eq!(m.remove(&"a".into()).await, Some(1));
        assert!(m.is_empty().await);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_constructor_clamps_to_default_cap() {
        let m: AsyncShardedHashMap<String, i32> =
            AsyncShardedHashMap::with_shards_and_hasher(usize::MAX, FxBuildHasher);
        assert_eq!(m.shard_count(), MAX_SHARDS);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_constructor_supports_custom_cap() {
        let m: AsyncShardedHashMap<String, i32> =
            AsyncShardedHashMap::with_shards_and_hasher_capped(10_000, FxBuildHasher, 256);
        assert_eq!(m.shard_count(), 256);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_try_constructor_rejects_oversized_shards() {
        let err = match AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher(
            MAX_SHARDS + 1,
            FxBuildHasher,
        ) {
            Ok(_) => panic!("expected oversized shard_count to return error"),
            Err(err) => err,
        };
        assert_eq!(err.requested(), MAX_SHARDS + 1);
        assert_eq!(err.max_allowed(), MAX_SHARDS);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_try_constructor_custom_cap_success_and_rejection() {
        let ok = AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            48,
            FxBuildHasher,
            64,
        )
        .expect("expected shard_count within custom cap to succeed");
        assert_eq!(ok.shard_count(), 48);

        let err = match AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            65,
            FxBuildHasher,
            64,
        ) {
            Ok(_) => panic!("expected shard_count above custom cap to fail"),
            Err(err) => err,
        };
        assert_eq!(err.requested(), 65);
        assert_eq!(err.max_allowed(), 64);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_try_constructor_custom_cap_zero_normalizes_to_one() {
        let ok = AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            1,
            FxBuildHasher,
            0,
        )
        .expect("expected shard_count=1 to pass when cap=0 normalizes to 1");
        assert_eq!(ok.shard_count(), 1);

        let err = match AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
            2,
            FxBuildHasher,
            0,
        ) {
            Ok(_) => panic!("expected shard_count=2 to fail when cap=0 normalizes to 1"),
            Err(err) => err,
        };
        assert_eq!(err.requested(), 2);
        assert_eq!(err.max_allowed(), 1);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_contains() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
        assert!(!m.contains(&"a".into()).await);
        m.insert("a".into(), 1).await;
        assert!(m.contains(&"a".into()).await);
        m.insert("b".into(), 2).await;
        assert!(m.contains(&"b".into()).await);
        m.remove(&"a".into()).await;
        assert!(!m.contains(&"a".into()).await);
        assert!(m.contains(&"b".into()).await);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_rebalance_stop_the_world() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        for i in 0..200 {
            m.insert(format!("k{i}"), i).await;
        }
        let before_len = m.len().await;
        let report = m
            .rebalance_to(64, RebalanceOptions::default())
            .await
            .expect("async rebalance should succeed");
        assert_eq!(report.from_shards, 4);
        assert_eq!(report.to_shards, 64);
        assert_eq!(report.moved_entries, before_len);
        assert_eq!(m.shard_count(), 64);
        assert_eq!(m.len().await, before_len);
        for i in 0..200 {
            assert_eq!(m.get(&format!("k{i}")).await, Some(i));
        }
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_rebalance_rejects_oversized_target() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        let err = m
            .rebalance_to(MAX_SHARDS + 1, RebalanceOptions::default())
            .await
            .expect_err("oversized rebalance target should fail");
        assert_eq!(err.requested(), MAX_SHARDS + 1);
        assert_eq!(err.max_allowed(), MAX_SHARDS);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_rebalance_status_is_idle_after_rebalance() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("k".into(), 1).await;
        let status_before = m.rebalance_status();
        assert_eq!(status_before.state, "idle");
        m.rebalance_to(16, RebalanceOptions::default())
            .await
            .expect("rebalance should succeed");
        let status_after = m.rebalance_status();
        assert_eq!(status_after.state, "idle");
        assert_eq!(status_after.total_shards, 0);
        assert_eq!(status_after.moved_shards, 0);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_online_rebalance_incremental_migration() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        for i in 0..120 {
            m.insert(format!("k{i}"), i).await;
        }
        assert_eq!(m.len().await, 120);
        m.start_rebalance_online(32)
            .await
            .expect("online rebalance start should succeed");
        assert_eq!(m.rebalance_status().state, "migrating");
        assert_eq!(m.get(&"k3".to_string()).await, Some(3));

        assert_eq!(m.remove(&"k42".to_string()).await, Some(42));
        assert_eq!(m.insert("hot".to_string(), 888).await, None);

        while m.rebalance_status().state == "migrating" {
            let advanced = m.advance_rebalance(2).await;
            assert!(advanced <= 2);
            if advanced == 0 {
                break;
            }
        }

        let status = m.rebalance_status();
        assert_eq!(status.state, "idle");
        assert_eq!(m.shard_count(), 32);
        assert_eq!(m.get(&"k42".to_string()).await, None);
        assert_eq!(m.get(&"hot".to_string()).await, Some(888));
        assert_eq!(m.len().await, 120);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trip() {
        use serde_json;
        let m: ShardedHashMap<String, u32> = ShardedHashMap::new(4);
        m.insert("x".into(), 10);
        m.insert("y".into(), 20);
        let s = serde_json::to_string(&m).unwrap();
        let de: ShardedHashMap<String, u32> = serde_json::from_str(&s).unwrap();
        assert_eq!(de.len(), 2);
        assert_eq!(de.get(&"x".into()), Some(10));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_deserialize_oversized_shard_count_is_clamped() {
        use serde_json;
        let json = format!(
            r#"{{"shard_count":{},"entries":[["k",1]]}}"#,
            MAX_SHARDS + 1024
        );
        let de: ShardedHashMap<String, u32> =
            serde_json::from_str(&json).expect("oversized shard_count JSON should deserialize");
        assert_eq!(de.shard_count(), MAX_SHARDS);
        assert_eq!(de.get(&"k".into()), Some(1));
    }

    #[cfg(all(feature = "async", feature = "serde"))]
    #[tokio::test]
    async fn async_snapshot_serialize() {
        use serde_json;
        let m = AsyncShardedHashMap::<u32, u32>::new(8);
        m.insert(1, 10).await;
        let snap = m.async_snapshot_serializable().await;
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("[[1,10]]"));
    }

    /* ==================== v0.8.0 Feature Tests ==================== */

    #[test]
    fn batch_insert_new_entries() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        let entries = vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)];
        let inserted = m.batch_insert(entries);
        assert_eq!(inserted, 3);
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&"b".into()), Some(2));
    }

    #[test]
    fn batch_insert_with_replacements() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 1);
        m.insert("b".into(), 2);

        let entries = vec![
            ("a".into(), 10), // replacement
            ("c".into(), 3),  // new
        ];
        let inserted = m.batch_insert(entries);
        assert_eq!(inserted, 1); // only new entries count
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&"a".into()), Some(10));
    }

    #[test]
    fn batch_remove() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 1);
        m.insert("b".into(), 2);
        m.insert("c".into(), 3);

        let keys = vec!["a".into(), "b".into(), "d".into()];
        let removed = m.batch_remove(keys);
        assert_eq!(removed, 2);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&"c".into()), Some(3));
    }

    #[test]
    fn batch_get() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 1);
        m.insert("c".into(), 3);

        let keys = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let results = m.batch_get(&keys);
        assert_eq!(results, vec![Some(1), None, Some(3)]);
    }

    #[test]
    fn compute_if_present_exists() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 10);

        let result = m.compute_if_present(&"a".into(), |v| Some(v * 2));
        assert_eq!(result, Some(20));
        assert_eq!(m.get(&"a".into()), Some(20));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn compute_if_present_absent() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        let result = m.compute_if_present(&"a".into(), |v| Some(v * 2));
        assert_eq!(result, None);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn compute_if_present_remove() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 10);
        m.insert("b".into(), 20);

        let result = m.compute_if_present(&"a".into(), |_v| None);
        assert_eq!(result, None);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&"b".into()), Some(20));
    }

    #[test]
    fn compute_if_absent_exists() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 10);

        let result = m.compute_if_absent("a".into(), || 20);
        assert_eq!(result, 10);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn compute_if_absent_inserts() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        let result = m.compute_if_absent("a".into(), || 20);
        assert_eq!(result, 20);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&"a".into()), Some(20));
    }

    #[test]
    fn retain_filter() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        for i in 0..10 {
            m.insert(format!("k{i}"), i);
        }
        assert_eq!(m.len(), 10);

        m.retain(|_, v| v % 2 == 0); // keep only even values
        assert_eq!(m.len(), 5);
        assert_eq!(m.get(&"k2".into()), Some(2));
        assert_eq!(m.get(&"k3".into()), None);
    }

    #[test]
    fn keys_iteration() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 1);
        m.insert("b".into(), 2);
        m.insert("c".into(), 3);

        let mut keys: Vec<_> = m.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn values_iteration() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 10);
        m.insert("b".into(), 20);
        m.insert("c".into(), 30);

        let mut values: Vec<_> = m.values().collect();
        values.sort();
        assert_eq!(values, vec![10, 20, 30]);
    }

    #[test]
    fn shard_stats_basic() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        for i in 0..10 {
            m.insert(format!("k{i}"), i);
        }

        let stats = m.shard_stats();
        assert_eq!(stats.total, 4);
        assert!(stats.initialized > 0);
        assert!(stats.max_load > 0);
        assert!(stats.avg_load > 0.0);
    }

    #[test]
    fn shard_stats_empty_count() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("k".into(), 1);
        m.remove(&"k".into());

        let stats = m.shard_stats();
        assert_eq!(stats.initialized, 1);
        assert_eq!(stats.empty, 1);
    }

    #[test]
    fn shard_utilization() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(16);
        m.insert("a".into(), 1);

        let util = m.shard_utilization();
        assert!(util > 0.0 && util <= 100.0);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_batch_insert() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        let entries = vec![("a".into(), 1), ("b".into(), 2)];
        let inserted = m.batch_insert(entries).await;
        assert_eq!(inserted, 2);
        assert_eq!(m.len().await, 2);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_batch_remove() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 1).await;
        m.insert("b".into(), 2).await;
        m.insert("c".into(), 3).await;

        let removed = m.batch_remove(vec!["a".into(), "b".into()]).await;
        assert_eq!(removed, 2);
        assert_eq!(m.len().await, 1);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_batch_get() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 1).await;
        m.insert("c".into(), 3).await;

        let keys = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let results = m.batch_get(&keys).await;
        assert_eq!(results, vec![Some(1), None, Some(3)]);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_compute_if_present() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 10).await;

        let result = m.compute_if_present(&"a".into(), |v| Some(v * 2)).await;
        assert_eq!(result, Some(20));
        assert_eq!(m.get(&"a".into()).await, Some(20));
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_compute_if_absent() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        let result = m.compute_if_absent("a".into(), || 20).await;
        assert_eq!(result, 20);
        assert_eq!(m.len().await, 1);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_retain() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        for i in 0..10 {
            m.insert(format!("k{i}"), i).await;
        }

        m.retain(|_, v| v % 2 == 0).await;
        assert_eq!(m.len().await, 5);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_keys() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 1).await;
        m.insert("b".into(), 2).await;

        let mut keys = m.keys().await;
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_values() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 1).await;
        m.insert("b".into(), 2).await;

        let mut values = m.values().await;
        values.sort();
        assert_eq!(values, vec![1, 2]);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_shard_stats() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        for i in 0..10 {
            m.insert(format!("k{i}"), i).await;
        }

        let stats = m.shard_stats().await;
        assert_eq!(stats.total, 4);
        assert!(stats.initialized > 0);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_shard_stats_empty_count() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("k".into(), 1).await;
        m.remove(&"k".into()).await;

        let stats = m.shard_stats().await;
        assert_eq!(stats.initialized, 1);
        assert_eq!(stats.empty, 1);
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn lifecycle_memory_stats_and_drain() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 1);
        m.insert("b".into(), 2);
        m.insert("c".into(), 3);

        let memory = m.memory_stats();
        assert!(memory.shards_allocated > 0);
        assert!(memory.total_capacity > 0);
        assert!(memory.load_factor > 0.0);

        let drained: Vec<_> = m.drain().collect();
        assert_eq!(drained.len(), 3);
        assert_eq!(m.len(), 0);
    }

    #[cfg(all(feature = "async", feature = "lifecycle"))]
    #[tokio::test]
    async fn async_lifecycle_memory_load_and_drain() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 1).await;
        m.insert("b".into(), 2).await;
        m.insert("c".into(), 3).await;

        let loads = m.per_shard_load().await;
        assert!(!loads.is_empty());

        let memory = m.memory_stats().await;
        assert!(memory.shards_allocated > 0);
        assert!(memory.total_capacity > 0);
        assert!(memory.load_factor > 0.0);

        let drained: Vec<_> = m.drain().await.collect();
        assert_eq!(drained.len(), 3);
        assert_eq!(m.len().await, 0);
    }
}
