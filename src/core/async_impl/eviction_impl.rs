//! Configured capacity eviction, reconciliation, and lock-free callback delivery.

use super::*;
use crate::eviction::{
    AsyncEvictionHandle, CapacityPolicy, EvictionConfig, EvictionError, EvictionOptions,
    EvictionRuntimeStats, EvictionWorkerStatus, LifecycleState, MetricsStats,
};
use std::time::Instant;

/// Declared before map guards: normal exit and unwinding release every map lock
/// before invoking user callbacks or destroying retired policy keys.
struct EvictionDispatch<'a, K, V> {
    state: &'a LifecycleState<K, V>,
    entries: Vec<(K, V)>,
}
impl<K, V> Drop for EvictionDispatch<'_, K, V> {
    fn drop(&mut self) {
        self.state
            .dispatch_callbacks(std::mem::take(&mut self.entries));
    }
}

impl<K, V> AsyncShardedHashMap<K, V, FxBuildHasher>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Construct a capacity-managed map with the default hasher.
    ///
    /// # Panics
    /// Panics for unsupported configuration, or when automatic scheduling needs
    /// an unavailable Tokio runtime/time driver. Prefer `try_with_eviction` for errors.
    pub fn with_eviction(shards: usize, config: EvictionConfig) -> Self {
        Self::try_with_eviction(shards, config).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Construct a configured map, starting a supervised periodic task when enabled.
    pub fn try_with_eviction(shards: usize, config: EvictionConfig) -> Result<Self, EvictionError> {
        Self::try_with_eviction_options(
            shards,
            FxBuildHasher,
            SnapshotMode::Clone,
            EvictionOptions::new(config),
        )
    }
}

