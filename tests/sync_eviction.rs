#![cfg(feature = "lifecycle")]

use starshard::{CapacityPolicy, EvictionConfig, EvictionOptions, ShardedHashMap, SnapshotMode};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_mul(256).wrapping_add(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type Identity = BuildHasherDefault<IdentityHasher>;
type Map = ShardedHashMap<u64, u64, Identity>;

fn map(policy: CapacityPolicy, max_entries: Option<usize>) -> Map {
    Map::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(EvictionConfig {
            max_entries,
            background_enabled: false,
            ..EvictionConfig::default()
        })
        .with_policy(policy),
    )
    .unwrap()
}

#[test]
fn lru_and_lfu_follow_reads_and_share_configuration_across_handles() {
    for policy in [CapacityPolicy::Lru, CapacityPolicy::Lfu] {
        let map = map(policy, Some(2));
        map.insert(0, 10);
        map.insert(1, 11);
        assert_eq!(map.get(&0), Some(10));
        assert_eq!(map.get(&0), Some(10));
        map.clone().insert(2, 12);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&0), Some(10));
        assert_eq!(map.get(&1), None);
        assert_eq!(map.get(&2), Some(12));
        assert_eq!(map.metrics_snapshot().evictions, 1);
    }
}

#[test]
fn fifo_replacement_preserves_insertion_order_and_cow_view() {
    let map = map(CapacityPolicy::Fifo, Some(2));
    map.insert(0, 10);
    map.insert(1, 11);
    let old = map.snapshot_view();
    map.insert(0, 99);
    map.insert(2, 12);
    assert_eq!(map.get(&0), None);
    assert_eq!(map.len(), 2);
    assert_eq!(
        old.iter()
            .find(|(key, _)| **key == 0)
            .map(|(_, value)| *value),
        Some(10)
    );
}

#[test]
fn manual_eviction_targets_remaining_count_and_can_override_policy() {
    let map = map(CapacityPolicy::Fifo, None);
    map.batch_insert([(0, 10), (1, 11), (2, 12)]);
    map.get(&0);
    assert_eq!(map.evict_lru(1), 2);
    assert_eq!(map.get(&0), Some(10));
    assert_eq!(map.evict(0), 1);
    assert!(map.is_empty());
    let disabled = ShardedHashMap::new(2);
    disabled.insert(0, 10);
    assert_eq!(disabled.evict(0), 0);
}

#[test]
fn per_shard_budget_accounts_for_previous_generation_entries() {
    let map = Map::try_with_manual_eviction(
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
    map.batch_insert([(0, 10), (1, 11), (2, 12), (3, 13)]);
    map.start_rebalance_online(2).unwrap();
    map.insert(4, 14);
    assert!(map.len() <= 2);
    let view = map.snapshot_view();
    for shard in 0..2 {
        assert!(view.iter().filter(|(key, _)| **key % 2 == shard).count() <= 1);
    }
}

#[test]
fn callbacks_run_without_map_locks_and_isolate_panics() {
    let map = map(CapacityPolicy::Lru, Some(1));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let callback_map = map.clone();
    let callback_observed = observed.clone();
    map.set_evict_callback(move |key, value| {
        assert!(!callback_map.contains(key));
        assert!(callback_map.metrics_snapshot().evictions > 0);
        callback_observed.lock().unwrap().push((*key, *value));
        panic!("injected eviction callback panic");
    });
    map.insert(0, 10);
    map.insert(1, 11);
    map.insert(2, 12);
    map.clear_evict_callback();
    assert_eq!(map.len(), 1);
    assert_eq!(*observed.lock().unwrap(), vec![(0, 10), (1, 11)]);
    assert_eq!(map.metrics_snapshot().evictions, 2);
}

#[test]
fn reentrant_callback_insert_enforces_capacity_without_recursive_dispatch() {
    let map = map(CapacityPolicy::Lru, Some(1));
    let callback_map = map.clone();
    let called = Arc::new(AtomicBool::new(false));
    let callback_called = called.clone();
    map.set_evict_callback(move |_, _| {
        assert!(!callback_called.swap(true, Ordering::Relaxed));
        callback_map.insert(99, 199);
    });
    map.insert(0, 10);
    map.insert(1, 11);
    map.clear_evict_callback();
    assert!(called.load(Ordering::Relaxed));
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&99), Some(199));
}

