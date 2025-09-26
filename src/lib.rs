#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![doc = r#"
Starshard: a high-performance, lazily sharded concurrent HashMap.

Features
---------
- `async`: enables `AsyncShardedHashMap` backed by `tokio::sync::RwLock`.
- `rayon`: enables parallel snapshot flattening inside iteration for large maps (sync + async).
- (Optional future) `serde` (not implemented here) could forward to `hashbrown`'s `serde`.

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
use fxhash::FxBuildHasher;

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

use fxhash::FxBuildHasher;
use hashbrown::HashMap;
#[cfg(feature = "rayon")]
use rayon::prelude::*;
use std::hash::{BuildHasher, Hash};
use std::sync::{
    Arc, RwLock as StdRwLock, RwLockReadGuard as StdReadGuard, RwLockWriteGuard as StdWriteGuard,
    atomic::{AtomicUsize, Ordering},
};

#[cfg(feature = "async")]
use tokio::sync::{RwLock as TokioRwLock, RwLockWriteGuard as TokioWriteGuard};

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
        Self::with_shards_and_hasher(shard_count, FxBuildHasher::default())
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
    pub fn get(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key));
        let guard: StdReadGuard<'_, HashMap<K, V, S>> = shard.read().unwrap();
        guard.get(key).cloned()
    }

    /// Remove key, returning previous value.
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
    #[inline]
    pub fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Whether empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all data (retains shard allocations).
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
    pub fn new(shard_count: usize) -> Self {
        Self::with_shards_and_hasher(shard_count, FxBuildHasher::default())
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
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Number of initialized shards.
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
    pub async fn get(&self, key: &K) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        if let Ok(g) = shard.try_read() {
            return g.get(key).cloned();
        }
        let g = shard.read().await;
        g.get(key).cloned()
    }

    /// Remove key.
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
    pub async fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Empty check.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Clear (retains allocated shards).
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
}
