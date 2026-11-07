//! Configured policy maintenance and callbacks outside map locks.

use super::*;
use crate::eviction::LifecycleState;
use crate::eviction::scheduler::{EvictionHandle, EvictionWorkerStatus, SchedulerOwner};

/// Covers panics after a per-entry ticket finishes, including returned value
/// destructors and a later batch element. Declare before routing and payload
/// buffers so metadata retirement also happens after map locks are released.
pub(super) struct OperationUnwind<'a, K, V>(&'a LifecycleState<K, V>);

impl<'a, K, V> OperationUnwind<'a, K, V> {
    pub(super) fn new(state: &'a LifecycleState<K, V>) -> Self {
        Self(state)
    }
}

impl<K, V> Drop for OperationUnwind<'_, K, V> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.mark_dirty();
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.0.dispatch_callbacks(Vec::new());
            })) {
                // A user key destructor must not replace the original failure.
                std::mem::forget(payload);
            }
        }
    }
}

/// Declare before routing guards so committed evictions are reported even if a
/// later key's Hash, Eq or Cow clone unwinds the maintenance pass.
struct EvictionBatch<'a, K, V> {
    state: &'a LifecycleState<K, V>,
    entries: Vec<(K, V)>,
    started: std::time::Instant,
    dispatched: bool,
}

impl<'a, K, V> EvictionBatch<'a, K, V> {
    fn new(state: &'a LifecycleState<K, V>, started: std::time::Instant) -> Self {
        Self {
            state,
            entries: Vec::new(),
            started,
            dispatched: false,
        }
    }

    fn dispatch(mut self) -> usize {
        let removed = self.entries.len();
        self.dispatched = true;
        self.state.record_eviction_duration(self.started.elapsed());
        self.state
            .dispatch_callbacks(std::mem::take(&mut self.entries));
        removed
    }
}

impl<K, V> Drop for EvictionBatch<'_, K, V> {
    fn drop(&mut self) {
        if self.dispatched {
            return;
        }
        self.dispatched = true;
        self.state.record_eviction_duration(self.started.elapsed());
        let entries = std::mem::take(&mut self.entries);
        if std::thread::panicking() {
            // Preserve the original panic if retiring user values also panics.
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.state.dispatch_callbacks(entries);
            })) {
                std::mem::forget(payload);
            }
        } else {
            self.state.dispatch_callbacks(entries);
        }
    }
}
use crate::eviction::{CapacityPolicy, EvictionError, EvictionOptions, EvictionRuntimeStats};

impl<K, V> ShardedHashMap<K, V, FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create a configured cache with the default hasher and snapshot mode.
    ///
    /// Starts its periodic worker when `background_enabled` is true.
    /// Panics for unsupported configuration or failure to start the worker.
    pub fn with_eviction(shard_count: usize, config: EvictionConfig) -> Self {
        Self::try_with_eviction(shard_count, config)
            .unwrap_or_else(|error| panic!("invalid eviction configuration: {error}"))
    }

    /// Fallible version of [`Self::with_eviction`].
    pub fn try_with_eviction(
        shard_count: usize,
        config: EvictionConfig,
    ) -> Result<Self, EvictionError> {
        Self::try_with_eviction_options(
            shard_count,
            FxBuildHasher,
            SnapshotMode::Clone,
            EvictionOptions::new(config),
        )
    }
}

