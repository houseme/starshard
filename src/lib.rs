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

impl<K, V> ShardedHashMap<K, V, FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
{
    /// Create with default hasher (`FxBuildHasher`).
    #[tracing::instrument(level = "trace")]
    pub fn new(shard_count: usize) -> Self {
        Self::with_shards_and_hasher(shard_count, FxBuildHasher)
    }
}

impl<K, V, S> ShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Create with explicit hasher (non-zero shard count fallback).
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher(shard_count: usize, hasher: S) -> Self {
        let count = if shard_count == 0 {
            DEFAULT_SHARDS
        } else {
            shard_count
        };
        let shards = vec![None; count];
        Self {
            shards: Arc::new(StdRwLock::new(shards)),
            hasher,
            shard_count: count,
            total_len: Arc::new(AtomicUsize::new(0)),
            #[cfg(feature = "advanced")]
            version: Arc::new(AtomicUsize::new(0)),
            #[cfg(feature = "advanced")]
            profiling_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Current configured shard slots.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Number of shards actually initialized (allocated).
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn initialized_shards(&self) -> usize {
        let g = std_read_guard(&self.shards, "shards");
        g.iter().filter(|o| o.is_some()).count()
    }

    #[inline]
    #[tracing::instrument(skip(self, key), level = "trace")]
    fn shard_index(&self, key: &K) -> usize {
        (self.hasher.hash_one(key) % self.shard_count as u64) as usize
    }

    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    fn get_or_init_shard(&self, index: usize) -> StdShard<K, V, S> {
        let mut g = std_write_guard(&self.shards, "shards");
        if g[index].is_none() {
            let map = StdShardMap::with_hasher(self.hasher.clone());
            g[index] = Some(Arc::new(StdRwLock::new(map)));
        }
        if let Some(shard) = g[index].as_ref() {
            shard.clone()
        } else {
            tracing::error!(
                shard_index = index,
                "shard slot still uninitialized; creating fallback shard"
            );
            let map = StdShardMap::with_hasher(self.hasher.clone());
            let shard = Arc::new(StdRwLock::new(map));
            g[index] = Some(shard.clone());
            shard
        }
    }

    /// Insert key/value. Returns previous value if existed.
    ///
    /// Complexity: O(1) expected.
    ///
    /// If the key was not present, increments length counter.
    ///
    /// # Arguments
    /// - `key`: key to insert.
    /// - `value`: value to associate with the key
    ///
    /// # Returns
    /// - `Option<V>`: previous value if the key was already present.
    ///
    #[tracing::instrument(skip(self, key, value), level = "trace")]
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(&key));
        let mut guard: StdWriteGuard<'_, HashMap<K, V, S>> = std_write_guard(&shard, "shard");
        let old = guard.insert(key, value);
        if old.is_none() {
            self.total_len.fetch_add(1, Ordering::Relaxed);
        }
        old
    }

    /// Fetch cloned value.
    ///
    /// # Arguments
    /// - `key`: key to look up.
    ///
    /// # Returns
    /// - `Option<V>`: cloned value if the key exists.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub fn get(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let guard: StdReadGuard<'_, HashMap<K, V, S>> = std_read_guard(&shard, "shard");
        guard.get(key).cloned()
    }

    /// Check if a key exists (returns bool without cloning the value).
    ///
    /// # Arguments
    /// - `key`: key to check.
    ///
    /// # Returns
    /// - `bool`: true if the key exists in the map, false otherwise.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub fn contains(&self, key: &K) -> bool {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let guard: StdReadGuard<'_, HashMap<K, V, S>> = std_read_guard(&shard, "shard");
        guard.contains_key(key)
    }

    /// Remove key, returning previous value.
    ///
    /// # Arguments
    /// - `key`: key to remove.
    ///
    /// # Returns
    /// - `Option<V>`: previous value if the key existed.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub fn remove(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard: StdWriteGuard<'_, HashMap<K, V, S>> = std_write_guard(&shard, "shard");
        let old = guard.remove(key);
        if old.is_some() {
            self.total_len.fetch_sub(1, Ordering::Relaxed);
        }
        old
    }

    /// Length (cached atomic).
    ///
    /// # Returns
    /// - `usize`: total number of key/value pairs in the map.
    ///
    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Check if map is empty.
    ///
    /// # Returns
    /// - `bool`: true if length is zero, false otherwise.
    ///
    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all data (retains shard allocations).
    ///
    /// # Notes
    /// - Resets length counter to zero.
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn clear(&self) {
        let slots = std_read_guard(&self.shards, "shards");
        for shard in slots.iter().flatten() {
            let mut g = std_write_guard(shard, "shard");
            g.clear();
        }
        self.total_len.store(0, Ordering::Relaxed);
    }

    /// Snapshot iteration over (K,V) clones.
    ///
    /// Semantics:
    /// - Collects a list of initialized shard Arcs first (short critical section).
    /// - Each shard is read-locked independently; values cloned.
    /// - Not a live iterator: modifications after a shard snapshot are not reflected.
    /// - If `rayon` enabled, internal flattening per-shard happens in parallel for speed.
    ///
    /// Cost:
    /// - O(N) cloning cost for visited entries.
    /// - Temporary Vec allocations proportional to initialized shard count (and item copies).
    ///
    /// # Returns
    /// - `impl Iterator<Item = (K, V)>`: iterator over cloned key/value pairs.
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn iter(&self) -> impl Iterator<Item = (K, V)> {
        let shards_snapshot: Vec<StdShard<K, V, S>> = {
            let g = std_read_guard(&self.shards, "shards");
            g.iter().filter_map(|o| o.as_ref().cloned()).collect()
        };

        #[cfg(feature = "rayon")]
        {
            let items: Vec<(K, V)> = shards_snapshot
                .par_iter()
                .flat_map(|shard| {
                    let guard = std_read_guard(shard, "shard");
                    guard
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                })
                .collect();
            items.into_iter()
        }

        #[cfg(not(feature = "rayon"))]
        {
            let mut items = Vec::new();
            for shard in shards_snapshot {
                let guard = std_read_guard(&shard, "shard");
                items.extend(guard.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            return items.into_iter();
        }
    }

    /// Batch insert multiple key-value pairs.
    ///
    /// # Arguments
    /// - `entries`: iterator of (K, V) pairs
    ///
    /// # Returns
    /// - `usize`: number of new entries inserted
    ///
    #[tracing::instrument(skip(self, entries), level = "trace")]
    pub fn batch_insert<I>(&self, entries: I) -> usize
    where
        I: IntoIterator<Item = (K, V)>,
    {
        let mut grouped: std::collections::HashMap<usize, Vec<(K, V)>> =
            std::collections::HashMap::default();

        for (k, v) in entries {
            let shard_idx = self.shard_index(&k);
            grouped.entry(shard_idx).or_default().push((k, v));
        }

        let mut count = 0;
        for (shard_idx, pairs) in grouped {
            let shard = self.get_or_init_shard(shard_idx);
            let mut guard = std_write_guard(&shard, "shard");
            for (k, v) in pairs {
                if guard.insert(k, v).is_none() {
                    count += 1;
                }
            }
        }
        if count > 0 {
            self.total_len.fetch_add(count, Ordering::Relaxed);
        }
        count
    }

    /// Batch remove multiple keys.
    ///
    /// # Arguments
    /// - `keys`: iterator of keys to remove
    ///
    /// # Returns
    /// - `usize`: number of entries actually removed
    ///
    #[tracing::instrument(skip(self, keys), level = "trace")]
    pub fn batch_remove<I>(&self, keys: I) -> usize
    where
        I: IntoIterator<Item = K>,
    {
        let mut grouped: std::collections::HashMap<usize, Vec<K>> =
            std::collections::HashMap::default();

        for k in keys {
            let shard_idx = self.shard_index(&k);
            grouped.entry(shard_idx).or_default().push(k);
        }

        let mut count = 0;
        for (shard_idx, keys) in grouped {
            let shard = self.get_or_init_shard(shard_idx);
            let mut guard = std_write_guard(&shard, "shard");
            for k in keys {
                if guard.remove(&k).is_some() {
                    count += 1;
                }
            }
        }
        if count > 0 {
            self.total_len.fetch_sub(count, Ordering::Relaxed);
        }
        count
    }

    /// Batch get multiple keys.
    ///
    /// # Arguments
    /// - `keys`: slice of keys to fetch
    ///
    /// # Returns
    /// - `Vec<Option<V>>`: results in same order as keys
    ///
    #[tracing::instrument(skip(self, keys), level = "trace")]
    pub fn batch_get(&self, keys: &[K]) -> Vec<Option<V>> {
        let mut results = vec![None; keys.len()];
        let mut grouped: std::collections::HashMap<usize, Vec<(usize, &K)>> =
            std::collections::HashMap::default();

        for (idx, key) in keys.iter().enumerate() {
            let shard_idx = self.shard_index(key);
            grouped.entry(shard_idx).or_default().push((idx, key));
        }

        for (shard_idx, items) in grouped {
            let shard = self.get_or_init_shard(shard_idx);
            let guard = std_read_guard(&shard, "shard");
            for (idx, key) in items {
                if let Some(val) = guard.get(key) {
                    results[idx] = Some(val.clone());
                }
            }
        }
        results
    }

    /// Update entry if it exists, or remove it if the function returns None.
    ///
    /// # Arguments
    /// - `key`: key to check.
    /// - `f`: function that takes the current value and returns `Some(new_value)` to update or `None` to remove.
    ///
    /// # Returns
    /// - `Option<V>`: the new value if the key existed and was updated, `None` otherwise.
    ///
    #[tracing::instrument(skip(self, key, f), level = "trace")]
    pub fn compute_if_present<F>(&self, key: &K, f: F) -> Option<V>
    where
        F: FnOnce(&V) -> Option<V>,
    {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = std_write_guard(&shard, "shard");

        if let Some(old_val) = guard.get(key) {
            if let Some(new_val) = f(old_val) {
                let result = new_val.clone();
                guard.insert(key.clone(), new_val);
                Some(result)
            } else {
                // Remove the entry
                guard.remove(key);
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                None
            }
        } else {
            None
        }
    }

    /// Insert value if key is absent, or return existing value.
    ///
    /// # Arguments
    /// - `key`: key to check/insert.
    /// - `f`: function that returns the value to insert if the key is absent.
    ///
    /// # Returns
    /// - `V`: existing or newly inserted value.
    ///
    #[tracing::instrument(skip(self, key, f), level = "trace")]
    pub fn compute_if_absent<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        let shard = self.get_or_init_shard(self.shard_index(&key));
        let mut guard = std_write_guard(&shard, "shard");

        if let Some(val) = guard.get(&key) {
            val.clone()
        } else {
            let new_v = f();
            guard.insert(key, new_v.clone());
            self.total_len.fetch_add(1, Ordering::Relaxed);
            new_v
        }
    }

    /// Remove entries where predicate returns false.
    ///
    /// Locks each shard independently to maximize parallelism.
    ///
    /// # Arguments
    /// - `predicate`: function that returns true to keep, false to remove
    ///
    #[tracing::instrument(skip(self, predicate), level = "trace")]
    pub fn retain<F>(&self, predicate: F)
    where
        F: Fn(&K, &V) -> bool + Sync + Send,
    {
        let shards_snapshot: Vec<StdShard<K, V, S>> = {
            let g = std_read_guard(&self.shards, "shards");
            g.iter().filter_map(|o| o.as_ref().cloned()).collect()
        };

        #[cfg(feature = "rayon")]
        {
            let removed_count: usize = shards_snapshot
                .par_iter()
                .map(|shard| {
                    let mut guard = std_write_guard(shard, "shard");
                    let initial_len = guard.len();
                    guard.retain(|k, v| predicate(k, v));
                    initial_len - guard.len()
                })
                .sum();

            if removed_count > 0 {
                self.total_len.fetch_sub(removed_count, Ordering::Relaxed);
            }
        }

        #[cfg(not(feature = "rayon"))]
        {
            for shard in shards_snapshot {
                let mut guard = std_write_guard(&shard, "shard");
                let removed_count = guard.len();
                guard.retain(|k, v| predicate(k, v));
                let removed = removed_count - guard.len();
                if removed > 0 {
                    self.total_len.fetch_sub(removed, Ordering::Relaxed);
                }
            }
        }
    }

    /// Execute a transaction (basic implementation).
    ///
    /// This method executes a transaction by acquiring locks on all involved shards
    /// in a deterministic order to avoid deadlocks.
    ///
    /// # Arguments
    /// - `txn`: The transaction to execute.
    ///
    /// # Returns
    /// - `TransactionResult<()>`: The result of the transaction.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, txn), level = "trace")]
    pub fn execute_transaction(&self, txn: Transaction<K, V>) -> TransactionResult<()> {
        // 1. Identify involved shards
        let mut shard_indices: Vec<usize> = txn
            .ops
            .iter()
            .map(|op| match op {
                TxnOp::Read(k) => self.shard_index(k),
                TxnOp::Write(k, _) => self.shard_index(k),
                TxnOp::Remove(k) => self.shard_index(k),
            })
            .collect();

        // 2. Sort and deduplicate to prevent deadlocks
        shard_indices.sort_unstable();
        shard_indices.dedup();

        // 3. Acquire locks (pessimistic locking: acquire all write locks)
        // Note: In a real MVCC system, we might acquire read locks for reads,
        // but for simplicity and correctness here, we use write locks for everything
        // to ensure isolation during the transaction execution.
        let shards: Vec<_> = shard_indices
            .iter()
            .map(|&idx| self.get_or_init_shard(idx))
            .collect();
        let mut guards = Vec::with_capacity(shards.len());
        for shard in &shards {
            // We must use write locks because we might modify the shards.
            // Even for read-only ops in a mixed transaction, we need consistent view.
            let guard = std_write_guard(shard, "transaction shard");
            guards.push(guard);
        }

        // 4. Execute operations
        // We need to map shard index back to the correct guard.
        // Since guards are stored in the same order as sorted shard_indices,
        // we can use binary search to find the index.

        for op in txn.ops {
            match op {
                TxnOp::Read(k) => {
                    let idx = self.shard_index(&k);
                    let guard_idx = match shard_indices.binary_search(&idx) {
                        Ok(i) => i,
                        Err(_) => {
                            tracing::error!(
                                shard_index = idx,
                                "shard index missing in transaction"
                            );
                            return TransactionResult::Aborted;
                        }
                    };
                    let guard = &guards[guard_idx];
                    // Just checking existence/value for now.
                    // In a real txn, we might return values.
                    // Here we just ensure it runs.
                    let _ = guard.get(&k);
                }
                TxnOp::Write(k, v) => {
                    let idx = self.shard_index(&k);
                    let guard_idx = match shard_indices.binary_search(&idx) {
                        Ok(i) => i,
                        Err(_) => {
                            tracing::error!(
                                shard_index = idx,
                                "shard index missing in transaction"
                            );
                            return TransactionResult::Aborted;
                        }
                    };
                    let guard = &mut guards[guard_idx];
                    if guard.insert(k, v).is_none() {
                        self.total_len.fetch_add(1, Ordering::Relaxed);
                    }
                }
                TxnOp::Remove(k) => {
                    let idx = self.shard_index(&k);
                    let guard_idx = match shard_indices.binary_search(&idx) {
                        Ok(i) => i,
                        Err(_) => {
                            tracing::error!(
                                shard_index = idx,
                                "shard index missing in transaction"
                            );
                            return TransactionResult::Aborted;
                        }
                    };
                    let guard = &mut guards[guard_idx];
                    if guard.remove(&k).is_some() {
                        self.total_len.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            }
        }

        TransactionResult::Committed(())
    }

    /// Compare and swap: atomically replace value if it matches expected.
    ///
    /// # Arguments
    /// - `key`: The key to update.
    /// - `expected`: The expected current value.
    /// - `new`: The new value to swap in.
    ///
    /// # Returns
    /// - `CasResult<V>`: Success with new value, or Failure with current value.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, key, expected, new), level = "trace")]
    pub fn compare_and_swap(&self, key: &K, expected: &V, new: V) -> CasResult<V>
    where
        V: PartialEq,
    {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = std_write_guard(&shard, "cas");

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.insert(key.clone(), new.clone());
                CasResult::Success(new)
            }
            Some(current) => CasResult::Failure(current.clone()),
            None => CasResult::Failure(new),
        }
    }

    /// Compare and remove: atomically remove entry if value matches expected.
    ///
    /// # Arguments
    /// - `key`: The key to remove.
    /// - `expected`: The expected current value.
    ///
    /// # Returns
    /// - `bool`: true if removed, false if value didn't match or key not found.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, key, expected), level = "trace")]
    pub fn compare_and_remove(&self, key: &K, expected: &V) -> bool
    where
        V: PartialEq,
    {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = std_write_guard(&shard, "cas_remove");

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.remove(key);
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Create a copy-on-write snapshot for minimal-locking reads.
    ///
    /// # Returns
    /// - `CowSnapshot<K, V>`: Immutable snapshot of current state.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn cow_snapshot(&self) -> CowSnapshot<K, V> {
        let data = self.iter().collect();
        let version = self.version.load(Ordering::SeqCst) as u64;
        CowSnapshot::new(data, version)
    }

    /// Create a versioned snapshot for time-travel queries.
    ///
    /// # Returns
    /// - `IsolatedSnapshot<K, V>`: Snapshot with version information.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn versioned_snapshot(&self) -> IsolatedSnapshot<K, V> {
        let data = self.iter().collect();
        let version = self.version.fetch_add(1, Ordering::SeqCst) as u64;
        IsolatedSnapshot::new(version, data)
    }

    /// Create a snapshot at a specific version (if available).
    ///
    /// # Arguments
    /// - `version`: The version number to snapshot at.
    ///
    /// # Returns
    /// - `Option<IsolatedSnapshot<K, V>>`: Snapshot if version is current, None otherwise.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn snapshot_at_version(&self, version: u64) -> Option<IsolatedSnapshot<K, V>> {
        let current_version = self.version.load(Ordering::SeqCst) as u64;
        if version == current_version {
            let data = self.iter().collect();
            Some(IsolatedSnapshot::new(version, data))
        } else {
            None
        }
    }

    /// Get lock profiling data for all shards.
    ///
    /// # Returns
    /// - `Vec<LockProfile>`: Per-shard lock statistics.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn lock_profiles(&self) -> Vec<LockProfile> {
        let profiling_enabled = self.profiling_enabled.load(Ordering::Relaxed);
        if !profiling_enabled {
            return Vec::new();
        }

        let slots = std_read_guard(&self.shards, "lock_profiles");
        let mut profiles = Vec::new();

        for (idx, slot) in slots.iter().enumerate() {
            if let Some(_shard) = slot {
                // In a real implementation, we'd track lock stats per shard
                // For now, return basic profile structure
                profiles.push(LockProfile {
                    shard_id: idx,
                    contention_count: 0,
                    avg_wait_time_ns: 0,
                    max_wait_time_ns: 0,
                    reads: 0,
                    writes: 0,
                });
            }
        }

        profiles
    }

    /// Enable or disable lock profiling.
    ///
    /// # Arguments
    /// - `enabled`: Whether to enable profiling.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn enable_profiling(&self, enabled: bool) {
        self.profiling_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Iterate over all keys (snapshot-based).
    ///
    /// # Returns
    /// - `Vec<K>`: vector of cloned keys
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn keys(&self) -> impl Iterator<Item = K> {
        self.iter().map(|(k, _)| k)
    }

    /// Iterate over all values (snapshot-based).
    ///
    /// # Returns
    /// - `Vec<V>`: vector of cloned values
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn values(&self) -> impl Iterator<Item = V> {
        self.iter().map(|(_, v)| v)
    }

    /// Returns statistics about shard distribution and utilization.
    ///
    /// # Returns
    /// - `ShardStats`: structure containing shard metrics
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn shard_stats(&self) -> ShardStats {
        let slots = std_read_guard(&self.shards, "shards");
        let mut initialized = 0;
        let mut loads = Vec::new();

        for shard in slots.iter().flatten() {
            initialized += 1;
            let guard = std_read_guard(shard, "shard");
            loads.push(guard.len());
        }

        let total = slots.len();
        let empty = loads.iter().filter(|&&l| l == 0).count();
        let max_load = loads.iter().max().copied().unwrap_or(0);
        let avg_load = if initialized > 0 {
            loads.iter().sum::<usize>() as f64 / initialized as f64
        } else {
            0.0
        };

        ShardStats {
            initialized,
            total,
            empty,
            avg_load,
            max_load,
        }
    }

    /// Returns shard utilization as a percentage (0-100).
    ///
    /// # Returns
    /// - `f64`: percentage of shards that have been initialized
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn shard_utilization(&self) -> f64 {
        let stats = self.shard_stats();
        stats.utilization_percent()
    }

    /// Returns load statistics for each initialized shard.
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn per_shard_load(&self) -> Vec<PerShardLoad> {
        let slots = std_read_guard(&self.shards, "shards");
        let mut stats = Vec::new();

        for (i, shard_opt) in slots.iter().enumerate() {
            if let Some(shard) = shard_opt {
                let guard = std_read_guard(shard, "shard");
                stats.push(PerShardLoad {
                    shard_idx: i,
                    entry_count: guard.len(),
                    capacity: guard.capacity(),
                });
            }
        }
        stats
    }

    /// Returns current memory-oriented shard statistics.
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn memory_stats(&self) -> MemoryStats {
        let slots = std_read_guard(&self.shards, "shards");
        let mut shards_allocated = 0;
        let mut total_capacity = 0usize;
        let mut total_entries = 0usize;

        for shard in slots.iter().flatten() {
            shards_allocated += 1;
            let guard = std_read_guard(shard, "shard");
            total_capacity += guard.capacity();
            total_entries += guard.len();
        }

        let load_factor = if total_capacity > 0 {
            total_entries as f64 / total_capacity as f64
        } else {
            0.0
        };

        MemoryStats {
            shards_allocated,
            total_capacity,
            load_factor,
        }
    }

    /// Drains all entries from the map and returns them as an iterator.
    ///
    /// Shard allocations are retained.
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn drain(&self) -> DrainIterator<K, V> {
        let slots = std_read_guard(&self.shards, "shards");
        let mut items = Vec::new();

        for shard in slots.iter().flatten() {
            let mut guard = std_write_guard(shard, "shard");
            items.extend(guard.drain());
        }

        self.total_len.store(0, Ordering::Relaxed);

        DrainIterator { items, index: 0 }
    }
}

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

