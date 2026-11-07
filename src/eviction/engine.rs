//! Policy metadata is advisory; data-shard guards serialize each key's hooks.
//! Recovery publishes a staged rebuild under the caller's exclusive routing cut.

use super::scheduler::SchedulerRegistry;
use super::{
    AtomicMetrics, CapacityPolicy, EvictionConfig, EvictionError, EvictionOptions,
    EvictionRuntimeStats, MetricsStats,
};
use hashbrown::HashMap;
use rustc_hash::FxBuildHasher;
use std::borrow::Borrow;
use std::cell::Cell;
use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

const STRIPES: usize = 64;
type Callback<K, V> = Arc<dyn Fn(&K, &V) + Send + Sync>;

thread_local! { static DISPATCHING: Cell<bool> = const { Cell::new(false) }; }

struct DispatchGuard;
impl Drop for DispatchGuard {
    fn drop(&mut self) {
        DISPATCHING.with(|depth| depth.set(false));
    }
}

struct Entry<K> {
    key: Arc<K>,
    incarnation: u64,
    inserted: u64,
    accessed: AtomicU64,
    frequency: AtomicU64,
    weight: usize,
}

struct Stripe<K> {
    // A precomputed integer key means commit/validation never rehash user keys.
    // User equality is confined to lookups and insert preparation/synchronization.
    buckets: HashMap<u64, Vec<Entry<K>>, FxBuildHasher>,
    weight: u128,
    retired: Vec<Arc<K>>,
}

impl<K> Default for Stripe<K> {
    fn default() -> Self {
        Self {
            buckets: HashMap::with_hasher(FxBuildHasher),
            weight: 0,
            retired: Vec::new(),
        }
    }
}

struct Configured<K, V> {
    options: EvictionOptions<K, V>,
    stripes: [RwLock<Stripe<K>>; STRIPES],
    entries: AtomicUsize,
    retired_stripes: AtomicU64,
}

#[derive(Default)]
struct Timing {
    last: Option<Instant>,
    duration: Duration,
}

pub(crate) struct LifecycleState<K, V> {
    configured: OnceLock<Box<Configured<K, V>>>,
    metrics: AtomicMetrics,
    sequence: AtomicU64,
    epoch: AtomicU64,
    dirty_epoch: AtomicU64,
    repaired_epoch: AtomicU64,
    work_epoch: AtomicU64,
    maintained_epoch: AtomicU64,
    reconciliations: AtomicU64,
    callback_panics: AtomicU64,
    suppressed_callbacks: AtomicU64,
    callback: RwLock<Option<Callback<K, V>>>,
    timing: Mutex<Timing>,
    pub(crate) scheduler: Arc<SchedulerRegistry>,
}

pub(crate) struct PreparedInsert<'a, K, V> {
    state: &'a LifecycleState<K, V>,
    key: Option<Arc<K>>,
    hash: u64,
    weight: usize,
}

impl<K, V> Drop for PreparedInsert<'_, K, V> {
    fn drop(&mut self) {
        // Preparation can precede user Hash/Drop in the actual map mutation.
        // Keep unused key copies for the outer lock-free retirement boundary.
        if let Some(key) = self.key.take() {
            self.state.defer_key(self.hash, key);
        }
    }
}

pub(crate) struct EvictionCandidate<K> {
    pub(crate) key: Arc<K>,
    pub(crate) weight: usize,
    hash: u64,
    epoch: u64,
    incarnation: u64,
    inserted: u64,
    accessed: u64,
    frequency: u64,
}

impl<K> EvictionCandidate<K> {
    fn rank(&self, policy: CapacityPolicy) -> (u64, u64, u64) {
        policy_rank(
            policy,
            self.inserted,
            self.accessed,
            self.frequency,
            self.incarnation,
        )
    }
}

fn policy_rank(
    policy: CapacityPolicy,
    inserted: u64,
    accessed: u64,
    frequency: u64,
    incarnation: u64,
) -> (u64, u64, u64) {
    match policy {
        CapacityPolicy::Lru => (accessed, inserted, incarnation),
        CapacityPolicy::Lfu => (frequency, accessed, inserted),
        CapacityPolicy::Fifo => (inserted, 0, incarnation),
    }
}

pub(crate) struct MutationTicket<'a> {
    dirty: Option<&'a AtomicU64>,
    finished: bool,
}

/// Keep a newly prepared key alive across an Eq panic after the real data write.
/// This guard precedes the metadata lock, so its unwind retirement cannot invert
/// lock order or run the key's destructor while a data guard remains held.
struct PreparedKeyGuard<'a, K, V> {
    state: &'a LifecycleState<K, V>,
    key: Arc<K>,
    hash: u64,
}

impl<K, V> Drop for PreparedKeyGuard<'_, K, V> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.state.defer_key(self.hash, self.key.clone());
        }
    }
}

impl MutationTicket<'_> {
    pub(crate) fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for MutationTicket<'_> {
    fn drop(&mut self) {
        if !self.finished
            && let Some(dirty) = self.dirty
        {
            dirty.fetch_add(1, Ordering::Release);
        }
    }
}

impl<K, V> LifecycleState<K, V> {
    pub(crate) fn new() -> Self {
        Self {
            configured: OnceLock::new(),
            metrics: AtomicMetrics::default(),
            sequence: AtomicU64::new(1),
            epoch: AtomicU64::new(1),
            dirty_epoch: AtomicU64::new(0),
            repaired_epoch: AtomicU64::new(0),
            work_epoch: AtomicU64::new(0),
            maintained_epoch: AtomicU64::new(0),
            reconciliations: AtomicU64::new(0),
            callback_panics: AtomicU64::new(0),
            suppressed_callbacks: AtomicU64::new(0),
            callback: RwLock::new(None),
            timing: Mutex::new(Timing::default()),
            scheduler: Arc::new(SchedulerRegistry::new()),
        }
    }