#[test]
fn metrics_include_default_maps_and_count_batch_elements() {
    let map = ShardedHashMap::new(2);
    map.batch_insert([(0, 10), (1, 11), (0, 20)]);
    assert_eq!(map.batch_get(&[0, 1, 2]), vec![Some(20), Some(11), None]);
    map.batch_remove([0, 0, 9]);
    let metrics = map.metrics_snapshot();
    assert_eq!(metrics.total_inserts, 3);
    assert_eq!(metrics.total_gets, 3);
    assert_eq!(metrics.hits, 2);
    assert_eq!(metrics.misses, 1);
    assert_eq!(metrics.total_removes, 1);
    map.reset_metrics();
    assert_eq!(map.metrics_snapshot().total_inserts, 0);
}

#[test]
fn zero_capacity_and_weight_budgets_are_enforced_after_conditional_writes() {
    let empty = map(CapacityPolicy::Lru, Some(0));
    assert_eq!(empty.get_or_insert_with(1, || 10), 10);
    assert!(empty.is_empty());

    let weighted = Map::try_with_manual_eviction(
        2,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(EvictionConfig {
            background_enabled: false,
            ..EvictionConfig::default()
        })
        .with_memory_budget(10, |_, value: &u64| *value as usize),
    )
    .unwrap();
    weighted.insert(0, 4);
    weighted.insert(1, 4);
    weighted.compute_if_present(&1, |_| Some(8));
    assert_eq!(weighted.get(&0), None);
    assert_eq!(weighted.get(&1), Some(8));
}

#[cfg(feature = "advanced")]
#[test]
fn transactions_cas_entry_and_clear_update_policy_metadata() {
    use starshard::{Transaction, TransactionResult};
    let map = map(CapacityPolicy::Fifo, Some(2));
    map.entry(0).or_insert(10);
    map.entry(1).or_insert(11);
    assert!(map.compare_and_swap(&0, &10, 20).is_success());
    let mut transaction = Transaction::new();
    transaction.remove(1);
    transaction.write(2, 12);
    transaction.write(3, 13);
    assert!(matches!(
        map.execute_transaction(transaction),
        TransactionResult::Committed(())
    ));
    assert_eq!(map.get(&0), None);
    assert_eq!(map.len(), 2);
    map.retain(|key, _| *key == 2);
    assert_eq!(map.drain().count(), 1);
    map.batch_insert([(4, 14), (5, 15)]);
    map.clear();
    map.insert(4, 140);
    assert_eq!(map.evict(0), 1);
    assert!(map.is_empty());
}

#[test]
fn panicking_retired_value_preserves_completed_batch_and_policy() {
    #[derive(Clone)]
    struct Value {
        number: u64,
        panic_on_drop: Arc<AtomicBool>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            assert!(
                !self.panic_on_drop.swap(false, Ordering::Relaxed),
                "injected value destructor panic"
            );
        }
    }
    let map = ShardedHashMap::try_with_manual_eviction(
        1,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(EvictionConfig {
            max_entries: Some(1),
            background_enabled: false,
            ..EvictionConfig::default()
        })
        .with_policy(CapacityPolicy::Fifo),
    )
    .unwrap();
    let panic_on_drop = Arc::new(AtomicBool::new(false));
    map.insert(
        1_u64,
        Value {
            number: 10,
            panic_on_drop: panic_on_drop.clone(),
        },
    );
    let old = map.snapshot_view();
    panic_on_drop.store(true, Ordering::Relaxed);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        map.batch_insert([
            (
                2,
                Value {
                    number: 12,
                    panic_on_drop: Arc::default(),
                },
            ),
            (
                1,
                Value {
                    number: 20,
                    panic_on_drop: Arc::default(),
                },
            ),
        ]);
    }));
    assert!(result.is_err());
    assert_eq!(map.len(), 1);
    assert!(map.eviction_runtime_stats().pending);
    assert!(map.eviction_runtime_stats().dirty);
    assert_eq!(map.read_with(&1, |value| value.number), None);
    assert_eq!(old.iter().next().unwrap().1.number, 10);
    assert_eq!(map.enforce_eviction_limits(), 0);
    assert_eq!(map.len(), 1);
    assert_eq!(map.read_with(&2, |value| value.number), Some(12));
    assert!(!map.eviction_runtime_stats().pending);
}

