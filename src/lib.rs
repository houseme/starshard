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
- No eviction / TTL (can be layered externally).
- Iteration allocates temporary vectors proportional to initialized shards (to snapshot).
- Not lock-free; large writer pressure can still cause convoying on hot shards.

Future Extension Ideas
-----------------------
- Optional background shard growth / rebalancing.
- Configurable eviction: LRU per shard / clock / segmented queue.
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
    atomic::{AtomicUsize, Ordering}, Arc, RwLock as StdRwLock, RwLockReadGuard as StdReadGuard,
    RwLockWriteGuard as StdWriteGuard,
};

#[cfg(feature = "async")]
use tokio::sync::{RwLock as TokioRwLock, RwLockWriteGuard as TokioWriteGuard};

/* ======================== Module Declarations ======================== */

/// Version 0.9.0 features: TTL, eviction, metrics, and advanced iteration.
#[cfg(any(feature = "ttl", feature = "metrics", feature = "advanced-iter"))]
pub mod v090_eviction;

/// Version 1.0.0 features: Transactions, CAS, replication, and diagnostics.
#[cfg(any(
    feature = "transactions",
    feature = "cas",
    feature = "cow-snapshot",
    feature = "replication",
    feature = "diagnostics"
))]
pub mod v100_advanced;

// Re-export key v0.9.0 types
#[cfg(feature = "ttl")]
pub use v090_eviction::{EvictionConfig, EvictionPolicy};

#[cfg(feature = "metrics")]
pub use v090_eviction::{AtomicMetrics, MemoryStats, MetricsStats};

#[cfg(feature = "advanced-iter")]
pub use v090_eviction::{DrainIterator, IterBuilder};

// Re-export key v1.0.0 types
#[cfg(feature = "transactions")]
pub use v100_advanced::{Transaction, TransactionResult, TxnOp};

#[cfg(feature = "cas")]
pub use v100_advanced::CasResult;

#[cfg(feature = "cow-snapshot")]
pub use v100_advanced::CowSnapshot;

#[cfg(feature = "replication")]
pub use v100_advanced::{QuorumConfig, Replica, ReplicaError, ReplicationOp};

#[cfg(feature = "diagnostics")]
pub use v100_advanced::{IsolatedSnapshot, LockProfile};

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
    pub fn utilization_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.initialized as f64 / self.total as f64) * 100.0
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
}

