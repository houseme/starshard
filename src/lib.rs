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
- No dynamic shard rebalancing / rehash across shards yet.
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
    Arc, RwLock as StdRwLock, RwLockReadGuard as StdReadGuard, RwLockWriteGuard as StdWriteGuard,
    atomic::{AtomicUsize, Ordering},
};

#[cfg(feature = "async")]
use tokio::sync::{RwLock as TokioRwLock, RwLockWriteGuard as TokioWriteGuard};

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

/* ======================== Serde Imports ======================== */
#[cfg(feature = "serde")]
use serde::Serialize;

/* -------------------------------------------------------------------------- */
/* Internal Type Aliases (reduce visible complexity & silence Clippy)         */
/* -------------------------------------------------------------------------- */

type StdShardMap<K, V, S> = HashMap<K, V, S>;
type StdShard<K, V, S> = Arc<StdRwLock<StdShardMap<K, V, S>>>;
type StdShardVec<K, V, S> = Vec<Option<StdShard<K, V, S>>>;
type StdShardVecArc<K, V, S> = Arc<StdRwLock<StdShardVec<K, V, S>>>;

#[cfg(feature = "async")]
type AsyncShardMap<K, V, S> = HashMap<K, V, S>;
#[cfg(feature = "async")]
type AsyncShard<K, V, S> = Arc<TokioRwLock<AsyncShardMap<K, V, S>>>;
#[cfg(feature = "async")]
type AsyncShardVec<K, V, S> = Vec<Option<AsyncShard<K, V, S>>>;
#[cfg(feature = "async")]
type AsyncShardVecArc<K, V, S> = Arc<TokioRwLock<AsyncShardVec<K, V, S>>>;

/// Type alias for replica list to reduce type complexity
#[cfg(all(feature = "async", feature = "advanced"))]
type ReplicaList<K, V> = Arc<StdRwLock<Vec<Arc<dyn advanced::Replica<K, V>>>>>;

/// Default shard count (power-of-two not required; hashing modulo used).
pub const DEFAULT_SHARDS: usize = 64;

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

/* -------------------------------------------------------------------------- */
/* Sync Lock Helpers (poisoned lock fallback + logging)                       */
/* -------------------------------------------------------------------------- */

#[inline]
fn std_read_guard<'a, T>(lock: &'a StdRwLock<T>, context: &'static str) -> StdReadGuard<'a, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(context = %context, "std rwlock poisoned (read)");
            poisoned.into_inner()
        }
    }
}

#[inline]
fn std_write_guard<'a, T>(lock: &'a StdRwLock<T>, context: &'static str) -> StdWriteGuard<'a, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(context = %context, "std rwlock poisoned (write)");
            poisoned.into_inner()
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
    hasher: S,
    shard_count: usize,
    total_len: Arc<AtomicUsize>,
    #[cfg(feature = "advanced")]
    version: Arc<AtomicUsize>,
    #[cfg(feature = "advanced")]
    profiling_enabled: Arc<std::sync::atomic::AtomicBool>,
}

mod core;

/* ---------------------- Serde for ShardedHashMap ------------------------ */

#[cfg(feature = "serde")]
mod serde_impl;

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
    hasher: S,
    shard_count: usize,
    total_len: Arc<AtomicUsize>,
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
pub use core::async_impl::AsyncShardedHashMapSnapshot;

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
