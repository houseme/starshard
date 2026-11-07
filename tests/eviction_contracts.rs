#![cfg(feature = "lifecycle")]

use starshard::{CapacityPolicy, EvictionConfig, EvictionOptions, ShardedHashMap, SnapshotMode};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0_u64, |n, byte| {
            n.wrapping_mul(256).wrapping_add(u64::from(*byte))
        });
    }
    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
type Identity = BuildHasherDefault<IdentityHasher>;
type Map<K = u64> = ShardedHashMap<K, u64, Identity>;

fn options<K>(policy: CapacityPolicy, max_entries: Option<usize>) -> EvictionOptions<K, u64> {
    EvictionOptions::new(EvictionConfig {
        max_entries,
        background_enabled: false,
        ..EvictionConfig::default()
    })
    .with_policy(policy)
}

fn map(policy: CapacityPolicy, limit: Option<usize>) -> Map {
    Map::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Cow,
        options(policy, limit),
    )
    .unwrap()
}

#[cfg(feature = "advanced")]
fn assert_combined_metrics(metrics: starshard::MetricsStats) {
    assert_eq!(metrics.total_inserts, 7);
    assert_eq!(metrics.total_removes, 1);
    assert_eq!(metrics.total_gets, 7);
    assert_eq!(metrics.hits, 3);
    assert_eq!(metrics.misses, 4);
    assert_eq!(metrics.evictions, 3);
    assert!(metrics.last_evict_time.is_some());
}

#[cfg(feature = "advanced")]
#[test]
fn entry_conditional_transaction_and_batch_share_exact_metrics_and_cow_limits() {
    use starshard::{Transaction, TransactionResult};
    let map = map(CapacityPolicy::Fifo, Some(2));
    map.insert(0, 10);
    map.insert(1, 11);
    let old = map.snapshot_view();
    map.clone().reset_metrics();

    assert_eq!(map.entry(2).or_insert(12), 12); // Two misses: entry + initializer lookup.
    assert_eq!(
        map.compute_if_present(&1, |value| Some(value + 10)),
        Some(21)
    );
    assert!(!map.compare_and_swap(&9, &0, 90).is_success());
    assert!(map.compare_and_swap(&1, &21, 31).is_success());
    let mut transaction = Transaction::new();
    transaction.read(1);
    transaction.read(9);
    transaction.write(3, 13);
    transaction.remove(2);
    assert!(matches!(
        map.execute_transaction(transaction),
        TransactionResult::Committed(())
    ));
    assert_eq!(map.batch_insert([(4, 14), (5, 15), (5, 25)]), 2);

    assert_combined_metrics(map.clone().metrics_snapshot());
    let mut current: Vec<_> = map.snapshot_view().iter().map(|(k, v)| (*k, *v)).collect();
    current.sort_unstable();
    assert_eq!(current, [(4, 14), (5, 25)]);
    let mut retained: Vec<_> = old.iter().map(|(k, v)| (*k, *v)).collect();
    retained.sort_unstable();
    assert_eq!(retained, [(0, 10), (1, 11)]);
    assert_eq!(map.len(), 2);
    assert_eq!(map.eviction_runtime_stats().tracked_entries, 2);
}

#[test]
fn migration_preserves_lru_rank_and_uses_destination_buckets() {
    let map = Map::<u64>::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(EvictionConfig {
            max_items_per_shard: Some(1),
            background_enabled: false,
            ..EvictionConfig::default()
        }),
    )
    .unwrap();
    map.insert(0, 10);
    map.insert(2, 12);
    assert_eq!(map.len(), 2, "seed keys must occupy separate source shards");
    assert_eq!(map.shard_stats().initialized, 2);
    assert_eq!(map.metrics_snapshot().evictions, 0);
    assert_eq!(map.get(&2), Some(12));
    let retained = map.snapshot_view();
    assert_eq!(retained.len(), 2);
    map.reset_metrics();
    map.start_rebalance_online(2).unwrap();
    assert_eq!(map.advance_rebalance(1), 1); // Physically move the colder key 0 last.
    assert_eq!(map.enforce_eviction_limits(), 1);
    while map.rebalance_status().state == "migrating" {
        map.advance_rebalance(1);
    }
    assert_eq!(
        map.snapshot_view()
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect::<Vec<_>>(),
        [(2, 12)]
    );
    assert_eq!(retained.len(), 2);
    let metrics = map.metrics_snapshot();
    assert_eq!(
        (
            metrics.total_gets,
            metrics.total_inserts,
            metrics.total_removes
        ),
        (0, 0, 0)
    );
    assert_eq!(metrics.evictions, 1);
    assert_eq!(map.eviction_runtime_stats().tracked_entries, 1);
}

