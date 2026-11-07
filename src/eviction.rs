//! Version 0.9.0: Eviction, Metrics, and Advanced Iteration Support
//!
//! Capacity policies, logical entry budgets, operation metrics, and maintenance.
//! Time-based expiration is deliberately separate from capacity eviction.

mod engine;
pub(crate) mod scheduler;

pub(crate) use engine::LifecycleState;
#[cfg(feature = "async")]
pub use scheduler::AsyncEvictionHandle;
pub use scheduler::{
    EvictionHandle, EvictionSchedulerError, EvictionWorkerStatus, EvictionWorkerStop,
};

#[cfg(feature = "rayon")]
use rayon::prelude::*;

// Initial scheduling cutoff, not a benchmark-derived optimum. Small filters
// retain their allocation and avoid dispatching work to the Rayon pool.
#[cfg(feature = "rayon")]
const PARALLEL_FILTER_MIN_ITEMS: usize = 1024;

/// Eviction policy for cache entries.
#[derive(Clone)]
pub enum EvictionPolicy {
    /// Least Recently Used: removes least recently accessed entries
    Lru,
    /// Least Frequently Used: removes least frequently accessed entries
    Lfu,
    /// Reserved expiry policy. Capacity constructors return [`EvictionError::UnsupportedTtl`].
    TimeToLive(std::time::Duration),
    /// Legacy custom predicate. Capacity constructors return [`EvictionError::UnsupportedCustomPolicy`].
    Custom(std::sync::Arc<dyn Fn() -> bool + Send + Sync>),
}

impl std::fmt::Debug for EvictionPolicy {
    #[tracing::instrument(skip(self, f), level = "trace")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvictionPolicy::Lru => f.write_str("LRU"),
            EvictionPolicy::Lfu => f.write_str("LFU"),
            EvictionPolicy::TimeToLive(duration) => {
                f.debug_tuple("TimeToLive").field(duration).finish()
            }
            EvictionPolicy::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

/// Configuration for capacity eviction behavior.
///
/// Successful mutations enforce limits after committing. Concurrent, cancelled,
/// or panicking operations can leave pending maintenance for a later mutation,
/// an explicit `enforce_eviction_limits` call, or a periodic worker.
#[derive(Debug, Clone)]
pub struct EvictionConfig {
    /// The eviction policy to apply
    pub policy: EvictionPolicy,
    /// Maximum total entries; None = unlimited
    pub max_entries: Option<usize>,
    /// Maximum items per shard; None = unlimited
    pub max_items_per_shard: Option<usize>,
    /// Reserved expiry setting. Capacity constructors require `None`.
    pub ttl: Option<std::time::Duration>,
    /// How often to check for eviction candidates
    pub check_interval: std::time::Duration,
    /// Whether to enable background eviction task
    pub background_enabled: bool,
}

impl Default for EvictionConfig {
    #[tracing::instrument(level = "trace")]
    fn default() -> Self {
        Self {
            policy: EvictionPolicy::Lru,
            max_entries: None,
            max_items_per_shard: None,
            ttl: None,
            check_interval: std::time::Duration::from_secs(60),
            background_enabled: true,
        }
    }
}

impl EvictionConfig {
    /// Configure automatic LRU maintenance with a global entry limit.
    ///
    /// Worker startup validates the interval and, for async maps, the runtime.
    pub fn auto_lru(max_entries: usize, check_interval: std::time::Duration) -> Self {
        Self {
            max_entries: Some(max_entries),
            check_interval,
            ..Self::default()
        }
    }
}

/// Ordering used when capacity maintenance selects entries to remove.
///
/// This separate enum leaves exhaustive matches on [`EvictionPolicy`] compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityPolicy {
    /// Least recently accessed entry first; successful writes also count as access.
    Lru,
    /// Least frequently accessed entry first, breaking ties by recency then insertion.
    Lfu,
    /// Oldest inserted entry first. Replacing an existing value preserves its age.
    Fifo,
}

/// A configuration or maintenance worker could not be installed.
#[derive(Debug)]
pub enum EvictionError {
    /// TTL policy or the TTL option requires the separate expiry implementation.
    UnsupportedTtl,
    /// The legacy zero-argument custom predicate has no entry selection contract.
    UnsupportedCustomPolicy,
    /// Global and per-shard entry limits cannot both be configured.
    ConflictingEntryLimits,
    /// A manual-only constructor was passed background_enabled=true.
    BackgroundNotAllowed,
    /// A lifecycle state can only be configured once, before sharing its map.
    AlreadyConfigured,
    /// Capacity tracking was not installed on this map.
    NotConfigured,
    /// Maintenance scheduling failed or the worker could not complete.
    Scheduler(EvictionSchedulerError),
}

