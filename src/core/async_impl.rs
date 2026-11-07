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

use super::types::ShardData;
use super::*;

#[cfg(feature = "lifecycle")]
mod eviction_impl;
#[cfg(feature = "lifecycle")]
fn defer_retired_value<V>(retired: &mut Vec<V>, old: Option<V>, tracking: bool) -> bool {
    if let Some(value) = old {
        if tracking {
            retired.push(value);
        } else {
            // Metrics-only hooks cannot call user Hash/Eq/Clone. Preserve the
            // original payload-drop point and avoid allocating retirement storage.
            drop(value);
        }
        true
    } else {
        false
    }
}

mod rebalance_impl;

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
            #[cfg(feature = "lifecycle")]
            lifecycle: Arc::new(crate::eviction::LifecycleState::new()),
            #[cfg(feature = "lifecycle")]
            lifecycle_owner: None,
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

    /// Internal workers retain map storage without retaining the external-owner
    /// token that supervises automatic eviction scheduling.
    pub(crate) fn clone_for_worker(&self) -> Self {
        Self {
            snapshot_mode: self.snapshot_mode,
            shards: self.shards.clone(),
            previous_shards: self.previous_shards.clone(),
            hasher: self.hasher.clone(),
            shard_count: self.shard_count.clone(),
            previous_shard_count: self.previous_shard_count.clone(),
            total_len: self.total_len.clone(),
            write_epoch: self.write_epoch.clone(),
            snapshot_cache: self.snapshot_cache.clone(),
            snapshot_build_lock: self.snapshot_build_lock.clone(),
            routing_lock: self.routing_lock.clone(),
            rebalance_lock: self.rebalance_lock.clone(),
            rebalance_tracker: self.rebalance_tracker.clone(),
            #[cfg(feature = "lifecycle")]
            lifecycle: self.lifecycle.clone(),
            #[cfg(feature = "lifecycle")]
            lifecycle_owner: None,
            #[cfg(feature = "advanced")]
            profiling_enabled: self.profiling_enabled.clone(),
            #[cfg(feature = "advanced")]
            replicas: self.replicas.clone(),
            #[cfg(feature = "advanced")]
            quorum_config: self.quorum_config.clone(),
            #[cfg(feature = "advanced")]
            replication_state: self.replication_state.clone(),
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
    async fn lock_key<Q>(&self, key: &Q) -> tokio::sync::OwnedRwLockWriteGuard<ShardData<K, V, S>>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let shard = self.get_or_init_shard(self.shard_index(key)).await;
        let mut active = shard.write_owned().await;
        if let Some(previous) = self.previous_shard(key).await {
            let mut previous = previous.write().await;
            if previous.contains_key(key) {
                promote_entry(&mut active, &mut previous, key);
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
        let observe = |value: Option<&V>| {
            #[cfg(feature = "lifecycle")]
            self.lifecycle.record_read(key, value.is_some());
            f(value)
        };
        let slots = self.shards.read().await;
        if let Some(shard) = slots[self.shard_index(key)].clone() {
            drop(slots);
            let active = shard.read().await;
            if let Some(value) = active.get(key) {
                return observe(Some(value));
            }
            if let Some(previous) = self.previous_shard(key).await {
                let previous = previous.read().await;
                return observe(previous.get(key));
            }
            observe(None)
        } else {
            let result = if let Some(previous) = self.previous_shard(key).await {
                let previous = previous.read().await;
                observe(previous.get(key))
            } else {
                observe(None)
            };
            drop(slots);
            result
        }
    }

    async fn capture_view_data(&self, shards: &mut Vec<Arc<HashMap<K, V, S>>>) -> (u64, usize) {
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
        let shard_count = active.len();
        shards.reserve_exact(guards.len());
        for guard in &guards {
            shards.push(guard.snapshot());
        }
        (epoch, shard_count)
    }

    /// Capture an immutable view with a coherent mutation version and shard count.
    ///
    /// Cow mode shares shard storage without cloning keys or values. Other modes
    /// clone each initialized shard. Iteration borrows data without map locks;
    /// retaining a Cow view makes subsequent writes copy its touched shards.
    pub async fn snapshot_view(&self) -> SnapshotView<K, V, S> {
        let mut shards = Vec::new();
        let builder = self.snapshot_build_lock.lock().await;
        let (epoch, shard_count) = self.capture_view_data(&mut shards).await;
        drop(builder);
        SnapshotView::new(shards, epoch, shard_count)
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

    async fn collect_with<T>(&self, mut project: impl FnMut(&K, &V) -> T) -> Vec<T> {
        if self.snapshot_mode == SnapshotMode::Cow {
            let view = self.snapshot_view().await;
            let mut output = Vec::with_capacity(view.len());
            output.extend(view.iter().map(|(key, value)| project(key, value)));
            return output;
        }
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
        let mut source_shards = Vec::new();
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
        let epoch = if self.snapshot_mode == SnapshotMode::Cow {
            let (epoch, _) = self.capture_view_data(&mut source_shards).await;
            output.reserve_exact(source_shards.iter().map(|shard| shard.len()).sum());
            // Source Arcs preserve the cut while user Clone runs without map locks.
            output.extend(
                source_shards
                    .iter()
                    .flat_map(|shard| shard.iter())
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            epoch
        } else {
            self.collect_into(&mut output, |key, value| (key.clone(), value.clone()))
                .await
        };
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
                Arc::new(TokioRwLock::new(ShardData::new(
                    HashMap::with_hasher(self.hasher.clone()),
                    self.snapshot_mode,
                )))
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let old = {
            let _routing = self.routing_lock.read().await;
            #[cfg(feature = "lifecycle")]
            let old = self.insert_inner(key, value, &mut retired).await;
            #[cfg(not(feature = "lifecycle"))]
            let old = self.insert_inner(key, value).await;
            old
        };
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        old
    }

    async fn insert_inner(
        &self,
        key: K,
        value: V,
        #[cfg(feature = "lifecycle")] retired: &mut Vec<V>,
    ) -> Option<V> {
        let mut guard = self.lock_key(&key).await;
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let prepared = self.lifecycle.prepare_insert(&key, &value);
        let old = {
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            mutation.insert(key, value)
        };
        #[cfg(feature = "lifecycle")]
        {
            if !self.lifecycle.enabled() {
                self.lifecycle.record_insert(prepared, old.is_some());
                ticket.finish();
                return old;
            }
            let replaced = defer_retired_value(retired, old, true);
            self.lifecycle.record_insert(prepared, replaced);
            ticket.finish();
            if replaced { retired.pop() } else { None }
        }
        #[cfg(not(feature = "lifecycle"))]
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let old = {
            let _routing = self.routing_lock.read().await;
            #[cfg(feature = "lifecycle")]
            let old = self.remove_inner(key, &mut retired).await;
            #[cfg(not(feature = "lifecycle"))]
            let old = self.remove_inner(key).await;
            old
        };
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        old
    }

    async fn remove_inner<Q>(
        &self,
        key: &Q,
        #[cfg(feature = "lifecycle")] retired: &mut Vec<V>,
    ) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut guard = self.lock_key(key).await;
        if self.snapshot_mode == SnapshotMode::Cow && !guard.contains_key(key) {
            return None;
        }
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        let old = {
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            mutation.remove(key)
        };
        #[cfg(feature = "lifecycle")]
        {
            if !self.lifecycle.enabled() {
                if old.is_some() {
                    self.lifecycle.record_remove(key);
                }
                ticket.finish();
                return old;
            }
            let removed = defer_retired_value(retired, old, true);
            if removed {
                self.lifecycle.record_remove(key);
            }
            ticket.finish();
            if removed { retired.pop() } else { None }
        }
        #[cfg(not(feature = "lifecycle"))]
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        let mut removed = Vec::new();
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
        removed.reserve(self.total_len.load(Ordering::Relaxed));
        // Detach every retained Cow shard before committing any drain. A Clone
        // panic on a later shard must leave data and metadata unchanged.
        let mut maps: Vec<&mut HashMap<K, V, S>> =
            guards.iter_mut().map(|guard| &mut ***guard).collect();
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        for map in &mut maps {
            removed.extend(map.drain());
        }
        drop(maps);
        let changed = !removed.is_empty();
        *previous = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        self.total_len.store(0, Ordering::Relaxed);
        if changed {
            self.on_structural_write();
        }
        #[cfg(feature = "lifecycle")]
        {
            for (key, _) in &removed {
                self.lifecycle.record_remove(key);
            }
            ticket.finish();
        }
        drop(guards);
        drop(previous);
        drop(slots);
        drop(_routing);
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        drop(removed);
        #[cfg(feature = "lifecycle")]
        operation.finish();
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let entries: Vec<_> = entries.into_iter().collect();
        let inserted = {
            let _routing = self.routing_lock.read().await;
            let mut count = 0;
            if self.rebalance_tracker.is_migrating() {
                for (key, value) in entries {
                    #[cfg(feature = "lifecycle")]
                    let old = self.insert_inner(key, value, &mut retired).await;
                    #[cfg(not(feature = "lifecycle"))]
                    let old = self.insert_inner(key, value).await;
                    count += usize::from(old.is_none());
                    #[cfg(feature = "lifecycle")]
                    defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                }
            } else {
                for (index, pairs) in self.bucketize_entries(entries) {
                    let shard = self.get_or_init_shard(index).await;
                    let mut guard = shard.write().await;
                    let mut mutation =
                        ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
                    for (key, value) in pairs {
                        #[cfg(feature = "lifecycle")]
                        let ticket = self.lifecycle.mutation();
                        #[cfg(feature = "lifecycle")]
                        let prepared = self.lifecycle.prepare_insert(&key, &value);
                        let old = mutation.insert(key, value);
                        let replaced = old.is_some();
                        #[cfg(feature = "lifecycle")]
                        defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                        count += usize::from(!replaced);
                        #[cfg(feature = "lifecycle")]
                        {
                            self.lifecycle.record_insert(prepared, replaced);
                            ticket.finish();
                        }
                        #[cfg(not(feature = "lifecycle"))]
                        drop(old);
                    }
                }
            }
            count
        };
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        inserted
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let keys: Vec<_> = keys.into_iter().collect();
        let removed = {
            let _routing = self.routing_lock.read().await;
            let mut count = 0;
            if self.rebalance_tracker.is_migrating() {
                for key in keys {
                    #[cfg(feature = "lifecycle")]
                    let old = self.remove_inner(&key, &mut retired).await;
                    #[cfg(not(feature = "lifecycle"))]
                    let old = self.remove_inner(&key).await;
                    count += usize::from(old.is_some());
                    #[cfg(feature = "lifecycle")]
                    defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                }
            } else {
                for (index, keys) in self.bucketize_keys(keys) {
                    let shard = self.shards.read().await[index].clone();
                    let Some(shard) = shard else {
                        continue;
                    };
                    let mut guard = shard.write().await;
                    if self.snapshot_mode == SnapshotMode::Cow
                        && !keys.iter().any(|key| guard.contains_key(key))
                    {
                        continue;
                    }
                    let mut mutation =
                        ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
                    for key in keys {
                        #[cfg(feature = "lifecycle")]
                        let ticket = self.lifecycle.mutation();
                        let old = mutation.remove(&key);
                        let removed = old.is_some();
                        #[cfg(feature = "lifecycle")]
                        defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                        if removed {
                            count += 1;
                            #[cfg(feature = "lifecycle")]
                            self.lifecycle.record_remove(&key);
                        }
                        #[cfg(feature = "lifecycle")]
                        ticket.finish();
                    }
                }
            }
            count
        };
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        removed
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
        let _routing = self.routing_lock.read().await;
        if self.rebalance_tracker.is_migrating() {
            for (position, key) in keys.iter().enumerate() {
                results[position] = self.read_key(key, |value| value.cloned()).await;
            }
            return results;
        }
        for (index, items) in self.bucketize_key_refs(keys) {
            let shard = self.shards.read().await[index].clone();
            let Some(shard) = shard else {
                #[cfg(feature = "lifecycle")]
                for (_, key) in items {
                    self.lifecycle.record_read(key, false);
                }
                continue;
            };
            let guard = shard.read().await;
            for (position, key) in items {
                let value = guard.get(key);
                #[cfg(feature = "lifecycle")]
                self.lifecycle.record_read(key, value.is_some());
                results[position] = value.cloned();
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        let result = self.compute_if_present_inner(key, f).await;
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        result
    }

    async fn compute_if_present_inner<F>(&self, key: &K, f: F) -> Option<V>
    where
        F: FnOnce(V) -> Option<V>,
    {
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let result;
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(key).await;
        let old = guard.get(key);
        #[cfg(feature = "lifecycle")]
        self.lifecycle.record_read(key, old.is_some());
        result = f(old.cloned()?);
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        if let Some(value) = result.as_ref() {
            let stored_key = key.clone();
            let stored_value = value.clone();
            #[cfg(feature = "lifecycle")]
            let prepared = self.lifecycle.prepare_insert(&stored_key, &stored_value);
            let _replaced = {
                let mut mutation =
                    ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
                {
                    let old = mutation.insert(stored_key, stored_value);
                    #[cfg(feature = "lifecycle")]
                    {
                        defer_retired_value(&mut retired, old, self.lifecycle.enabled())
                    }
                    #[cfg(not(feature = "lifecycle"))]
                    {
                        old.is_some()
                    }
                }
            };
            #[cfg(feature = "lifecycle")]
            self.lifecycle.record_insert(prepared, _replaced);
        } else {
            {
                let mut mutation =
                    ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
                let old = mutation.remove(key);
                #[cfg(feature = "lifecycle")]
                defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                #[cfg(not(feature = "lifecycle"))]
                drop(old);
            }
            #[cfg(feature = "lifecycle")]
            self.lifecycle.record_remove(key);
        }
        #[cfg(feature = "lifecycle")]
        ticket.finish();
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        let value = self.get_or_insert_with_inner(key, f).await;
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        value
    }

    #[inline]
    async fn get_or_insert_with_inner<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let value;
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(&key).await;
        if let Some(value) = guard.get(&key) {
            #[cfg(feature = "lifecycle")]
            self.lifecycle.record_read(&key, true);
            return value.clone();
        }
        #[cfg(feature = "lifecycle")]
        self.lifecycle.record_read(&key, false);
        value = f();
        let stored_value = value.clone();
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let prepared = self.lifecycle.prepare_insert(&key, &stored_value);
        let _replaced = {
            let mut mutation = ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
            {
                let old = mutation.insert(key, stored_value);
                #[cfg(feature = "lifecycle")]
                {
                    defer_retired_value(&mut retired, old, self.lifecycle.enabled())
                }
                #[cfg(not(feature = "lifecycle"))]
                {
                    old.is_some()
                }
            }
        };
        #[cfg(feature = "lifecycle")]
        {
            self.lifecycle.record_insert(prepared, _replaced);
            ticket.finish();
        }
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        let value = self.get_or_insert_with_inner(key, f).await;
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        value
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let _routing = self.routing_lock.write().await;
        let mut shards: Vec<_> = self.shards.read().await.iter().flatten().cloned().collect();
        if let Some(previous) = self.previous_shards.read().await.as_ref() {
            shards.extend(previous.iter().flatten().cloned());
        }
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        for shard in shards {
            let mut guard = shard.write().await;
            for removed in guard.extract_if(|key, value| !predicate(key, value)) {
                // Commit each removal before another user callback or destructor
                // can panic; cancellation between shards also preserves metadata.
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                self.on_structural_write();
                #[cfg(feature = "lifecycle")]
                {
                    if self.lifecycle.enabled() {
                        retired.push(removed);
                        self.lifecycle
                            .record_remove(&retired.last().expect("removed pair was retained").0);
                    } else {
                        self.lifecycle.record_remove(&removed.0);
                        drop(removed);
                    }
                }
                #[cfg(not(feature = "lifecycle"))]
                drop(removed);
            }
        }
        #[cfg(feature = "lifecycle")]
        ticket.finish();
        drop(_routing);
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
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
                    && previous.contains_key(key)
                {
                    promote_entry(active, previous, key);
                }
            }
            match op {
                TxnOp::Read(key) => {
                    #[cfg(feature = "lifecycle")]
                    self.lifecycle.record_read(&key, active.contains_key(&key));
                    #[cfg(not(feature = "lifecycle"))]
                    let _ = key;
                }
                TxnOp::Write(key, value) => {
                    #[cfg(feature = "lifecycle")]
                    let ticket = self.lifecycle.mutation();
                    #[cfg(feature = "lifecycle")]
                    let prepared = self.lifecycle.prepare_insert(&key, &value);
                    let mut mutation =
                        ShardMutation::new(active, &self.total_len, &self.write_epoch);
                    let old = mutation.insert(key, value);
                    #[cfg(feature = "lifecycle")]
                    {
                        let replaced =
                            defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                        self.lifecycle.record_insert(prepared, replaced);
                        ticket.finish();
                    }
                    #[cfg(not(feature = "lifecycle"))]
                    drop(old);
                }
                TxnOp::Remove(key) => {
                    if self.snapshot_mode == SnapshotMode::Cow && !active.contains_key(&key) {
                        continue;
                    }
                    #[cfg(feature = "lifecycle")]
                    let ticket = self.lifecycle.mutation();
                    let mut mutation =
                        ShardMutation::new(active, &self.total_len, &self.write_epoch);
                    let old = mutation.remove(&key);
                    #[cfg(feature = "lifecycle")]
                    {
                        if defer_retired_value(&mut retired, old, self.lifecycle.enabled()) {
                            self.lifecycle.record_remove(&key);
                        }
                        ticket.finish();
                    }
                    #[cfg(not(feature = "lifecycle"))]
                    drop(old);
                }
            }
        }
        drop(previous_guards);
        drop(active_guards);
        drop(_routing);
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(key).await;
        #[cfg(feature = "lifecycle")]
        self.lifecycle.record_read(key, guard.contains_key(key));
        let result = match guard.get(key) {
            Some(current) if current == expected => {
                let stored_key = key.clone();
                let stored_value = new.clone();
                #[cfg(feature = "lifecycle")]
                let ticket = self.lifecycle.mutation();
                #[cfg(feature = "lifecycle")]
                let prepared = self.lifecycle.prepare_insert(&stored_key, &stored_value);
                let _replaced = {
                    let mut mutation =
                        ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
                    {
                        let old = mutation.insert(stored_key, stored_value);
                        #[cfg(feature = "lifecycle")]
                        {
                            defer_retired_value(&mut retired, old, self.lifecycle.enabled())
                        }
                        #[cfg(not(feature = "lifecycle"))]
                        {
                            old.is_some()
                        }
                    }
                };
                #[cfg(feature = "lifecycle")]
                {
                    self.lifecycle.record_insert(prepared, _replaced);
                    ticket.finish();
                }
                CasResult::Success(new)
            }
            Some(current) => CasResult::Failure(current.clone()),
            None => CasResult::Failure(new),
        };
        drop(guard);
        drop(_routing);
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        result
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        #[cfg(feature = "lifecycle")]
        let mut retired = Vec::new();
        let _routing = self.routing_lock.read().await;
        let mut guard = self.lock_key(key).await;
        #[cfg(feature = "lifecycle")]
        self.lifecycle.record_read(key, guard.contains_key(key));
        let removed = if guard.get(key).is_some_and(|current| current == expected) {
            #[cfg(feature = "lifecycle")]
            let ticket = self.lifecycle.mutation();
            {
                let mut mutation =
                    ShardMutation::new(&mut guard, &self.total_len, &self.write_epoch);
                let old = mutation.remove(key);
                #[cfg(feature = "lifecycle")]
                defer_retired_value(&mut retired, old, self.lifecycle.enabled());
                #[cfg(not(feature = "lifecycle"))]
                drop(old);
            }
            #[cfg(feature = "lifecycle")]
            {
                self.lifecycle.record_remove(key);
                ticket.finish();
            }
            true
        } else {
            false
        };
        drop(guard);
        drop(_routing);
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        removed
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

    /// Returns active shard distribution statistics. During migration, entries
    /// still in the previous generation are excluded from active utilization.
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

    /// Returns active shard lengths and capacities.
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

    /// Returns allocation statistics including previous-generation shards.
    #[cfg(feature = "lifecycle")]
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn memory_stats(&self) -> MemoryStats {
        let _routing = self.routing_lock.read().await;
        let mut shards: Vec<_> = self.shards.read().await.iter().flatten().cloned().collect();
        if let Some(previous) = self.previous_shards.read().await.as_ref() {
            shards.extend(previous.iter().flatten().cloned());
        }
        let mut total_capacity = 0usize;
        let mut total_entries = 0usize;

        for shard in &shards {
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
            shards_allocated: shards.len(),
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
        #[cfg(feature = "lifecycle")]
        let operation = self.lifecycle.mutation();
        let mut entries = Vec::new();
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
        entries.reserve(self.total_len.load(Ordering::Relaxed));
        // Prepare all Cow copies before the first committed removal.
        let mut maps: Vec<&mut HashMap<K, V, S>> =
            guards.iter_mut().map(|guard| &mut ***guard).collect();
        #[cfg(feature = "lifecycle")]
        let ticket = self.lifecycle.mutation();
        for map in &mut maps {
            entries.extend(map.drain());
        }
        drop(maps);
        *previous = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        self.total_len.store(0, Ordering::Relaxed);
        if !entries.is_empty() {
            self.on_structural_write();
        }
        #[cfg(feature = "lifecycle")]
        {
            for (key, _) in &entries {
                self.lifecycle.record_remove(key);
            }
            ticket.finish();
        }
        drop(guards);
        drop(previous);
        drop(slots);
        drop(_routing);
        #[cfg(feature = "lifecycle")]
        self.enforce_eviction_limits().await;
        #[cfg(feature = "lifecycle")]
        operation.finish();
        DrainIterator {
            items: entries,
            index: 0,
        }
    }
}
