use super::*;
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
    #[inline]
    fn build_with_count(count: usize, hasher: S, snapshot_mode: SnapshotMode) -> Self {
        let shards = vec![None; count];
        Self {
            snapshot_mode,
            shards: Arc::new(StdRwLock::new(shards)),
            cow_shards: Arc::new(StdRwLock::new(vec![None; count])),
            previous_shards: Arc::new(StdRwLock::new(None)),
            hasher,
            shard_count: Arc::new(AtomicUsize::new(count)),
            previous_shard_count: Arc::new(AtomicUsize::new(0)),
            total_len: Arc::new(AtomicUsize::new(0)),
            write_epoch: Arc::new(AtomicU64::new(0)),
            snapshot_cache: Arc::new(StdRwLock::new(None)),
            snapshot_cache_epoch: Arc::new(AtomicU64::new(0)),
            rebalance_lock: Arc::new(StdMutex::new(())),
            rebalance_tracker: Arc::new(RebalanceTracker::new()),
            #[cfg(feature = "advanced")]
            version: Arc::new(AtomicUsize::new(0)),
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
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn shard_count(&self) -> usize {
        self.shard_count.load(Ordering::Relaxed)
    }

    /// Number of shards actually initialized (allocated).
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn initialized_shards(&self) -> usize {
        let g = std_read_guard(&self.shards, "shards");
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

    fn previous_get(&self, key: &K) -> Option<V> {
        let idx = self.previous_shard_index(key)?;
        let prev = std_read_guard(&self.previous_shards, "previous_shards_read");
        let shards = prev.as_ref()?;
        let shard = shards.get(idx)?.as_ref()?.clone();
        let guard = std_read_guard(&shard, "previous_shard_read");
        guard.get(key).cloned()
    }

    fn previous_contains(&self, key: &K) -> bool {
        self.previous_get(key).is_some()
    }

    fn previous_remove(&self, key: &K) -> Option<V> {
        let idx = self.previous_shard_index(key)?;
        let prev = std_write_guard(&self.previous_shards, "previous_shards_write");
        let shards = prev.as_ref()?;
        let shard = shards.get(idx)?.as_ref()?.clone();
        drop(prev);
        let mut guard = std_write_guard(&shard, "previous_shard_write");
        guard.remove(key)
    }

    fn previous_take(&self, key: &K) -> Option<V> {
        self.previous_remove(key)
    }

    #[inline]
    fn cache_enabled(&self) -> bool {
        !matches!(self.snapshot_mode, SnapshotMode::Clone)
    }

    #[inline]
    fn cow_enabled(&self) -> bool {
        matches!(self.snapshot_mode, SnapshotMode::Cow)
    }

    fn invalidate_snapshot_cache(&self) {
        if !self.cache_enabled() {
            return;
        }
        let mut cache = std_write_guard(&self.snapshot_cache, "snapshot_cache");
        *cache = None;
    }

    fn on_structural_write(&self) {
        self.write_epoch.fetch_add(1, Ordering::Relaxed);
        self.invalidate_snapshot_cache();
    }

    fn publish_write_for_touched_shards<I>(&self, touched: I)
    where
        I: IntoIterator<Item = usize>,
    {
        if self.cow_enabled() {
            for idx in touched {
                self.sync_cow_shard_from_active(idx);
            }
        }
        self.on_structural_write();
    }

    fn publish_write_for_all_shards(&self) {
        if self.cow_enabled() {
            self.sync_all_cow_shards_from_active();
        }
        self.on_structural_write();
    }

    fn sync_all_cow_shards_from_active(&self) {
        let count = self.shard_count();
        let active_slots: Vec<Option<StdShard<K, V, S>>> = {
            let slots = std_read_guard(&self.shards, "cow_sync_all_active_slots");
            slots.iter().cloned().collect()
        };
        let mut merged_shards: Vec<HashMap<K, V, S>> = Vec::with_capacity(count);
        for idx in 0..count {
            let base = match active_slots.get(idx).and_then(|slot| slot.as_ref()).cloned() {
                Some(shard) => {
                    let guard = std_read_guard(&shard, "cow_sync_all_active_shard");
                    guard.clone()
                }
                None => HashMap::with_hasher(self.hasher.clone()),
            };
            merged_shards.push(base);
        }

        if self.rebalance_tracker.is_migrating() {
            let prev = std_read_guard(&self.previous_shards, "cow_sync_all_previous");
            if let Some(prev_shards) = prev.as_ref() {
                for prev_shard in prev_shards.iter().flatten() {
                    let prev_guard = std_read_guard(prev_shard, "cow_sync_all_previous_shard");
                    for (k, v) in prev_guard.iter() {
                        let idx = (self.hasher.hash_one(k) % count as u64) as usize;
                        merged_shards[idx]
                            .entry(k.clone())
                            .or_insert_with(|| v.clone());
                    }
                }
            }
        }

        for (idx, merged) in merged_shards.into_iter().enumerate() {
            let snapshot = Arc::new(merged);
            let cow_shard = self.get_or_init_cow_shard(idx);
            let mut cow_guard = std_write_guard(&cow_shard, "cow_sync_all_target");
            *cow_guard = snapshot;
        }
    }

    fn get_or_init_cow_shard(&self, index: usize) -> StdCowShard<K, V, S> {
        let mut shards = std_write_guard(&self.cow_shards, "cow_shards");
        if index >= shards.len() {
            shards.resize_with(index + 1, || None);
        }
        let slot = &mut shards[index];
        match slot {
            Some(existing) => existing.clone(),
            None => {
                let shard = Arc::new(StdRwLock::new(Arc::new(HashMap::with_hasher(
                    self.hasher.clone(),
                ))));
                *slot = Some(shard.clone());
                shard
            }
        }
    }

    fn sync_cow_shard_from_active(&self, index: usize) {
        if !self.cow_enabled() {
            return;
        }
        let source = self.get_or_init_shard(index);
        let guard = std_read_guard(&source, "cow_sync_source");
        let mut merged = guard.clone();
        drop(guard);

        // During online rebalance, unmigrated entries still live in previous shards.
        // Keep COW views complete by merging fallback entries that route to this active shard.
        if self.rebalance_tracker.is_migrating() {
            let prev = std_read_guard(&self.previous_shards, "cow_sync_previous");
            if let Some(prev_shards) = prev.as_ref() {
                for prev_shard in prev_shards.iter().flatten() {
                    let prev_guard = std_read_guard(prev_shard, "cow_sync_previous_shard");
                    for (k, v) in prev_guard.iter() {
                        if self.shard_index(k) == index {
                            merged.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
            }
        }

        let snapshot = Arc::new(merged);
        let cow_shard = self.get_or_init_cow_shard(index);
        let mut cow_guard = std_write_guard(&cow_shard, "cow_sync_target");
        *cow_guard = snapshot;
    }

    fn cow_shard_snapshot(&self, index: usize) -> Arc<HashMap<K, V, S>> {
        let cow_shard = self.get_or_init_cow_shard(index);
        {
            let cow_guard = std_read_guard(&cow_shard, "cow_shard_read");
            if !cow_guard.is_empty() {
                return cow_guard.clone();
            }
        }

        let source = self.get_or_init_shard(index);
        let source_guard = std_read_guard(&source, "cow_shard_seed");
        let seeded = Arc::new(source_guard.clone());
        drop(source_guard);
        let mut cow_guard = std_write_guard(&cow_shard, "cow_shard_seed_write");
        *cow_guard = seeded.clone();
        seeded
    }

    /// Start an online incremental rebalance.
    ///
    /// Writes route to the new active shard epoch immediately; reads fallback to previous
    /// shards until migration is fully advanced via `advance_rebalance`.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn start_rebalance_online(&self, new_shard_count: usize) -> Result<(), ShardCountError> {
        let _rebalance_guard = self
            .rebalance_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        if self.rebalance_status().state == "migrating" {
            return Ok(());
        }
        let current = self.shard_count();
        if target == current {
            return Ok(());
        }

        let mut active = std_write_guard(&self.shards, "start_online_rebalance_active");
        let old_active = std::mem::replace(&mut *active, vec![None; target]);
        let total_shards = old_active.len();
        {
            let mut prev = std_write_guard(&self.previous_shards, "start_online_rebalance_prev");
            *prev = Some(old_active);
        }
        self.previous_shard_count.store(current, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker.begin(total_shards);
        drop(active);
        self.publish_write_for_all_shards();
        Ok(())
    }

    /// Advance online rebalance by up to `max_shards` source shards.
    ///
    /// Returns number of source shards processed in this call.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn advance_rebalance(&self, max_shards: usize) -> usize {
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
                let mut prev =
                    std_write_guard(&self.previous_shards, "advance_rebalance_prev_take");
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
                    let guard = std_read_guard(&shard, "advance_rebalance_source_read");
                    guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                };
                for (k, v) in snapshot {
                    let target_idx = self.shard_index(&k);
                    let target = self.get_or_init_shard(target_idx);
                    let mut guard = std_write_guard(&target, "advance_rebalance_target_write");
                    guard.entry(k).or_insert(v);
                }
            }

            self.rebalance_tracker.step();
            processed += 1;
        }

        let status = self.rebalance_tracker.snapshot();
        if status.state == "migrating" && status.moved_shards >= status.total_shards {
            {
                let mut prev =
                    std_write_guard(&self.previous_shards, "advance_rebalance_finish_prev");
                *prev = None;
            }
            self.previous_shard_count.store(0, Ordering::Relaxed);
            self.rebalance_tracker.finish();
        }

        if processed > 0 {
            self.publish_write_for_all_shards();
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
    pub fn rebalance_to(
        &self,
        new_shard_count: usize,
        options: RebalanceOptions,
    ) -> Result<RebalanceReport, ShardCountError> {
        let _rebalance_guard = self
            .rebalance_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let mut old_slots = std_write_guard(&self.shards, "rebalance_shards");
        let mut prev_slots = std_write_guard(&self.previous_shards, "rebalance_previous_shards");
        let total_prev = prev_slots.as_ref().map_or(0, Vec::len);
        self.rebalance_tracker.begin(old_slots.len() + total_prev);
        let mut new_slots: Vec<Option<StdShard<K, V, S>>> = vec![None; target];
        let mut moved_entries = 0usize;

        for shard_opt in old_slots.iter() {
            if let Some(shard) = shard_opt {
                let guard = std_read_guard(shard, "rebalance_source_shard");
                for (k, v) in guard.iter() {
                    let new_idx = (self.hasher.hash_one(k) % target as u64) as usize;
                    if new_slots[new_idx].is_none() {
                        let map = StdShardMap::with_hasher(self.hasher.clone());
                        new_slots[new_idx] = Some(Arc::new(StdRwLock::new(map)));
                    }
                    if let Some(dest) = new_slots[new_idx].as_ref() {
                        let mut dest_guard = std_write_guard(dest, "rebalance_target_shard");
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
                    let guard = std_read_guard(shard, "rebalance_previous_source_shard");
                    for (k, v) in guard.iter() {
                        let new_idx = (self.hasher.hash_one(k) % target as u64) as usize;
                        if new_slots[new_idx].is_none() {
                            let map = StdShardMap::with_hasher(self.hasher.clone());
                            new_slots[new_idx] = Some(Arc::new(StdRwLock::new(map)));
                        }
                        if let Some(dest) = new_slots[new_idx].as_ref() {
                            let mut dest_guard =
                                std_write_guard(dest, "rebalance_previous_target_shard");
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
            "sync stop-the-world rebalance completed"
        );
        drop(prev_slots);
        drop(old_slots);
        self.publish_write_for_all_shards();

        Ok(RebalanceReport {
            from_shards: current,
            to_shards: target,
            moved_entries,
            elapsed_ms: started.elapsed().as_millis(),
        })
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
        let lookup_key = key.clone();
        let shard_idx = self.shard_index(&key);
        let shard = self.get_or_init_shard(shard_idx);
        let old = {
            let mut guard: StdWriteGuard<'_, HashMap<K, V, S>> = std_write_guard(&shard, "shard");
            guard.insert(key, value)
        };
        if old.is_none() {
            let previous_old = self.previous_take(&lookup_key);
            if previous_old.is_none() {
                self.total_len.fetch_add(1, Ordering::Relaxed);
            }
            self.publish_write_for_touched_shards([shard_idx]);
            previous_old
        } else {
            self.publish_write_for_touched_shards([shard_idx]);
            old
        }
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
        if let Some(v) = guard.get(key) {
            Some(v.clone())
        } else {
            drop(guard);
            self.previous_get(key)
        }
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
        if guard.contains_key(key) {
            true
        } else {
            drop(guard);
            self.previous_contains(key)
        }
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
        let shard_idx = self.shard_index(key);
        let shard = self.get_or_init_shard(shard_idx);
        let old = {
            let mut guard: StdWriteGuard<'_, HashMap<K, V, S>> = std_write_guard(&shard, "shard");
            guard.remove(key)
        };
        if let Some(old_val) = old {
            self.total_len.fetch_sub(1, Ordering::Relaxed);
            let _ = self.previous_remove(key);
            self.publish_write_for_touched_shards([shard_idx]);
            Some(old_val)
        } else {
            let prev = self.previous_remove(key);
            if prev.is_some() {
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                self.publish_write_for_touched_shards([shard_idx]);
            }
            prev
        }
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
        let mut changed = false;
        {
            let slots = std_read_guard(&self.shards, "shards");
            for shard in slots.iter().flatten() {
                let mut g = std_write_guard(shard, "shard");
                changed |= !g.is_empty();
                g.clear();
            }
        }
        {
            let prev = std_write_guard(&self.previous_shards, "clear_previous_shards");
            if let Some(prev_shards) = prev.as_ref() {
                for shard in prev_shards.iter().flatten() {
                    let mut g = std_write_guard(shard, "previous_shard");
                    changed |= !g.is_empty();
                    g.clear();
                }
            }
        }
        {
            let mut prev = std_write_guard(&self.previous_shards, "clear_previous_shards_reset");
            *prev = None;
        }
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.rebalance_tracker.finish();
        self.total_len.store(0, Ordering::Relaxed);
        if changed {
            self.publish_write_for_all_shards();
        }
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
        let start_epoch = self.write_epoch.load(Ordering::Relaxed);
        if self.cache_enabled() {
            let cached_epoch = self.snapshot_cache_epoch.load(Ordering::Relaxed);
            if cached_epoch == start_epoch {
                let cache = std_read_guard(&self.snapshot_cache, "snapshot_cache_read");
                if let Some(entries) = cache.as_ref() {
                    return entries.as_ref().clone().into_iter();
                }
            }
        }

        let items: Vec<(K, V)> = if self.cow_enabled() {
            let shard_count = self.shard_count();
            let mut snapshots = Vec::new();
            for i in 0..shard_count {
                let snapshot = self.cow_shard_snapshot(i);
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
                items
            }

            #[cfg(not(feature = "rayon"))]
            {
                let mut items = Vec::new();
                for shard in shards_snapshot {
                    let guard = std_read_guard(&shard, "shard");
                    items.extend(guard.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
                items
            }
        };

        if self.cache_enabled() {
            let end_epoch = self.write_epoch.load(Ordering::Relaxed);
            if start_epoch == end_epoch {
                let arc_items = Arc::new(items.clone());
                let mut cache = std_write_guard(&self.snapshot_cache, "snapshot_cache_write");
                *cache = Some(arc_items);
                self.snapshot_cache_epoch
                    .store(end_epoch, Ordering::Relaxed);
            }
        }

        items.into_iter()
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
        if self.rebalance_tracker.is_migrating() {
            let mut inserted = 0usize;
            for (k, v) in entries {
                if self.insert(k, v).is_none() {
                    inserted += 1;
                }
            }
            return inserted;
        }

        let buckets = self.bucketize_entries(entries);
        let mut count = 0;
        let mut touched = Vec::new();
        for (shard_idx, pairs) in buckets {
            let shard = self.get_or_init_shard(shard_idx);
            let mut guard = std_write_guard(&shard, "shard");
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
            self.publish_write_for_touched_shards(touched);
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
        if self.rebalance_tracker.is_migrating() {
            let mut removed = 0usize;
            for k in keys {
                if self.remove(&k).is_some() {
                    removed += 1;
                }
            }
            return removed;
        }

        let buckets = self.bucketize_keys(keys);
        let mut count = 0;
        let mut touched = Vec::new();
        for (shard_idx, keys) in buckets {
            let shard = self.get_or_init_shard(shard_idx);
            let mut guard = std_write_guard(&shard, "shard");
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
            self.publish_write_for_touched_shards(touched);
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
        if self.rebalance_tracker.is_migrating() {
            return keys.iter().map(|k| self.get(k)).collect();
        }

        let mut results = vec![None; keys.len()];
        let buckets = self.bucketize_key_refs(keys);
        for (shard_idx, items) in buckets {
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
        if self.rebalance_tracker.is_migrating() {
            let current = self.get(key)?;
            if let Some(new_val) = f(&current) {
                let result = new_val.clone();
                let _ = self.insert(key.clone(), new_val);
                return Some(result);
            }
            let _ = self.remove(key);
            return None;
        }

        let shard = self.get_or_init_shard(self.shard_index(key));
        let shard_idx = self.shard_index(key);
        let mut guard = std_write_guard(&shard, "shard");

        if let Some(old_val) = guard.get(key) {
            if let Some(new_val) = f(old_val) {
                let result = new_val.clone();
                guard.insert(key.clone(), new_val);
                drop(guard);
                self.publish_write_for_touched_shards([shard_idx]);
                Some(result)
            } else {
                // Remove the entry
                guard.remove(key);
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                drop(guard);
                self.publish_write_for_touched_shards([shard_idx]);
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
        if self.rebalance_tracker.is_migrating() {
            if let Some(existing) = self.get(&key) {
                return existing;
            }
            let new_v = f();
            let _ = self.insert(key, new_v.clone());
            return new_v;
        }

        let shard_idx = self.shard_index(&key);
        let shard = self.get_or_init_shard(shard_idx);
        let mut guard = std_write_guard(&shard, "shard");

        if let Some(val) = guard.get(&key) {
            val.clone()
        } else {
            let new_v = f();
            guard.insert(key, new_v.clone());
            self.total_len.fetch_add(1, Ordering::Relaxed);
            drop(guard);
            self.publish_write_for_touched_shards([shard_idx]);
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
                self.publish_write_for_all_shards();
            }
        }

        #[cfg(not(feature = "rayon"))]
        {
            let mut removed_total = 0usize;
            for shard in shards_snapshot {
                let mut guard = std_write_guard(&shard, "shard");
                let before = guard.len();
                guard.retain(|k, v| predicate(k, v));
                removed_total += before - guard.len();
            }
            if removed_total > 0 {
                self.total_len.fetch_sub(removed_total, Ordering::Relaxed);
                self.publish_write_for_all_shards();
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
                    changed = true;
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
                        changed = true;
                    }
                }
            }
        }

        drop(guards);
        if changed {
            self.publish_write_for_touched_shards(shard_indices);
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
        let shard_idx = self.shard_index(key);
        let mut guard = std_write_guard(&shard, "cas");

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.insert(key.clone(), new.clone());
                drop(guard);
                self.publish_write_for_touched_shards([shard_idx]);
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
        let shard_idx = self.shard_index(key);
        let mut guard = std_write_guard(&shard, "cas_remove");

        match guard.get(key) {
            Some(current) if current == expected => {
                guard.remove(key);
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                drop(guard);
                self.publish_write_for_touched_shards([shard_idx]);
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
        if self.cache_enabled() {
            let cache_epoch = self.snapshot_cache_epoch.load(Ordering::Relaxed);
            let write_epoch = self.write_epoch.load(Ordering::Relaxed);
            if cache_epoch == write_epoch {
                let cache = std_read_guard(&self.snapshot_cache, "snapshot_cache_read");
                if let Some(entries) = cache.as_ref() {
                    return CowSnapshot::from_arc(entries.clone(), cache_epoch);
                }
            }
        }

        // Best-effort stable epoch labeling: retry a few times if writes overlap snapshot build.
        for _ in 0..3 {
            let begin = self.write_epoch.load(Ordering::Relaxed);
            let data: Vec<(K, V)> = self.iter().collect();
            let end = self.write_epoch.load(Ordering::Relaxed);
            if begin == end {
                return CowSnapshot::new(data, end);
            }
        }

        let data: Vec<(K, V)> = self.iter().collect();
        let version = self.write_epoch.load(Ordering::Relaxed);
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
        let mut items = Vec::new();
        let mut changed = false;

        {
            let slots = std_read_guard(&self.shards, "shards");
            for shard in slots.iter().flatten() {
                let mut guard = std_write_guard(shard, "shard");
                changed |= !guard.is_empty();
                items.extend(guard.drain());
            }
        }

        self.total_len.store(0, Ordering::Relaxed);
        if changed {
            self.publish_write_for_all_shards();
        }

        DrainIterator { items, index: 0 }
    }
}