#[derive(Default)]
struct Faults {
    clone: AtomicBool,
    hashes: AtomicUsize,
    panic_hash_at: AtomicUsize,
}
struct Key {
    id: u64,
    faults: Arc<Faults>,
}
impl Key {
    fn new(id: u64, faults: &Arc<Faults>) -> Self {
        Self {
            id,
            faults: faults.clone(),
        }
    }
}
impl Clone for Key {
    fn clone(&self) -> Self {
        assert!(
            !self.faults.clone.swap(false, Ordering::Relaxed),
            "injected metadata key clone"
        );
        Self::new(self.id, &self.faults)
    }
}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let call = self.faults.hashes.fetch_add(1, Ordering::Relaxed) + 1;
        assert_ne!(
            call,
            self.faults.panic_hash_at.load(Ordering::Relaxed),
            "injected metadata key hash"
        );
        state.write_u64(self.id);
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Key {}

fn entries(map: &Map<Key>) -> Vec<(u64, u64)> {
    let mut entries: Vec<_> = map
        .snapshot_view()
        .iter()
        .map(|(key, value)| (key.id, *value))
        .collect();
    entries.sort_unstable();
    entries
}

#[test]
fn failed_weigher_and_failed_repair_preserve_data_and_existing_read_rank() {
    let faults = Arc::new(Faults::default());
    let fail_weight = Arc::new(AtomicBool::new(false));
    let weight_fault = fail_weight.clone();
    let map = Map::<Key>::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Cow,
        options(CapacityPolicy::Lru, Some(2)).with_memory_budget(100, move |_, value| {
            assert!(
                !weight_fault.swap(false, Ordering::Relaxed),
                "injected weigher panic"
            );
            *value as usize
        }),
    )
    .unwrap();
    map.insert(Key::new(0, &faults), 4);
    map.insert(Key::new(1, &faults), 4);
    assert_eq!(map.get(&Key::new(0, &faults)), Some(4));
    fail_weight.store(true, Ordering::Relaxed);
    assert!(catch_unwind(AssertUnwindSafe(|| map.insert(Key::new(2, &faults), 4))).is_err());
    assert!(map.eviction_runtime_stats().dirty);
    assert_eq!(entries(&map), [(0, 4), (1, 4)]);

    faults.clone.store(true, Ordering::Relaxed);
    assert!(catch_unwind(AssertUnwindSafe(|| map.enforce_eviction_limits())).is_err());
    assert!(map.eviction_runtime_stats().dirty);
    assert_eq!(entries(&map), [(0, 4), (1, 4)]);
    assert_eq!(map.metrics_snapshot().evictions, 0);

    map.insert(Key::new(2, &faults), 4);
    assert_eq!(entries(&map), [(0, 4), (2, 4)]);
    let runtime = map.eviction_runtime_stats();
    assert!(!runtime.dirty && !runtime.pending);
    assert!(runtime.reconciliations > 0);
    assert_eq!((runtime.tracked_entries, runtime.estimated_weight), (2, 8));
}

