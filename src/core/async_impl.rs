//! Asynchronous `AsyncShardedHashMap` implementation.
//!
//! Mirrors the synchronous [`super::sync_impl`] module but uses Tokio
//! `RwLock`/`Mutex` for non-blocking concurrency. Operations pin shard routing
//! while looking up and locking data, so directory changes cannot invalidate indices.
//! Snapshot builders retain ordered shard read locks under shared routing,
//! allowing other readers to proceed while source data remains stable.
//!
//! # Additional async-only features
//!
//! - Replication support (`with_replication`, `insert_replicated`,
//!   `remove_replicated`) behind the `advanced` feature flag.

use super::*;
use std::time::Instant;

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

    /// Create with default hasher and explicit snapshot mode.
    #[tracing::instrument(level = "trace")]
    pub fn with_snapshot_mode(shard_count: usize, mode: SnapshotMode) -> Self {
        Self::with_shards_and_hasher_and_snapshot_mode(shard_count, FxBuildHasher, mode)
    }
}

#[cfg(feature = "async")]
impl<K, V, S> AsyncShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Core constructor: allocates all shard vectors, atomics, and locks.
    ///
    /// All shard slots start as `None` (lazy initialization).
    #[inline]
    fn build_with_count(count: usize, hasher: S, snapshot_mode: SnapshotMode) -> Self {
        Self {
            snapshot_mode,
            shards: Arc::new(TokioRwLock::new(vec![None; count])),
            previous_shards: Arc::new(TokioRwLock::new(None)),
            hasher,
            shard_count: Arc::new(AtomicUsize::new(count)),
            previous_shard_count: Arc::new(AtomicUsize::new(0)),
            total_len: Arc::new(AtomicUsize::new(0)),
            write_epoch: Arc::new(AtomicU64::new(0)),
            snapshot_cache: Arc::new(StdRwLock::new(None)),
            snapshot_build_lock: Arc::new(TokioMutex::new(())),
            routing_lock: Arc::new(TokioRwLock::new(())),
            rebalance_lock: Arc::new(TokioMutex::new(())),
            rebalance_tracker: Arc::new(RebalanceTracker::new()),
            #[cfg(feature = "advanced")]
            profiling_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(feature = "advanced")]
            replicas: Arc::new(StdRwLock::new(Vec::new())),
            #[cfg(feature = "advanced")]
            quorum_config: Arc::new(StdRwLock::new(None)),
            #[cfg(feature = "advanced")]
            replication_state: Arc::new(crate::core::replication::ReplicationState::default()),
        }
    }

    /// Create with custom hasher.
    ///
    /// This preserves backward compatibility while enforcing the default
    /// safety cap (`MAX_SHARDS`) to avoid oversized allocations.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher(shard_count: usize, hasher: S) -> Self {
        Self::with_shards_and_hasher_and_snapshot_mode(shard_count, hasher, SnapshotMode::Clone)
    }

    /// Create with custom hasher and snapshot mode.
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

    /// Create with custom hasher and a custom cap.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn with_shards_and_hasher_capped(shard_count: usize, hasher: S, max_shards: usize) -> Self {
        Self::with_shards_and_hasher_capped_and_snapshot_mode(
            shard_count,
            hasher,
            max_shards,
            SnapshotMode::Clone,
        )
    }

    /// Create with custom hasher, cap and snapshot mode.
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

    /// Strict constructor with custom hasher.
    ///
    /// Returns an error when the requested shard count exceeds `MAX_SHARDS`.
    #[tracing::instrument(skip(hasher), level = "trace")]
    pub fn try_with_shards_and_hasher(
        shard_count: usize,
        hasher: S,
    ) -> Result<Self, ShardCountError> {
        Self::try_with_shards_and_hasher_capped(shard_count, hasher, MAX_SHARDS)
    }

    /// Strict constructor with custom hasher and caller-provided cap.
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

    /// Configured shard capacity.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn shard_count(&self) -> usize {
        self.shard_count.load(Ordering::Relaxed)
    }

    /// Number of initialized shards.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn initialized_shards(&self) -> usize {
        let _routing = self.routing_lock.read().await;
        self.shards
            .read()
            .await
            .iter()
            .filter(|slot| slot.is_some())
            .count()
    }

    /// Current rebalance status snapshot.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn rebalance_status(&self) -> RebalanceStatus {
        self.rebalance_tracker.snapshot()
    }

    /// Resolve a previous-generation shard while the caller pins routing.
    async fn previous_shard<Q>(&self, key: &Q) -> Option<AsyncShard<K, V, S>>
    where
        Q: Hash + ?Sized,
    {
        let count = self.previous_shard_count.load(Ordering::Relaxed);
        if count == 0 {
            return None;
        }
        let index = (self.hasher.hash_one(key) % count as u64) as usize;
        let previous = self.previous_shards.read().await;
        previous.as_ref()?.get(index)?.clone()
    }

    #[inline]
    fn cache_enabled(&self) -> bool {
        !matches!(self.snapshot_mode, SnapshotMode::Clone)
    }

    /// Publish mutation metadata before releasing its shard lock or awaiting again.
    /// Cached snapshots are rebuilt lazily, avoiding copies on the write path.
    fn on_structural_write(&self) {
        self.write_epoch.fetch_add(1, Ordering::Release);
    }

    /// Lock active before previous, then promote the key without an await point.
    /// The caller holds routing and must finish its mutation before awaiting again.
    async fn lock_key<Q>(&self, key: &Q) -> tokio::sync::OwnedRwLockWriteGuard<HashMap<K, V, S>>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        let mut active = shard.write_owned().await;
        if let Some(previous) = self.previous_shard(key).await {
            let mut previous = previous.write().await;
            if let Some((key, value)) = previous.remove_entry(key) {
                active.entry(key).or_insert(value);
            }
        }
        active
    }

    /// Read both generations under the active shard lock. An absent active slot
    /// stays protected by its directory read lock until the fallback finishes,
    /// preventing promotion from slipping between the two lookups.
    async fn read_key<Q, R>(&self, key: &Q, f: impl FnOnce(Option<&V>) -> R) -> R
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let slots = self.shards.read().await;
        if let Some(shard) = slots[self.shard_index(key)].clone() {
            drop(slots);
            let active = shard.read().await;
            if let Some(value) = active.get(key) {
                return f(Some(value));
            }
            if let Some(previous) = self.previous_shard(key).await {
                let previous = previous.read().await;
                return f(previous.get(key));
            }
            f(None)
        } else {
            let result = if let Some(previous) = self.previous_shard(key).await {
                let previous = previous.read().await;
                f(previous.get(key))
            } else {
                f(None)
            };
            drop(slots);
            result
        }
    }

    /// Freeze all existing shards while allowing concurrent reads. The caller
    /// holds the builder gate and owns output before acquiring that gate, so
    /// unwinding drops successfully collected output entries after the map locks.
    /// Temporaries inside user Clone implementations still obey those implementations.
    async fn collect_into<T>(
        &self,
        output: &mut Vec<T>,
        mut project: impl FnMut(&K, &V) -> T,
    ) -> u64 {
        let _routing = self.routing_lock.read().await;
        // Keep empty slots stable too: a cold-shard insertion must wait until
        // this snapshot has captured both its data and mutation version.
        let active = self.shards.read().await;
        let previous = self.previous_shards.read().await;
        let initialized = active.iter().flatten().count()
            + previous
                .as_ref()
                .map_or(0, |slots| slots.iter().flatten().count());
        let mut guards = Vec::with_capacity(initialized);
        // Transactions and promotion acquire active shards before previous
        // shards. Matching that order prevents cycles with their write locks.
        for shard in active.iter().flatten() {
            guards.push(shard.read().await);
        }
        if let Some(previous) = previous.as_ref() {
            for shard in previous.iter().flatten() {
                guards.push(shard.read().await);
            }
        }
        let epoch = self.write_epoch.load(Ordering::Acquire);
        output.reserve_exact(guards.iter().map(|guard| guard.len()).sum());
        // Promotion removes the old entry while holding both shard locks;
        // freezing both generations makes each logical key appear exactly once.
        for guard in &guards {
            for (key, value) in guard.iter() {
                output.push(project(key, value));
            }
        }
        epoch
    }

    async fn collect_with<T>(&self, project: impl FnMut(&K, &V) -> T) -> Vec<T> {
        let mut output = Vec::new();
        let _builder = self.snapshot_build_lock.lock().await;
        self.collect_into(&mut output, project).await;
        output
    }

    /// Validate immutable cached data without consulting or locking routing.
    /// A concurrent write may linearize after this epoch check; a completed write
    /// publishes its new epoch before releasing its shard lock. Misses acquire
    /// all shard read locks and capture data together with its mutation version.
    async fn capture_snapshot(&self) -> CapturedSnapshot<K, V> {
        if self.cache_enabled() {
            // Cache hits do not otherwise touch Tokio resources. Preserve
            // cooperative scheduling without holding a lock across this await.
            tokio::task::coop::consume_budget().await;
            let cached = std_read_guard(&self.snapshot_cache, "async_snapshot_cache");
            let epoch = self.write_epoch.load(Ordering::Acquire);
            if let Some((cached_epoch, entries)) = cached.as_ref()
                && *cached_epoch == epoch
            {
                return (epoch, entries.clone());
            }
        }
        // Declare owned data before the gate so Clone panics release the gate
        // before destroying any successfully collected entries.
        let mut output = Vec::new();
        let _builder = self.snapshot_build_lock.lock().await;
        // Recheck after locking: another snapshot may already have rebuilt
        // the cache, or a writer may have advanced the epoch while we waited.
        let epoch = self.write_epoch.load(Ordering::Acquire);
        if self.cache_enabled() {
            let cached = std_read_guard(&self.snapshot_cache, "async_snapshot_cache");
            if let Some((cached_epoch, entries)) = cached.as_ref()
                && *cached_epoch == epoch
            {
                return (epoch, entries.clone());
            }
        }
        let epoch = self
            .collect_into(&mut output, |key, value| (key.clone(), value.clone()))
            .await;
        let entries = Arc::new(output);
        let retired = if self.cache_enabled() {
            std_write_guard(&self.snapshot_cache, "async_snapshot_cache")
                .replace((epoch, entries.clone()))
        } else {
            None
        };
        // Retired entries may run user destructors, so release the builder gate
        // as well as the already released routing and shard guards first.
        drop(_builder);
        drop(retired);
        (epoch, entries)
    }

    /// Start an online incremental rebalance.
    ///
    /// Writes route to the new active shard epoch immediately; reads fallback to previous
    /// shards until migration is fully advanced via `advance_rebalance`.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn start_rebalance_online(
        &self,
        new_shard_count: usize,
    ) -> Result<(), ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let _rebalance = self.rebalance_lock.lock().await;
        let _routing = self.routing_lock.write().await;
        if self.rebalance_tracker.is_migrating() || target == self.shard_count() {
            return Ok(());
        }
        let mut active = self.shards.write().await;
        let mut previous = self.previous_shards.write().await;
        let current = active.len();
        // All potentially suspending locks precede the directory swap.
        *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
        self.previous_shard_count.store(current, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker.begin(current);
        Ok(())
    }

    /// Advance online rebalance by up to `max_shards` source shards.
    ///
    /// Returns the number of source shards processed. Each complete source shard
    /// is moved under exclusive routing, so a large shard can cause a long pause.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn advance_rebalance(&self, max_shards: usize) -> usize {
        if max_shards == 0 {
            return 0;
        }
        // Declare retired storage before lock guards so cancellation or unwinding
        // also releases all locks before invoking retired values' destructors.
        let mut retired = Vec::new();
        let _rebalance = self.rebalance_lock.lock().await;
        let _routing = self.routing_lock.write().await;
        let mut active = self.shards.write().await;
        let mut previous = self.previous_shards.write().await;
        let Some(previous_slots) = previous.as_mut() else {
            return 0;
        };
        let mut processed = 0;
        for _ in 0..max_shards {
            let status = self.rebalance_tracker.snapshot();
            let index = status.moved_shards;
            if !self.rebalance_tracker.is_migrating() || index >= previous_slots.len() {
                break;
            }
            if let Some(source) = previous_slots[index].clone() {
                let source = source.read_owned().await;
                let mut target_indices: Vec<_> =
                    source.keys().map(|key| self.shard_index(key)).collect();
                target_indices.sort_unstable();
                target_indices.dedup();
                let mut replacements = Vec::with_capacity(target_indices.len());
                for &target_index in &target_indices {
                    let staged = match active[target_index].as_ref() {
                        Some(target) => target.read().await.clone(),
                        None => HashMap::with_hasher(self.hasher.clone()),
                    };
                    replacements.push(staged);
                }
                // User Hash/Clone/Eq can panic. Stage every replacement while
                // source and target maps remain untouched and reachable.
                for (key, value) in source.iter() {
                    let target_index = self.shard_index(key);
                    if let Ok(position) = target_indices.binary_search(&target_index) {
                        replacements[position]
                            .entry(key.clone())
                            .or_insert_with(|| value.clone());
                    }
                }
                let replacements: Vec<_> = replacements
                    .into_iter()
                    .map(|map| Arc::new(TokioRwLock::new(map)))
                    .collect();
                retired.reserve(replacements.len() + 1);
                // Publication calls no user code and contains no await points.
                // Retain replaced maps until progress is committed and locks
                // are released, so user destructors cannot interrupt it.
                for (target_index, replacement) in target_indices.into_iter().zip(replacements) {
                    if let Some(old) = active[target_index].replace(replacement) {
                        retired.push(old);
                    }
                }
            }
            if let Some(source) = previous_slots[index].take() {
                retired.push(source);
            }
            self.rebalance_tracker.step();
            processed += 1;
        }
        if self.rebalance_tracker.snapshot().moved_shards >= previous_slots.len() {
            *previous = None;
            self.previous_shard_count.store(0, Ordering::Relaxed);
            self.rebalance_tracker.finish();
        }
        drop(previous);
        drop(active);
        drop(_routing);
        drop(_rebalance);
        drop(retired);
        processed
    }

    /// Returns the shard index for `key` under the current shard count.
    #[inline]
    #[tracing::instrument(skip(self, key), level = "trace")]
    fn shard_index<Q: Hash + ?Sized>(&self, key: &Q) -> usize {
        (self.hasher.hash_one(key) % self.shard_count() as u64) as usize
    }

    /// Returns the shard at `index`, lazily initializing it if the slot is `None`.
    ///
    /// This is the core lazy-materialization primitive: cold shards cost only
    /// a `None` slot until the first key routes to them.
    #[inline]
    #[tracing::instrument(skip(self), level = "trace")]
    async fn get_or_init_shard(&self, index: usize) -> AsyncShard<K, V, S> {
        if let Some(shard) = self.shards.read().await[index].clone() {
            return shard;
        }
        let mut slots = self.shards.write().await;
        slots[index]
            .get_or_insert_with(|| {
                Arc::new(TokioRwLock::new(HashMap::with_hasher(self.hasher.clone())))
            })
            .clone()
    }

    /// Groups `(K, V)` pairs by target shard index for batch insertion.
    ///
    /// Each shard gets its own `Vec` so we can acquire the shard lock once
    /// and insert all pairs in a single critical section.
    #[inline]
    fn bucketize_entries<I>(&self, entries: I) -> HashMap<usize, Vec<(K, V)>, FxBuildHasher>
    where
        I: IntoIterator<Item = (K, V)>,
    {
        let iter = entries.into_iter();
        let estimated = iter.size_hint().0.min(self.shard_count());
        let mut buckets: HashMap<usize, Vec<(K, V)>, FxBuildHasher> =
            HashMap::with_capacity_and_hasher(estimated, FxBuildHasher);
        for (k, v) in iter {
            let shard_idx = self.shard_index(&k);
            buckets.entry(shard_idx).or_default().push((k, v));
        }
        buckets
    }

    /// Groups owned keys by target shard index for batch removal.
    #[inline]
    fn bucketize_keys<I>(&self, keys: I) -> HashMap<usize, Vec<K>, FxBuildHasher>
    where
        I: IntoIterator<Item = K>,
    {
        let iter = keys.into_iter();
        let estimated = iter.size_hint().0.min(self.shard_count());
        let mut buckets: HashMap<usize, Vec<K>, FxBuildHasher> =
            HashMap::with_capacity_and_hasher(estimated, FxBuildHasher);
        for k in iter {
            let shard_idx = self.shard_index(&k);
            buckets.entry(shard_idx).or_default().push(k);
        }
        buckets
    }

    /// Groups key references (with original index) by target shard for batch reads.
    ///
    /// The original index is preserved so `batch_get` can return results in the
    /// same order as the input key slice.
    #[inline]
    fn bucketize_key_refs<'a>(
        &self,
        keys: &'a [K],
    ) -> HashMap<usize, Vec<(usize, &'a K)>, FxBuildHasher> {
        let estimated = keys.len().min(self.shard_count());
        let mut buckets: HashMap<usize, Vec<(usize, &'a K)>, FxBuildHasher> =
            HashMap::with_capacity_and_hasher(estimated, FxBuildHasher);
        for (idx, key) in keys.iter().enumerate() {
            let shard_idx = self.shard_index(key);
            buckets.entry(shard_idx).or_default().push((idx, key));
        }
        buckets
    }

    /// Rebalance to a new shard count using stop-the-world full migration.
    ///
    /// Routing is pinned exclusively until all entries have been moved.
    #[tracing::instrument(skip(self, options), level = "trace")]
    pub async fn rebalance_to(
        &self,
        new_shard_count: usize,
        options: RebalanceOptions,
    ) -> Result<RebalanceReport, ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let _rebalance = self.rebalance_lock.lock().await;
        let _routing = self.routing_lock.write().await;
        let current = self.shard_count();
        if target == current && !self.rebalance_tracker.is_migrating() {
            return Ok(RebalanceReport {
                from_shards: current,
                to_shards: target,
                moved_entries: 0,
                elapsed_ms: 0,
            });
        }
        let started = Instant::now();
        let mut active = self.shards.write().await;
        let mut previous = self.previous_shards.write().await;
        let mut guards = Vec::new();
        if let Some(slots) = previous.as_ref() {
            for shard in slots.iter().flatten() {
                guards.push(shard.clone().read_owned().await);
            }
        }
        for shard in active.iter().flatten() {
            guards.push(shard.clone().read_owned().await);
        }
        let mut destinations: Vec<Option<HashMap<K, V, S>>> = (0..target).map(|_| None).collect();
        // Stage the entire new generation before replacing either directory.
        // Hash/Clone/Eq panics or cancellation leave both old generations intact.
        // Previous values are copied first so active values win any ties.
        for source in &guards {
            for (key, value) in source.iter() {
                let index = (self.hasher.hash_one(key) % target as u64) as usize;
                destinations[index]
                    .get_or_insert_with(|| HashMap::with_hasher(self.hasher.clone()))
                    .insert(key.clone(), value.clone());
            }
        }
        let moved_entries = destinations.iter().flatten().map(HashMap::len).sum();
        let replacements = destinations
            .into_iter()
            .map(|map| map.map(|map| Arc::new(TokioRwLock::new(map))))
            .collect();
        let retired_active = std::mem::replace(&mut *active, replacements);
        let retired_previous = previous.take();
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.total_len.store(moved_entries, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        drop(guards);
        drop(previous);
        drop(active);
        drop(_routing);
        drop(_rebalance);
        drop(retired_previous);
        drop(retired_active);
        tracing::info!(
            from_shards = current,
            to_shards = target,
            moved_entries,
            background = options.background,
            batch_size = options.batch_size,
            max_pause_ns = options.max_pause_ns,
            "async stop-the-world rebalance completed"
        );
        Ok(RebalanceReport {
            from_shards: current,
            to_shards: target,
            moved_entries,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    /// Returns an entry handle for in-place style operations on a key.
    ///
    /// The returned variant reflects the state observed at call time. Methods
    /// such as [`AsyncEntry::or_insert_with`](crate::AsyncEntry::or_insert_with)
    /// perform their own shard write operation and preserve length/snapshot
    /// metadata.
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn entry(&self, key: K) -> crate::AsyncEntry<'_, K, V, S> {
        if self.contains(&key).await {
            crate::AsyncEntry::occupied(self, key)
        } else {
            crate::AsyncEntry::vacant(self, key)
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
        let _routing = self.routing_lock.read().await;
        self.insert_inner(key, value).await
    }

    async fn insert_inner(&self, key: K, value: V) -> Option<V> {
        let mut guard = self.lock_key(&key).await;
        let old = guard.insert(key, value);
        if old.is_none() {
            self.total_len.fetch_add(1, Ordering::Relaxed);
        }
        self.on_structural_write();
        old
    }

    /// Get a cloned value for an owned-key reference.
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn get(&self, key: &K) -> Option<V> {
        self.get_borrowed(key).await
    }

    /// Get a cloned value using a borrowed key, without initializing empty shards.
    ///
    /// # Arguments
    /// - `key`: key to look up.
    ///
    /// # Returns
    /// - `Option<V>`: cloned value if the key exists.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn get_borrowed<Q>(&self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let _routing = self.routing_lock.read().await;
        self.read_key(key, |value| value.cloned()).await
    }

    /// Project a value under its read lock without cloning the full value.
    ///
    /// The callback must not reenter this map or wait for another operation on
    /// it: the shard and routing locks remain held while the callback runs.
    #[tracing::instrument(skip(self, key, f), level = "trace")]
    pub async fn read_with<Q, R>(&self, key: &Q, f: impl FnOnce(&V) -> R) -> Option<R>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let _routing = self.routing_lock.read().await;
        self.read_key(key, |value| value.map(f)).await
    }

    /// Check whether an owned-key reference exists.
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn contains(&self, key: &K) -> bool {
        self.contains_borrowed(key).await
    }

    /// Check whether a borrowed key exists without cloning its value.
    ///
    /// # Arguments
    /// - `key`: key to check.
    ///
    /// # Returns
    /// - `bool`: true if the key exists in the map, false otherwise.
    ///
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn contains_borrowed<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let _routing = self.routing_lock.read().await;
        self.read_key(key, |value| value.is_some()).await
    }

    /// Remove an entry using an owned-key reference.
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn remove(&self, key: &K) -> Option<V> {
        self.remove_borrowed(key).await
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
    pub async fn remove_borrowed<Q>(&self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let _routing = self.routing_lock.read().await;
        self.remove_inner(key).await
    }

    async fn remove_inner<Q>(&self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut guard = self.lock_key(key).await;
        let old = guard.remove(key);
        if old.is_some() {
            self.total_len.fetch_sub(1, Ordering::Relaxed);
            self.on_structural_write();
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
        let _routing = self.routing_lock.write().await;
        let slots = self.shards.read().await;
        let mut previous = self.previous_shards.write().await;
        let mut guards = Vec::new();
        for shard in slots.iter().flatten() {
            guards.push(shard.clone().write_owned().await);
        }
        if let Some(slots) = previous.as_ref() {
            for shard in slots.iter().flatten() {
                guards.push(shard.clone().write_owned().await);
            }
        }
        let mut removed = Vec::with_capacity(self.total_len.load(Ordering::Relaxed));
        for guard in &mut guards {
            removed.extend(guard.drain());
        }
        let changed = !removed.is_empty();
        *previous = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        self.total_len.store(0, Ordering::Relaxed);
        if changed {
            self.on_structural_write();
        }
        drop(guards);
        drop(previous);
        drop(slots);
        drop(_routing);
        drop(removed);
    }

    /// Materialize a consistent snapshot across active and previous shards.
    ///
    /// Snapshot construction holds all shard read locks. `Cached` and `Cow` reuse
    /// an immutable versioned snapshot until the next write; cache hits use only
    /// the snapshot cache lock. Returning an owned Vec clones cached entries after
    /// routing is released. Use `shared_snapshot()` to share them without cloning.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn iter(&self) -> Vec<(K, V)> {
        if self.cache_enabled() {
            Arc::unwrap_or_clone(self.capture_snapshot().await.1)
        } else {
            self.collect_with(|key, value| (key.clone(), value.clone()))
                .await
        }
    }

    /// Share an immutable, consistent snapshot without cloning cached entries.
    ///
    /// `Cached` and `Cow` reuse the same allocation until a write changes the
    /// mutation version. `Clone` builds a fresh snapshot for each call.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn shared_snapshot(&self) -> Arc<Vec<(K, V)>> {
        self.capture_snapshot().await.1
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
        // User-provided iterators may consult this map while producing items.
        // Consume them before taking any routing or shard lock.
        let entries: Vec<_> = entries.into_iter().collect();
        let _routing = self.routing_lock.read().await;
        if self.rebalance_tracker.is_migrating() {
            let mut inserted = 0;
            for (key, value) in entries {
                inserted += usize::from(self.insert_inner(key, value).await.is_none());
            }
            return inserted;
        }
        let mut count = 0;
        for (index, pairs) in self.bucketize_entries(entries) {
            let shard = self.get_or_init_shard(index).await;
            let mut guard = shard.write().await;
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            for (key, value) in pairs {
                count += usize::from(mutation.insert(key, value).is_none());
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
    #[tracing::instrument(skip(self, keys), level = "trace")]
    pub async fn batch_remove<I>(&self, keys: I) -> usize
    where
        I: IntoIterator<Item = K>,
    {
        // User-provided iterators may consult this map while producing items.
        // Consume them before taking any routing or shard lock.
        let keys: Vec<_> = keys.into_iter().collect();
        let _routing = self.routing_lock.read().await;
        if self.rebalance_tracker.is_migrating() {
            let mut removed = 0;
            for key in keys {
                removed += usize::from(self.remove_inner(&key).await.is_some());
            }
            return removed;
        }
        let mut count = 0;
        for (index, keys) in self.bucketize_keys(keys) {
            let shard = self.shards.read().await[index].clone();
            let Some(shard) = shard else {
                continue;
            };
            let mut guard = shard.write().await;
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            for key in keys {
                count += usize::from(mutation.remove(&key).is_some());
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
    #[tracing::instrument(skip(self, keys), level = "trace")]
    pub async fn batch_get(&self, keys: &[K]) -> Vec<Option<V>> {
        let _routing = self.routing_lock.read().await;
        if self.rebalance_tracker.is_migrating() {
            let mut values = Vec::with_capacity(keys.len());
            for key in keys {
                values.push(self.read_key(key, |value| value.cloned()).await);
            }
            return values;
        }
        let mut results = vec![None; keys.len()];
        for (index, items) in self.bucketize_key_refs(keys) {
            let shard = self.shards.read().await[index].clone();
            let Some(shard) = shard else {
                continue;
            };
            let guard = shard.read().await;
            for (position, key) in items {
                results[position] = guard.get(key).cloned();
            }
        }
        results
    }

    /// Update value only if key is present; remove if closure returns None (async).
    ///
    /// The callback must not reenter this map or wait for another operation on it.
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
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(key).await;
        let old = guard.get(key).cloned()?;
        let result = f(old);
        if let Some(value) = result.as_ref() {
            guard.insert(key.clone(), value.clone());
        } else {
            guard.remove(key);
            self.total_len.fetch_sub(1, Ordering::Relaxed);
        }
        self.on_structural_write();
        result
    }

    /// Insert value only if key is absent; returns final value (async).
    ///
    /// The callback must not reenter this map or wait for another operation on it.
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
        self.get_or_insert_with_inner(key, f).await
    }

    #[inline]
    async fn get_or_insert_with_inner<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(&key).await;
        if let Some(value) = guard.get(&key) {
            return value.clone();
        }
        let value = f();
        guard.insert(key, value.clone());
        self.total_len.fetch_add(1, Ordering::Relaxed);
        self.on_structural_write();
        value
    }

    /// Gets the value for the given key, inserting with `f` if the key does not exist.
    ///
    /// The callback must not reenter this map or wait for another operation on it.
    ///
    /// This shares the same hot-path implementation as [`Self::compute_if_absent`],
    /// preserving online-rebalance fallback semantics, length accounting, and
    /// snapshot publication.
    #[tracing::instrument(skip(self, key, f), level = "trace")]
    pub async fn get_or_insert_with<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        self.get_or_insert_with_inner(key, f).await
    }

    /// Remove entries where predicate returns false (async).
    ///
    /// The callback must not reenter this map or wait for another operation on it.
    ///
    /// Pins routing exclusively so promotion cannot skip the predicate. If
    /// cancelled between shards, completed changes retain correct metadata.
    ///
    /// # Arguments
    /// - `predicate`: function that returns true to keep, false to remove
    ///
    #[tracing::instrument(skip(self, predicate), level = "trace")]
    pub async fn retain<F>(&self, predicate: F)
    where
        F: Fn(&K, &V) -> bool,
    {
        let _routing = self.routing_lock.write().await;
        let mut shards: Vec<_> = self.shards.read().await.iter().flatten().cloned().collect();
        if let Some(previous) = self.previous_shards.read().await.as_ref() {
            shards.extend(previous.iter().flatten().cloned());
        }
        for shard in shards {
            let mut guard = shard.write().await;
            for removed in guard.extract_if(|key, value| !predicate(key, value)) {
                // Commit each removal before another user callback or destructor
                // can panic; cancellation between shards also preserves metadata.
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                self.on_structural_write();
                drop(removed);
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
        let _routing = self.routing_lock.read().await;
        let mut active_indices = Vec::new();
        let previous_count = self.previous_shard_count.load(Ordering::Relaxed);
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
        // Resolve directories before acquiring any shard locks. All writes use
        // ascending active indices followed by ascending previous indices.
        let mut active_shards = Vec::with_capacity(active_indices.len());
        for &index in &active_indices {
            active_shards.push(self.get_or_init_shard(index).await);
        }
        let previous_shards: Vec<_> = {
            let previous = self.previous_shards.read().await;
            previous
                .as_ref()
                .map(|slots| {
                    previous_indices
                        .iter()
                        .filter_map(|&index| {
                            slots[index].as_ref().map(|shard| (index, shard.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut active_guards = Vec::with_capacity(active_shards.len());
        for shard in active_shards {
            active_guards.push(shard.write_owned().await);
        }
        let mut previous_guards = Vec::with_capacity(previous_shards.len());
        for (index, shard) in previous_shards {
            previous_guards.push((index, shard.write_owned().await));
        }
        for op in txn.ops {
            let key = match &op {
                TxnOp::Read(key) | TxnOp::Write(key, _) | TxnOp::Remove(key) => key,
            };
            let index = self.shard_index(key);
            let Ok(position) = active_indices.binary_search(&index) else {
                return TransactionResult::Aborted;
            };
            let active = &mut active_guards[position];
            if previous_count != 0 {
                let previous_index = (self.hasher.hash_one(key) % previous_count as u64) as usize;
                if let Some((_, previous)) = previous_guards
                    .iter_mut()
                    .find(|(index, _)| *index == previous_index)
                    && let Some((key, value)) = previous.remove_entry(key)
                {
                    active.entry(key).or_insert(value);
                }
            }
            match op {
                TxnOp::Read(_) => {}
                TxnOp::Write(key, value) => {
                    if active.insert(key, value).is_none() {
                        self.total_len.fetch_add(1, Ordering::Relaxed);
                    }
                    self.on_structural_write();
                }
                TxnOp::Remove(key) => {
                    if active.remove(&key).is_some() {
                        self.total_len.fetch_sub(1, Ordering::Relaxed);
                        self.on_structural_write();
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
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(key).await;
        match guard.get(key) {
            Some(current) if current == expected => {
                guard.insert(key.clone(), new.clone());
                self.on_structural_write();
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
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(key).await;
        if guard.get(key).is_some_and(|current| current == expected) {
            guard.remove(key);
            self.total_len.fetch_sub(1, Ordering::Relaxed);
            self.on_structural_write();
            true
        } else {
            false
        }
    }

    /// Create a copy-on-write snapshot for minimal-locking reads (async).
    ///
    /// # Returns
    /// - `CowSnapshot<K, V>`: Immutable snapshot of current state.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn cow_snapshot(&self) -> CowSnapshot<K, V> {
        let (epoch, entries) = self.capture_snapshot().await;
        CowSnapshot::from_arc(entries, epoch)
    }

    /// Snapshot the current mutation version. Historical versions are not retained.
    ///
    /// # Returns
    /// - `IsolatedSnapshot<K, V>`: Snapshot with version information.
    #[cfg(feature = "advanced")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn versioned_snapshot(&self) -> IsolatedSnapshot<K, V> {
        let (version, entries) = self.capture_snapshot().await;
        IsolatedSnapshot::from_arc(version, entries)
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
        if self.write_epoch.load(Ordering::Acquire) != version {
            tokio::task::coop::consume_budget().await;
            return None;
        }
        let (epoch, entries) = self.capture_snapshot().await;
        (epoch == version).then(|| IsolatedSnapshot::from_arc(epoch, entries))
    }

    /// Lock timing instrumentation is not implemented; returns no samples.
    #[cfg(feature = "advanced")]
    pub async fn lock_profiles(&self) -> Vec<LockProfile> {
        Vec::new()
    }

    /// Reserves the profiling preference. No lock timing samples are collected yet.
    #[cfg(feature = "advanced")]
    pub fn enable_profiling(&self, enabled: bool) {
        self.profiling_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Iterate over all keys (snapshot-based, async).
    ///
    /// # Returns
    /// - `Vec<K>`: vector of cloned keys
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn keys(&self) -> Vec<K> {
        self.collect_with(|key, _| key.clone()).await
    }

    /// Iterate over all values (snapshot-based, async).
    ///
    /// # Returns
    /// - `Vec<V>`: vector of cloned values
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn values(&self) -> Vec<V> {
        self.collect_with(|_, value| value.clone()).await
    }

    /// Returns statistics about shard distribution and utilization (async).
    ///
    /// # Returns
    /// - `ShardStats`: structure containing shard metrics
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn shard_stats(&self) -> ShardStats {
        let _routing = self.routing_lock.read().await;
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
        let _routing = self.routing_lock.read().await;
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
        let _routing = self.routing_lock.read().await;
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
        let _routing = self.routing_lock.write().await;
        let slots = self.shards.read().await;
        let mut previous = self.previous_shards.write().await;
        let mut guards = Vec::new();
        if let Some(slots) = previous.as_ref() {
            for shard in slots.iter().flatten() {
                guards.push(shard.clone().write_owned().await);
            }
        }
        for shard in slots.iter().flatten() {
            guards.push(shard.clone().write_owned().await);
        }
        let mut entries = Vec::with_capacity(self.total_len.load(Ordering::Relaxed));
        for guard in &mut guards {
            entries.extend(guard.drain());
        }
        *previous = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        self.total_len.store(0, Ordering::Relaxed);
        if !entries.is_empty() {
            self.on_structural_write();
        }
        DrainIterator {
            items: entries,
            index: 0,
        }
    }
}