impl std::fmt::Display for EvictionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedTtl => {
                f.write_str("TTL expiration is not supported by capacity eviction")
            }
            Self::UnsupportedCustomPolicy => {
                f.write_str("custom eviction predicates are not supported")
            }
            Self::ConflictingEntryLimits => {
                f.write_str("global and per-shard entry limits are mutually exclusive")
            }
            Self::BackgroundNotAllowed => {
                f.write_str("manual eviction requires background_enabled=false")
            }
            Self::AlreadyConfigured => f.write_str("eviction is already configured"),
            Self::NotConfigured => f.write_str("eviction is not configured"),
            Self::Scheduler(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for EvictionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scheduler(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EvictionSchedulerError> for EvictionError {
    fn from(error: EvictionSchedulerError) -> Self {
        Self::Scheduler(error)
    }
}

type EntryWeigher<K, V> = std::sync::Arc<dyn Fn(&K, &V) -> usize + Send + Sync>;

/// Additional capacity options without changing existing configuration literals.
///
/// Estimated weight includes only live entries as defined by the supplied
/// estimator. It does not measure allocator overhead or retained Cow snapshots.
pub struct EvictionOptions<K, V> {
    config: EvictionConfig,
    policy: Option<CapacityPolicy>,
    memory_budget: Option<usize>,
    weigher: Option<EntryWeigher<K, V>>,
}

impl<K, V> EvictionOptions<K, V> {
    /// Extend a legacy eviction configuration with optional capacity features.
    pub fn new(config: EvictionConfig) -> Self {
        Self {
            config,
            policy: None,
            memory_budget: None,
            weigher: None,
        }
    }

    /// Override LRU/LFU ordering, or opt into FIFO ordering.
    ///
    /// Unsupported TTL/custom legacy policies remain errors even with an override.
    pub fn with_policy(mut self, policy: CapacityPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Limit the sum of estimator-reported weights of live entries.
    ///
    /// The estimator runs before inserting a value, and may run during recovery.
    /// It must return a stable weight for unchanged values and must not reenter
    /// this map. Zero is a valid budget and zero-weight entries consume no budget.
    pub fn with_memory_budget(
        mut self,
        bytes: usize,
        weigher: impl Fn(&K, &V) -> usize + Send + Sync + 'static,
    ) -> Self {
        self.memory_budget = Some(bytes);
        self.weigher = Some(std::sync::Arc::new(weigher));
        self
    }

    /// Validate supported policies and mutually exclusive entry limits.
    /// Scheduler-specific interval/runtime checks occur when a worker starts.
    pub fn validate(&self) -> Result<(), EvictionError> {
        if self.config.ttl.is_some() || matches!(self.config.policy, EvictionPolicy::TimeToLive(_))
        {
            return Err(EvictionError::UnsupportedTtl);
        }
        if matches!(self.config.policy, EvictionPolicy::Custom(_)) {
            return Err(EvictionError::UnsupportedCustomPolicy);
        }
        if self.config.max_entries.is_some() && self.config.max_items_per_shard.is_some() {
            return Err(EvictionError::ConflictingEntryLimits);
        }
        Ok(())
    }

    /// The legacy scheduling and entry-limit configuration.
    pub fn config(&self) -> &EvictionConfig {
        &self.config
    }

    /// Capacity ordering selected by these options.
    pub fn policy(&self) -> CapacityPolicy {
        self.policy.unwrap_or(match self.config.policy {
            EvictionPolicy::Lfu => CapacityPolicy::Lfu,
            _ => CapacityPolicy::Lru,
        })
    }

    /// Optional maximum sum of estimator-reported live-entry weights.
    pub fn memory_budget(&self) -> Option<usize> {
        self.memory_budget
    }
}

impl<K, V> Clone for EvictionOptions<K, V> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            policy: self.policy,
            memory_budget: self.memory_budget,
            weigher: self.weigher.clone(),
        }
    }
}