impl<K, V, S> AsyncShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Construct a manual capacity-managed map without starting a background task.
    /// `background_enabled` must be false; no Tokio runtime is required to construct it.
    pub fn try_with_manual_eviction(
        shards: usize,
        hasher: S,
        mode: SnapshotMode,
        options: EvictionOptions<K, V>,
    ) -> Result<Self, EvictionError> {
        options.validate()?;
        if options.config().background_enabled {
            return Err(EvictionError::BackgroundNotAllowed);
        }
        Self::build_with_eviction(shards, hasher, mode, options)
    }

    fn build_with_eviction(
        shards: usize,
        hasher: S,
        mode: SnapshotMode,
        options: EvictionOptions<K, V>,
    ) -> Result<Self, EvictionError> {
        let mut map = Self::with_shards_and_hasher_and_snapshot_mode(shards, hasher, mode);
        map.lifecycle.configure(options)?;
        map.lifecycle_owner = Some(Arc::new(crate::eviction::scheduler::SchedulerOwner::new(
            map.lifecycle.scheduler.clone(),
        )));
        Ok(map)
    }

    /// Construct a map with policy and optional estimated-weight limits.
    /// Automatic scheduling requires an entered Tokio runtime with time enabled.
    pub fn try_with_eviction_options(
        shards: usize,
        hasher: S,
        mode: SnapshotMode,
        options: EvictionOptions<K, V>,
    ) -> Result<Self, EvictionError>
    where
        S: 'static,
    {
        let map = Self::build_with_eviction(shards, hasher, mode, options)?;
        if map
            .lifecycle
            .config()
            .is_some_and(|config| config.background_enabled)
        {
            let handle = map.spawn_eviction_task()?;
            map.lifecycle_owner
                .as_ref()
                .expect("configured maps have an external owner")
                .retain_async(handle);
        }
        Ok(map)
    }

    /// Start a supervised periodic task. One worker is allowed per shared map.
    /// Dropping its handle or the last external map owner stops it.
    pub fn spawn_eviction_task(&self) -> Result<AsyncEvictionHandle, EvictionError>
    where
        S: 'static,
    {
        let config = self
            .lifecycle
            .config()
            .ok_or(EvictionError::NotConfigured)?;
        let interval = config.check_interval;
        let map = Arc::new(self.clone_for_worker());
        self.lifecycle
            .scheduler
            .start_async(interval, move || {
                let map = map.clone();
                async move { map.enforce_eviction_limits().await }
            })
            .map_err(EvictionError::from)
    }

    /// Observe the current or last maintenance worker without retaining its owner.
    pub fn eviction_worker_status(&self) -> EvictionWorkerStatus {
        self.lifecycle.scheduler.status()
    }

    /// Set a shared eviction callback. It runs synchronously after all map locks
    /// are released. Panics are isolated and nested dispatch is suppressed.
    pub fn set_evict_callback(&self, callback: impl Fn(&K, &V) + Send + Sync + 'static) {
        self.lifecycle.set_callback(callback);
    }
    /// Remove the shared callback.
    pub fn clear_evict_callback(&self) {
        self.lifecycle.clear_callback();
    }
    /// Read per-element operation counters; snapshot scans and migration are excluded.
    pub fn metrics_snapshot(&self) -> MetricsStats {
        self.lifecycle.metrics_snapshot()
    }
    /// Reset operation counters for every clone of this map.
    pub fn reset_metrics(&self) {
        self.lifecycle.reset_metrics();
    }
    /// Inspect policy bookkeeping, pending work, callback failures, and estimated weight.
    pub fn eviction_runtime_stats(&self) -> EvictionRuntimeStats {
        self.lifecycle.runtime_stats()
    }

    /// Evict by the configured policy until at most `target_count` entries remain,
    /// also honoring configured limits. Returns zero when eviction is not configured.
    pub async fn evict(&self, target_count: usize) -> usize {
        self.maintain_eviction(Some(target_count), None).await
    }
    /// Evict in LRU order without changing the configured policy. Configuration is required.
    pub async fn evict_lru(&self, target_count: usize) -> usize {
        self.maintain_eviction(Some(target_count), Some(CapacityPolicy::Lru))
            .await
    }
    /// Repair pending policy state and enforce configured limits.
    ///
    /// Cancelled writes may have committed before their maintenance completed.
    /// Their pending state remains visible until this method, a later completed
    /// mutation, or a background tick repairs it. This is not an in-flight hard cap.
    pub async fn enforce_eviction_limits(&self) -> usize {
        self.maintain_eviction(None, None).await
    }

    /// Stable-layout maintenance pins topology with a shared guard and only
    /// write-locks overfull shards. Other initialized shards remain usable.
    /// Dirty recovery and topology changes use the global stable-cut path.
    async fn maintain_per_shard(&self) -> Option<usize> {
        let limit = self.lifecycle.config()?.max_items_per_shard?;
        let mut dispatch = EvictionDispatch {
            state: &self.lifecycle,
            entries: Vec::new(),
        };
        let mut plans = Vec::new();
        let _routing = self.routing_lock.read().await;
        if self.previous_shard_count.load(Ordering::Relaxed) != 0 || self.lifecycle.dirty() {
            return None;
        }
        let started = Instant::now();
        let pass = self.lifecycle.mutation();
        let observed = self.lifecycle.maintenance_generation();
        let policy = self
            .lifecycle
            .policy()
            .expect("configured map has a policy");
        let initialized: Vec<_> = {
            let slots = self.shards.read().await;
            slots
                .iter()
                .enumerate()
                .filter_map(|(index, shard)| shard.as_ref().map(|shard| (index, shard.clone())))
                .collect()
        };
        // Inspect lengths without retaining read guards. Directory order keeps
        // the selected write-lock acquisitions in ascending shard index order.
        let mut selected = Vec::new();
        for (index, shard) in initialized {
            let overfull = shard.read().await.len() > limit;
            if overfull {
                selected.push((index, shard));
            }
        }
        let mut guards = Vec::with_capacity(selected.len());
        for (_, shard) in selected {
            guards.push(shard.write_owned().await);
        }
        if self.lifecycle.dirty() {
            return None;
        }
        // Another maintenance pass may have trimmed a selected shard before
        // its lock was acquired. Do not copy retained Cow storage unnecessarily.
        guards.retain(|guard| guard.len() > limit);
        // Only these shards contribute candidates. Re-sample after locking so
        // their recency and incarnations cannot change before deletion.
        for (position, guard) in guards.iter().enumerate() {
            plans.extend(
                self.lifecycle
                    .candidates_for_keys(policy, guard.keys())
                    .into_iter()
                    .map(|candidate| (candidate, position)),
            );
        }
        dispatch.entries.reserve(plans.len());
        let mut maps: Vec<&mut HashMap<K, V, S>> =
            guards.iter_mut().map(|guard| &mut ***guard).collect();
        for (candidate, position) in &plans {
            if maps[*position].len() <= limit || !self.lifecycle.validate(candidate) {
                continue;
            }
            let removed = {
                let mut mutation =
                    ShardMutation::new(maps[*position], &self.total_len, &self.write_epoch);
                mutation.remove_entry(candidate.key.as_ref())
            };
            if let Some(entry) = removed {
                dispatch.entries.push(entry);
                self.lifecycle.record_eviction(candidate);
            } else {
                self.lifecycle.mark_dirty();
            }
        }
        let removed = dispatch.entries.len();
        let within_limits = maps.iter().all(|map| map.len() <= limit);
        pass.finish();
        if within_limits {
            self.lifecycle.mark_maintained(observed);
        }
        self.lifecycle.record_eviction_duration(started.elapsed());
        drop(maps);
        drop(guards);
        drop(_routing);
        drop(dispatch);
        Some(removed)
    }

    async fn maintain_eviction(
        &self,
        target_count: Option<usize>,
        force_policy: Option<CapacityPolicy>,
    ) -> usize {
        let Some(config) = self.lifecycle.config() else {
            return 0;
        };
        let mut dispatch = EvictionDispatch {
            state: &self.lifecycle,
            entries: Vec::new(),
        };
        if target_count.is_none()
            && force_policy.is_none()
            && config.max_items_per_shard.is_some()
            && self.lifecycle.memory_budget().is_none()
            && let Some(removed) = self.maintain_per_shard().await
        {
            // Even a zero-eviction pass must release metadata keys retired by
            // ordinary remove/replace operations, after the helper's locks.
            drop(dispatch);
            return removed;
        }
        let observed = self.lifecycle.maintenance_generation();
        // No global gate is needed for a clean map already under global limits.
        // An outer mutation ticket marks cancellation/panic dirty independently.
        let entry_limit = match (target_count, config.max_entries) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if !self.lifecycle.dirty()
            && config.max_items_per_shard.is_none()
            && entry_limit.is_none_or(|limit| self.total_len.load(Ordering::Relaxed) <= limit)
            && !self.lifecycle.over_memory_budget()
        {
            self.lifecycle.mark_maintained(observed);
            // No map lock is held here. Flush retired policy keys even when
            // capacity was never exceeded and there are no callback events.
            drop(dispatch);
            return 0;
        }
        // Announce any forthcoming await after a committed mutation. Another
        // pass may only acknowledge the older sequence it actually observed.
        self.lifecycle.mark_pending();
        let started = Instant::now();
        let mut repair = Vec::new();
        let mut plans = Vec::new();
        let _routing = self.routing_lock.write().await;
        let pass = self.lifecycle.mutation();
        let observed = self.lifecycle.maintenance_generation();
        let active = self.shards.read().await;
        let previous = self.previous_shards.read().await;
        if self.lifecycle.dirty() {
            for shard in active
                .iter()
                .flatten()
                .chain(previous.iter().flatten().flatten())
            {
                let guard = shard.read().await;
                for (key, value) in guard.iter() {
                    let weight = self.lifecycle.weight(key, value);
                    repair.push((Arc::new(key.clone()), weight));
                }
            }
            self.lifecycle.reconcile(&repair);
        }
        let policy = force_policy
            .or_else(|| self.lifecycle.policy())
            .expect("configured maps have a policy");
        let current_len = self.total_len.load(Ordering::Relaxed);
        let only_one_global_victim = config.max_items_per_shard.is_none()
            && self.lifecycle.memory_budget().is_none()
            && entry_limit.is_some_and(|limit| current_len.checked_sub(limit) == Some(1));
        let candidates = if only_one_global_victim {
            // The exclusive routing guard makes the minimum's rank stable;
            // avoid materializing and sorting the full metadata population.
            self.lifecycle
                .minimum_candidate(policy)
                .into_iter()
                .collect()
        } else {
            self.lifecycle.candidates(policy)
        };
        let mut counts = vec![0usize; self.shard_count()];
        let mut weighted = 0u128;
        let mut routed = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let hash = self.hasher.hash_one(candidate.key.as_ref());
            let index = (hash % counts.len() as u64) as usize;
            counts[index] += 1;
            weighted += candidate.weight as u128;
            routed.push((candidate, hash, index));
        }
        // Candidate count may be one even when the map contains many entries.
        let mut remaining = current_len;
        let mut shard_arcs = Vec::new();
        for (candidate, hash, index) in routed {
            let over_entries = entry_limit.is_some_and(|limit| remaining > limit);
            let over_shard = config
                .max_items_per_shard
                .is_some_and(|limit| counts[index] > limit);
            let over_weight = self
                .lifecycle
                .memory_budget()
                .is_some_and(|budget| weighted > budget as u128);
            if !(over_entries || over_shard || over_weight) {
                continue;
            }
            if !self.lifecycle.validate(&candidate) {
                continue;
            }
            let mut location = None;
            if let Some(shard) = active[index].as_ref()
                && shard.read().await.contains_key(candidate.key.as_ref())
            {
                location = Some((false, index, shard.clone()));
            }
            if location.is_none()
                && let Some(previous) = previous.as_ref()
            {
                let old_index = (hash % previous.len() as u64) as usize;
                if let Some(shard) = previous[old_index].as_ref()
                    && shard.read().await.contains_key(candidate.key.as_ref())
                {
                    location = Some((true, old_index, shard.clone()));
                }
            }
            if let Some((old, index, shard)) = location {
                shard_arcs.push(((old, index), shard));
                plans.push((candidate, (old, index)));
                remaining -= 1;
                weighted -= plans.last().expect("candidate was added").0.weight as u128;
                // Index here is physical for a previous shard; budgets always
                // use the destination-generation routing of the same hash.
                let logical_index = (hash % counts.len() as u64) as usize;
                counts[logical_index] -= 1;
            } else {
                self.lifecycle.mark_dirty();
            }
        }
        shard_arcs.sort_unstable_by_key(|(location, _)| *location);
        shard_arcs.dedup_by_key(|(location, _)| *location);
        let locations: Vec<_> = shard_arcs.iter().map(|(location, _)| *location).collect();
        let mut guards = Vec::with_capacity(shard_arcs.len());
        for (_, shard) in shard_arcs {
            guards.push(shard.write_owned().await);
        }
        // Resolve all Cow copies and reserve callback storage before the first
        // deletion. From here through callback dispatch there is no await.
        dispatch.entries.reserve(plans.len());
        let mut maps: Vec<&mut HashMap<K, V, S>> =
            guards.iter_mut().map(|guard| &mut ***guard).collect();
        for (candidate, location) in &plans {
            if !self.lifecycle.validate(candidate) {
                continue;
            }
            let position = locations
                .binary_search(location)
                .expect("selected shard was locked");
            let removed = {
                let mut mutation =
                    ShardMutation::new(maps[position], &self.total_len, &self.write_epoch);
                mutation.remove_entry(candidate.key.as_ref())
            };
            if let Some(entry) = removed {
                dispatch.entries.push(entry);
                self.lifecycle.record_eviction(candidate);
            } else {
                self.lifecycle.mark_dirty();
            }
        }
        let removed = dispatch.entries.len();
        pass.finish();
        self.lifecycle.mark_maintained(observed);
        self.lifecycle.record_eviction_duration(started.elapsed());
        drop(maps);
        drop(guards);
        drop(previous);
        drop(active);
        drop(_routing);
        drop(dispatch);
        removed
    }
}