impl<K, V> ShardedHashMap<K, V, FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
{
    /// Create with default hasher (`FxBuildHasher`).
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
        }
    }

    /// Current configured shard slots.
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Number of shards actually initialized (allocated).
    pub fn initialized_shards(&self) -> usize {
        let g = self.shards.read().unwrap();
        g.iter().filter(|o| o.is_some()).count()
    }

    #[inline]
    fn shard_index(&self, key: &K) -> usize {
        (self.hasher.hash_one(key) % self.shard_count as u64) as usize
    }

    #[inline]
    fn get_or_init_shard(&self, index: usize) -> StdShard<K, V, S> {
        let mut g = self.shards.write().unwrap();
        if g[index].is_none() {
            let map = StdShardMap::with_hasher(self.hasher.clone());
            g[index] = Some(Arc::new(StdRwLock::new(map)));
        }
        g[index].as_ref().unwrap().clone()
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
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(&key));
        let mut guard: StdWriteGuard<'_, HashMap<K, V, S>> = shard.write().unwrap();
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
    pub fn get(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let guard: StdReadGuard<'_, HashMap<K, V, S>> = shard.read().unwrap();
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
    pub fn contains(&self, key: &K) -> bool {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let guard: StdReadGuard<'_, HashMap<K, V, S>> = shard.read().unwrap();
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
    pub fn remove(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard: StdWriteGuard<'_, HashMap<K, V, S>> = shard.write().unwrap();
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
    pub fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Whether empty.
    ///
    /// # Returns
    /// - `bool`: true if the map contains no entries.
    ///
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all data (retains shard allocations).
    ///
    /// # Notes
    /// - Resets length counter to zero.
    ///
    pub fn clear(&self) {
        let slots = self.shards.read().unwrap();
        for shard in slots.iter().flatten() {
            let mut g = shard.write().unwrap();
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
    pub fn iter(&self) -> impl Iterator<Item = (K, V)> {
        let shards_snapshot: Vec<StdShard<K, V, S>> = {
            let g = self.shards.read().unwrap();
            g.iter().filter_map(|o| o.as_ref().cloned()).collect()
        };

        #[cfg(feature = "rayon")]
        {
            let items: Vec<(K, V)> = shards_snapshot
                .par_iter()
                .flat_map(|shard| {
                    let guard = shard.read().unwrap();
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
                let guard = shard.read().unwrap();
                items.extend(guard.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            return items.into_iter();
        }
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
    pub fn compute_if_present<F>(&self, key: &K, f: F) -> Option<V>
    where
        F: FnOnce(&V) -> Option<V>,
    {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = shard.write().unwrap();

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
    pub fn compute_if_absent<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        let shard = self.get_or_init_shard(self.shard_index(&key));
        let mut guard = shard.write().unwrap();

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
    pub fn retain<F>(&self, predicate: F)
    where
        F: Fn(&K, &V) -> bool,
    {
        let shards_snapshot: Vec<StdShard<K, V, S>> = {
            let g = self.shards.read().unwrap();
            g.iter().filter_map(|o| o.as_ref().cloned()).collect()
        };

        for shard in shards_snapshot {
            let mut guard = shard.write().unwrap();
            let removed_count = guard.len();
            guard.retain(|k, v| predicate(k, v));
            let removed = removed_count - guard.len();
            if removed > 0 {
                self.total_len.fetch_sub(removed, Ordering::Relaxed);
            }
        }
    }

    /// Iterate over all keys (snapshot-based).
    ///
    /// # Returns
    /// - `Vec<K>`: vector of cloned keys
    ///
    pub fn keys(&self) -> impl Iterator<Item = K> {
        self.iter().map(|(k, _)| k)
    }

    /// Iterate over all values (snapshot-based).
    ///
    /// # Returns
    /// - `Vec<V>`: vector of cloned values
    ///
    pub fn values(&self) -> impl Iterator<Item = V> {
        self.iter().map(|(_, v)| v)
    }

    /// Returns statistics about shard distribution and utilization.
    ///
    /// # Returns
    /// - `ShardStats`: structure containing shard metrics
    ///
    pub fn shard_stats(&self) -> ShardStats {
        let slots = self.shards.read().unwrap();
        let mut initialized = 0;
        let mut loads = Vec::new();

        for shard_opt in slots.iter() {
            if let Some(shard) = shard_opt {
                initialized += 1;
                let guard = shard.read().unwrap();
                loads.push(guard.len());
            }
        }

        let total = slots.len();
        let empty = initialized - loads.iter().filter(|&&l| l == 0).count();
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
    pub fn shard_utilization(&self) -> f64 {
        let stats = self.shard_stats();
        stats.utilization_percent()
    }
}

/* ---------------------- Serde for ShardedHashMap ------------------------ */

#[cfg(feature = "serde")]
mod serde_impl {
    use super::ShardedHashMap;
    use super::DEFAULT_SHARDS;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::hash::{BuildHasher, Hash};

    // Transit representation
    #[derive(Serialize, Deserialize)]
    struct ShardedMapRepr<K, V> {
        shard_count: usize,
        entries: Vec<(K, V)>,
    }

    // Serialize
    impl<K, V, S> Serialize for ShardedHashMap<K, V, S>
    where
        K: Eq + Hash + Clone + Send + Sync + Serialize,
        V: Clone + Send + Sync + Serialize,
        S: BuildHasher + Clone + Send + Sync + Default,
    {
        fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
        where
            Ser: Serializer,
        {
            let repr = ShardedMapRepr {
                shard_count: self.shard_count(),
                entries: self.iter().collect(),
            };
            repr.serialize(serializer)
        }
    }

    // Deserialize
    impl<'de, K, V, S> Deserialize<'de> for ShardedHashMap<K, V, S>
    where
        K: Eq + Hash + Clone + Send + Sync + Deserialize<'de>,
        V: Clone + Send + Sync + Deserialize<'de>,
        S: BuildHasher + Clone + Send + Sync + Default,
    {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let repr = ShardedMapRepr::<K, V>::deserialize(deserializer)?;
            let sc = if repr.shard_count == 0 {
                DEFAULT_SHARDS
            } else {
                repr.shard_count
            };
            let map = ShardedHashMap::<K, V, S>::with_shards_and_hasher(sc, S::default());
            for (k, v) in repr.entries {
                map.insert(k, v);
            }
            Ok(map)
        }
    }
}

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
}

#[cfg(feature = "async")]
impl<K, V> AsyncShardedHashMap<K, V, FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
{
    /// Create with default hasher.
    ///
    /// # Arguments
    /// - `shard_count`: number of shards (0 defaults to `DEFAULT_SHARDS`).
    ///
    /// # Returns
    /// - `Self`: new async sharded hash map.
    ///
    pub fn new(shard_count: usize) -> Self {
        Self::with_shards_and_hasher(shard_count, FxBuildHasher)
    }
}

#[cfg(feature = "async")]
impl<K, V, S> AsyncShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Create with custom hasher.
    ///
    /// # Arguments
    /// - `shard_count`: number of shards (0 defaults to `DEFAULT_SHARDS
    /// - `hasher`: user-supplied hasher instance.
    ///
    /// # Returns
    /// - `Self`: new async sharded hash map.
    ///
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
        }
    }

    /// Configured shard capacity.
    ///
    /// # Returns
    /// - `usize`: number of shard slots.
    ///
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Number of initialized shards.
    ///
    /// # Returns
    /// - `usize`: count of allocated shards.
    ///
    pub async fn initialized_shards(&self) -> usize {
        let g = self.shards.read().await;
        g.iter().filter(|o| o.is_some()).count()
    }

    #[inline]
    fn shard_index(&self, key: &K) -> usize {
        (self.hasher.hash_one(key) % self.shard_count as u64) as usize
    }

    #[inline]
    async fn get_or_init_shard(&self, index: usize) -> AsyncShard<K, V, S> {
        let mut g = self.shards.write().await;
        if g[index].is_none() {
            let map = AsyncShardMap::with_hasher(self.hasher.clone());
            g[index] = Some(Arc::new(TokioRwLock::new(map)));
        }
        g[index].as_ref().unwrap().clone()
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
    pub async fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Empty check.
    ///
    /// # Returns
    /// - `bool`: true if the map contains no entries.
    ///
    #[inline]
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Clear (retains allocated shards).
    ///
    /// # Notes
    /// - Resets length counter to zero.
    ///
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
    /// 1. Snapshot Arc list of initialized shards under read lock of the shard vector.
    /// 2. For each shard: try `try_read`; fallback to `await` read.
    /// 3. Clone inner HashMaps (short critical sections).
    /// 4. If `rayon` enabled, parallel flatten of snapshots.
    ///
    /// Returns a materialized `Vec`.
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
    #[cfg(feature = "batch")]
    pub async fn batch_insert<I>(&self, entries: I) -> usize
    where
        I: IntoIterator<Item = (K, V)>,
    {
        let mut grouped: std::collections::HashMap<usize, Vec<(K, V)>> =
            std::collections::HashMap::new();

        for (k, v) in entries {
            let shard_idx = self.shard_index(&k);
            grouped
                .entry(shard_idx)
                .or_insert_with(Vec::new)
                .push((k, v));
        }

        let mut count = 0;
        for (_shard_idx, pairs) in grouped {
            for (k, v) in pairs {
                let shard = self.get_or_init_shard(self.shard_index(&k)).await;
                let mut guard = shard.write().await;
                if guard.insert(k, v).is_none() {
                    count += 1;
                    self.total_len.fetch_add(1, Ordering::Relaxed);
                }
            }
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
    #[cfg(feature = "batch")]
    pub async fn batch_remove<I>(&self, keys: I) -> usize
    where
        I: IntoIterator<Item = K>,
    {
        let mut grouped: std::collections::HashMap<usize, Vec<K>> =
            std::collections::HashMap::new();

        for k in keys {
            let shard_idx = self.shard_index(&k);
            grouped.entry(shard_idx).or_insert_with(Vec::new).push(k);
        }

        let mut count = 0;
        for (_shard_idx, keys) in grouped {
            for k in keys {
                let shard = self.get_or_init_shard(self.shard_index(&k)).await;
                let mut guard = shard.write().await;
                if guard.remove(&k).is_some() {
                    count += 1;
                    self.total_len.fetch_sub(1, Ordering::Relaxed);
                }
            }
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
    #[cfg(feature = "batch")]
    pub async fn batch_get(&self, keys: &[K]) -> Vec<Option<V>> {
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            results.push(self.get(key).await);
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

    /// Iterate over all keys (snapshot-based, async).
    ///
    /// # Returns
    /// - `Vec<K>`: vector of cloned keys
    ///
    pub async fn keys(&self) -> Vec<K> {
        self.iter().await.into_iter().map(|(k, _)| k).collect()
    }

    /// Iterate over all values (snapshot-based, async).
    ///
    /// # Returns
    /// - `Vec<V>`: vector of cloned values
    ///
    pub async fn values(&self) -> Vec<V> {
        self.iter().await.into_iter().map(|(_, v)| v).collect()
    }

    /// Returns statistics about shard distribution and utilization (async).
    ///
    /// # Returns
    /// - `ShardStats`: structure containing shard metrics
    ///
    pub async fn shard_stats(&self) -> ShardStats {
        let slots = self.shards.read().await;
        let mut initialized = 0;
        let mut loads = Vec::new();

        for shard_opt in slots.iter() {
            if let Some(shard) = shard_opt {
                initialized += 1;
                let guard = shard.read().await;
                loads.push(guard.len());
            }
        }

        let total = slots.len();
        let empty = initialized - loads.iter().filter(|&&l| l == 0).count();
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
    pub async fn shard_utilization(&self) -> f64 {
        let stats = self.shard_stats().await;
        stats.utilization_percent()
    }
}

#[cfg(all(feature = "async", feature = "serde"))]
impl<K, V> AsyncShardedHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + Serialize,
    V: Clone + Send + Sync + Serialize,
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

    #[cfg(feature = "batch")]
    #[test]
    fn batch_insert_new_entries() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        let entries = vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)];
        let inserted = m.batch_insert(entries);
        assert_eq!(inserted, 3);
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&"b".into()), Some(2));
    }

    #[cfg(feature = "batch")]
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

    #[cfg(feature = "batch")]
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

    #[cfg(feature = "batch")]
    #[test]
    fn batch_get() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
        m.insert("a".into(), 1);
        m.insert("c".into(), 3);

        let keys = vec!["a", "b", "c"];
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
    fn shard_utilization() {
        let m: ShardedHashMap<String, i32> = ShardedHashMap::new(16);
        m.insert("a".into(), 1);

        let util = m.shard_utilization();
        assert!(util > 0.0 && util <= 100.0);
    }

    #[cfg(feature = "batch")]
    #[tokio::test]
    async fn async_batch_insert() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        let entries = vec![("a".into(), 1), ("b".into(), 2)];
        let inserted = m.batch_insert(entries).await;
        assert_eq!(inserted, 2);
        assert_eq!(m.len().await, 2);
    }

    #[cfg(feature = "batch")]
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

    #[cfg(feature = "batch")]
    #[tokio::test]
    async fn async_batch_get() {
        let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
        m.insert("a".into(), 1).await;
        m.insert("c".into(), 3).await;

        let keys = vec!["a", "b", "c"];
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
}
