use super::*;

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

    #[inline]
    fn bucketize_entries<I>(&self, entries: I) -> Vec<Vec<(K, V)>>
    where
        I: IntoIterator<Item = (K, V)>,
    {
        let mut buckets = vec![Vec::new(); self.shard_count];
        for (k, v) in entries {
            let shard_idx = self.shard_index(&k);
            buckets[shard_idx].push((k, v));
        }
        buckets
    }

    #[inline]
    fn bucketize_keys<I>(&self, keys: I) -> Vec<Vec<K>>
    where
        I: IntoIterator<Item = K>,
    {
        let mut buckets = vec![Vec::new(); self.shard_count];
        for k in keys {
            let shard_idx = self.shard_index(&k);
            buckets[shard_idx].push(k);
        }
        buckets
    }

    #[inline]
    fn bucketize_key_refs<'a>(&self, keys: &'a [K]) -> Vec<Vec<(usize, &'a K)>> {
        let mut buckets = vec![Vec::new(); self.shard_count];
        for (idx, key) in keys.iter().enumerate() {
            let shard_idx = self.shard_index(key);
            buckets[shard_idx].push((idx, key));
        }
        buckets
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
        let buckets = self.bucketize_entries(entries);
        let mut count = 0;
        for (shard_idx, pairs) in buckets.into_iter().enumerate() {
            if pairs.is_empty() {
                continue;
            }
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
        let buckets = self.bucketize_keys(keys);
        let mut count = 0;
        for (shard_idx, keys) in buckets.into_iter().enumerate() {
            if keys.is_empty() {
                continue;
            }
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
        let buckets = self.bucketize_key_refs(keys);
        for (shard_idx, items) in buckets.into_iter().enumerate() {
            if items.is_empty() {
                continue;
            }
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