    pub(crate) fn configure(&self, options: EvictionOptions<K, V>) -> Result<(), EvictionError> {
        options.validate()?;
        self.configured
            .set(Box::new(Configured {
                options,
                stripes: std::array::from_fn(|_| RwLock::new(Stripe::default())),
                entries: AtomicUsize::new(0),
                retired_stripes: AtomicU64::new(0),
            }))
            .map_err(|_| EvictionError::AlreadyConfigured)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.configured.get().is_some()
    }
    pub(crate) fn config(&self) -> Option<&EvictionConfig> {
        self.configured.get().map(|c| c.options.config())
    }
    pub(crate) fn policy(&self) -> Option<CapacityPolicy> {
        self.configured.get().map(|c| c.options.policy())
    }
    pub(crate) fn memory_budget(&self) -> Option<usize> {
        self.configured
            .get()
            .and_then(|c| c.options.memory_budget())
    }

    pub(crate) fn mutation(&self) -> MutationTicket<'_> {
        if self.enabled() {
            self.mark_pending();
            MutationTicket {
                dirty: Some(&self.dirty_epoch),
                finished: false,
            }
        } else {
            MutationTicket {
                dirty: None,
                finished: false,
            }
        }
    }

    pub(crate) fn mark_dirty(&self) {
        if self.enabled() {
            self.dirty_epoch.fetch_add(1, Ordering::Release);
            self.mark_pending();
        }
    }

    pub(crate) fn dirty(&self) -> bool {
        self.dirty_epoch.load(Ordering::Acquire) != self.repaired_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn mark_pending(&self) {
        if self.enabled() {
            self.work_epoch.fetch_add(1, Ordering::Release);
        }
    }

    pub(crate) fn maintenance_generation(&self) -> u64 {
        self.work_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn mark_maintained(&self, observed: u64) {
        self.maintained_epoch.fetch_max(observed, Ordering::Release);
    }

    pub(crate) fn pending(&self) -> bool {
        self.dirty()
            || self.work_epoch.load(Ordering::Acquire)
                != self.maintained_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn weight(&self, key: &K, value: &V) -> usize {
        self.configured
            .get()
            .and_then(|c| c.options.weigher.as_ref())
            .map_or(0, |weigher| weigher(key, value))
    }

    pub(crate) fn set_callback(&self, callback: impl Fn(&K, &V) + Send + Sync + 'static) {
        let retired = self
            .callback
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .replace(Arc::new(callback));
        drop(retired);
    }

    pub(crate) fn clear_callback(&self) {
        let retired = self
            .callback
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(retired);
    }

    pub(crate) fn metrics_snapshot(&self) -> MetricsStats {
        let mut result = self.metrics.snapshot();
        result.last_evict_time = self.timing.lock().unwrap_or_else(|e| e.into_inner()).last;
        result
    }

    pub(crate) fn reset_metrics(&self) {
        self.metrics.reset();
        self.callback_panics.store(0, Ordering::Relaxed);
        self.suppressed_callbacks.store(0, Ordering::Relaxed);
        *self.timing.lock().unwrap_or_else(|e| e.into_inner()) = Timing::default();
    }

    pub(crate) fn record_eviction_duration(&self, duration: Duration) {
        self.timing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .duration = duration;
    }

    fn exact_weight(&self) -> u128 {
        self.configured.get().map_or(0, |configured| {
            configured
                .stripes
                .iter()
                .map(|stripe| stripe.read().unwrap_or_else(|e| e.into_inner()).weight)
                .sum()
        })
    }

    #[cfg(test)]
    fn total_weight(&self) -> usize {
        self.exact_weight().min(usize::MAX as u128) as usize
    }

    pub(crate) fn over_memory_budget(&self) -> bool {
        self.memory_budget()
            .is_some_and(|budget| self.exact_weight() > budget as u128)
    }

    pub(crate) fn runtime_stats(&self) -> EvictionRuntimeStats {
        let weight = self.exact_weight();
        let timing = self.timing.lock().unwrap_or_else(|e| e.into_inner());
        EvictionRuntimeStats {
            enabled: self.enabled(),
            pending: self.pending(),
            dirty: self.dirty(),
            tracked_entries: self
                .configured
                .get()
                .map_or(0, |c| c.entries.load(Ordering::Relaxed)),
            estimated_weight: weight.min(usize::MAX as u128) as usize,
            weight_overflow: weight > usize::MAX as u128,
            reconciliations: self.reconciliations.load(Ordering::Relaxed),
            callback_panics: self.callback_panics.load(Ordering::Relaxed),
            suppressed_callbacks: self.suppressed_callbacks.load(Ordering::Relaxed),
            last_evict_time: timing.last,
            last_eviction_duration: timing.duration,
        }
    }

    fn drain_retired(&self) -> Vec<Arc<K>> {
        let Some(configured) = self.configured.get() else {
            return Vec::new();
        };
        let mut pending = configured.retired_stripes.swap(0, Ordering::AcqRel);
        if pending == 0 {
            return Vec::new();
        }
        let mut retired = Vec::new();
        while pending != 0 {
            let index = pending.trailing_zeros() as usize;
            pending &= pending - 1;
            let mut stripe = configured.stripes[index]
                .write()
                .unwrap_or_else(|e| e.into_inner());
            retired.append(&mut stripe.retired);
        }
        retired
    }

    fn defer_key(&self, hash: u64, key: Arc<K>) {
        if let Some(configured) = self.configured.get() {
            let index = hash as usize % STRIPES;
            configured.stripes[index]
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .retired
                .push(key);
            // Publish only after enqueueing. A concurrent drain may consume
            // this key using an older bit; a redundant empty bit is harmless.
            configured
                .retired_stripes
                .fetch_or(1_u64 << index, Ordering::Release);
        }
    }

    /// Call only after all routing, shard, metadata, and maintenance guards drop.
    pub(crate) fn dispatch_callbacks(&self, entries: Vec<(K, V)>) {
        let retired = self.drain_retired();
        let callback = self
            .callback
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(callback) = callback
            && !entries.is_empty()
        {
            if DISPATCHING.with(|depth| depth.replace(true)) {
                self.suppressed_callbacks
                    .fetch_add(entries.len() as u64, Ordering::Relaxed);
            } else {
                let _dispatch = DispatchGuard;
                for (key, value) in &entries {
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            callback(key, value)
                        }))
                    {
                        self.callback_panics.fetch_add(1, Ordering::Relaxed);
                        // A custom panic payload may itself panic when dropped.
                        // Destroy ordinary payloads normally; if destruction
                        // panics, do not run the secondary payload's destructor.
                        if let Err(secondary) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
                        {
                            std::mem::forget(secondary);
                        }
                    }
                }
            }
        }
        // Both original values and metadata key copies retire outside every lock.
        drop(entries);
        drop(retired);
    }
}

