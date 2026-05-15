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
    #[inline]
    fn build_with_count(count: usize, hasher: S, snapshot_mode: SnapshotMode) -> Self {
        Self {
            snapshot_mode,
            shards: Arc::new(TokioRwLock::new(vec![None; count])),
            cow_shards: Arc::new(TokioRwLock::new(vec![None; count])),
            previous_shards: Arc::new(TokioRwLock::new(None)),
            hasher,
            shard_count: Arc::new(AtomicUsize::new(count)),
            previous_shard_count: Arc::new(AtomicUsize::new(0)),
            total_len: Arc::new(AtomicUsize::new(0)),
            write_epoch: Arc::new(AtomicU64::new(0)),
            snapshot_cache: Arc::new(TokioRwLock::new(None)),
            snapshot_cache_epoch: Arc::new(AtomicU64::new(0)),
            rebalance_lock: Arc::new(TokioMutex::new(())),
            rebalance_tracker: Arc::new(RebalanceTracker::new()),
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
        let g = self.shards.read().await;
        g.iter().filter(|o| o.is_some()).count()
    }

    /// Current rebalance status snapshot.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn rebalance_status(&self) -> RebalanceStatus {
        self.rebalance_tracker.snapshot()
    }

    #[inline]
    fn previous_shard_index(&self, key: &K) -> Option<usize> {
        let prev_count = self.previous_shard_count.load(Ordering::Relaxed);
        if prev_count == 0 {
            None
        } else {
            Some((self.hasher.hash_one(key) % prev_count as u64) as usize)
        }
    }

    async fn previous_get(&self, key: &K) -> Option<V> {
        let idx = self.previous_shard_index(key)?;
        let prev = self.previous_shards.read().await;
        let shards = prev.as_ref()?;
        let shard = shards.get(idx)?.as_ref()?.clone();
        drop(prev);
        let guard = shard.read().await;
        guard.get(key).cloned()
    }

    async fn previous_contains(&self, key: &K) -> bool {
        self.previous_get(key).await.is_some()
    }

    async fn previous_remove(&self, key: &K) -> Option<V> {
        let idx = self.previous_shard_index(key)?;
        let prev = self.previous_shards.write().await;
        let shards = prev.as_ref()?;
        let shard = shards.get(idx)?.as_ref()?.clone();
        drop(prev);
        let mut guard = shard.write().await;
        guard.remove(key)
    }

    async fn previous_take(&self, key: &K) -> Option<V> {
        self.previous_remove(key).await
    }

    #[inline]
    fn cache_enabled(&self) -> bool {
        !matches!(self.snapshot_mode, SnapshotMode::Clone)
    }

    #[inline]
    fn cow_enabled(&self) -> bool {
        matches!(self.snapshot_mode, SnapshotMode::Cow)
    }

    async fn invalidate_snapshot_cache(&self) {
        if !self.cache_enabled() {
            return;
        }
        let mut cache = self.snapshot_cache.write().await;
        *cache = None;
    }

    async fn on_structural_write(&self) {
        self.write_epoch.fetch_add(1, Ordering::Relaxed);
        self.invalidate_snapshot_cache().await;
    }

    async fn publish_write_for_touched_shards<I>(&self, touched: I)
    where
        I: IntoIterator<Item = usize>,
    {
        if self.cow_enabled() {
            for idx in touched {
                self.sync_cow_shard_from_active(idx).await;
            }
        }
        self.on_structural_write().await;
    }

    async fn publish_write_for_all_shards(&self) {
        if self.cow_enabled() {
            self.sync_all_cow_shards_from_active().await;
        }
        self.on_structural_write().await;
    }

    async fn sync_all_cow_shards_from_active(&self) {
        let count = self.shard_count();
        let active_slots: Vec<Option<AsyncShard<K, V, S>>> = {
            let slots = self.shards.read().await;
            slots.iter().cloned().collect()
        };
        let mut merged_shards: Vec<HashMap<K, V, S>> = Vec::with_capacity(count);
        for idx in 0..count {
            let base = match active_slots
                .get(idx)
                .and_then(|slot| slot.as_ref())
                .cloned()
            {
                Some(shard) => {
                    let guard = shard.read().await;
                    guard.clone()
                }
                None => HashMap::with_hasher(self.hasher.clone()),
            };
            merged_shards.push(base);
        }

        if self.rebalance_tracker.is_migrating() {
            let prev_shards: Vec<AsyncShard<K, V, S>> = {
                let prev = self.previous_shards.read().await;
                prev.as_ref()
                    .map(|shards| shards.iter().filter_map(|o| o.as_ref().cloned()).collect())
                    .unwrap_or_default()
            };
            for prev_shard in prev_shards {
                let prev_guard = prev_shard.read().await;
                for (k, v) in prev_guard.iter() {
                    let idx = (self.hasher.hash_one(k) % count as u64) as usize;
                    merged_shards[idx]
                        .entry(k.clone())
                        .or_insert_with(|| v.clone());
                }
            }
        }

        for (idx, merged) in merged_shards.into_iter().enumerate() {
            let snapshot = Arc::new(merged);
            let cow_shard = self.get_or_init_cow_shard(idx).await;
            let mut cow_guard = cow_shard.write().await;
            *cow_guard = snapshot;
        }
    }

    async fn get_or_init_cow_shard(&self, index: usize) -> AsyncCowShard<K, V, S> {
        let mut shards = self.cow_shards.write().await;
        if index >= shards.len() {
            shards.resize_with(index + 1, || None);
        }
        let slot = &mut shards[index];
        match slot {
            Some(existing) => existing.clone(),
            None => {
                let shard = Arc::new(TokioRwLock::new(Arc::new(HashMap::with_hasher(
                    self.hasher.clone(),
                ))));
                *slot = Some(shard.clone());
                shard
            }
        }
    }

    async fn sync_cow_shard_from_active(&self, index: usize) {
        if !self.cow_enabled() {
            return;
        }
        let source = self.get_or_init_shard(index).await;
        let guard = source.read().await;
        let mut merged = guard.clone();
        drop(guard);

        // During online rebalance, some keys still reside in previous shards.
        // Merge fallback entries so Cow snapshots remain complete.
        if self.rebalance_tracker.is_migrating() {
            let prev_shards: Vec<AsyncShard<K, V, S>> = {
                let prev = self.previous_shards.read().await;
                prev.as_ref()
                    .map(|shards| shards.iter().filter_map(|o| o.as_ref().cloned()).collect())
                    .unwrap_or_default()
            };

            for prev_shard in prev_shards {
                let prev_guard = prev_shard.read().await;
                for (k, v) in prev_guard.iter() {
                    if self.shard_index(k) == index {
                        merged.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }

        let snapshot = Arc::new(merged);
        let cow_shard = self.get_or_init_cow_shard(index).await;
        let mut cow_guard = cow_shard.write().await;
        *cow_guard = snapshot;
    }

    async fn cow_shard_snapshot(&self, index: usize) -> Arc<HashMap<K, V, S>> {
        let cow_shard = self.get_or_init_cow_shard(index).await;
        {
            let cow_guard = cow_shard.read().await;
            if !cow_guard.is_empty() {
                return cow_guard.clone();
            }
        }

        let source = self.get_or_init_shard(index).await;
        let source_guard = source.read().await;
        let seeded = Arc::new(source_guard.clone());
        drop(source_guard);
        let mut cow_guard = cow_shard.write().await;
        *cow_guard = seeded.clone();
        seeded
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
        let _rebalance_guard = self.rebalance_lock.lock().await;
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        if self.rebalance_status().state == "migrating" {
            return Ok(());
        }
        let current = self.shard_count();
        if target == current {
            return Ok(());
        }

        let mut active = self.shards.write().await;
        let old_active = std::mem::replace(&mut *active, vec![None; target]);
        let total_shards = old_active.len();
        {
            let mut prev = self.previous_shards.write().await;
            *prev = Some(old_active);
        }
        self.previous_shard_count.store(current, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker.begin(total_shards);
        drop(active);
        self.publish_write_for_all_shards().await;
        Ok(())
    }

    /// Advance online rebalance by up to `max_shards` source shards.
    ///
    /// Returns number of source shards processed in this call.
    #[tracing::instrument(skip(self), level = "trace")]
    pub async fn advance_rebalance(&self, max_shards: usize) -> usize {
        if max_shards == 0 || self.rebalance_status().state != "migrating" {
            return 0;
        }

        let mut processed = 0usize;
        for _ in 0..max_shards {
            let status = self.rebalance_tracker.snapshot();
            if status.state != "migrating" || status.moved_shards >= status.total_shards {
                break;
            }
            let idx = status.moved_shards;
            let source_shard = {
                let mut prev = self.previous_shards.write().await;
                let Some(shards) = prev.as_mut() else {
                    break;
                };
                if idx >= shards.len() {
                    break;
                }
                shards[idx].take()
            };

            if let Some(shard) = source_shard {
                let snapshot: Vec<(K, V)> = {
                    let guard = shard.read().await;
                    guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                };
                for (k, v) in snapshot {
                    let target_idx = self.shard_index(&k);
                    let target = self.get_or_init_shard(target_idx).await;
                    let mut guard = target.write().await;
                    guard.entry(k).or_insert(v);
                }
            }

            self.rebalance_tracker.step();
            processed += 1;
        }

        let status = self.rebalance_tracker.snapshot();
        if status.state == "migrating" && status.moved_shards >= status.total_shards {
            {
                let mut prev = self.previous_shards.write().await;
                *prev = None;
            }
            self.previous_shard_count.store(0, Ordering::Relaxed);
            self.rebalance_tracker.finish();
        }

        if processed > 0 {
            self.publish_write_for_all_shards().await;
        }

        processed
    }

    #[inline]
    #[tracing::instrument(skip(self, key), level = "trace")]
    fn shard_index(&self, key: &K) -> usize {
        (self.hasher.hash_one(key) % self.shard_count() as u64) as usize
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
    /// During migration, operations are blocked by holding the shard-vector write lock.
    #[tracing::instrument(skip(self, options), level = "trace")]
    pub async fn rebalance_to(
        &self,
        new_shard_count: usize,
        options: RebalanceOptions,
    ) -> Result<RebalanceReport, ShardCountError> {
        let _rebalance_guard = self.rebalance_lock.lock().await;
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let current = self.shard_count();
        if target == current {
            return Ok(RebalanceReport {
                from_shards: current,
                to_shards: target,
                moved_entries: 0,
                elapsed_ms: 0,
            });
        }

        let started = Instant::now();
        let mut old_slots = self.shards.write().await;
        let mut prev_slots = self.previous_shards.write().await;
        let total_prev = prev_slots.as_ref().map_or(0, Vec::len);
        self.rebalance_tracker.begin(old_slots.len() + total_prev);
        let mut new_slots: Vec<Option<AsyncShard<K, V, S>>> = vec![None; target];
        let mut moved_entries = 0usize;

        for shard_opt in old_slots.iter() {
            if let Some(shard) = shard_opt {
                let guard = shard.read().await;
                for (k, v) in guard.iter() {
                    let new_idx = (self.hasher.hash_one(k) % target as u64) as usize;
                    if new_slots[new_idx].is_none() {
                        let map = AsyncShardMap::with_hasher(self.hasher.clone());
                        new_slots[new_idx] = Some(Arc::new(TokioRwLock::new(map)));
                    }
                    if let Some(dest) = new_slots[new_idx].as_ref() {
                        let mut dest_guard = dest.write().await;
                        dest_guard.insert(k.clone(), v.clone());
                        moved_entries += 1;
                    }
                }
            }
            self.rebalance_tracker.step();
        }

        if let Some(prev_vec) = prev_slots.as_ref() {
            for shard_opt in prev_vec {
                if let Some(shard) = shard_opt {
                    let guard = shard.read().await;
                    for (k, v) in guard.iter() {
                        let new_idx = (self.hasher.hash_one(k) % target as u64) as usize;
                        if new_slots[new_idx].is_none() {
                            let map = AsyncShardMap::with_hasher(self.hasher.clone());
                            new_slots[new_idx] = Some(Arc::new(TokioRwLock::new(map)));
                        }
                        if let Some(dest) = new_slots[new_idx].as_ref() {
                            let mut dest_guard = dest.write().await;
                            if dest_guard.insert(k.clone(), v.clone()).is_none() {
                                moved_entries += 1;
                            }
                        }
                    }
                }
                self.rebalance_tracker.step();
            }
        }

        *old_slots = new_slots;
        *prev_slots = None;
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker.finish();

        tracing::info!(
            from_shards = current,
            to_shards = target,
            moved_entries,
            background = options.background,
            batch_size = options.batch_size,
            max_pause_ns = options.max_pause_ns,
            "async stop-the-world rebalance completed"
        );
        drop(prev_slots);
        drop(old_slots);
        self.publish_write_for_all_shards().await;

        Ok(RebalanceReport {
            from_shards: current,
            to_shards: target,
            moved_entries,
            elapsed_ms: started.elapsed().as_millis(),
        })
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
        let lookup_key = key.clone();
        let shard_idx = self.shard_index(&key);
        let shard = self.get_or_init_shard(shard_idx).await;
        let old = {
            let mut guard: TokioWriteGuard<'_, HashMap<K, V, S>> = shard.write().await;
            guard.insert(key, value)
        };
        if old.is_none() {
            let previous_old = self.previous_take(&lookup_key).await;
            if previous_old.is_none() {
                self.total_len.fetch_add(1, Ordering::Relaxed);
            }
            self.publish_write_for_touched_shards([shard_idx]).await;
            previous_old
        } else {
            self.publish_write_for_touched_shards([shard_idx]).await;
            old
        }
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
            if let Some(v) = g.get(key) {
                return Some(v.clone());
            }
        } else {
            let g = shard.read().await;
            if let Some(v) = g.get(key) {
                return Some(v.clone());
            }
        }
        self.previous_get(key).await
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
            if g.contains_key(key) {
                return true;
            }
        } else {
            let g = shard.read().await;
            if g.contains_key(key) {
                return true;
            }
        }
        self.previous_contains(key).await
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
        let shard_idx = self.shard_index(key);
        let shard = self.get_or_init_shard(shard_idx).await;
        let old = {
            let mut g = shard.write().await;
            g.remove(key)
        };
        if let Some(old_val) = old {
            self.total_len.fetch_sub(1, Ordering::Relaxed);
            let _ = self.previous_remove(key).await;
            self.publish_write_for_touched_shards([shard_idx]).await;
            Some(old_val)
        } else {
            let prev = self.previous_remove(key).await;
            if prev.is_some() {
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                self.publish_write_for_touched_shards([shard_idx]).await;
            }
            prev
        }
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
        let mut changed = false;
        {
            let slots = self.shards.read().await;
            for shard in slots.iter().flatten() {
                let mut g = shard.write().await;
                changed |= !g.is_empty();
                g.clear();
            }
        }
        {
            let prev = self.previous_shards.write().await;
            if let Some(prev_shards) = prev.as_ref() {
                for shard in prev_shards.iter().flatten() {
                    let mut g = shard.write().await;
                    changed |= !g.is_empty();
                    g.clear();
                }
            }
        }
        {
            let mut prev = self.previous_shards.write().await;
            *prev = None;
        }
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        self.total_len.store(0, Ordering::Relaxed);
        if changed {
            self.publish_write_for_all_shards().await;
        }
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
        let start_epoch = self.write_epoch.load(Ordering::Relaxed);
        if self.cache_enabled() {
            let cached_epoch = self.snapshot_cache_epoch.load(Ordering::Relaxed);
            if cached_epoch == start_epoch {
                let cache = self.snapshot_cache.read().await;
                if let Some(entries) = cache.as_ref() {
                    return entries.as_ref().clone();
                }
            }
        }

        let items: Vec<(K, V)> = if self.cow_enabled() {
            let shard_count = self.shard_count();
            let mut snapshots = Vec::new();
            for i in 0..shard_count {
                let snapshot = self.cow_shard_snapshot(i).await;
                if !snapshot.is_empty() {
                    snapshots.push(snapshot);
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
        } else {
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
        };

        if self.cache_enabled() {
            let end_epoch = self.write_epoch.load(Ordering::Relaxed);
            if start_epoch == end_epoch {
                let mut cache = self.snapshot_cache.write().await;
                *cache = Some(Arc::new(items.clone()));
                self.snapshot_cache_epoch
                    .store(end_epoch, Ordering::Relaxed);
            }
        }

        items
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
        if self.rebalance_tracker.is_migrating() {
            let mut inserted = 0usize;
            for (k, v) in entries {
                if self.insert(k, v).await.is_none() {
                    inserted += 1;
                }
            }
            return inserted;
        }

        let buckets = self.bucketize_entries(entries);
        let mut count = 0;
        let mut touched = Vec::new();
        for (shard_idx, pairs) in buckets {
            let shard = self.get_or_init_shard(shard_idx).await;
            let mut guard = shard.write().await;
            let mut shard_changed = false;
            for (k, v) in pairs {
                if guard.insert(k, v).is_none() {
                    count += 1;
                }
                shard_changed = true;
            }
            if shard_changed {
                touched.push(shard_idx);
            }
        }
        if count > 0 {
            self.total_len.fetch_add(count, Ordering::Relaxed);
        }
        if !touched.is_empty() {
            self.publish_write_for_touched_shards(touched).await;
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
        if self.rebalance_tracker.is_migrating() {
            let mut removed = 0usize;
            for k in keys {
                if self.remove(&k).await.is_some() {
                    removed += 1;
                }
            }
            return removed;
        }

        let buckets = self.bucketize_keys(keys);
        let mut count = 0;
        let mut touched = Vec::new();
        for (shard_idx, keys) in buckets {
            let shard = self.get_or_init_shard(shard_idx).await;
            let mut guard = shard.write().await;
            let mut shard_changed = false;
            for k in keys {
                if guard.remove(&k).is_some() {
                    count += 1;
                    shard_changed = true;
                }
            }
            if shard_changed {
                touched.push(shard_idx);
            }
        }
        if count > 0 {
            self.total_len.fetch_sub(count, Ordering::Relaxed);
            self.publish_write_for_touched_shards(touched).await;
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
        if self.rebalance_tracker.is_migrating() {
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                out.push(self.get(k).await);
            }
            return out;
        }

        let mut results = vec![None; keys.len()];
        let buckets = self.bucketize_key_refs(keys);
        for (shard_idx, items) in buckets {
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
        if self.rebalance_tracker.is_migrating() {
            let current = self.get(key).await?;
            if let Some(new_v) = f(current) {
                let result = new_v.clone();
                let _ = self.insert(key.clone(), new_v).await;
                return Some(result);
            }
            let _ = self.remove(key).await;
            return None;
        }

        let shard_idx = self.shard_index(key);
        let shard = self.get_or_init_shard(shard_idx).await;
        let mut guard = shard.write().await;

        let old_v = guard.get(key).cloned()?;
        if let Some(new_v) = f(old_v) {
            let result = new_v.clone();
            guard.insert(key.clone(), new_v);
            drop(guard);
            self.publish_write_for_touched_shards([shard_idx]).await;
            Some(result)
        } else {
            guard.remove(key);
            self.total_len.fetch_sub(1, Ordering::Relaxed);
            drop(guard);
            self.publish_write_for_touched_shards([shard_idx]).await;
            None
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
        if self.rebalance_tracker.is_migrating() {
            if let Some(existing) = self.get(&key).await {
                return existing;
            }
            let new_v = f();
            let _ = self.insert(key, new_v.clone()).await;
            return new_v;
        }

        let shard_idx = self.shard_index(&key);
        let shard = self.get_or_init_shard(shard_idx).await;
        let mut guard = shard.write().await;

        if let Some(v) = guard.get(&key) {
            v.clone()
        } else {
            let new_v = f();
            guard.insert(key, new_v.clone());
            self.total_len.fetch_add(1, Ordering::Relaxed);
            drop(guard);
            self.publish_write_for_touched_shards([shard_idx]).await;
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

        let mut removed_total = 0usize;
        for shard in shards_snapshot {
            let mut guard = shard.write().await;
            let before = guard.len();
            guard.retain(|k, v| predicate(k, v));
            removed_total += before - guard.len();
        }
        if removed_total > 0 {
            self.total_len.fetch_sub(removed_total, Ordering::Relaxed);
            self.publish_write_for_all_shards().await;
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
        let mut changed = false;
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
                    changed = true;
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
                        changed = true;
                    }
                }
            }
        }

        drop(guards);
        if changed {
            self.publish_write_for_touched_shards(shard_indices).await;
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
        let shard_idx = self.shard_index(key);
        let shard = self.get_or_init_shard(shard_idx).await;
        let mut guard = shard.write().await;

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.insert(key.clone(), new.clone());
                drop(guard);
                self.publish_write_for_touched_shards([shard_idx]).await;
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
        let shard_idx = self.shard_index(key);
        let shard = self.get_or_init_shard(shard_idx).await;
        let mut guard = shard.write().await;

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.remove(key);
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                drop(guard);
                self.publish_write_for_touched_shards([shard_idx]).await;
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
        if self.cache_enabled() {
            let cache_epoch = self.snapshot_cache_epoch.load(Ordering::Relaxed);
            let write_epoch = self.write_epoch.load(Ordering::Relaxed);
            if cache_epoch == write_epoch {
                let cache = self.snapshot_cache.read().await;
                if let Some(entries) = cache.as_ref() {
                    return CowSnapshot::from_arc(entries.clone(), cache_epoch);
                }
            }
        }

        // Best-effort stable epoch labeling: retry a few times if writes overlap snapshot build.
        for _ in 0..3 {
            let begin = self.write_epoch.load(Ordering::Relaxed);
            let data = self.iter().await;
            let end = self.write_epoch.load(Ordering::Relaxed);
            if begin == end {
                return CowSnapshot::new(data, end);
            }
        }

        let data = self.iter().await;
        let version = self.write_epoch.load(Ordering::Relaxed);
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
        let mut items = Vec::new();
        let mut changed = false;

        {
            let slots = self.shards.read().await;
            for shard in slots.iter().flatten() {
                let mut guard = shard.write().await;
                changed |= !guard.is_empty();
                items.extend(guard.drain());
            }
        }

        self.total_len.store(0, Ordering::Relaxed);
        if changed {
            self.publish_write_for_all_shards().await;
        }

        DrainIterator { items, index: 0 }
    }
}