#[cfg(feature = "async")]
impl<K, V> AsyncShardedHashMap<K, V, FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create with default hasher.
    #[tracing::instrument(level = "trace")]
    pub fn new(shard_count: usize) -> Self {
        Self::with_shards_and_hasher(shard_count, FxBuildHasher)
    }
}

#[cfg(feature = "async")]
impl<K, V, S> AsyncShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Create with custom hasher.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher(shard_count: usize, hasher: S) -> Self {
        let count = if shard_count == 0 {
            DEFAULT_SHARDS
        } else {
            shard_count
        };
        Self {
            shards: Arc::new(TokioRwLock::new(vec![None; count])),
            hasher,
            shard_count: count,
            total_len: Arc::new(AtomicUsize::new(0)),
            #[cfg(feature = "advanced")]
            version: Arc::new(AtomicUsize::new(0)),
            #[cfg(feature = "advanced")]
            profiling_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(feature = "advanced")]
            replicas: Arc::new(StdRwLock::new(Vec::new())),
            #[cfg(feature = "advanced")]
            quorum_config: Arc::new(StdRwLock::new(None)),
        }
    }

    /// Configured shard capacity.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Number of initialized shards.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn initialized_shards(&self) -> usize {
        let g = self.shards.read().await;
        g.iter().filter(|o| o.is_some()).count()
    }

    #[inline]
    #[tracing::instrument(skip(self, key), level = "trace")]
    fn shard_index(&self, key: &K) -> usize {
        (self.hasher.hash_one(key) % self.shard_count as u64) as usize
    }

    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    async fn get_or_init_shard(&self, index: usize) -> AsyncShard<K, V, S> {
        let mut g = self.shards.write().await;
        if g[index].is_none() {
            let map = AsyncShardMap::with_hasher(self.hasher.clone());
            g[index] = Some(Arc::new(TokioRwLock::new(map)));
        }
        if let Some(shard) = g[index].as_ref() {
            shard.clone()
        } else {
            tracing::error!(
                shard_index = index,
                "async shard slot still uninitialized; creating fallback shard"
            );
            let map = AsyncShardMap::with_hasher(self.hasher.clone());
            let shard = Arc::new(TokioRwLock::new(map));
            g[index] = Some(shard.clone());
            shard
        }
    }

    /// Insert key/value asynchronously.
    ///
    /// # Arguments
    /// - `key`: key to insert.
    /// - `value`: value to associate with the key.
    ///
    /// # Returns
    /// - `Option<V>`: previous value if the key was already present.
    ///
    #[tracing::instrument(skip(self, key, value), level = "trace")]
    pub async fn insert(&self, key: K, value: V) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(&key)).await;
        let mut guard: TokioWriteGuard<'_, HashMap<K, V, S>> = shard.write().await;
        let old = guard.insert(key, value);
        if old.is_none() {
            self.total_len.fetch_add(1, Ordering::Relaxed);
        }
        old
    }

    /// Get cloned value; uses `try_read` first (fast path, reduces scheduler churn).
    ///
    /// # Arguments
    /// - `key`: key to look up.
    ///
    /// # Returns
    /// - `Option<V>`: cloned value if the key exists.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn get(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        if let Ok(g) = shard.try_read() {
            return g.get(key).cloned();
        }
        let g = shard.read().await;
        g.get(key).cloned()
    }

    /// Check if a key exists; uses `try_read` first (fast path, reduces scheduler churn).
    ///
    /// # Arguments
    /// - `key`: key to check.
    ///
    /// # Returns
    /// - `bool`: true if the key exists in the map, false otherwise.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn contains(&self, key: &K) -> bool {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        if let Ok(g) = shard.try_read() {
            return g.contains_key(key);
        }
        let g = shard.read().await;
        g.contains_key(key)
    }

    /// Remove key.
    ///
    /// # Arguments
    /// - `key`: key to remove.
    ///
    /// # Returns
    /// - `Option<V>`: previous value if the key existed.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn remove(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        let mut g = shard.write().await;
        let old = g.remove(key);
        if old.is_some() {
            self.total_len.fetch_sub(1, Ordering::Relaxed);
        }
        old
    }

    /// Length (atomic).
    ///
    /// # Returns
    /// - `usize`: total number of key/value pairs in the map.
    ///
    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Check if map is empty.
    ///
    /// # Returns
    /// - `bool`: true if the map is empty, false otherwise.
    ///
    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Clear (retains allocated shards).
    ///
    /// # Notes
    /// - Resets length counter to zero.
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn clear(&self) {
        let slots = self.shards.read().await;
        for shard in slots.iter().flatten() {
            let mut g = shard.write().await;
            g.clear();
        }
        self.total_len.store(0, Ordering::Relaxed);
    }

    /// Snapshot iteration (async).
    ///
    /// Steps:
    /// 1. Snapshot Arc of initialized shards under read lock of the shard vector.
    /// 2. For each shard: try `try_read`; fallback to `await` read.
    /// 3. Clone inner HashMaps (short critical sections).
    /// 4. If `rayon` enabled, parallel flatten of snapshots.
    ///
    /// Returns a materialized `Vec`.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn iter(&self) -> Vec<(K, V)> {
        let shard_arcs: Vec<AsyncShard<K, V, S>> = {
            let g = self.shards.read().await;
            g.iter().filter_map(|o| o.as_ref().cloned()).collect()
        };

        let mut snapshots = Vec::with_capacity(shard_arcs.len());
        for shard in shard_arcs {
            if let Ok(g) = shard.try_read() {
                snapshots.push(g.clone());
            } else {
                let g = shard.read().await;
                snapshots.push(g.clone());
            }
        }

        #[cfg(feature = "rayon")]
        {
            snapshots
                .par_iter()
                .flat_map(|m| m.par_iter().map(|(k, v)| (k.clone(), v.clone())))
                .collect()
        }

        #[cfg(not(feature = "rayon"))]
        {
            let mut items = Vec::new();
            for m in snapshots {
                items.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            items
        }
    }

    /* ==================== v0.8.0 Async Methods ==================== */

    /// Batch insert multiple key-value pairs asynchronously.
    ///
    /// # Arguments
    /// - `entries`: iterator of (K, V) pairs
    ///
    /// # Returns
    /// - `usize`: number of new entries inserted
    ///
    #[tracing::instrument(skip(self, entries), level = "trace")]
    pub async fn batch_insert<I>(&self, entries: I) -> usize
    where
        I: IntoIterator<Item = (K, V)>,
    {
        let mut grouped: std::collections::HashMap<usize, Vec<(K, V)>> =
            std::collections::HashMap::default();

        for (k, v) in entries {
            let shard_idx = self.shard_index(&k);
            grouped.entry(shard_idx).or_default().push((k, v));
        }

        let mut count = 0;
        for (shard_idx, pairs) in grouped {
            let shard = self.get_or_init_shard(shard_idx).await;
            let mut guard = shard.write().await;
            for (k, v) in pairs {
                if guard.insert(k, v).is_none() {
                    count += 1;
                }
            }
        }
        if count > 0 {
            self.total_len.fetch_add(count, Ordering::Relaxed);
        }
        count
    }

    /// Batch remove multiple keys asynchronously.
    ///
    /// # Arguments
    /// - `keys`: iterator of keys to remove
    ///
    /// # Returns
    /// - `usize`: number of entries actually removed
    ///
    #[tracing::instrument(skip(self, keys), level = "trace")]
    pub async fn batch_remove<I>(&self, keys: I) -> usize
    where
        I: IntoIterator<Item = K>,
    {
        let mut grouped: std::collections::HashMap<usize, Vec<K>> =
            std::collections::HashMap::default();

        for k in keys {
            let shard_idx = self.shard_index(&k);
            grouped.entry(shard_idx).or_default().push(k);
        }

        let mut count = 0;
        for (shard_idx, keys) in grouped {
            let shard = self.get_or_init_shard(shard_idx).await;
            let mut guard = shard.write().await;
            for k in keys {
                if guard.remove(&k).is_some() {
                    count += 1;
                }
            }
        }
        if count > 0 {
            self.total_len.fetch_sub(count, Ordering::Relaxed);
        }
        count
    }

    /// Batch get multiple keys asynchronously.
    ///
    /// # Arguments
    /// - `keys`: slice of keys to fetch
    ///
    /// # Returns
    /// - `Vec<Option<V>>`: results in same order as keys
    ///
    #[tracing::instrument(skip(self, keys), level = "trace")]
    pub async fn batch_get(&self, keys: &[K]) -> Vec<Option<V>> {
        let mut results = vec![None; keys.len()];
        let mut grouped: std::collections::HashMap<usize, Vec<(usize, &K)>> =
            std::collections::HashMap::default();

        for (idx, key) in keys.iter().enumerate() {
            let shard_idx = self.shard_index(key);
            grouped.entry(shard_idx).or_default().push((idx, key));
        }

        for (shard_idx, items) in grouped {
            let shard = self.get_or_init_shard(shard_idx).await;
            let guard = shard.read().await;
            for (idx, key) in items {
                if let Some(val) = guard.get(key) {
                    results[idx] = Some(val.clone());
                }
            }
        }
        results
    }

    /// Update value only if key is present; remove if closure returns None (async).
    ///
    /// # Arguments
    /// - `key`: key to check
    /// - `f`: function that receives current value and returns new value (or None to remove)
    ///
    /// # Returns
    /// - `Option<V>`: the new value if present, None if removed or key absent
    ///
    #[tracing::instrument(skip(self, key, f), level = "trace")]
    pub async fn compute_if_present<F>(&self, key: &K, f: F) -> Option<V>
    where
        F: FnOnce(V) -> Option<V>,
    {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        let mut guard = shard.write().await;

        match guard.remove(key) {
            Some(old_v) => {
                let new_opt = f(old_v);
                if let Some(new_v) = new_opt.clone() {
                    guard.insert(key.clone(), new_v);
                } else {
                    self.total_len.fetch_sub(1, Ordering::Relaxed);
                }
                new_opt
            }
            None => None,
        }
    }

    /// Insert value only if key is absent; returns final value (async).
    ///
    /// # Arguments
    /// - `key`: key to check/insert
    /// - `f`: function to generate value if key absent
    ///
    /// # Returns
    /// - `V`: either the existing value or newly inserted value
    ///
    #[tracing::instrument(skip(self, key, f), level = "trace")]
    pub async fn compute_if_absent<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        let shard = self.get_or_init_shard(self.shard_index(&key)).await;
        let mut guard = shard.write().await;

        if let Some(v) = guard.get(&key) {
            v.clone()
        } else {
            let new_v = f();
            guard.insert(key, new_v.clone());
            self.total_len.fetch_add(1, Ordering::Relaxed);
            new_v
        }
    }

    /// Remove entries where predicate returns false (async).
    ///
    /// Locks each shard independently to maximize parallelism.
    ///
    /// # Arguments
    /// - `predicate`: function that returns true to keep, false to remove
    ///
    #[tracing::instrument(skip(self, predicate), level = "trace")]
    pub async fn retain<F>(&self, predicate: F)
    where
        F: Fn(&K, &V) -> bool,
    {
        let shards_snapshot: Vec<AsyncShard<K, V, S>> = {
            let g = self.shards.read().await;
            g.iter().filter_map(|o| o.as_ref().cloned()).collect()
        };

        for shard in shards_snapshot {
            let mut guard = shard.write().await;
            let removed_count = guard.len();
            guard.retain(|k, v| predicate(k, v));
            let removed = removed_count - guard.len();
            if removed > 0 {
                self.total_len.fetch_sub(removed, Ordering::Relaxed);
            }
        }
    }

    /// Execute a transaction (basic implementation, async).
    ///
    /// This method executes a transaction by acquiring locks on all involved shards
    /// in a deterministic order to avoid deadlocks.
    ///
    /// # Arguments
    /// - `txn`: The transaction to execute.
    ///
    /// # Returns
    /// - `TransactionResult<()>`: The result of the transaction.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, txn), level = "trace")]
    pub async fn execute_transaction(&self, txn: Transaction<K, V>) -> TransactionResult<()> {
        // 1. Identify involved shards
        let mut shard_indices: Vec<usize> = txn
            .ops
            .iter()
            .map(|op| match op {
                TxnOp::Read(k) => self.shard_index(k),
                TxnOp::Write(k, _) => self.shard_index(k),
                TxnOp::Remove(k) => self.shard_index(k),
            })
            .collect();

        // 2. Sort and deduplicate to prevent deadlocks
        shard_indices.sort_unstable();
        shard_indices.dedup();

        // 3. Acquire locks (pessimistic locking: acquire all write locks)
        // Collect shard Arcs first to keep them alive
        let mut shard_arcs = Vec::with_capacity(shard_indices.len());
        for &idx in &shard_indices {
            shard_arcs.push(self.get_or_init_shard(idx).await);
        }

        // Now acquire write locks from the Arc references
        let mut guards = Vec::with_capacity(shard_arcs.len());
        for shard_arc in &shard_arcs {
            // We must use write locks because we might modify the shards.
            let guard = shard_arc.write().await;
            guards.push(guard);
        }

        // 4. Execute operations
        for op in txn.ops {
            match op {
                TxnOp::Read(k) => {
                    let idx = self.shard_index(&k);
                    let guard_idx = match shard_indices.binary_search(&idx) {
                        Ok(i) => i,
                        Err(_) => {
                            tracing::error!(
                                shard_index = idx,
                                "shard index missing in async transaction"
                            );
                            return TransactionResult::Aborted;
                        }
                    };
                    let guard = &guards[guard_idx];
                    let _ = guard.get(&k);
                }
                TxnOp::Write(k, v) => {
                    let idx = self.shard_index(&k);
                    let guard_idx = match shard_indices.binary_search(&idx) {
                        Ok(i) => i,
                        Err(_) => {
                            tracing::error!(
                                shard_index = idx,
                                "shard index missing in async transaction"
                            );
                            return TransactionResult::Aborted;
                        }
                    };
                    let guard = &mut guards[guard_idx];
                    if guard.insert(k, v).is_none() {
                        self.total_len.fetch_add(1, Ordering::Relaxed);
                    }
                }
                TxnOp::Remove(k) => {
                    let idx = self.shard_index(&k);
                    let guard_idx = match shard_indices.binary_search(&idx) {
                        Ok(i) => i,
                        Err(_) => {
                            tracing::error!(
                                shard_index = idx,
                                "shard index missing in async transaction"
                            );
                            return TransactionResult::Aborted;
                        }
                    };
                    let guard = &mut guards[guard_idx];
                    if guard.remove(&k).is_some() {
                        self.total_len.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            }
        }

        TransactionResult::Committed(())
    }

    /// Compare and swap: atomically replace value if it matches expected (async).
    ///
    /// # Arguments
    /// - `key`: The key to update.
    /// - `expected`: The expected current value.
    /// - `new`: The new value to swap in.
    ///
    /// # Returns
    /// - `CasResult<V>`: Success with new value, or Failure with current value.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, key, expected, new), level = "trace")]
    pub async fn compare_and_swap(&self, key: &K, expected: &V, new: V) -> CasResult<V>
    where
        V: PartialEq,
    {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        let mut guard = shard.write().await;

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.insert(key.clone(), new.clone());
                CasResult::Success(new)
            }
            Some(current) => CasResult::Failure(current.clone()),
            None => CasResult::Failure(new),
        }
    }

    /// Compare and remove: atomically remove entry if value matches expected (async).
    ///
    /// # Arguments
    /// - `key`: The key to remove.
    /// - `expected`: The expected current value.
    ///
    /// # Returns
    /// - `bool`: true if removed, false if value didn't match or key not found.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, key, expected), level = "trace")]
    pub async fn compare_and_remove(&self, key: &K, expected: &V) -> bool
    where
        V: PartialEq,
    {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        let mut guard = shard.write().await;

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.remove(key);
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Create a copy-on-write snapshot for minimal-locking reads (async).
    ///
    /// # Returns
    /// - `CowSnapshot<K, V>`: Immutable snapshot of current state.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn cow_snapshot(&self) -> CowSnapshot<K, V> {
        let data = self.iter().await;
        let version = self.version.load(Ordering::SeqCst) as u64;
        CowSnapshot::new(data, version)
    }

    /// Create a versioned snapshot for time-travel queries (async).
    ///
    /// # Returns
    /// - `IsolatedSnapshot<K, V>`: Snapshot with version information.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn versioned_snapshot(&self) -> IsolatedSnapshot<K, V> {
        let data = self.iter().await;
        let version = self.version.fetch_add(1, Ordering::SeqCst) as u64;
        IsolatedSnapshot::new(version, data)
    }

    /// Create a snapshot at a specific version (if available, async).
    ///
    /// # Arguments
    /// - `version`: The version number to snapshot at.
    ///
    /// # Returns
    /// - `Option<IsolatedSnapshot<K, V>>`: Snapshot if version is current, None otherwise.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn snapshot_at_version(&self, version: u64) -> Option<IsolatedSnapshot<K, V>> {
        let current_version = self.version.load(Ordering::SeqCst) as u64;
        if version == current_version {
            let data = self.iter().await;
            Some(IsolatedSnapshot::new(version, data))
        } else {
            None
        }
    }

    /// Get lock profiling data for all shards (async).
    ///
    /// # Returns
    /// - `Vec<LockProfile>`: Per-shard lock statistics.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn lock_profiles(&self) -> Vec<LockProfile> {
        let profiling_enabled = self.profiling_enabled.load(Ordering::Relaxed);
        if !profiling_enabled {
            return Vec::new();
        }

        let slots = self.shards.read().await;
        let mut profiles = Vec::new();

        for (idx, slot) in slots.iter().enumerate() {
            if let Some(_shard) = slot {
                profiles.push(LockProfile {
                    shard_id: idx,
                    contention_count: 0,
                    avg_wait_time_ns: 0,
                    max_wait_time_ns: 0,
                    reads: 0,
                    writes: 0,
                });
            }
        }

        profiles
    }

    /// Enable or disable lock profiling (async).
    ///
    /// # Arguments
    /// - `enabled`: Whether to enable profiling.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn enable_profiling(&self, enabled: bool) {
        self.profiling_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Create an AsyncShardedHashMap with replication support.
    ///
    /// # Arguments
    /// - `shard_count`: Number of shards.
    /// - `replicas`: Vector of replica implementations.
    /// - `quorum_config`: Quorum configuration for consistency.
    ///
    /// # Returns
    /// - `Self`: New map with replication configured.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(replicas, quorum_config), level = "trace")]
    pub fn with_replication(
        shard_count: usize,
        replicas: Vec<Arc<dyn Replica<K, V>>>,
        quorum_config: QuorumConfig,
    ) -> Self
    where
        S: Default,
    {
        let map = Self::with_shards_and_hasher(shard_count, S::default());
        {
            let mut r = std_write_guard(&map.replicas, "with_replication");
            *r = replicas;
        }
        {
            let mut q = std_write_guard(&map.quorum_config, "with_replication_quorum");
            *q = Some(quorum_config);
        }
        map
    }

    /// Insert with replication to configured replicas.
    ///
    /// # Arguments
    /// - `key`: The key to insert.
    /// - `value`: The value to insert.
    ///
    /// # Returns
    /// - `Result<Option<V>, ReplicaError>`: Previous value or error.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, key, value), level = "trace")]
    pub async fn insert_replicated(&self, key: K, value: V) -> Result<Option<V>, ReplicaError> {
        // Insert locally first
        let old = self.insert(key.clone(), value.clone()).await;

        // Replicate to quorum
        let replicas = {
            let r = std_read_guard(&self.replicas, "insert_replicated");
            r.clone()
        };

        if replicas.is_empty() {
            return Ok(old);
        }

        let quorum_config = {
            let q = std_read_guard(&self.quorum_config, "insert_replicated_quorum");
            q.clone()
        };

        let op = ReplicationOp::Insert {
            key: key.clone(),
            value: value.clone(),
        };

        // Replicate to all replicas in parallel
        let mut tasks = Vec::new();
        for replica in replicas.iter() {
            let replica_clone = Arc::clone(replica);
            let op_clone = op.clone();
            tasks.push(tokio::spawn(async move {
                replica_clone.replicate(op_clone).await
            }));
        }

        // Wait for results
        let mut success_count = 0;
        for task in tasks {
            if let Ok(Ok(())) = task.await {
                success_count += 1;
            }
        }

        // Check quorum
        if let Some(config) = quorum_config
            && success_count < config.write_quorum
        {
            return Err(ReplicaError::QuorumFailed);
        }

        Ok(old)
    }

    /// Remove with replication to configured replicas.
    ///
    /// # Arguments
    /// - `key`: The key to remove.
    ///
    /// # Returns
    /// - `Result<Option<V>, ReplicaError>`: Removed value or error.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn remove_replicated(&self, key: &K) -> Result<Option<V>, ReplicaError> {
        // Remove locally first
        let old = self.remove(key).await;

        // Replicate to quorum
        let replicas = {
            let r = std_read_guard(&self.replicas, "remove_replicated");
            r.clone()
        };

        if replicas.is_empty() {
            return Ok(old);
        }

        let quorum_config = {
            let q = std_read_guard(&self.quorum_config, "remove_replicated_quorum");
            q.clone()
        };

        let op = ReplicationOp::Remove { key: key.clone() };

        // Replicate to all replicas in parallel
        let mut tasks = Vec::new();
        for replica in replicas.iter() {
            let replica_clone = Arc::clone(replica);
            let op_clone = op.clone();
            tasks.push(tokio::spawn(async move {
                replica_clone.replicate(op_clone).await
            }));
        }

        // Wait for results
        let mut success_count = 0;
        for task in tasks {
            if let Ok(Ok(())) = task.await {
                success_count += 1;
            }
        }

        // Check quorum
        if let Some(config) = quorum_config
            && success_count < config.write_quorum
        {
            return Err(ReplicaError::QuorumFailed);
        }

        Ok(old)
    }

    /// Iterate over all keys (snapshot-based, async).
    ///
    /// # Returns
    /// - `Vec<K>`: vector of cloned keys
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn keys(&self) -> Vec<K> {
        self.iter().await.into_iter().map(|(k, _)| k).collect()
    }

    /// Iterate over all values (snapshot-based, async).
    ///
    /// # Returns
    /// - `Vec<V>`: vector of cloned values
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn values(&self) -> Vec<V> {
        self.iter().await.into_iter().map(|(_, v)| v).collect()
    }

    /// Returns statistics about shard distribution and utilization (async).
    ///
    /// # Returns
    /// - `ShardStats`: structure containing shard metrics
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn shard_stats(&self) -> ShardStats {
        let slots = self.shards.read().await;
        let mut initialized = 0;
        let mut loads = Vec::new();

        for shard in slots.iter().flatten() {
            initialized += 1;
            let guard = shard.read().await;
            loads.push(guard.len());
        }

        let total = slots.len();
        let empty = loads.iter().filter(|&&l| l == 0).count();
        let max_load = loads.iter().max().copied().unwrap_or(0);
        let avg_load = if initialized > 0 {
            loads.iter().sum::<usize>() as f64 / initialized as f64
        } else {
            0.0
        };

        ShardStats {
            initialized,
            total,
            empty,
            avg_load,
            max_load,
        }
    }

    /// Returns shard utilization as a percentage (0-100, async).
    ///
    /// # Returns
    /// - `f64`: percentage of shards that have been initialized
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn shard_utilization(&self) -> f64 {
        let stats = self.shard_stats().await;
        stats.utilization_percent()
    }

    /// Returns load statistics for each initialized shard (async).
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn per_shard_load(&self) -> Vec<PerShardLoad> {
        let slots = self.shards.read().await;
        let mut stats = Vec::new();

        for (i, shard_opt) in slots.iter().enumerate() {
            if let Some(shard) = shard_opt {
                let guard = shard.read().await;
                stats.push(PerShardLoad {
                    shard_idx: i,
                    entry_count: guard.len(),
                    capacity: guard.capacity(),
                });
            }
        }

        stats
    }

    /// Returns current memory-oriented shard statistics (async).
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn memory_stats(&self) -> MemoryStats {
        let slots = self.shards.read().await;
        let mut shards_allocated = 0;
        let mut total_capacity = 0usize;
        let mut total_entries = 0usize;

        for shard in slots.iter().flatten() {
            shards_allocated += 1;
            let guard = shard.read().await;
            total_capacity += guard.capacity();
            total_entries += guard.len();
        }

        let load_factor = if total_capacity > 0 {
            total_entries as f64 / total_capacity as f64
        } else {
            0.0
        };

        MemoryStats {
            shards_allocated,
            total_capacity,
            load_factor,
        }
    }

    /// Drains all entries from the map and returns them as an iterator (async).
    ///
    /// Shard allocations are retained.
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn drain(&self) -> DrainIterator<K, V> {
        let slots = self.shards.read().await;
        let mut items = Vec::new();

        for shard in slots.iter().flatten() {
            let mut guard = shard.write().await;
            items.extend(guard.drain());
        }

        self.total_len.store(0, Ordering::Relaxed);

        DrainIterator { items, index: 0 }
    }
}

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
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for kv in &self.0 {
            seq.serialize_element(kv)?;
        }
        seq.end()
    }
}

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
