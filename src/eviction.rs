//! Version 0.9.0: Eviction, Metrics, and Advanced Iteration Support
//!
//! This module provides production-grade lifecycle management, observability hooks,
//! and advanced iteration patterns for the Starshard sharded HashMap.

/// Eviction policy for cache entries.
#[cfg(feature = "lifecycle")]
#[derive(Clone)]
pub enum EvictionPolicy {
    /// Least Recently Used: removes least recently accessed entries
    LRU,
    /// Least Frequently Used: removes least frequently accessed entries
    LFU,
    /// Time-To-Live: removes entries after specified duration
    TimeToLive(std::time::Duration),
    /// Custom predicate: removes entries matching predicate
    Custom(std::sync::Arc<dyn Fn() -> bool + Send + Sync>),
}

#[cfg(feature = "lifecycle")]
impl std::fmt::Debug for EvictionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvictionPolicy::LRU => f.write_str("LRU"),
            EvictionPolicy::LFU => f.write_str("LFU"),
            EvictionPolicy::TimeToLive(duration) => {
                f.debug_tuple("TimeToLive").field(duration).finish()
            }
            EvictionPolicy::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

/// Configuration for eviction behavior.
#[cfg(feature = "lifecycle")]
#[derive(Debug, Clone)]
pub struct EvictionConfig {
    /// The eviction policy to apply
    pub policy: EvictionPolicy,
    /// Maximum total entries; None = unlimited
    pub max_entries: Option<usize>,
    /// How often to check for eviction candidates
    pub check_interval: std::time::Duration,
    /// Whether to enable background eviction task
    pub background_enabled: bool,
}

#[cfg(feature = "lifecycle")]
impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            policy: EvictionPolicy::LRU,
            max_entries: None,
            check_interval: std::time::Duration::from_secs(60),
            background_enabled: true,
        }
    }
}

/// Per-shard eviction statistics.
#[cfg(feature = "lifecycle")]
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
#[cfg(feature = "lifecycle")]
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
#[cfg(feature = "lifecycle")]
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

#[cfg(feature = "lifecycle")]
impl Default for AtomicMetrics {
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

#[cfg(feature = "lifecycle")]
impl AtomicMetrics {
    /// Create snapshot of current metrics
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
#[cfg(feature = "lifecycle")]
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
#[cfg(feature = "lifecycle")]
type IterFilter<K, V> = std::sync::Arc<dyn Fn(&K, &V) -> bool + Send + Sync>;

#[cfg(feature = "lifecycle")]
/// Builder for creating customized iterators over the HashMap.
pub struct IterBuilder<K, V> {
    /// Optional filter predicate
    pub(crate) filter: Option<IterFilter<K, V>>,

    /// Optional limit on result count
    pub(crate) limit: Option<usize>,

    /// Whether to use parallel iteration
    pub(crate) parallel: bool,
}

#[cfg(feature = "lifecycle")]
impl<K: Clone + Send + Sync, V: Clone + Send + Sync> IterBuilder<K, V> {
    /// Create new iterator builder
    pub fn new() -> Self {
        Self {
            filter: None,
            limit: None,
            parallel: false,
        }
    }

    /// Add a filter predicate
    pub fn filter<F: Fn(&K, &V) -> bool + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.filter = Some(std::sync::Arc::new(f));
        self
    }

    /// Set maximum result count
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Enable or disable parallel processing
    pub fn parallel(mut self, enabled: bool) -> Self {
        self.parallel = enabled;
        self
    }

    /// Execute the iteration and collect results
    pub fn collect(&self, items: Vec<(K, V)>) -> Vec<(K, V)> {
        let mut result = items;

        if let Some(ref filter) = self.filter {
            result.retain(|(k, v)| filter(k, v));
        }

        if let Some(limit) = self.limit {
            result.truncate(limit);
        }

        result
    }

    /// Execute the iteration with a callback
    pub fn for_each<F: Fn((K, V))>(&self, items: Vec<(K, V)>, f: F) {
        let items = self.collect(items);
        for item in items {
            f(item);
        }
    }
}

#[cfg(feature = "lifecycle")]
impl<K: Clone + Send + Sync, V: Clone + Send + Sync> Default for IterBuilder<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Drain iterator for bulk removal.
#[cfg(feature = "lifecycle")]
pub struct DrainIterator<K, V> {
    /// All items to drain
    pub(crate) items: Vec<(K, V)>,
    /// Current position
    pub(crate) index: usize,
}

#[cfg(feature = "lifecycle")]
impl<K, V> Iterator for DrainIterator<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        if self.index < self.items.len() {
            let item = self.items.swap_remove(self.index);
            Some(item)
        } else {
            None
        }
    }
}

#[cfg(feature = "lifecycle")]
impl<K, V> ExactSizeIterator for DrainIterator<K, V> {
    fn len(&self) -> usize {
        self.items.len() - self.index
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
    #[cfg(feature = "lifecycle")]
    #[test]
    fn eviction_config_default() {
        let config = crate::EvictionConfig::default();
        assert_eq!(config.check_interval, std::time::Duration::from_secs(60));
        assert!(config.background_enabled);
        assert_eq!(config.max_entries, None);
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn atomic_metrics_default() {
        let metrics = crate::AtomicMetrics::default();
        let snap = metrics.snapshot();
        assert_eq!(snap.total_inserts, 0);
        assert_eq!(snap.hit_rate, 0.0);
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn atomic_metrics_hit_rate() {
        let metrics = crate::AtomicMetrics::default();
        metrics.gets.store(10, std::sync::atomic::Ordering::Relaxed);
        metrics.hits.store(7, std::sync::atomic::Ordering::Relaxed);
        let snap = metrics.snapshot();
        assert!((snap.hit_rate - 0.7).abs() < 0.01);
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn iter_builder_filter() {
        let builder = crate::IterBuilder::<String, i32>::new()
            .filter(|_, v| v > &10)
            .limit(5);

        let items = vec![("a".into(), 5), ("b".into(), 15), ("c".into(), 25)];
        let result = builder.collect(items);
        assert_eq!(result.len(), 2);
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn drain_iterator() {
        let drain = crate::DrainIterator {
            items: vec![("a", 1), ("b", 2), ("c", 3)],
            index: 0,
        };
        let collected: Vec<_> = drain.collect();
        assert_eq!(collected.len(), 3);
    }
}