#[test]
fn metadata_hash_failure_after_removal_does_not_reuse_old_fifo_incarnation() {
    let faults = Arc::new(Faults::default());
    let map = Map::<Key>::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Clone,
        options(CapacityPolicy::Fifo, Some(2)),
    )
    .unwrap();
    map.insert(Key::new(0, &faults), 10);
    map.insert(Key::new(1, &faults), 11);
    faults.hashes.store(0, Ordering::Relaxed);
    // Route hash, data removal hash, then metadata removal hash. The last
    // failure leaves the old metadata entry while the actual key is absent.
    faults.panic_hash_at.store(3, Ordering::Relaxed);
    assert!(catch_unwind(AssertUnwindSafe(|| map.remove(&Key::new(0, &faults)))).is_err());
    faults.panic_hash_at.store(0, Ordering::Relaxed);
    assert_eq!(entries(&map), [(1, 11)]);
    assert_eq!(map.len(), 1);
    assert!(map.eviction_runtime_stats().dirty);

    map.insert(Key::new(0, &faults), 100);
    map.insert(Key::new(2, &faults), 12);
    assert_eq!(entries(&map), [(0, 100), (2, 12)]);
    assert_eq!(map.len(), 2);
    assert!(!map.eviction_runtime_stats().dirty);
}

#[test]
fn unrelated_dirty_failure_does_not_reset_fifo_age_of_a_replacement() {
    let fail_weight = Arc::new(AtomicBool::new(false));
    let weight_fault = fail_weight.clone();
    let map = Map::<u64>::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Cow,
        options(CapacityPolicy::Fifo, Some(2)).with_memory_budget(1000, move |_, value| {
            assert!(
                !weight_fault.swap(false, Ordering::Relaxed),
                "injected weigher panic"
            );
            *value as usize
        }),
    )
    .unwrap();
    map.insert(0, 10);
    map.insert(1, 11);
    fail_weight.store(true, Ordering::Relaxed);
    assert!(catch_unwind(AssertUnwindSafe(|| map.insert(2, 12))).is_err());
    assert!(map.eviction_runtime_stats().dirty);
    // This key was never removed. Repair must distinguish its replacement
    // from the remove-and-reinsert case instead of resetting every dirty age.
    assert_eq!(map.insert(0, 100), Some(10));
    map.insert(2, 12);
    let mut current: Vec<_> = map.snapshot_view().iter().map(|(k, v)| (*k, *v)).collect();
    current.sort_unstable();
    assert_eq!(current, [(1, 11), (2, 12)]);
}