#[test]
fn manual_constructor_accepts_borrowed_keys_and_values() {
    let key = String::from("key");
    let value = String::from("value");
    let map = ShardedHashMap::try_with_manual_eviction(
        1,
        rustc_hash::FxBuildHasher,
        SnapshotMode::Cow,
        EvictionOptions::new(EvictionConfig {
            max_entries: Some(1),
            background_enabled: false,
            ..EvictionConfig::default()
        }),
    )
    .unwrap();
    map.insert(key.as_str(), value.as_str());
    assert_eq!(map.get(&key.as_str()), Some(value.as_str()));
    assert_eq!(map.evict(0), 1);
}

#[test]
fn later_candidate_hash_panic_dispatches_already_committed_evictions() {
    #[derive(Clone)]
    struct Key {
        number: u64,
        panic: Arc<AtomicBool>,
    }
    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            self.number == other.number
        }
    }
    impl Eq for Key {}
    impl std::hash::Hash for Key {
        fn hash<H: Hasher>(&self, hasher: &mut H) {
            assert!(
                self.number != 2 || !self.panic.swap(false, Ordering::Relaxed),
                "injected second eviction candidate panic"
            );
            hasher.write_u64(self.number);
        }
    }
    let armed = Arc::new(AtomicBool::new(false));
    let key = |number| Key {
        number,
        panic: armed.clone(),
    };
    let map = ShardedHashMap::try_with_manual_eviction(
        2,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(EvictionConfig {
            background_enabled: false,
            ..EvictionConfig::default()
        })
        .with_policy(CapacityPolicy::Fifo),
    )
    .unwrap();
    map.insert(key(1), 10);
    map.insert(key(2), 20);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let callback_observed = observed.clone();
    let callback_map = map.clone();
    map.set_evict_callback(move |key, value| {
        assert!(!callback_map.contains(key));
        callback_observed.lock().unwrap().push((key.number, *value));
    });
    armed.store(true, Ordering::Relaxed);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.evict(0)));
    map.clear_evict_callback();
    assert!(result.is_err());
    assert_eq!(*observed.lock().unwrap(), vec![(1, 10)]);
    assert_eq!(map.len(), 1);
    assert_eq!(map.metrics_snapshot().evictions, 1);
    assert!(map.eviction_runtime_stats().pending);
    map.enforce_eviction_limits();
    assert_eq!(map.get(&key(2)), Some(20));
    assert!(!map.eviction_runtime_stats().pending);
}

#[derive(Default)]
struct MetadataFault {
    hashes: AtomicUsize,
    fail_hash: AtomicUsize,
    fail_eq: AtomicBool,
}

struct MetadataKey {
    number: u64,
    metadata_copy: bool,
    fault: Arc<MetadataFault>,
}

impl Clone for MetadataKey {
    fn clone(&self) -> Self {
        Self {
            number: self.number,
            metadata_copy: true,
            fault: self.fault.clone(),
        }
    }
}

impl PartialEq for MetadataKey {
    fn eq(&self, other: &Self) -> bool {
        assert!(
            !(self.metadata_copy || other.metadata_copy)
                || !self.fault.fail_eq.swap(false, Ordering::Relaxed),
            "injected metadata equality panic"
        );
        self.number == other.number
    }
}

impl Eq for MetadataKey {}