/// Additional lifecycle observations, sampled independently of map snapshots.
#[derive(Debug, Clone, Default)]
pub struct EvictionRuntimeStats {
    /// Whether capacity tracking was installed at construction.
    pub enabled: bool,
    /// Whether mutation/pending work remains after the latest completed pass.
    pub pending: bool,
    /// Whether interrupted metadata synchronization requires reconciliation.
    pub dirty: bool,
    /// Number of currently tracked logical entries.
    pub tracked_entries: usize,
    /// Saturating sum of estimator weights; excludes old Cow views and allocator overhead.
    pub estimated_weight: usize,
    /// Whether the exact weight sum exceeds usize::MAX.
    pub weight_overflow: bool,
    /// Number of successful metadata repair passes.
    pub reconciliations: u64,
    /// Number of callbacks whose panic was isolated after committed eviction.
    pub callback_panics: u64,
    /// Callbacks suppressed to prevent synchronous reentrant callback recursion.
    pub suppressed_callbacks: u64,
    /// Most recent committed eviction time.
    pub last_evict_time: Option<std::time::Instant>,
    /// Latest maintenance duration before callback dispatch and retired-data destruction.
    pub last_eviction_duration: std::time::Duration,
}

/// Per-shard eviction statistics.
#[derive(Debug, Clone, Default)]
pub struct ShardEvictionStats {
    /// Total evictions performed on this shard
    pub total_evictions: u64,
    /// Last time eviction ran on this shard
    pub last_eviction_time: Option<std::time::Instant>,
    /// Entries marked for eviction pending lock
    pub pending_evictions: usize,
    /// Shard epoch for version tracking
    pub epoch: u64,
}

/// Metrics collected during operation.
#[derive(Debug, Clone)]
pub struct MetricsStats {
    /// Total insert operations
    pub total_inserts: u64,
    /// Total remove operations
    pub total_removes: u64,
    /// Total get operations
    pub total_gets: u64,
    /// Successful get hits
    pub hits: u64,
    /// Get misses
    pub misses: u64,
    /// Total evictions performed
    pub evictions: u64,
    /// Last eviction time
    pub last_evict_time: Option<std::time::Instant>,
    /// Current hit rate
    pub hit_rate: f64,
}

impl Default for MetricsStats {
    #[tracing::instrument(level = "trace")]
    fn default() -> Self {
        Self {
            total_inserts: 0,
            total_removes: 0,
            total_gets: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            last_evict_time: None,
            hit_rate: 0.0,
        }
    }
}

/// Internal atomic counters for metrics.
pub struct AtomicMetrics {
    /// Total insert operations.
    pub inserts: std::sync::atomic::AtomicU64,
    /// Total remove operations.
    pub removes: std::sync::atomic::AtomicU64,
    /// Total get operations.
    pub gets: std::sync::atomic::AtomicU64,
    /// Total hit operations.
    pub hits: std::sync::atomic::AtomicU64,
    /// Total miss operations.
    pub misses: std::sync::atomic::AtomicU64,
    /// Total eviction operations.
    pub evictions: std::sync::atomic::AtomicU64,
}