impl<K, V, S> ShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Configure a cache with a custom hasher, snapshot mode and optional weight budget.
    ///
    /// When requested, the scheduler stops after the last external map clone is
    /// dropped. Limits apply after successful operations; panicking operations
    /// can leave committed prefixes requiring subsequent maintenance.
    pub fn try_with_eviction_options(
        shard_count: usize,
        hasher: S,
        mode: SnapshotMode,
        options: EvictionOptions<K, V>,
    ) -> Result<Self, EvictionError>
    where
        K: 'static,
        V: 'static,
        S: 'static,
    {
        let background = options.config().background_enabled;
        let map = Self::build_eviction_map(shard_count, hasher, mode, options)?;
        if background {
            let handle = map.spawn_eviction_task()?;
            if let Some(owner) = &map.lifecycle_owner {
                owner.retain_sync(handle);
            }
        }
        Ok(map)
    }

    /// Create a configured cache without a background worker, permitting borrowed types.
    ///
    /// Returns an error when `background_enabled` is true. Ordinary mutations
    /// still enforce capacity after committing, and explicit maintenance is available.
    pub fn try_with_manual_eviction(
        shard_count: usize,
        hasher: S,
        mode: SnapshotMode,
        options: EvictionOptions<K, V>,
    ) -> Result<Self, EvictionError> {
        if options.config().background_enabled {
            return Err(EvictionError::BackgroundNotAllowed);
        }
        Self::build_eviction_map(shard_count, hasher, mode, options)
    }

    fn build_eviction_map(
        shard_count: usize,
        hasher: S,
        mode: SnapshotMode,
        options: EvictionOptions<K, V>,
    ) -> Result<Self, EvictionError> {
        options.validate()?;
        let mut map = Self::with_shards_and_hasher_and_snapshot_mode(shard_count, hasher, mode);
        map.lifecycle.configure(options)?;
        map.lifecycle_owner = Some(Arc::new(SchedulerOwner::new(
            map.lifecycle.scheduler.clone(),
        )));
        Ok(map)
    }

    /// Spawn one periodic maintenance worker, rejecting duplicate workers.
    ///
    /// A configured eviction policy and a positive, representable interval are required.
    pub fn spawn_eviction_task(&self) -> Result<EvictionHandle, EvictionError>
    where
        K: 'static,
        V: 'static,
        S: 'static,
    {
        let config = self
            .lifecycle
            .config()
            .ok_or(EvictionError::NotConfigured)?;
        let map = self.clone_for_worker();
        self.lifecycle
            .scheduler
            .start_sync(config.check_interval, move || map.enforce_eviction_limits())
            .map_err(EvictionError::from)
    }

    /// Observe the shared scheduler without starting a worker.
    pub fn eviction_worker_status(&self) -> EvictionWorkerStatus {
        self.lifecycle.scheduler.status()
    }

    /// Install a callback invoked after eviction releases all map locks.
    ///
    /// Callback panics are isolated. Recursive callback dispatch is suppressed;
    /// mutations made by a callback still perform their own capacity maintenance.
    pub fn set_evict_callback(&self, callback: impl Fn(&K, &V) + Send + Sync + 'static) {
        self.lifecycle.set_callback(callback);
    }

    /// Remove the callback shared by all map clones.
    pub fn clear_evict_callback(&self) {
        self.lifecycle.clear_callback();
    }

    /// Snapshot operation counters shared by all map clones, even without eviction configured.
    pub fn metrics_snapshot(&self) -> MetricsStats {
        self.lifecycle.metrics_snapshot()
    }

    /// Reset shared operation counters. Concurrent operations may immediately increment them.
    pub fn reset_metrics(&self) {
        self.lifecycle.reset_metrics();
    }

    /// Inspect policy maintenance, callback and pending-repair diagnostics.
    pub fn eviction_runtime_stats(&self) -> EvictionRuntimeStats {
        self.lifecycle.runtime_stats()
    }

    /// Evict according to the configured policy until at most `target_count` remain.
    ///
    /// Returns the number removed by this pass. A map without configured eviction
    /// returns zero because it has no policy ordering metadata.
    pub fn evict(&self, target_count: usize) -> usize {
        self.run_eviction(Some(target_count), None)
    }

    /// Evict using recency ordering until at most `target_count` remain.
    ///
    /// Requires configured eviction metadata; otherwise returns zero.
    pub fn evict_lru(&self, target_count: usize) -> usize {
        self.run_eviction(Some(target_count), Some(CapacityPolicy::Lru))
    }

    /// Complete pending maintenance and enforce configured entry and weight limits.
    ///
    /// Concurrent or panicking operations may leave pending work. This method,
    /// the next successful mutation, or a background tick can resume maintenance.
    pub fn enforce_eviction_limits(&self) -> usize {
        self.run_eviction(None, None)
    }

    fn reconcile_eviction_metadata(&self, entries: &mut Vec<(Arc<K>, usize)>) {
        if !self.lifecycle.dirty() {
            return;
        }
        let ticket = self.lifecycle.mutation();
        entries.reserve(self.len());
        for shard in self.all_shards() {
            let guard = std_read_guard(&shard, "eviction_reconcile");
            for (key, value) in guard.iter() {
                let weight = self.lifecycle.weight(key, value);
                entries.push((Arc::new(key.clone()), weight));
            }
        }
        self.lifecycle.reconcile(entries);
        ticket.finish();
    }

    fn run_eviction(&self, target_count: Option<usize>, policy: Option<CapacityPolicy>) -> usize {
        let Some(config) = self.lifecycle.config() else {
            return 0;
        };
        let Some(policy) = policy.or_else(|| self.lifecycle.policy()) else {
            return 0;
        };
        let started = std::time::Instant::now();
        let observed = self.lifecycle.maintenance_generation();
        let target_count = match (target_count, config.max_entries) {
            (Some(requested), Some(configured)) => Some(requested.min(configured)),
            (requested, configured) => requested.or(configured),
        };
        if !self.lifecycle.dirty()
            && config.max_items_per_shard.is_none()
            && target_count.is_none_or(|target| self.len() <= target)
            && !self.lifecycle.over_memory_budget()
        {
            self.lifecycle.mark_maintained(observed);
            self.lifecycle.dispatch_callbacks(Vec::new());
            return 0;
        }

        // Both buffers precede routing guards so even an unwinding Hash/Clone
        // drops retired entries and candidate keys after releasing map locks.
        let mut evicted = EvictionBatch::new(&self.lifecycle, started);
        let mut candidates = Vec::new();
        let mut retired_candidates = Vec::new();
        let mut reconcile_entries = Vec::new();
        if target_count.is_none()
            && self.lifecycle.memory_budget().is_none()
            && !self.lifecycle.dirty()
            && let Some(limit) = config.max_items_per_shard
        {
            let routing = std_read_guard(&self.routing_lock, "eviction_per_shard_routing");
            if self.previous_shard_count.load(Ordering::Relaxed) == 0 && !self.lifecycle.dirty() {
                let ticket = self.lifecycle.mutation();
                let observed = self.lifecycle.maintenance_generation();
                let slots = std_read_guard(&self.shards, "eviction_slots").clone();
                let mut retry_exclusive = false;
                for shard in &slots {
                    let Some(shard) = shard else { continue };
                    if std_read_guard(shard, "eviction_limit").len() <= limit {
                        continue;
                    }
                    let mut guard = std_write_guard(shard, "eviction_shard");
                    let candidate_start = candidates.len();
                    candidates.extend(self.lifecycle.candidates_for_keys(policy, guard.keys()));
                    for candidate in &candidates[candidate_start..] {
                        if guard.len() <= limit {
                            break;
                        }
                        if !self.lifecycle.validate(candidate) {
                            continue;
                        }
                        if let Some(entry) = guard.remove_entry(&*candidate.key) {
                            self.record_write(0, 1);
                            self.lifecycle.record_eviction(candidate);
                            evicted.entries.push(entry);
                        }
                    }
                    retry_exclusive |= guard.len() > limit;
                }
                ticket.finish();
                if !retry_exclusive && !self.lifecycle.dirty() {
                    self.lifecycle.mark_maintained(observed);
                    drop(routing);
                    drop(candidates);
                    return evicted.dispatch();
                }
                // A concurrent access or replacement may invalidate every
                // candidate. Retry from an exclusive cut to finish this call.
                drop(routing);
                retired_candidates.append(&mut candidates);
            } else {
                drop(routing);
            }
        }

        let routing = std_write_guard(&self.routing_lock, "eviction_global_routing");
        let ticket = self.lifecycle.mutation();
        self.reconcile_eviction_metadata(&mut reconcile_entries);
        let observed = self.lifecycle.maintenance_generation();
        let mut per_shard = Vec::new();
        if config.max_items_per_shard.is_some() {
            per_shard.resize(self.shard_count(), 0_usize);
            for shard in self.all_shards() {
                let guard = std_read_guard(&shard, "eviction_logical_count");
                for key in guard.keys() {
                    per_shard[self.shard_index(key)] += 1;
                }
            }
        }
        let mut over_weight = self.lifecycle.over_memory_budget();
        let over_count = target_count.is_some_and(|target| self.len() > target);
        if config.max_items_per_shard.is_some() || over_count || over_weight {
            if config.max_items_per_shard.is_none()
                && self.lifecycle.memory_budget().is_none()
                && target_count.is_some_and(|target| self.len().checked_sub(target) == Some(1))
            {
                candidates.extend(self.lifecycle.minimum_candidate(policy));
            } else {
                candidates = self.lifecycle.candidates(policy);
            }
        }
        for candidate in &candidates {
            let over_count = target_count.is_some_and(|target| self.len() > target);
            if config.max_items_per_shard.is_none() && !over_count && !over_weight {
                break;
            }
            let index = self.shard_index(&*candidate.key);
            let over_shard = config
                .max_items_per_shard
                .is_some_and(|limit| per_shard[index] > limit);
            if !over_count && !over_shard && !over_weight {
                continue;
            }
            if !self.lifecycle.validate(candidate) {
                continue;
            }
            let active = std_read_guard(&self.shards, "eviction_active")[index].clone();
            let previous = self.previous_shard(&*candidate.key);
            for shard in active.into_iter().chain(previous) {
                let mut guard = std_write_guard(&shard, "eviction_remove");
                if !guard.contains_key(&*candidate.key) {
                    continue;
                }
                if let Some(entry) = guard.remove_entry(&*candidate.key) {
                    self.record_write(0, 1);
                    self.lifecycle.record_eviction(candidate);
                    if over_weight {
                        over_weight = self.lifecycle.over_memory_budget();
                    }
                    if config.max_items_per_shard.is_some() {
                        per_shard[index] -= 1;
                    }
                    evicted.entries.push(entry);
                    break;
                }
            }
        }
        ticket.finish();
        self.lifecycle.mark_maintained(observed);
        drop(routing);
        drop(candidates);
        drop(retired_candidates);
        drop(reconcile_entries);
        evicted.dispatch()
    }
}