impl std::hash::Hash for MetadataKey {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        let call = self.fault.hashes.fetch_add(1, Ordering::Relaxed) + 1;
        let fail = self.fault.fail_hash.load(Ordering::Relaxed);
        if fail != 0 && call == fail {
            self.fault.fail_hash.store(0, Ordering::Relaxed);
            panic!("injected metadata hash panic");
        }
        hasher.write_u64(self.number);
    }
}

struct DropProbe {
    armed: AtomicBool,
    request: mpsc::Sender<()>,
    response: Mutex<mpsc::Receiver<()>>,
    read_completed_in_drop: AtomicBool,
}

#[derive(Clone)]
struct ProbedValue(Arc<DropProbe>);

impl Drop for ProbedValue {
    fn drop(&mut self) {
        if self.0.armed.swap(false, Ordering::Relaxed) {
            let _ = self.0.request.send(());
            let completed = self
                .0
                .response
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .is_ok();
            self.0
                .read_completed_in_drop
                .store(completed, Ordering::Relaxed);
        }
    }
}

#[test]
fn metadata_remove_and_retain_panics_retire_values_after_unlocking() {
    for retain in [false, true] {
        for equality in [false, true] {
            let fault = Arc::new(MetadataFault::default());
            let key = || MetadataKey {
                number: 1,
                metadata_copy: false,
                fault: fault.clone(),
            };
            let map = ShardedHashMap::try_with_manual_eviction(
                1,
                Identity::default(),
                SnapshotMode::Clone,
                EvictionOptions::new(EvictionConfig {
                    background_enabled: false,
                    ..EvictionConfig::default()
                }),
            )
            .unwrap();
            let (request, requests) = mpsc::channel();
            let (reply, response) = mpsc::channel();
            let probe = Arc::new(DropProbe {
                armed: AtomicBool::new(false),
                request,
                response: Mutex::new(response),
                read_completed_in_drop: AtomicBool::new(false),
            });
            map.insert(key(), ProbedValue(probe.clone()));
            let reader_map = map.clone();
            let reader_key = key();
            let worker = std::thread::spawn(move || {
                requests.recv().unwrap();
                reader_map.contains(&reader_key);
                let _ = reply.send(());
            });
            fault.hashes.store(0, Ordering::Relaxed);
            if equality {
                fault.fail_eq.store(true, Ordering::Relaxed);
            } else {
                // remove hashes routing, data, then metadata; retain's extraction
                // invokes no user Hash before its metadata hook.
                fault
                    .fail_hash
                    .store(if retain { 1 } else { 3 }, Ordering::Relaxed);
            }
            probe.armed.store(true, Ordering::Relaxed);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if retain {
                    map.retain(|_, _| false);
                } else {
                    drop(map.remove(&key()));
                }
            }));
            // Always unblock the probe worker even if a regression fails to remove
            // the entry; the bounded Drop wait makes lock-order failures observable.
            let _ = probe.request.send(());
            worker.join().unwrap();
            assert!(result.is_err());
            assert_eq!(map.len(), 0);
            assert!(probe.read_completed_in_drop.load(Ordering::Relaxed));
            map.enforce_eviction_limits();
            assert!(!map.eviction_runtime_stats().dirty);
        }
    }
}

#[test]
fn metadata_read_failure_does_not_create_a_returned_value_under_lock() {
    struct Counted(Arc<AtomicUsize>);
    impl Clone for Counted {
        fn clone(&self) -> Self {
            self.0.fetch_add(1, Ordering::Relaxed);
            Self(self.0.clone())
        }
    }
    for equality in [false, true] {
        let fault = Arc::new(MetadataFault::default());
        let key = || MetadataKey {
            number: 1,
            metadata_copy: false,
            fault: fault.clone(),
        };
        let copies = Arc::new(AtomicUsize::new(0));
        let map = ShardedHashMap::try_with_manual_eviction(
            1,
            Identity::default(),
            SnapshotMode::Clone,
            EvictionOptions::new(EvictionConfig {
                background_enabled: false,
                ..EvictionConfig::default()
            }),
        )
        .unwrap();
        map.insert(key(), Counted(copies.clone()));
        fault.hashes.store(0, Ordering::Relaxed);
        if equality {
            fault.fail_eq.store(true, Ordering::Relaxed);
        } else {
            fault.fail_hash.store(3, Ordering::Relaxed);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.get(&key())));
        assert!(result.is_err());
        assert_eq!(copies.load(Ordering::Relaxed), 0);
        assert_eq!(map.len(), 1);
    }
}