impl<K: Eq + Hash + Clone, V> LifecycleState<K, V> {
    fn stamp(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn prepare_insert(&self, key: &K, value: &V) -> PreparedInsert<'_, K, V> {
        let Some(configured) = self.configured.get() else {
            return PreparedInsert {
                state: self,
                key: None,
                hash: 0,
                weight: 0,
            };
        };
        let weight = self.weight(key, value);
        let hash = FxBuildHasher.hash_one(key);
        let existing = {
            let stripe = configured.stripes[hash as usize % STRIPES]
                .read()
                .unwrap_or_else(|e| e.into_inner());
            stripe
                .buckets
                .get(&hash)
                .and_then(|bucket| bucket.iter().find(|entry| *entry.key == *key))
                .map(|entry| entry.key.clone())
        };
        PreparedInsert {
            state: self,
            key: Some(existing.unwrap_or_else(|| Arc::new(key.clone()))),
            hash,
            weight,
        }
    }

    pub(crate) fn record_insert(&self, mut prepared: PreparedInsert<'_, K, V>, replaced: bool) {
        self.metrics.inserts.fetch_add(1, Ordering::Relaxed);
        let (Some(configured), Some(key)) = (self.configured.get(), prepared.key.take()) else {
            return;
        };
        let _prepared_key = PreparedKeyGuard {
            state: self,
            key: key.clone(),
            hash: prepared.hash,
        };
        let mut stripe = configured.stripes[prepared.hash as usize % STRIPES]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let stamp = self.stamp();
        let bucket = stripe
            .buckets
            .entry(prepared.hash)
            .or_insert_with(|| Vec::with_capacity(1));
        if let Some(entry) = bucket
            .iter_mut()
            .find(|entry| Arc::ptr_eq(&entry.key, &key) || *entry.key == *key)
        {
            let old_weight = entry.weight;
            entry.weight = prepared.weight;
            entry.incarnation = stamp;
            entry.accessed.store(stamp, Ordering::Relaxed);
            if !replaced {
                // Authoritative insert results distinguish a new incarnation
                // from replacement even when an interrupted remove left a ghost.
                entry.inserted = stamp;
                entry.frequency.store(1, Ordering::Relaxed);
            } else {
                entry
                    .frequency
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_add(1))
                    })
                    .ok();
            }
            stripe.weight = stripe.weight - old_weight as u128 + prepared.weight as u128;
        } else {
            bucket.push(Entry {
                key,
                incarnation: stamp,
                inserted: stamp,
                accessed: AtomicU64::new(stamp),
                frequency: AtomicU64::new(1),
                weight: prepared.weight,
            });
            stripe.weight += prepared.weight as u128;
            configured.entries.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_read<Q: Eq + Hash + ?Sized>(&self, key: &Q, hit: bool)
    where
        K: Borrow<Q>,
    {
        self.metrics.gets.fetch_add(1, Ordering::Relaxed);
        if !hit {
            self.metrics.misses.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.metrics.hits.fetch_add(1, Ordering::Relaxed);
        let Some(configured) = self.configured.get() else {
            return;
        };
        let hash = FxBuildHasher.hash_one(key);
        let stripe = configured.stripes[hash as usize % STRIPES]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = stripe.buckets.get(&hash).and_then(|bucket| {
            bucket
                .iter()
                .find(|entry| entry.key.as_ref().borrow() == key)
        }) {
            // Allocate the stamp only after obtaining the shared stripe guard.
            // fetch_max also preserves order between concurrent read hooks.
            entry.accessed.fetch_max(self.stamp(), Ordering::Relaxed);
            entry
                .frequency
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_add(1))
                })
                .ok();
        }
    }

    pub(crate) fn record_remove<Q: Eq + Hash + ?Sized>(&self, key: &Q)
    where
        K: Borrow<Q>,
    {
        self.metrics.removes.fetch_add(1, Ordering::Relaxed);
        let Some(configured) = self.configured.get() else {
            return;
        };
        let hash = FxBuildHasher.hash_one(key);
        let mut stripe = configured.stripes[hash as usize % STRIPES]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let removed = stripe.buckets.get_mut(&hash).and_then(|bucket| {
            let index = bucket
                .iter()
                .position(|entry| entry.key.as_ref().borrow() == key)?;
            Some(bucket.swap_remove(index))
        });
        if let Some(entry) = removed {
            stripe.weight -= entry.weight as u128;
            stripe.retired.push(entry.key);
            configured.entries.fetch_sub(1, Ordering::Relaxed);
            configured
                .retired_stripes
                .fetch_or(1_u64 << (hash as usize % STRIPES), Ordering::Release);
        }
        if stripe.buckets.get(&hash).is_some_and(Vec::is_empty) {
            stripe.buckets.remove(&hash);
        }
    }

    pub(crate) fn candidates(&self, policy: CapacityPolicy) -> Vec<EvictionCandidate<K>> {
        let Some(configured) = self.configured.get() else {
            return Vec::new();
        };
        if self.dirty() {
            return Vec::new();
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        let mut candidates = Vec::with_capacity(configured.entries.load(Ordering::Relaxed));
        for stripe in &configured.stripes {
            let stripe = stripe.read().unwrap_or_else(|e| e.into_inner());
            for (&hash, bucket) in &stripe.buckets {
                for entry in bucket {
                    candidates.push(EvictionCandidate {
                        key: entry.key.clone(),
                        weight: entry.weight,
                        hash,
                        epoch,
                        incarnation: entry.incarnation,
                        inserted: entry.inserted,
                        accessed: entry.accessed.load(Ordering::Relaxed),
                        frequency: entry.frequency.load(Ordering::Relaxed),
                    });
                }
            }
        }
        candidates.sort_unstable_by_key(|candidate| candidate.rank(policy));
        candidates
    }

    /// Select one victim without allocating or sorting all candidate entries.
    /// The caller must hold exclusive routing for the entire scan and removal.
    pub(crate) fn minimum_candidate(&self, policy: CapacityPolicy) -> Option<EvictionCandidate<K>> {
        let configured = self.configured.get()?;
        if self.dirty() {
            return None;
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        let mut minimum: Option<EvictionCandidate<K>> = None;
        for stripe in &configured.stripes {
            let stripe = stripe.read().unwrap_or_else(|e| e.into_inner());
            for (&hash, bucket) in &stripe.buckets {
                for entry in bucket {
                    let accessed = entry.accessed.load(Ordering::Relaxed);
                    let frequency = entry.frequency.load(Ordering::Relaxed);
                    let rank = policy_rank(
                        policy,
                        entry.inserted,
                        accessed,
                        frequency,
                        entry.incarnation,
                    );
                    if minimum.as_ref().is_none_or(|old| rank < old.rank(policy)) {
                        minimum = Some(EvictionCandidate {
                            key: entry.key.clone(),
                            weight: entry.weight,
                            hash,
                            epoch,
                            incarnation: entry.incarnation,
                            inserted: entry.inserted,
                            accessed,
                            frequency,
                        });
                    }
                }
            }
        }
        minimum
    }

    /// Rank only the live keys of a data shard already locked by the caller.
    /// User Hash/Eq may panic during lookup, before any deletion has occurred.
    /// Routing and that data-shard guard must outlive candidate selection/use.
    pub(crate) fn candidates_for_keys<'a, I>(
        &self,
        policy: CapacityPolicy,
        keys: I,
    ) -> Vec<EvictionCandidate<K>>
    where
        K: 'a,
        I: IntoIterator<Item = &'a K>,
    {
        let Some(configured) = self.configured.get() else {
            return Vec::new();
        };
        if self.dirty() {
            return Vec::new();
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        let keys = keys.into_iter();
        let mut candidates = Vec::with_capacity(
            keys.size_hint()
                .0
                .min(configured.entries.load(Ordering::Relaxed)),
        );
        for key in keys {
            let hash = FxBuildHasher.hash_one(key);
            let stripe = configured.stripes[hash as usize % STRIPES]
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = stripe
                .buckets
                .get(&hash)
                .and_then(|bucket| bucket.iter().find(|entry| *entry.key == *key))
            {
                candidates.push(EvictionCandidate {
                    key: entry.key.clone(),
                    weight: entry.weight,
                    hash,
                    epoch,
                    incarnation: entry.incarnation,
                    inserted: entry.inserted,
                    accessed: entry.accessed.load(Ordering::Relaxed),
                    frequency: entry.frequency.load(Ordering::Relaxed),
                });
            }
        }
        candidates.sort_unstable_by_key(|candidate| candidate.rank(policy));
        candidates
    }

    pub(crate) fn validate(&self, candidate: &EvictionCandidate<K>) -> bool {
        if self.dirty() || candidate.epoch != self.epoch.load(Ordering::Acquire) {
            return false;
        }
        let Some(configured) = self.configured.get() else {
            return false;
        };
        let stripe = configured.stripes[candidate.hash as usize % STRIPES]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        stripe.buckets.get(&candidate.hash).is_some_and(|bucket| {
            bucket.iter().any(|entry| {
                entry.incarnation == candidate.incarnation
                    && entry.weight == candidate.weight
                    && entry.accessed.load(Ordering::Relaxed) == candidate.accessed
                    && entry.frequency.load(Ordering::Relaxed) == candidate.frequency
            })
        })
    }

    pub(crate) fn record_eviction(&self, candidate: &EvictionCandidate<K>) {
        self.metrics.evictions.fetch_add(1, Ordering::Relaxed);
        self.timing.lock().unwrap_or_else(|e| e.into_inner()).last = Some(Instant::now());
        let Some(configured) = self.configured.get() else {
            return;
        };
        let mut stripe = configured.stripes[candidate.hash as usize % STRIPES]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let removed = stripe.buckets.get_mut(&candidate.hash).and_then(|bucket| {
            let index = bucket
                .iter()
                .position(|entry| entry.incarnation == candidate.incarnation)?;
            Some(bucket.swap_remove(index))
        });
        if let Some(entry) = removed {
            stripe.weight -= entry.weight as u128;
            stripe.retired.push(entry.key);
            configured.entries.fetch_sub(1, Ordering::Relaxed);
            configured.retired_stripes.fetch_or(
                1_u64 << (candidate.hash as usize % STRIPES),
                Ordering::Release,
            );
        }
        if stripe
            .buckets
            .get(&candidate.hash)
            .is_some_and(Vec::is_empty)
        {
            stripe.buckets.remove(&candidate.hash);
        }
    }

    /// The caller must hold exclusive routing throughout its stable data scan
    /// and this publication. Successful hooks never clear an earlier failure.
    pub(crate) fn reconcile(&self, entries: &[(Arc<K>, usize)]) {
        let Some(configured) = self.configured.get() else {
            return;
        };
        let observed_dirty = self.dirty_epoch.load(Ordering::Acquire);
        let mut staged: [Stripe<K>; STRIPES] = std::array::from_fn(|_| Stripe::default());
        let count = entries.len();
        for (key, weight) in entries {
            let hash = FxBuildHasher.hash_one(key.as_ref());
            let index = hash as usize % STRIPES;
            let stamp = self.stamp();
            let (inserted, accessed, frequency) = {
                let old = configured.stripes[index]
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                old.buckets
                    .get(&hash)
                    .and_then(|bucket| bucket.iter().find(|entry| *entry.key == **key))
                    .map_or((stamp, stamp, 1), |entry| {
                        (
                            entry.inserted,
                            entry.accessed.load(Ordering::Relaxed),
                            entry.frequency.load(Ordering::Relaxed),
                        )
                    })
            };
            staged[index].weight += *weight as u128;
            staged[index]
                .buckets
                .entry(hash)
                .or_insert_with(|| Vec::with_capacity(1))
                .push(Entry {
                    key: key.clone(),
                    incarnation: stamp,
                    inserted,
                    accessed: AtomicU64::new(accessed),
                    frequency: AtomicU64::new(frequency),
                    weight: *weight,
                });
        }
        let mut guards: Vec<_> = configured
            .stripes
            .iter()
            .map(|stripe| stripe.write().unwrap_or_else(|e| e.into_inner()))
            .collect();
        let mut retired_stripes = 0_u64;
        for (index, (guard, mut replacement)) in guards.iter_mut().zip(staged).enumerate() {
            // No user Hash/Eq/Clone/Drop occurs during publication. Retire the
            // replaced key allocations at the outer lock-free dispatch boundary.
            replacement.retired.append(&mut guard.retired);
            for (_, bucket) in guard.buckets.drain() {
                replacement
                    .retired
                    .extend(bucket.into_iter().map(|entry| entry.key));
            }
            if !replacement.retired.is_empty() {
                retired_stripes |= 1_u64 << index;
            }
            **guard = replacement;
        }
        configured.entries.store(count, Ordering::Relaxed);
        configured
            .retired_stripes
            .fetch_or(retired_stripes, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::Release);
        self.repaired_epoch.store(observed_dirty, Ordering::Release);
        self.reconciliations.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn disabled_state_does_not_embed_the_configured_metadata_stripes() {
        assert!(
            std::mem::size_of::<LifecycleState<String, usize>>()
                < std::mem::size_of::<Configured<String, usize>>()
        );
        assert!(
            LifecycleState::<String, usize>::new()
                .configured
                .get()
                .is_none()
        );
    }

    #[test]
    fn single_entry_buckets_start_compact_and_collisions_remain_distinct() {
        #[derive(Clone, PartialEq, Eq)]
        struct Key(u64);
        impl Hash for Key {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                0_u64.hash(state);
            }
        }
        let state = LifecycleState::new();
        state
            .configure(
                EvictionOptions::new(EvictionConfig {
                    background_enabled: false,
                    ..EvictionConfig::default()
                })
                .with_policy(CapacityPolicy::Fifo),
            )
            .unwrap();
        let hash = FxBuildHasher.hash_one(Key(0));
        let bucket_shape = || {
            let configured = state.configured.get().unwrap();
            let stripe = configured.stripes[hash as usize % STRIPES].read().unwrap();
            let bucket = stripe.buckets.get(&hash).unwrap();
            (bucket.len(), bucket.capacity())
        };
        state.record_insert(state.prepare_insert(&Key(0), &0_usize), false);
        assert_eq!(bucket_shape(), (1, 1));
        state.reconcile(&[(Arc::new(Key(0)), 0)]);
        assert_eq!(bucket_shape(), (1, 1));

        state.record_insert(state.prepare_insert(&Key(1), &0), false);
        assert_eq!(bucket_shape().0, 2);
        state.reconcile(&[(Arc::new(Key(0)), 0), (Arc::new(Key(1)), 0)]);
        let candidates = state.candidates(CapacityPolicy::Fifo);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.key.0)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(candidates.iter().all(|candidate| state.validate(candidate)));
        state.record_remove(&Key(0));
        assert_eq!(
            state.minimum_candidate(CapacityPolicy::Fifo).unwrap().key.0,
            1
        );
        assert_eq!(bucket_shape().0, 1);
    }

    #[test]
    fn empty_and_sparse_retirement_drains_do_not_lock_unmarked_stripes() {
        for has_retired in [false, true] {
            let state = Arc::new(state(CapacityPolicy::Lru));
            if has_retired {
                state.defer_key(63, Arc::new("retired".to_owned()));
            }
            let configured = state.configured.get().unwrap();
            let unrelated = configured.stripes[0].write().unwrap();
            let (sent, received) = std::sync::mpsc::channel();
            let worker_state = state.clone();
            let worker = std::thread::spawn(move || {
                sent.send(worker_state.drain_retired().len()).unwrap();
            });
            let result = received.recv_timeout(Duration::from_secs(3));
            // Always release the deliberately held lock before asserting, so a
            // regression fails instead of stranding a blocked test worker.
            drop(unrelated);
            worker.join().unwrap();
            assert_eq!(result.unwrap(), usize::from(has_retired));
        }
    }

    #[test]
    fn concurrent_retirement_publication_and_draining_never_lose_keys() {
        struct Key(Arc<AtomicUsize>);
        impl Drop for Key {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let state: Arc<LifecycleState<Key, ()>> = Arc::new(LifecycleState::new());
        state
            .configure(EvictionOptions::new(EvictionConfig {
                background_enabled: false,
                ..EvictionConfig::default()
            }))
            .unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let consumer_state = state.clone();
        let consumer_done = done.clone();
        let consumer = std::thread::spawn(move || {
            while !consumer_done.load(Ordering::Acquire) {
                drop(consumer_state.drain_retired());
                std::thread::yield_now();
            }
            drop(consumer_state.drain_retired());
        });
        let mut producers = Vec::new();
        for worker in 0..4 {
            let state = state.clone();
            let dropped = dropped.clone();
            producers.push(std::thread::spawn(move || {
                for index in 0..512 {
                    state.defer_key((index + worker * 7) as u64, Arc::new(Key(dropped.clone())));
                    if index % 8 == 0 {
                        std::thread::yield_now();
                    }
                }
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }
        done.store(true, Ordering::Release);
        consumer.join().unwrap();
        drop(state.drain_retired());
        assert_eq!(dropped.load(Ordering::Relaxed), 4 * 512);
        let configured = state.configured.get().unwrap();
        assert_eq!(configured.retired_stripes.load(Ordering::Acquire), 0);
        assert!(
            configured
                .stripes
                .iter()
                .all(|stripe| stripe.read().unwrap().retired.is_empty())
        );
    }

    fn state(policy: CapacityPolicy) -> LifecycleState<String, usize> {
        let state = LifecycleState::new();
        state
            .configure(
                EvictionOptions::new(EvictionConfig {
                    background_enabled: false,
                    ..EvictionConfig::default()
                })
                .with_policy(policy),
            )
            .unwrap();
        state
    }

    fn insert(state: &LifecycleState<String, usize>, key: &str, weight: usize) {
        let ticket = state.mutation();
        let prepared = state.prepare_insert(&key.to_owned(), &weight);
        let replaced = state
            .candidates(CapacityPolicy::Fifo)
            .iter()
            .any(|candidate| candidate.key.as_str() == key);
        state.record_insert(prepared, replaced);
        ticket.finish();
    }

    fn repair<K: Eq + Hash + Clone, V>(state: &LifecycleState<K, V>, entries: Vec<(K, usize)>) {
        let entries: Vec<_> = entries
            .into_iter()
            .map(|(key, weight)| (Arc::new(key), weight))
            .collect();
        state.reconcile(&entries);
    }

    fn ordered(state: &LifecycleState<String, usize>, policy: CapacityPolicy) -> Vec<String> {
        state
            .candidates(policy)
            .into_iter()
            .map(|candidate| candidate.key.as_ref().clone())
            .collect()
    }

    #[test]
    fn policy_ordering_preserves_fifo_age_and_tracks_lru_for_every_policy() {
        let state = state(CapacityPolicy::Fifo);
        for key in ["a", "b", "c"] {
            insert(&state, key, 0);
        }
        state.record_read("a", true);
        assert_eq!(ordered(&state, CapacityPolicy::Lru), ["b", "c", "a"]);
        state.record_read("b", true);
        state.record_read("b", true);
        assert_eq!(ordered(&state, CapacityPolicy::Lfu), ["c", "a", "b"]);
        insert(&state, "a", 0);
        assert_eq!(ordered(&state, CapacityPolicy::Fifo), ["a", "b", "c"]);
    }

    #[test]
    fn minimum_candidate_matches_full_policy_order_after_accesses_and_replacements() {
        let state = state(CapacityPolicy::Lru);
        assert!(state.minimum_candidate(CapacityPolicy::Lru).is_none());
        for key in ["a", "b", "c", "d", "e"] {
            insert(&state, key, 0);
        }
        state.record_read("a", true);
        state.record_read("c", true);
        state.record_read("c", true);
        insert(&state, "b", 0);
        for policy in [
            CapacityPolicy::Lru,
            CapacityPolicy::Lfu,
            CapacityPolicy::Fifo,
        ] {
            let minimum = state.minimum_candidate(policy).unwrap();
            let full = state.candidates(policy);
            assert_eq!(minimum.key, full[0].key);
            assert_eq!(minimum.rank(policy), full[0].rank(policy));
            assert!(state.validate(&minimum));
        }
        state.mark_dirty();
        assert!(state.minimum_candidate(CapacityPolicy::Lru).is_none());
    }

    #[test]
    fn shard_candidates_only_include_requested_live_keys_in_policy_order() {
        let state = state(CapacityPolicy::Fifo);
        for key in ["a", "b", "c", "d", "e"] {
            insert(&state, key, 0);
        }
        state.record_read("a", true);
        state.record_read("c", true);
        state.record_read("c", true);
        let keys: Vec<_> = ["c", "missing", "a", "e"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for policy in [
            CapacityPolicy::Lru,
            CapacityPolicy::Lfu,
            CapacityPolicy::Fifo,
        ] {
            let local = state.candidates_for_keys(policy, keys.iter());
            let full = state.candidates(policy);
            let expected: Vec<_> = full
                .iter()
                .filter(|candidate| keys.contains(candidate.key.as_ref()))
                .map(|candidate| candidate.key.as_str())
                .collect();
            assert_eq!(
                local
                    .iter()
                    .map(|candidate| candidate.key.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(local.iter().all(|candidate| state.validate(candidate)));
        }
        let candidates = state.candidates_for_keys(CapacityPolicy::Lru, keys.iter());
        let first = &candidates[0];
        state.record_read(first.key.as_str(), true);
        assert!(!state.validate(first));
        state.mark_dirty();
        assert!(
            state
                .candidates_for_keys(CapacityPolicy::Lru, keys.iter())
                .is_empty()
        );
    }

    #[test]
    fn read_replace_remove_reinsert_and_repair_reject_stale_candidates() {
        let state = state(CapacityPolicy::Lru);
        insert(&state, "a", 0);
        let first = state.candidates(CapacityPolicy::Lru).remove(0);
        assert!(state.validate(&first));
        state.record_read("a", true);
        assert!(!state.validate(&first));
        let touched = state.candidates(CapacityPolicy::Lru).remove(0);
        insert(&state, "a", 0);
        assert!(!state.validate(&touched));
        let replaced = state.candidates(CapacityPolicy::Lru).remove(0);
        state.record_remove("a");
        insert(&state, "a", 0);
        assert!(!state.validate(&replaced));
        let before_repair = state.candidates(CapacityPolicy::Lru).remove(0);
        state.mark_dirty();
        assert!(state.candidates(CapacityPolicy::Lru).is_empty());
        repair(&state, vec![("a".to_owned(), 0)]);
        assert!(!state.validate(&before_repair));
        assert!(state.validate(&state.candidates(CapacityPolicy::Lru).remove(0)));
    }

    #[test]
    fn unrelated_success_never_clears_failure_or_newer_pending_work() {
        let state = state(CapacityPolicy::Lru);
        drop(state.mutation());
        insert(&state, "a", 0);
        assert!(state.dirty());
        repair(&state, vec![("a".to_owned(), 0)]);
        assert!(!state.dirty());
        let observed = state.maintenance_generation();
        insert(&state, "b", 0);
        state.mark_maintained(observed);
        assert!(state.pending());
        state.mark_maintained(state.maintenance_generation());
        assert!(!state.pending());
    }

    #[test]
    fn dirty_reinsertion_never_inherits_a_removed_incarnations_fifo_age() {
        let state = state(CapacityPolicy::Fifo);
        insert(&state, "a", 0);
        insert(&state, "b", 0);
        // The authoritative map removed a, but its metadata Hash hook panicked.
        // This deliberately leaves a ghost until reconciliation can run.
        drop(state.mutation());
        insert(&state, "a", 0);
        repair(&state, vec![("a".to_owned(), 0), ("b".to_owned(), 0)]);
        assert_eq!(ordered(&state, CapacityPolicy::Fifo), ["b", "a"]);
    }

    #[test]
    fn unrelated_failure_does_not_refresh_an_existing_replacements_fifo_age() {
        let state = state(CapacityPolicy::Fifo);
        insert(&state, "a", 0);
        insert(&state, "b", 0);
        drop(state.mutation());
        let ticket = state.mutation();
        state.record_insert(state.prepare_insert(&"a".to_owned(), &0), true);
        ticket.finish();
        repair(&state, vec![("a".to_owned(), 0), ("b".to_owned(), 0)]);
        assert_eq!(ordered(&state, CapacityPolicy::Fifo), ["a", "b"]);
    }

    #[test]
    fn weight_overflow_never_bypasses_the_budget() {
        let state = LifecycleState::new();
        state
            .configure(
                EvictionOptions::new(EvictionConfig {
                    background_enabled: false,
                    ..EvictionConfig::default()
                })
                .with_memory_budget(usize::MAX, |_: &String, weight: &usize| *weight),
            )
            .unwrap();
        insert(&state, "a", usize::MAX);
        insert(&state, "b", usize::MAX);
        assert_eq!(state.total_weight(), usize::MAX);
        assert!(state.over_memory_budget());
        assert!(state.runtime_stats().weight_overflow);
        state.record_remove("a");
        assert!(!state.over_memory_budget());
        assert!(!state.runtime_stats().weight_overflow);
    }

    #[test]
    fn disabled_tracking_never_calls_user_hash_clone_or_estimation() {
        #[derive(PartialEq, Eq)]
        struct Unusable;
        impl Clone for Unusable {
            fn clone(&self) -> Self {
                panic!("clone");
            }
        }
        impl Hash for Unusable {
            fn hash<H: std::hash::Hasher>(&self, _: &mut H) {
                panic!("hash");
            }
        }
        let state: LifecycleState<Unusable, usize> = LifecycleState::new();
        state.record_insert(state.prepare_insert(&Unusable, &1), false);
        state.record_read(&Unusable, true);
        state.record_read(&Unusable, false);
        state.record_remove(&Unusable);
        let stats = state.metrics_snapshot();
        assert_eq!(
            (
                stats.total_inserts,
                stats.total_removes,
                stats.total_gets,
                stats.hits,
                stats.misses
            ),
            (1, 1, 2, 1, 1)
        );
        assert!(state.configured.get().is_none());
    }

    #[test]
    fn callback_panic_isolated_after_real_eviction_metrics_and_time() {
        let state = state(CapacityPolicy::Lru);
        insert(&state, "a", 0);
        let candidate = state.candidates(CapacityPolicy::Lru).remove(0);
        state.record_eviction(&candidate);
        state.set_callback(|_, _| panic!("callback"));
        state.dispatch_callbacks(vec![("a".to_owned(), 0)]);
        let stats = state.metrics_snapshot();
        assert_eq!(stats.evictions, 1);
        assert_eq!(stats.total_removes, 0);
        assert!(stats.last_evict_time.is_some());
        assert_eq!(state.runtime_stats().callback_panics, 1);
    }

    #[test]
    fn callbacks_can_replace_themselves_and_nested_dispatch_is_suppressed() {
        let state = Arc::new(state(CapacityPolicy::Lru));
        let weak = Arc::downgrade(&state);
        state.set_callback(move |_, _| {
            let state = weak.upgrade().unwrap();
            state.dispatch_callbacks(vec![("nested".to_owned(), 0)]);
            state.clear_callback();
        });
        state.dispatch_callbacks(vec![("outer".to_owned(), 0)]);
        assert_eq!(state.runtime_stats().suppressed_callbacks, 1);
        assert_eq!(state.runtime_stats().callback_panics, 0);
    }

    #[test]
    fn ordinary_callback_panic_payload_is_destroyed_without_leaking() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let state = state(CapacityPolicy::Lru);
        let drops = Arc::new(AtomicUsize::new(0));
        let callback_drops = drops.clone();
        state.set_callback(move |_, _| std::panic::panic_any(Payload(callback_drops.clone())));
        state.dispatch_callbacks(vec![("a".to_owned(), 0)]);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(state.runtime_stats().callback_panics, 1);
    }

    #[test]
    fn panicking_callback_payload_destructor_stays_isolated() {
        struct Secondary(Arc<AtomicUsize>);
        impl Drop for Secondary {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
                panic!("secondary panic payload destructor must not run");
            }
        }
        struct Payload {
            primary_drops: Arc<AtomicUsize>,
            secondary_drops: Arc<AtomicUsize>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                self.primary_drops.fetch_add(1, Ordering::Relaxed);
                std::panic::panic_any(Secondary(self.secondary_drops.clone()));
            }
        }
        let state = state(CapacityPolicy::Lru);
        let primary_drops = Arc::new(AtomicUsize::new(0));
        let secondary_drops = Arc::new(AtomicUsize::new(0));
        let primary = primary_drops.clone();
        let secondary = secondary_drops.clone();
        state.set_callback(move |_, _| {
            std::panic::panic_any(Payload {
                primary_drops: primary.clone(),
                secondary_drops: secondary.clone(),
            })
        });
        state.dispatch_callbacks(vec![("a".to_owned(), 0)]);
        assert_eq!(primary_drops.load(Ordering::Relaxed), 1);
        assert_eq!(secondary_drops.load(Ordering::Relaxed), 0);
        assert_eq!(state.runtime_stats().callback_panics, 1);
        state.clear_callback();
        state.dispatch_callbacks(vec![("b".to_owned(), 0)]);
        assert_eq!(state.runtime_stats().callback_panics, 1);
    }

    #[test]
    fn repaired_metadata_preserves_valid_order_and_does_not_count_as_access() {
        let state = state(CapacityPolicy::Lru);
        for key in ["a", "b", "c"] {
            insert(&state, key, 0);
        }
        state.record_read("a", true);
        let previous = state.metrics_snapshot();
        state.mark_dirty();
        repair(
            &state,
            vec![("c".into(), 0), ("a".into(), 0), ("b".into(), 0)],
        );
        assert_eq!(ordered(&state, CapacityPolicy::Lru), ["b", "c", "a"]);
        assert_eq!(ordered(&state, CapacityPolicy::Fifo), ["a", "b", "c"]);
        assert_eq!(state.metrics_snapshot().total_gets, previous.total_gets);
        assert_eq!(
            state.metrics_snapshot().total_inserts,
            previous.total_inserts
        );
    }

    #[test]
    fn panicking_reconcile_never_publishes_partially_built_metadata() {
        #[derive(Clone)]
        struct Key {
            id: usize,
            panic: Arc<AtomicBool>,
        }
        impl PartialEq for Key {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }
        impl Eq for Key {}
        impl Hash for Key {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                assert!(!self.panic.load(Ordering::Relaxed), "hash panic");
                self.id.hash(state);
            }
        }
        let state = LifecycleState::new();
        state
            .configure(EvictionOptions::new(EvictionConfig {
                background_enabled: false,
                ..EvictionConfig::default()
            }))
            .unwrap();
        let first = Key {
            id: 0,
            panic: Arc::new(AtomicBool::new(false)),
        };
        let second = Key {
            id: 1,
            panic: Arc::new(AtomicBool::new(false)),
        };
        state.record_insert(state.prepare_insert(&first, &0_usize), false);
        state.mark_dirty();
        second.panic.store(true, Ordering::Relaxed);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repair(&state, vec![(first.clone(), 1), (second.clone(), 2)]);
            }))
            .is_err()
        );
        assert_eq!(state.runtime_stats().tracked_entries, 1);
        assert_eq!(state.total_weight(), 0);
        assert!(state.dirty());
        second.panic.store(false, Ordering::Relaxed);
        repair(&state, vec![(first, 1), (second, 2)]);
        assert_eq!(state.runtime_stats().tracked_entries, 2);
        assert_eq!(state.total_weight(), 3);
        assert!(!state.dirty());
    }

    #[test]
    fn removed_and_reconciled_metadata_keys_retire_after_all_engine_locks() {
        #[derive(Clone)]
        struct Key {
            id: usize,
            check: Arc<dyn Fn() + Send + Sync>,
        }
        impl PartialEq for Key {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }
        impl Eq for Key {}
        impl Hash for Key {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.id.hash(state);
            }
        }
        impl Drop for Key {
            fn drop(&mut self) {
                (self.check)();
            }
        }
        let state = Arc::new(LifecycleState::new());
        state
            .configure(EvictionOptions::new(EvictionConfig {
                background_enabled: false,
                ..EvictionConfig::default()
            }))
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let weak = Arc::downgrade(&state);
        let count = drops.clone();
        let key = Key {
            id: 0,
            check: Arc::new(move || {
                if let Some(state) = weak.upgrade() {
                    let configured: &Configured<Key, usize> = state.configured.get().unwrap();
                    assert!(
                        configured
                            .stripes
                            .iter()
                            .all(|stripe| stripe.try_read().is_ok())
                    );
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }),
        };
        state.record_insert(state.prepare_insert(&key, &0_usize), false);
        state.record_remove(&key);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        state.dispatch_callbacks(Vec::new());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        state.record_insert(state.prepare_insert(&key, &0), false);
        repair(&state, vec![(key.clone(), 0)]);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        state.dispatch_callbacks(Vec::new());
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}