#[test]
fn callback_reentry_panic_and_suppression_publish_separate_counters() {
    // Use a bounded caller so holding a map lock during callbacks fails this
    // test instead of hanging the whole test binary.
    let (finished, completion) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let map = map(CapacityPolicy::Lru, Some(1));
        let callback_map = map.clone();
        let called = Arc::new(Mutex::new(Vec::new()));
        let callback_called = called.clone();
        map.set_evict_callback(move |key, _| {
            callback_called.lock().unwrap().push(*key);
            assert_eq!(callback_map.snapshot_view().len(), 1);
            callback_map.insert(99, 199);
            panic!("callback failed after reentrant mutation");
        });
        map.insert(0, 10);
        map.insert(1, 11);
        map.clear_evict_callback();
        let metrics = map.metrics_snapshot();
        let runtime = map.eviction_runtime_stats();
        assert_eq!(*called.lock().unwrap(), [0]);
        assert_eq!(
            (
                metrics.evictions,
                metrics.total_removes,
                metrics.total_inserts
            ),
            (2, 0, 3)
        );
        assert_eq!(
            (runtime.callback_panics, runtime.suppressed_callbacks),
            (1, 1)
        );
        assert!(runtime.last_evict_time.is_some());
        assert_eq!(map.get(&99), Some(199));
        assert_eq!(map.len(), 1);
        finished.send(()).unwrap();
    });
    completion
        .recv_timeout(Duration::from_secs(5))
        .expect("eviction callback deadlocked");
    worker.join().unwrap();
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use starshard::AsyncShardedHashMap;

    type AsyncMap<K = u64> = AsyncShardedHashMap<K, u64, Identity>;

    async fn entries(map: &AsyncMap<Key>) -> Vec<(u64, u64)> {
        let mut entries: Vec<_> = map
            .snapshot_view()
            .await
            .iter()
            .map(|(key, value)| (key.id, *value))
            .collect();
        entries.sort_unstable();
        entries
    }

    #[cfg(feature = "advanced")]
    #[tokio::test]
    async fn combined_operations_share_exact_metrics_and_cow_limits() {
        use starshard::{Transaction, TransactionResult};
        let map = AsyncMap::<u64>::try_with_manual_eviction(
            4,
            Identity::default(),
            SnapshotMode::Cow,
            options(CapacityPolicy::Fifo, Some(2)),
        )
        .unwrap();
        map.insert(0, 10).await;
        map.insert(1, 11).await;
        let old = map.snapshot_view().await;
        map.reset_metrics();
        assert_eq!(map.entry(2).await.or_insert(12).await, 12);
        assert_eq!(
            map.compute_if_present(&1, |value| Some(value + 10)).await,
            Some(21)
        );
        assert!(!map.compare_and_swap(&9, &0, 90).await.is_success());
        assert!(map.compare_and_swap(&1, &21, 31).await.is_success());
        let mut transaction = Transaction::new();
        transaction.read(1);
        transaction.read(9);
        transaction.write(3, 13);
        transaction.remove(2);
        assert!(matches!(
            map.execute_transaction(transaction).await,
            TransactionResult::Committed(())
        ));
        assert_eq!(map.batch_insert([(4, 14), (5, 15), (5, 25)]).await, 2);
        assert_combined_metrics(map.metrics_snapshot());
        let mut current: Vec<_> = map
            .snapshot_view()
            .await
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        current.sort_unstable();
        assert_eq!(current, [(4, 14), (5, 25)]);
        let mut retained: Vec<_> = old.iter().map(|(k, v)| (*k, *v)).collect();
        retained.sort_unstable();
        assert_eq!(retained, [(0, 10), (1, 11)]);
        assert_eq!(map.len().await, 2);
    }

    #[tokio::test]
    async fn migration_keeps_read_rank_and_enforces_destination_bucket_limit() {
        let map = AsyncMap::<u64>::try_with_manual_eviction(
            4,
            Identity::default(),
            SnapshotMode::Cow,
            EvictionOptions::new(EvictionConfig {
                max_items_per_shard: Some(1),
                background_enabled: false,
                ..EvictionConfig::default()
            }),
        )
        .unwrap();
        map.insert(0, 10).await;
        map.insert(2, 12).await;
        assert_eq!(
            map.len().await,
            2,
            "seed keys must occupy separate source shards"
        );
        assert_eq!(map.shard_stats().await.initialized, 2);
        assert_eq!(map.metrics_snapshot().evictions, 0);
        assert_eq!(map.get(&2).await, Some(12));
        let old = map.snapshot_view().await;
        assert_eq!(old.len(), 2);
        map.reset_metrics();
        map.start_rebalance_online(2).await.unwrap();
        assert_eq!(map.advance_rebalance(1).await, 1);
        assert_eq!(map.enforce_eviction_limits().await, 1);
        while map.rebalance_status().state == "migrating" {
            map.advance_rebalance(1).await;
        }
        assert_eq!(
            map.snapshot_view()
                .await
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect::<Vec<_>>(),
            [(2, 12)]
        );
        assert_eq!(old.len(), 2);
        let metrics = map.metrics_snapshot();
        assert_eq!(
            (
                metrics.total_gets,
                metrics.total_inserts,
                metrics.total_removes
            ),
            (0, 0, 0)
        );
        assert_eq!(metrics.evictions, 1);
        assert_eq!(map.eviction_runtime_stats().tracked_entries, 1);
    }

    #[tokio::test]
    async fn failed_weigher_and_repair_preserve_hot_entry_and_pending_state() {
        let faults = Arc::new(Faults::default());
        let fail_weight = Arc::new(AtomicBool::new(false));
        let weight_fault = fail_weight.clone();
        let map = AsyncMap::<Key>::try_with_manual_eviction(
            4,
            Identity::default(),
            SnapshotMode::Cow,
            options(CapacityPolicy::Lru, Some(2)).with_memory_budget(100, move |_, value| {
                assert!(
                    !weight_fault.swap(false, Ordering::Relaxed),
                    "injected weigher panic"
                );
                *value as usize
            }),
        )
        .unwrap();
        map.insert(Key::new(0, &faults), 4).await;
        map.insert(Key::new(1, &faults), 4).await;
        assert_eq!(map.get(&Key::new(0, &faults)).await, Some(4));
        fail_weight.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        let key = Key::new(2, &faults);
        assert!(
            tokio::spawn(async move { worker_map.insert(key, 4).await })
                .await
                .unwrap_err()
                .is_panic()
        );
        assert!(map.eviction_runtime_stats().dirty);
        assert_eq!(entries(&map).await, [(0, 4), (1, 4)]);

        faults.clone.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        assert!(
            tokio::spawn(async move { worker_map.enforce_eviction_limits().await })
                .await
                .unwrap_err()
                .is_panic()
        );
        assert!(map.eviction_runtime_stats().dirty);
        assert_eq!(entries(&map).await, [(0, 4), (1, 4)]);
        assert_eq!(map.metrics_snapshot().evictions, 0);

        map.insert(Key::new(2, &faults), 4).await;
        assert_eq!(entries(&map).await, [(0, 4), (2, 4)]);
        let runtime = map.eviction_runtime_stats();
        assert!(!runtime.dirty && !runtime.pending);
        assert!(runtime.reconciliations > 0);
        assert_eq!((runtime.tracked_entries, runtime.estimated_weight), (2, 8));
    }

    #[tokio::test]
    async fn metadata_remove_failure_does_not_age_a_reinserted_incarnation() {
        let faults = Arc::new(Faults::default());
        let map = AsyncMap::<Key>::try_with_manual_eviction(
            4,
            Identity::default(),
            SnapshotMode::Clone,
            options(CapacityPolicy::Fifo, Some(2)),
        )
        .unwrap();
        map.insert(Key::new(0, &faults), 10).await;
        map.insert(Key::new(1, &faults), 11).await;
        faults.hashes.store(0, Ordering::Relaxed);
        faults.panic_hash_at.store(3, Ordering::Relaxed);
        let worker_map = map.clone();
        let key = Key::new(0, &faults);
        assert!(
            tokio::spawn(async move { worker_map.remove(&key).await })
                .await
                .unwrap_err()
                .is_panic()
        );
        faults.panic_hash_at.store(0, Ordering::Relaxed);
        assert_eq!(entries(&map).await, [(1, 11)]);
        assert_eq!(map.len().await, 1);
        assert!(map.eviction_runtime_stats().dirty);

        map.insert(Key::new(0, &faults), 100).await;
        map.insert(Key::new(2, &faults), 12).await;
        assert_eq!(entries(&map).await, [(0, 100), (2, 12)]);
        assert_eq!(map.len().await, 2);
        assert!(!map.eviction_runtime_stats().dirty);
    }

    #[tokio::test]
    async fn dirty_replacement_retains_fifo_age_without_a_logical_removal() {
        let fail_weight = Arc::new(AtomicBool::new(false));
        let weight_fault = fail_weight.clone();
        let map = AsyncMap::<u64>::try_with_manual_eviction(
            4,
            Identity::default(),
            SnapshotMode::Cow,
            options(CapacityPolicy::Fifo, Some(2)).with_memory_budget(1000, move |_, value| {
                assert!(
                    !weight_fault.swap(false, Ordering::Relaxed),
                    "injected weigher panic"
                );
                *value as usize
            }),
        )
        .unwrap();
        map.insert(0, 10).await;
        map.insert(1, 11).await;
        fail_weight.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        assert!(
            tokio::spawn(async move { worker_map.insert(2, 12).await })
                .await
                .unwrap_err()
                .is_panic()
        );
        assert!(map.eviction_runtime_stats().dirty);
        assert_eq!(map.insert(0, 100).await, Some(10));
        map.insert(2, 12).await;
        let mut current: Vec<_> = map
            .snapshot_view()
            .await
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        current.sort_unstable();
        assert_eq!(current, [(1, 11), (2, 12)]);
    }
}
