//! Synchronous sharded map implementation.
//!
//! Routing read guards pin directories; topology changes hold routing exclusively.
//! Snapshot builders serialize separately and retain ordered shard read locks.
//! Shard locks are ordered active first, then previous; transactions and snapshot
//! builders sort indices within each generation.
//! Snapshot caches are immutable and rebuilt lazily by committed write epoch.

use super::*;
use std::borrow::Borrow;
use std::time::Instant;

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

    /// Create with default hasher and explicit snapshot mode.
    #[tracing::instrument(level = "trace")]
    pub fn with_snapshot_mode(shard_count: usize, mode: SnapshotMode) -> Self {
        Self::with_shards_and_hasher_and_snapshot_mode(shard_count, FxBuildHasher, mode)
    }
}

impl<K, V, S> ShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Core constructor: allocates all shard vectors, atomics, and locks.
    ///
    /// All shard slots start as `None`; routing changes require exclusive access.
    #[inline]
    fn build_with_count(count: usize, hasher: S, snapshot_mode: SnapshotMode) -> Self {
        let shards = vec![None; count];
        Self {
            snapshot_mode,
            shards: Arc::new(StdRwLock::new(shards)),
            routing_lock: Arc::new(StdRwLock::new(())),
            snapshot_build_lock: Arc::new(StdMutex::new(())),
            previous_shards: Arc::new(StdRwLock::new(None)),
            hasher,
            shard_count: Arc::new(AtomicUsize::new(count)),
            previous_shard_count: Arc::new(AtomicUsize::new(0)),
            total_len: Arc::new(AtomicUsize::new(0)),
            write_epoch: Arc::new(AtomicU64::new(0)),
            snapshot_cache: Arc::new(StdRwLock::new(None)),
            rebalance_tracker: Arc::new(RebalanceTracker::new()),
            #[cfg(feature = "advanced")]
            profiling_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create with explicit hasher.
    ///
    /// This preserves backward compatibility while enforcing the default
    /// safety cap (`MAX_SHARDS`) to avoid oversized allocations.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher(shard_count: usize, hasher: S) -> Self {
        Self::with_shards_and_hasher_and_snapshot_mode(shard_count, hasher, SnapshotMode::Clone)
    }

    /// Create with explicit hasher and snapshot mode.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher_and_snapshot_mode(
        shard_count: usize,
        hasher: S,
        mode: SnapshotMode,
    ) -> Self {
        let requested = normalized_shard_count(shard_count);
        let count = capped_shard_count(requested, MAX_SHARDS);
        if requested != count {
            tracing::warn!(
                requested_shards = requested,
                capped_shards = count,
                max_shards = MAX_SHARDS,
                "requested shard_count exceeded default cap and was clamped"
            );
        }
        Self::build_with_count(count, hasher, mode)
    }

    /// Create with explicit hasher and a custom cap.
    ///
    /// This is useful when callers need to tune the shard upper bound for
    /// workload-specific memory/performance trade-offs.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher_capped(shard_count: usize, hasher: S, max_shards: usize) -> Self {
        Self::with_shards_and_hasher_capped_and_snapshot_mode(
            shard_count,
            hasher,
            max_shards,
            SnapshotMode::Clone,
        )
    }

    /// Create with explicit hasher, custom cap and snapshot mode.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher_capped_and_snapshot_mode(
        shard_count: usize,
        hasher: S,
        max_shards: usize,
        mode: SnapshotMode,
    ) -> Self {
        let effective_max = max_shards.max(1);
        let requested = normalized_shard_count(shard_count);
        let count = capped_shard_count(requested, effective_max);
        if requested != count {
            tracing::warn!(
                requested_shards = requested,
                capped_shards = count,
                max_shards = effective_max,
                "requested shard_count exceeded configured cap and was clamped"
            );
        }
        Self::build_with_count(count, hasher, mode)
    }

    /// Strict constructor with explicit hasher.
    ///
    /// Returns an error when the requested shard count exceeds `MAX_SHARDS`.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn try_with_shards_and_hasher(
        shard_count: usize,
        hasher: S,
    ) -> Result<Self, ShardCountError> {
        Self::try_with_shards_and_hasher_capped(shard_count, hasher, MAX_SHARDS)
    }

    /// Strict constructor with explicit hasher and a caller-provided cap.
    ///
    /// Returns an error instead of clamping when the effective shard count is
    /// out of range.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn try_with_shards_and_hasher_capped(
        shard_count: usize,
        hasher: S,
        max_shards: usize,
    ) -> Result<Self, ShardCountError> {
        let effective_max = max_shards.max(1);
        let count = strict_shard_count(shard_count, effective_max)?;
        Ok(Self::build_with_count(count, hasher, SnapshotMode::Clone))
    }

    /// Current configured shard slots.
    pub fn shard_count(&self) -> usize {
        self.shard_count.load(Ordering::Relaxed)
    }

    /// Number of allocated active shard slots.
    pub fn initialized_shards(&self) -> usize {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        std_read_guard(&self.shards, "shards")
            .iter()
            .flatten()
            .count()
    }

    /// Current rebalance progress.
    pub fn rebalance_status(&self) -> RebalanceStatus {
        self.rebalance_tracker.snapshot()
    }

    // Every caller must pin routing before calculating an index or using a shard.
    fn shard_index<Q: Hash + ?Sized>(&self, key: &Q) -> usize {
        (self.hasher.hash_one(key) % self.shard_count() as u64) as usize
    }

    fn get_or_init_shard(&self, index: usize) -> StdShard<K, V, S> {
        if let Some(shard) = std_read_guard(&self.shards, "shards")[index].as_ref() {
            return shard.clone();
        }
        let mut slots = std_write_guard(&self.shards, "shards_init");
        slots[index]
            .get_or_insert_with(|| {
                Arc::new(StdRwLock::new(HashMap::with_hasher(self.hasher.clone())))
            })
            .clone()
    }

    fn previous_shard<Q: Hash + ?Sized>(&self, key: &Q) -> Option<StdShard<K, V, S>> {
        let count = self.previous_shard_count.load(Ordering::Relaxed);
        if count == 0 {
            return None;
        }
        let index = (self.hasher.hash_one(key) % count as u64) as usize;
        std_read_guard(&self.previous_shards, "previous_shards")
            .as_ref()
            .and_then(|slots| slots[index].clone())
    }

    // Active shard is locked first. Promotion is a physical move, not a new key.
    fn promote_previous<Q>(&self, key: &Q, active: &mut HashMap<K, V, S>)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(shard) = self.previous_shard(key) {
            let mut previous = std_write_guard(&shard, "previous_shard");
            if let Some((key, value)) = previous.remove_entry(key) {
                active.entry(key).or_insert(value);
            }
        }
    }

    fn record_write(&self, inserted: usize, removed: usize) {
        if inserted != 0 {
            self.total_len.fetch_add(inserted, Ordering::Relaxed);
        }
        if removed != 0 {
            self.total_len.fetch_sub(removed, Ordering::Relaxed);
        }
        self.write_epoch.fetch_add(1, Ordering::Release);
    }

    fn all_shards(&self) -> Vec<StdShard<K, V, S>> {
        let mut shards: Vec<_> = std_read_guard(&self.shards, "shards")
            .iter()
            .flatten()
            .cloned()
            .collect();
        if let Some(previous) = std_read_guard(&self.previous_shards, "previous_shards").as_ref() {
            shards.extend(previous.iter().flatten().cloned());
        }
        shards
    }

    /// Creates an entry handle. Each mutating method is atomic independently;
    /// chaining methods does not hold a lock across the whole chain.
    pub fn entry(&self, key: K) -> crate::Entry<'_, K, V, S> {
        if self.contains(&key) {
            crate::Entry::occupied(self, key)
        } else {
            crate::Entry::vacant(self, key)
        }
    }

    fn insert_inner(&self, key: K, value: V) -> Option<V> {
        let shard = self.get_or_init_shard(self.shard_index(&key));
        let mut guard = std_write_guard(&shard, "insert");
        self.promote_previous(&key, &mut guard);
        let old = guard.insert(key, value);
        self.record_write(usize::from(old.is_none()), 0);
        old
    }

    /// Inserts a key/value, returning its previous value. Expected O(1).
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        self.insert_inner(key, value)
    }

    fn read_inner<Q, R>(&self, key: &Q, read: impl FnOnce(&V) -> R) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let slots = std_read_guard(&self.shards, "shards");
        if let Some(shard) = slots[self.shard_index(key)].as_ref().cloned() {
            drop(slots);
            let active = std_read_guard(&shard, "read");
            if let Some(value) = active.get(key) {
                return Some(read(value));
            }
            // Keep active locked through fallback so a concurrent promotion
            // cannot move the entry behind this lookup.
            let previous = self.previous_shard(key)?;
            let guard = std_read_guard(&previous, "previous_read");
            guard.get(key).map(read)
        } else {
            // Keep directory read access until fallback finishes, preventing a
            // writer from initializing an active destination and promoting it.
            let previous = self.previous_shard(key)?;
            let guard = std_read_guard(&previous, "previous_read");
            guard.get(key).map(read)
        }
    }

    /// Fetches a cloned value. Borrowed keys such as `&str` are supported.
    pub fn get(&self, key: &K) -> Option<V> {
        self.get_borrowed(key)
    }

    /// Fetches a value using a borrowed key without constructing an owned key.
    pub fn get_borrowed<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        self.read_inner(key, Clone::clone)
    }

    /// Reads a value without cloning it. The callback runs under a shard lock
    /// and must not call back into this map.
    pub fn read_with<Q, R>(&self, key: &Q, read: impl FnOnce(&V) -> R) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        self.read_inner(key, read)
    }

    /// Tests key presence without cloning values or allocating missing shards.
    pub fn contains(&self, key: &K) -> bool {
        self.contains_borrowed(key)
    }

    /// Tests key presence using a borrowed key.
    pub fn contains_borrowed<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        self.read_inner(key, |_| ()).is_some()
    }

    fn remove_inner<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        // A missing active slot can still hold data in the previous generation.
        let shard = {
            let slots = std_read_guard(&self.shards, "shards");
            match slots[self.shard_index(key)].as_ref().cloned() {
                Some(shard) => shard,
                None if self.previous_shard_count.load(Ordering::Relaxed) == 0 => return None,
                None => {
                    drop(slots);
                    self.get_or_init_shard(self.shard_index(key))
                }
            }
        };
        let mut guard = std_write_guard(&shard, "remove");
        self.promote_previous(key, &mut guard);
        let old = guard.remove(key);
        if old.is_some() {
            self.record_write(0, 1);
        }
        old
    }

    /// Removes a key, returning its previous value.
    pub fn remove(&self, key: &K) -> Option<V> {
        self.remove_borrowed(key)
    }

    /// Removes an entry using a borrowed key.
    pub fn remove_borrowed<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        self.remove_inner(key)
    }

    /// Returns the atomic length. In-flight operations may not yet be reflected.
    pub fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }

    /// Returns whether the length is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Inserts only when absent, evaluating `f` once under the key's shard lock.
    /// The callback must not call back into this map.
    pub fn get_or_insert_with<F: FnOnce() -> V>(&self, key: K, f: F) -> V {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        let shard = self.get_or_init_shard(self.shard_index(&key));
        let mut guard = std_write_guard(&shard, "get_or_insert");
        self.promote_previous(&key, &mut guard);
        if let Some(value) = guard.get(&key) {
            return value.clone();
        }
        let value = f();
        guard.insert(key, value.clone());
        self.record_write(1, 0);
        value
    }

    /// Alias of [`Self::get_or_insert_with`].
    pub fn compute_if_absent<F: FnOnce() -> V>(&self, key: K, f: F) -> V {
        self.get_or_insert_with(key, f)
    }

    /// Atomically updates a present value, or removes it when `f` returns None.
    /// The callback must not call back into this map.
    pub fn compute_if_present<F: FnOnce(&V) -> Option<V>>(&self, key: &K, f: F) -> Option<V> {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = std_write_guard(&shard, "compute");
        self.promote_previous(key, &mut guard);
        let next = f(guard.get(key)?);
        if let Some(value) = next {
            guard.insert(key.clone(), value.clone());
            self.record_write(0, 0);
            Some(value)
        } else {
            guard.remove(key);
            self.record_write(0, 1);
            None
        }
    }

    /// Inserts a batch, returning the number of new logical keys.
    pub fn batch_insert<I: IntoIterator<Item = (K, V)>>(&self, entries: I) -> usize {
        // Run caller iterator code before locking, preserving reentrant producers.
        let entries: Vec<_> = entries.into_iter().collect();
        let _routing = std_read_guard(&self.routing_lock, "routing");
        if self.previous_shard_count.load(Ordering::Relaxed) != 0 {
            let mut inserted = 0;
            for (key, value) in entries {
                inserted += usize::from(self.insert_inner(key, value).is_none());
            }
            return inserted;
        }
        let mut buckets: HashMap<usize, Vec<(K, V)>, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher);
        for (key, value) in entries {
            buckets
                .entry(self.shard_index(&key))
                .or_default()
                .push((key, value));
        }
        let mut inserted = 0;
        for (index, entries) in buckets {
            let shard = self.get_or_init_shard(index);
            let mut guard = std_write_guard(&shard, "batch_insert");
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            for (key, value) in entries {
                inserted += usize::from(mutation.insert(key, value).is_none());
            }
        }
        inserted
    }

    /// Removes a batch, returning the number of removed keys.
    pub fn batch_remove<I: IntoIterator<Item = K>>(&self, keys: I) -> usize {
        let keys: Vec<_> = keys.into_iter().collect();
        let _routing = std_read_guard(&self.routing_lock, "routing");
        if self.previous_shard_count.load(Ordering::Relaxed) != 0 {
            return keys
                .into_iter()
                .filter(|key| self.remove_inner(key).is_some())
                .count();
        }
        let mut buckets: HashMap<usize, Vec<K>, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher);
        for key in keys {
            buckets.entry(self.shard_index(&key)).or_default().push(key);
        }
        let mut removed = 0;
        for (index, keys) in buckets {
            let shard = std_read_guard(&self.shards, "shards")[index].clone();
            let Some(shard) = shard else {
                continue;
            };
            let mut guard = std_write_guard(&shard, "batch_remove");
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            for key in keys {
                removed += usize::from(mutation.remove(&key).is_some());
            }
        }
        removed
    }

    /// Gets keys in input order, taking one read lock per active shard.
    pub fn batch_get(&self, keys: &[K]) -> Vec<Option<V>> {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        if self.previous_shard_count.load(Ordering::Relaxed) != 0 {
            return keys
                .iter()
                .map(|key| self.read_inner(key, Clone::clone))
                .collect();
        }
        let mut buckets: HashMap<usize, Vec<(usize, &K)>, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher);
        for (index, key) in keys.iter().enumerate() {
            buckets
                .entry(self.shard_index(key))
                .or_default()
                .push((index, key));
        }
        let mut result = vec![None; keys.len()];
        for (index, keys) in buckets {
            let shard = std_read_guard(&self.shards, "shards")[index].clone();
            if let Some(shard) = shard {
                let guard = std_read_guard(&shard, "batch_get");
                for (position, key) in keys {
                    result[position] = guard.get(key).cloned();
                }
            }
        }
        result
    }

    /// Retains matching entries in both generations. The predicate must not
    /// call back into this map. Changes are committed per shard.
    pub fn retain<F: Fn(&K, &V) -> bool + Sync + Send>(&self, predicate: F) {
        // Exclusive routing prevents promotion between the two shard sets.
        let _routing = std_write_guard(&self.routing_lock, "routing_retain");
        for shard in self.all_shards() {
            let mut guard = std_write_guard(&shard, "retain");
            let removed = guard.extract_if(|key, value| !predicate(key, value));
            for _ in removed {
                self.record_write(0, 1);
            }
        }
    }

    /// Clears both generations, retaining allocated active shards.
    pub fn clear(&self) {
        let routing = std_write_guard(&self.routing_lock, "routing_clear");
        let mut removed = Vec::with_capacity(self.len());
        for shard in self.all_shards() {
            let mut guard = std_write_guard(&shard, "clear");
            let count = guard.len();
            removed.extend(guard.drain());
            if count != 0 {
                self.record_write(0, count);
            }
        }
        *std_write_guard(&self.previous_shards, "previous_clear") = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        drop(routing);
        drop(removed);
    }

    // Callers serialize builders before entering this method. Holding the
    // directory reads freezes lazy slots as well as topology. Once the final
    // ordered shard read is acquired, all maps describe one coherent instant.
    fn collect_into<T, F>(&self, items: &mut Vec<T>, project: F) -> u64
    where
        T: Send,
        F: Fn(&K, &V) -> T + Send + Sync,
    {
        debug_assert!(items.is_empty());
        let _routing = std_read_guard(&self.routing_lock, "snapshot_routing");
        let active = std_read_guard(&self.shards, "snapshot_directory");
        let previous = std_read_guard(&self.previous_shards, "snapshot_previous_directory");
        let initialized = active.iter().flatten().count()
            + previous
                .as_ref()
                .map_or(0, |slots| slots.iter().flatten().count());
        let mut guards = Vec::with_capacity(initialized);
        guards.extend(
            active
                .iter()
                .flatten()
                .map(|shard| std_read_guard(shard, "snapshot_shard")),
        );
        if let Some(previous) = previous.as_ref() {
            guards.extend(
                previous
                    .iter()
                    .flatten()
                    .map(|shard| std_read_guard(shard, "snapshot_previous_shard")),
            );
        }
        let epoch = self.write_epoch.load(Ordering::Acquire);
        let count = guards.iter().map(|guard| guard.len()).sum();
        items.reserve_exact(count);
        #[cfg(feature = "rayon")]
        if count >= 4096 && guards.len() > 1 {
            // The guards outlive all Rayon work, so flatten their borrowed
            // iterators directly without allocating a Vec for each shard.
            items.par_extend(
                guards
                    .par_iter()
                    .flat_map_iter(|guard| guard.iter().map(|(key, value)| project(key, value))),
            );
            return epoch;
        }
        for guard in &guards {
            items.extend(guard.iter().map(|(key, value)| project(key, value)));
        }
        epoch
    }

    fn collect_with<T, F>(&self, project: F) -> Vec<T>
    where
        T: Send,
        F: Fn(&K, &V) -> T + Send + Sync,
    {
        // Declare user-owned output before the gate so serial collection unwind
        // drops its partial values only after releasing the builder.
        let mut items = Vec::new();
        let _builder = self
            .snapshot_build_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.collect_into(&mut items, project);
        items
    }

    // Immutable cached data does not reference either shard directory. A
    // matching epoch is a read linearization point even while a topology-only
    // migration holds routing exclusively. Writers publish before shard unlock.
    fn cached_snapshot(&self) -> Option<CapturedSnapshot<K, V>> {
        if self.snapshot_mode == SnapshotMode::Clone {
            return None;
        }
        // Capturing only an epoch and Arc is a tiny critical section. Serialize
        // that capture instead of contending on both a shared-reader count and
        // the snapshot Arc count across cores; no routing lock is involved.
        let cache = std_write_guard(&self.snapshot_cache, "snapshot_cache_capture");
        let (epoch, data) = cache.as_ref()?;
        (*epoch == self.write_epoch.load(Ordering::Acquire)).then(|| (*epoch, data.clone()))
    }

    fn capture_snapshot(&self) -> CapturedSnapshot<K, V> {
        if let Some(snapshot) = self.cached_snapshot() {
            return snapshot;
        }
        let mut items = Vec::new();
        // Coalesce expensive cache misses independently of routing. Acquiring
        // this before routing also prevents queued builders from pinning it.
        let builder = self
            .snapshot_build_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(snapshot) = self.cached_snapshot() {
            return snapshot;
        }
        let epoch = self.collect_into(&mut items, |key, value| (key.clone(), value.clone()));
        let items = Arc::new(items);
        let retired = if self.snapshot_mode != SnapshotMode::Clone {
            std_write_guard(&self.snapshot_cache, "snapshot_cache").replace((epoch, items.clone()))
        } else {
            None
        };
        // A concurrent writer after collection can make this pair obsolete,
        // but its captured epoch ensures no future hit mislabels it as current.
        drop(builder);
        drop(retired);
        (epoch, items)
    }

    /// Returns a stable shared snapshot. Cached/Cow hits validate an immutable
    /// cache without routing access; only missing or stale data needs rebuilding.
    pub fn shared_snapshot(&self) -> Arc<Vec<(K, V)>> {
        self.capture_snapshot().1
    }

    /// Iterates owned key/value copies from a stable snapshot. Cached entries
    /// are cloned after releasing routing; Clone mode moves its newly built Vec.
    pub fn iter(&self) -> impl Iterator<Item = (K, V)> {
        let items = if self.snapshot_mode == SnapshotMode::Clone {
            self.collect_with(|key, value| (key.clone(), value.clone()))
        } else {
            Arc::unwrap_or_clone(self.capture_snapshot().1)
        };
        items.into_iter()
    }

    /// Returns key copies without cloning values.
    pub fn keys(&self) -> impl Iterator<Item = K> {
        self.collect_with(|key, _| key.clone()).into_iter()
    }

    /// Returns value copies without cloning keys.
    pub fn values(&self) -> impl Iterator<Item = V> {
        self.collect_with(|_, value| value.clone()).into_iter()
    }

    /// Starts incremental migration. Normal operations can access both
    /// generations; every advance call pauses them for its bounded shard batch.
    pub fn start_rebalance_online(&self, new_shard_count: usize) -> Result<(), ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let _routing = std_write_guard(&self.routing_lock, "routing_rebalance");
        if self.rebalance_tracker.is_migrating() || target == self.shard_count() {
            return Ok(());
        }
        let current = self.shard_count();
        let old = std::mem::replace(
            &mut *std_write_guard(&self.shards, "shards"),
            vec![None; target],
        );
        *std_write_guard(&self.previous_shards, "previous_shards") = Some(old);
        self.previous_shard_count.store(current, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker.begin(current);
        Ok(())
    }

    /// Migrates up to `max_shards` source slots. Source data remains published
    /// until a replacement directory is ready; concurrent advances serialize.
    pub fn advance_rebalance(&self, max_shards: usize) -> usize {
        if max_shards == 0 {
            return 0;
        }
        // On unwind too, routing must unlock before retired values are dropped.
        let mut retired = Vec::new();
        let _routing = std_write_guard(&self.routing_lock, "routing_rebalance");
        let mut processed = 0;
        while processed < max_shards && self.rebalance_tracker.is_migrating() {
            let index = self.rebalance_tracker.snapshot().moved_shards;
            let source = std_read_guard(&self.previous_shards, "previous_shards")
                .as_ref()
                .and_then(|s| s[index].clone());
            if let Some(source) = source {
                // Build replacements off to the side: a panicking user Clone/Hash
                // leaves the source and destination generations untouched.
                let source = std_read_guard(&source, "migration_source");
                let mut replacements: HashMap<usize, HashMap<K, V, S>, FxBuildHasher> =
                    HashMap::with_hasher(FxBuildHasher);
                for (key, value) in source.iter() {
                    let index = self.shard_index(key);
                    let target = replacements.entry(index).or_insert_with(|| {
                        let existing = std_read_guard(&self.shards, "shards")[index].clone();
                        existing.map_or_else(
                            || HashMap::with_hasher(self.hasher.clone()),
                            |s| std_read_guard(&s, "migration_target").clone(),
                        )
                    });
                    target.entry(key.clone()).or_insert_with(|| value.clone());
                }
                let replacements: Vec<_> = replacements
                    .into_iter()
                    .map(|(index, map)| (index, Arc::new(StdRwLock::new(map))))
                    .collect();
                retired.reserve(replacements.len() + 1);
                let mut active = std_write_guard(&self.shards, "migration_publish");
                for (index, replacement) in replacements {
                    if let Some(old) = active[index].replace(replacement) {
                        retired.push(old);
                    }
                }
            }
            if let Some(source) = std_write_guard(&self.previous_shards, "migration_remove")
                .as_mut()
                .expect("routing pins migration")[index]
                .take()
            {
                retired.push(source);
            }
            self.rebalance_tracker.step();
            processed += 1;
            let status = self.rebalance_tracker.snapshot();
            if status.moved_shards == status.total_shards {
                *std_write_guard(&self.previous_shards, "migration_finish") = None;
                self.previous_shard_count.store(0, Ordering::Relaxed);
                self.rebalance_tracker.finish();
            }
        }
        drop(_routing);
        drop(retired);
        processed
    }

    /// Rebuilds the shard layout while pausing all map operations. The options
    /// remain reserved; no background worker or pause budget is implied.
    pub fn rebalance_to(
        &self,
        new_shard_count: usize,
        _options: RebalanceOptions,
    ) -> Result<RebalanceReport, ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let _routing = std_write_guard(&self.routing_lock, "routing_rebalance");
        let current = self.shard_count();
        let started = Instant::now();
        if target == current && !self.rebalance_tracker.is_migrating() {
            return Ok(RebalanceReport {
                from_shards: current,
                to_shards: target,
                moved_entries: 0,
                elapsed_ms: 0,
            });
        }
        let mut maps: Vec<Option<HashMap<K, V, S>>> = (0..target).map(|_| None).collect();
        let mut moved = 0;
        for shard in self.all_shards() {
            for (key, value) in std_read_guard(&shard, "rebalance_source").iter() {
                let index = (self.hasher.hash_one(key) % target as u64) as usize;
                let map =
                    maps[index].get_or_insert_with(|| HashMap::with_hasher(self.hasher.clone()));
                if let hashbrown::hash_map::Entry::Vacant(entry) = map.entry(key.clone()) {
                    entry.insert(value.clone());
                    moved += 1;
                }
            }
        }
        let slots = maps
            .into_iter()
            .map(|map| map.map(|m| Arc::new(StdRwLock::new(m))))
            .collect();
        let old_active = std::mem::replace(
            &mut *std_write_guard(&self.shards, "rebalance_publish"),
            slots,
        );
        let old_previous = std_write_guard(&self.previous_shards, "rebalance_previous").take();
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        drop(_routing);
        drop(old_active);
        drop(old_previous);
        Ok(RebalanceReport {
            from_shards: current,
            to_shards: target,
            moved_entries: moved,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    /// Executes a pessimistic transaction by locking affected active shards,
    /// then previous shards, in ascending index order. Reads return no values.
    /// User Hash/Eq/Drop implementations must not reenter this map.
    #[cfg(feature = "advanced")]
    pub fn execute_transaction(&self, txn: Transaction<K, V>) -> TransactionResult<()> {
        let _routing = std_read_guard(&self.routing_lock, "routing_transaction");
        let previous_count = self.previous_shard_count.load(Ordering::Relaxed);
        let mut active_indices = Vec::new();
        let mut previous_indices = Vec::new();
        for op in &txn.ops {
            let key = match op {
                TxnOp::Read(key) | TxnOp::Write(key, _) | TxnOp::Remove(key) => key,
            };
            active_indices.push(self.shard_index(key));
            if previous_count != 0 {
                previous_indices.push((self.hasher.hash_one(key) % previous_count as u64) as usize);
            }
        }
        active_indices.sort_unstable();
        active_indices.dedup();
        previous_indices.sort_unstable();
        previous_indices.dedup();
        // Resolve directory handles before holding any shard locks.
        let active: Vec<_> = active_indices
            .iter()
            .map(|&i| self.get_or_init_shard(i))
            .collect();
        let previous: Vec<_> = {
            let directory = std_read_guard(&self.previous_shards, "previous_shards");
            previous_indices
                .iter()
                .filter_map(|&i| {
                    directory
                        .as_ref()
                        .and_then(|slots| slots[i].clone())
                        .map(|s| (i, s))
                })
                .collect()
        };
        let mut guards: Vec<_> = active
            .iter()
            .map(|s| std_write_guard(s, "transaction"))
            .collect();
        let mut previous_guards: Vec<_> = previous
            .iter()
            .map(|(i, s)| (*i, std_write_guard(s, "transaction_previous")))
            .collect();
        for op in txn.ops {
            let key = match &op {
                TxnOp::Read(key) | TxnOp::Write(key, _) | TxnOp::Remove(key) => key,
            };
            let index = self.shard_index(key);
            let position = active_indices
                .binary_search(&index)
                .expect("routing pins transaction indices");
            let active = &mut guards[position];
            if previous_count != 0 {
                let previous_index = (self.hasher.hash_one(key) % previous_count as u64) as usize;
                if let Some((_, previous)) = previous_guards
                    .iter_mut()
                    .find(|(i, _)| *i == previous_index)
                    && let Some((key, value)) = previous.remove_entry(key)
                {
                    active.entry(key).or_insert(value);
                }
            }
            match op {
                TxnOp::Read(_) => {}
                TxnOp::Write(key, value) => {
                    let new = active.insert(key, value).is_none();
                    self.record_write(usize::from(new), 0);
                }
                TxnOp::Remove(key) => {
                    if active.remove(&key).is_some() {
                        self.record_write(0, 1);
                    }
                }
            }
        }
        TransactionResult::Committed(())
    }

    /// Atomically replaces a matching value. For an absent key, Failure carries
    /// `new` for backward compatibility; it does not indicate a stored value.
    #[cfg(feature = "advanced")]
    pub fn compare_and_swap(&self, key: &K, expected: &V, new: V) -> CasResult<V>
    where
        V: PartialEq,
    {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = std_write_guard(&shard, "cas");
        self.promote_previous(key, &mut guard);
        match guard.get(key) {
            Some(current) if current == expected => {
                guard.insert(key.clone(), new.clone());
                self.record_write(0, 0);
                CasResult::Success(new)
            }
            Some(current) => CasResult::Failure(current.clone()),
            None => CasResult::Failure(new),
        }
    }

    /// Atomically removes a matching value.
    #[cfg(feature = "advanced")]
    pub fn compare_and_remove(&self, key: &K, expected: &V) -> bool
    where
        V: PartialEq,
    {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        let shard = self.get_or_init_shard(self.shard_index(key));
        let mut guard = std_write_guard(&shard, "cas_remove");
        self.promote_previous(key, &mut guard);
        if guard.get(key) == Some(expected) {
            guard.remove(key);
            self.record_write(0, 1);
            true
        } else {
            false
        }
    }

    /// Returns an immutable shared snapshot tagged with its committed write epoch.
    #[cfg(feature = "advanced")]
    pub fn cow_snapshot(&self) -> CowSnapshot<K, V> {
        let (version, data) = self.capture_snapshot();
        CowSnapshot::from_arc(data, version)
    }

    /// Captures the current data version. Repeated snapshots without writes have
    /// the same version. Historical versions are not retained by the map.
    #[cfg(feature = "advanced")]
    pub fn versioned_snapshot(&self) -> IsolatedSnapshot<K, V> {
        let (version, data) = self.capture_snapshot();
        IsolatedSnapshot::from_arc(version, data)
    }

    /// Returns the current snapshot only when `version` equals its write epoch.
    #[cfg(feature = "advanced")]
    pub fn snapshot_at_version(&self, version: u64) -> Option<IsolatedSnapshot<K, V>> {
        if version != self.write_epoch.load(Ordering::Acquire) {
            return None;
        }
        let (captured, data) = self.capture_snapshot();
        (version == captured).then(|| IsolatedSnapshot::from_arc(captured, data))
    }

    /// Lock timing instrumentation is not implemented; returns no samples.
    #[cfg(feature = "advanced")]
    pub fn lock_profiles(&self) -> Vec<LockProfile> {
        Vec::new()
    }

    /// Reserves the profiling preference. No lock timing samples are collected yet.
    #[cfg(feature = "advanced")]
    pub fn enable_profiling(&self, enabled: bool) {
        self.profiling_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Returns active shard distribution statistics. During migration, entries
    /// still in the previous generation are excluded from active utilization.
    pub fn shard_stats(&self) -> ShardStats {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        let slots = std_read_guard(&self.shards, "shards");
        let loads: Vec<_> = slots
            .iter()
            .flatten()
            .map(|s| std_read_guard(s, "stats").len())
            .collect();
        ShardStats {
            initialized: loads.len(),
            total: slots.len(),
            empty: loads.iter().filter(|&&n| n == 0).count(),
            avg_load: if loads.is_empty() {
                0.0
            } else {
                loads.iter().sum::<usize>() as f64 / loads.len() as f64
            },
            max_load: loads.into_iter().max().unwrap_or(0),
        }
    }

    /// Returns active shard allocation percentage.
    pub fn shard_utilization(&self) -> f64 {
        self.shard_stats().utilization_percent()
    }

    /// Returns active shard lengths and capacities.
    #[cfg(feature = "lifecycle")]
    pub fn per_shard_load(&self) -> Vec<PerShardLoad> {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        std_read_guard(&self.shards, "shards")
            .iter()
            .enumerate()
            .filter_map(|(index, s)| {
                s.as_ref().map(|s| {
                    let guard = std_read_guard(s, "stats");
                    PerShardLoad {
                        shard_idx: index,
                        entry_count: guard.len(),
                        capacity: guard.capacity(),
                    }
                })
            })
            .collect()
    }

    /// Returns allocation statistics including previous-generation shards.
    #[cfg(feature = "lifecycle")]
    pub fn memory_stats(&self) -> MemoryStats {
        let _routing = std_read_guard(&self.routing_lock, "routing");
        let shards = self.all_shards();
        let mut total_capacity = 0;
        let mut total_entries = 0;
        for shard in &shards {
            let guard = std_read_guard(shard, "stats");
            total_capacity += guard.capacity();
            total_entries += guard.len();
        }
        MemoryStats {
            shards_allocated: shards.len(),
            total_capacity,
            load_factor: if total_capacity == 0 {
                0.0
            } else {
                total_entries as f64 / total_capacity as f64
            },
        }
    }

    /// Drains both generations while excluding concurrent map operations.
    #[cfg(feature = "lifecycle")]
    pub fn drain(&self) -> DrainIterator<K, V> {
        let _routing = std_write_guard(&self.routing_lock, "routing_drain");
        let mut items = Vec::with_capacity(self.len());
        for shard in self.all_shards() {
            let mut guard = std_write_guard(&shard, "drain");
            let len = guard.len();
            items.extend(guard.drain());
            if len != 0 {
                self.record_write(0, len);
            }
        }
        *std_write_guard(&self.previous_shards, "drain_previous") = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        DrainIterator { items, index: 0 }
    }
}