impl Default for AtomicMetrics {
    #[tracing::instrument(level = "trace")]
    fn default() -> Self {
        Self {
            inserts: std::sync::atomic::AtomicU64::new(0),
            removes: std::sync::atomic::AtomicU64::new(0),
            gets: std::sync::atomic::AtomicU64::new(0),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            evictions: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl AtomicMetrics {
    /// Create snapshot of current metrics
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn snapshot(&self) -> MetricsStats {
        let total_gets = self.gets.load(std::sync::atomic::Ordering::Relaxed);
        let hits = self.hits.load(std::sync::atomic::Ordering::Relaxed);
        let hit_rate = if total_gets > 0 {
            (hits as f64) / (total_gets as f64)
        } else {
            0.0
        };

        MetricsStats {
            total_inserts: self.inserts.load(std::sync::atomic::Ordering::Relaxed),
            total_removes: self.removes.load(std::sync::atomic::Ordering::Relaxed),
            total_gets,
            hits,
            misses: self.misses.load(std::sync::atomic::Ordering::Relaxed),
            evictions: self.evictions.load(std::sync::atomic::Ordering::Relaxed),
            last_evict_time: None,
            hit_rate,
        }
    }

    /// Reset all metrics to zero
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn reset(&self) {
        self.inserts.store(0, std::sync::atomic::Ordering::Relaxed);
        self.removes.store(0, std::sync::atomic::Ordering::Relaxed);
        self.gets.store(0, std::sync::atomic::Ordering::Relaxed);
        self.hits.store(0, std::sync::atomic::Ordering::Relaxed);
        self.misses.store(0, std::sync::atomic::Ordering::Relaxed);
        self.evictions
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Memory utilization statistics.
#[derive(Debug, Clone)]
pub struct MemoryStats {
    /// Number of initialized shards
    pub shards_allocated: usize,
    /// Total capacity (sum of all shard capacities)
    pub total_capacity: usize,
    /// Load factor (entries / capacity)
    pub load_factor: f64,
}

/// Iterator builder for advanced iteration control.
type IterFilter<K, V> = std::sync::Arc<dyn Fn(&K, &V) -> bool + Send + Sync>;

/// Builder for creating customized iterators over the HashMap.
pub struct IterBuilder<K, V> {
    /// Optional filter predicate
    pub(crate) filter: Option<IterFilter<K, V>>,

    /// Optional limit on result count
    pub(crate) limit: Option<usize>,

    /// Whether to use parallel iteration
    pub(crate) parallel: bool,
}

impl<K: Clone + Send + Sync, V: Clone + Send + Sync> IterBuilder<K, V> {
    /// Create new iterator builder
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Self {
        Self {
            filter: None,
            limit: None,
            parallel: false,
        }
    }

    /// Add a filter predicate
    #[tracing::instrument(skip(self, f), level = "trace")]
    pub fn filter<F: Fn(&K, &V) -> bool + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.filter = Some(std::sync::Arc::new(f));
        self
    }

    /// Set maximum result count
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Enable or disable parallel filtering.
    ///
    /// With the `rayon` feature, filters over at least 1024 items may run
    /// concurrently. This initial cutoff has not been tuned for every workload.
    /// Smaller inputs and builds without `rayon` use sequential filtering.
    /// Result order is preserved; predicate invocation order is unspecified
    /// when filtering in parallel. Callbacks passed to [`Self::for_each`] still
    /// run sequentially in result order.
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn parallel(mut self, enabled: bool) -> Self {
        self.parallel = enabled;
        self
    }

    /// Filter all input items, then limit and collect results in input order.
    #[tracing::instrument(skip(self, items), level = "trace")]
    pub fn collect(&self, items: Vec<(K, V)>) -> Vec<(K, V)> {
        let mut result = items;

        if let Some(ref filter) = self.filter {
            #[cfg(feature = "rayon")]
            if self.parallel && result.len() >= PARALLEL_FILTER_MIN_ITEMS {
                result = result
                    .into_par_iter()
                    .filter(|(k, v)| filter(k, v))
                    .collect();
            } else {
                result.retain(|(k, v)| filter(k, v));
            }

            #[cfg(not(feature = "rayon"))]
            result.retain(|(k, v)| filter(k, v));
        }

        if let Some(limit) = self.limit {
            result.truncate(limit);
        }

        result
    }

    /// Filter and limit the input, then invoke the callback sequentially.
    #[tracing::instrument(skip(self, items, f), level = "trace")]
    pub fn for_each<F: Fn((K, V))>(&self, items: Vec<(K, V)>, f: F) {
        let items = self.collect(items);
        for item in items {
            f(item);
        }
    }
}

impl<K: Clone + Send + Sync, V: Clone + Send + Sync> Default for IterBuilder<K, V> {
    #[tracing::instrument(level = "trace")]
    fn default() -> Self {
        Self::new()
    }
}

/// Drain iterator for bulk removal.
pub struct DrainIterator<K, V> {
    /// All items to drain
    pub(crate) items: Vec<(K, V)>,
    /// Current position
    pub(crate) index: usize,
}

impl<K, V> Iterator for DrainIterator<K, V> {
    type Item = (K, V);

    #[tracing::instrument(skip(self), level = "trace")]
    fn next(&mut self) -> Option<(K, V)> {
        if self.index < self.items.len() {
            let item = self.items.swap_remove(self.index);
            Some(item)
        } else {
            None
        }
    }

    #[tracing::instrument(skip(self), level = "trace")]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len();
        (remaining, Some(remaining))
    }
}

impl<K, V> ExactSizeIterator for DrainIterator<K, V> {
    #[tracing::instrument(skip(self), level = "trace")]
    fn len(&self) -> usize {
        self.items.len().saturating_sub(self.index)
    }
}

/// Per-shard load tracking for diagnostics.
#[derive(Debug, Clone)]
pub struct PerShardLoad {
    /// Shard index
    pub shard_idx: usize,
    /// Number of entries in shard
    pub entry_count: usize,
    /// Shard capacity
    pub capacity: usize,
}

#[cfg(test)]
mod tests {
    use crate::{AtomicMetrics, DrainIterator, EvictionConfig, IterBuilder};

    #[test]
    fn eviction_config_default() {
        let config = EvictionConfig::default();
        assert_eq!(config.check_interval, std::time::Duration::from_secs(60));
        assert!(config.background_enabled);
        assert_eq!(config.max_entries, None);
    }

    #[test]
    fn atomic_metrics_default() {
        let metrics = AtomicMetrics::default();
        let snap = metrics.snapshot();
        assert_eq!(snap.total_inserts, 0);
        assert_eq!(snap.hit_rate, 0.0);
    }

    #[test]
    fn atomic_metrics_hit_rate() {
        let metrics = AtomicMetrics::default();
        metrics.gets.store(10, std::sync::atomic::Ordering::Relaxed);
        metrics.hits.store(7, std::sync::atomic::Ordering::Relaxed);
        let snap = metrics.snapshot();
        assert!((snap.hit_rate - 0.7).abs() < 0.01);
    }

    #[test]
    fn iter_builder_filter() {
        let builder = IterBuilder::<String, i32>::new()
            .filter(|_, v| v > &10)
            .limit(5);

        let items = vec![("a".into(), 5), ("b".into(), 15), ("c".into(), 25)];
        let result = builder.collect(items);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parallel_filter_preserves_order_and_filters_before_limiting() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for size in [0, 17, 4096] {
            for limit in [0, 5, size + 1] {
                let calls = Arc::new(AtomicUsize::new(0));
                let filter_calls = calls.clone();
                let builder = IterBuilder::new()
                    .parallel(true)
                    .filter(move |key: &usize, _| {
                        filter_calls.fetch_add(1, Ordering::Relaxed);
                        key.is_multiple_of(3)
                    })
                    .limit(limit);
                let expected: Vec<_> = (0..size)
                    .filter(|key| key.is_multiple_of(3))
                    .take(limit)
                    .map(|key| (key, key * 2))
                    .collect();
                let actual = builder.collect((0..size).map(|key| (key, key * 2)).collect());
                assert_eq!(actual, expected);
                assert_eq!(calls.load(Ordering::Relaxed), size);
            }
        }
    }

    #[test]
    fn parallel_filter_keeps_callbacks_sequential_and_non_send() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let results = Rc::new(RefCell::new(Vec::new()));
        let callback_results = results.clone();
        IterBuilder::new()
            .parallel(true)
            .filter(|key: &usize, _| key.is_multiple_of(2))
            .limit(4)
            .for_each((0..4096).map(|key| (key, key)).collect(), move |item| {
                callback_results.borrow_mut().push(item);
            });
        assert_eq!(*results.borrow(), vec![(0, 0), (2, 2), (4, 4), (6, 6)]);
    }

    #[cfg(feature = "rayon")]
    #[test]
    fn parallel_filter_dispatches_large_inputs_to_rayon() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let saw_worker = Arc::new(AtomicBool::new(false));
        let filter_worker = saw_worker.clone();
        let builder = IterBuilder::new()
            .parallel(true)
            .filter(move |_: &usize, _| {
                if rayon::current_thread_index().is_some() {
                    filter_worker.store(true, Ordering::Relaxed);
                }
                true
            });
        let result = builder.collect((0..4096).map(|key| (key, key)).collect());
        assert_eq!(result.len(), 4096);
        assert!(saw_worker.load(Ordering::Relaxed));
    }

    #[test]
    fn drain_iterator() {
        let drain = DrainIterator {
            items: vec![("a", 1), ("b", 2), ("c", 3)],
            index: 0,
        };
        let collected: Vec<_> = drain.collect();
        assert_eq!(collected.len(), 3);
    }

    #[test]
    fn drain_iterator_reports_exact_remaining_size() {
        let mut drain = DrainIterator {
            items: vec![("a", 1), ("b", 2), ("c", 3)],
            index: 0,
        };
        for remaining in (0..=3).rev() {
            assert_eq!(drain.size_hint(), (remaining, Some(remaining)));
            assert_eq!(drain.len(), remaining);
            assert_eq!(drain.next().is_some(), remaining > 0);
        }
        assert_eq!(drain.size_hint(), (0, Some(0)));
    }
}