#[test]
fn unlimited_configured_removals_release_retired_metadata_resources() {
    #[derive(Clone)]
    struct ResourceKey {
        number: u64,
        _resource: Arc<()>,
    }
    impl std::borrow::Borrow<u64> for ResourceKey {
        fn borrow(&self) -> &u64 {
            &self.number
        }
    }
    impl PartialEq for ResourceKey {
        fn eq(&self, other: &Self) -> bool {
            self.number == other.number
        }
    }
    impl Eq for ResourceKey {}
    impl std::hash::Hash for ResourceKey {
        fn hash<H: Hasher>(&self, hasher: &mut H) {
            hasher.write_u64(self.number);
        }
    }
    let map = ShardedHashMap::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Clone,
        EvictionOptions::new(EvictionConfig {
            background_enabled: false,
            ..EvictionConfig::default()
        }),
    )
    .unwrap();
    for number in 0..64_u64 {
        let resource = Arc::new(());
        let weak = Arc::downgrade(&resource);
        map.insert(
            ResourceKey {
                number,
                _resource: resource,
            },
            number,
        );
        assert!(weak.upgrade().is_some());
        assert_eq!(map.remove_borrowed(&number), Some(number));
        assert!(
            weak.upgrade().is_none(),
            "removed metadata must retire before the live map returns"
        );
    }
    assert!(map.is_empty());
    assert_eq!(map.metrics_snapshot().evictions, 0);
}

#[test]
fn maintenance_does_not_hash_unrelated_or_unneeded_candidates() {
    #[derive(Clone)]
    struct Key {
        number: u64,
        reject_odd_hashes: Arc<AtomicBool>,
    }
    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            self.number == other.number
        }
    }
    impl Eq for Key {}
    impl std::hash::Hash for Key {
        fn hash<H: Hasher>(&self, hasher: &mut H) {
            assert!(
                self.number.is_multiple_of(2) || !self.reject_odd_hashes.load(Ordering::Relaxed),
                "maintenance hashed an unrelated or unneeded key"
            );
            hasher.write_u64(self.number);
        }
    }

    for per_shard in [true, false] {
        let reject_odd_hashes = Arc::new(AtomicBool::new(false));
        let key = |number| Key {
            number,
            reject_odd_hashes: reject_odd_hashes.clone(),
        };
        let mut options = EvictionOptions::new(EvictionConfig {
            max_items_per_shard: per_shard.then_some(1),
            background_enabled: false,
            ..EvictionConfig::default()
        });
        if !per_shard {
            // A configured weight budget keeps this case on the full candidate
            // path; after one removal, a satisfied count must stop the scan.
            options = options.with_memory_budget(100, |_: &Key, _: &u64| 1);
        }
        let map = ShardedHashMap::try_with_manual_eviction(
            2,
            Identity::default(),
            SnapshotMode::Cow,
            options,
        )
        .unwrap();
        map.insert(key(0), 10);
        map.insert(key(1), 11);
        reject_odd_hashes.store(true, Ordering::Relaxed);
        if per_shard {
            map.insert(key(2), 12);
        } else {
            assert_eq!(map.evict(1), 1);
        }
        reject_odd_hashes.store(false, Ordering::Relaxed);
        assert_eq!(map.get(&key(0)), None);
        assert_eq!(map.get(&key(1)), Some(11));
        assert_eq!(map.len(), if per_shard { 2 } else { 1 });
    }
}
