use starshard::{RebalanceOptions, ShardedHashMap, SnapshotMode};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn migration_snapshots_include_both_generations_in_all_modes() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = ShardedHashMap::with_snapshot_mode(2, mode);
        map.batch_insert((0..128).map(|key| (key, key)));
        map.start_rebalance_online(8).unwrap();
        map.insert(0, 999);
        let snapshot = map.shared_snapshot();
        assert_eq!(snapshot.len(), 128);
        assert_eq!(snapshot.iter().find(|(key, _)| *key == 0), Some(&(0, 999)));
        while map.rebalance_status().state == "migrating" {
            map.advance_rebalance(1);
        }
        assert_eq!(map.iter().count(), 128);
    }
}

#[cfg(feature = "serde")]
#[test]
fn serialization_preserves_unmigrated_entries() {
    let map = ShardedHashMap::new(2);
    map.insert(1_u64, 10_u64);
    map.start_rebalance_online(8).unwrap();
    let restored: ShardedHashMap<u64, u64> =
        serde_json::from_str(&serde_json::to_string(&map).unwrap()).unwrap();
    assert_eq!(restored.get(&1), Some(10));
    assert_eq!(restored.len(), 1);
}

#[test]
fn online_initializer_executes_once_under_contention() {
    let map = ShardedHashMap::<u64, u64>::new(2);
    map.start_rebalance_online(8).unwrap();
    let start = Arc::new(Barrier::new(8));
    let initializations = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let map = map.clone();
            let start = start.clone();
            let initializations = initializations.clone();
            std::thread::spawn(move || {
                start.wait();
                map.get_or_insert_with(1, || {
                    initializations.fetch_add(1, Ordering::Relaxed);
                    42
                })
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), 42);
    }
    assert_eq!(initializations.load(Ordering::Relaxed), 1);
    assert_eq!(map.len(), 1);
}

#[test]
fn online_compute_preserves_all_concurrent_updates() {
    let map = ShardedHashMap::new(2);
    map.insert(1, 0);
    map.start_rebalance_online(8).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..500 {
                    map.compute_if_present(&1, |value| Some(value + 1));
                }
            });
        }
    });
    assert_eq!(map.get(&1), Some(4000));
}

#[test]
fn concurrent_advances_preserve_reachability_and_all_entries() {
    let map = ShardedHashMap::new(16);
    map.batch_insert((0..1024).map(|key| (key, key)));
    map.start_rebalance_online(32).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                while map.rebalance_status().state == "migrating" {
                    map.advance_rebalance(1);
                }
            });
        }
        scope.spawn(|| {
            for _ in 0..8 {
                for key in 0..1024 {
                    assert_eq!(map.get(&key), Some(key));
                }
            }
        });
    });
    assert_eq!(map.len(), 1024);
    assert_eq!(map.iter().count(), 1024);
}

#[test]
fn resize_and_read_write_never_use_an_obsolete_index() {
    let map = ShardedHashMap::new(64);
    map.batch_insert((0..64).map(|key| (key, 0)));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..40 {
                map.rebalance_to(if i % 2 == 0 { 1 } else { 64 }, RebalanceOptions::default())
                    .unwrap();
            }
        });
        for key in 0..8 {
            let map = &map;
            scope.spawn(move || {
                for value in 0..250 {
                    map.insert(key, value);
                    assert!(map.get(&key).is_some());
                }
            });
        }
    });
    assert_eq!(map.len(), 64);
    for key in 0..8 {
        assert_eq!(map.get(&key), Some(249));
    }
}

#[cfg(feature = "advanced")]
#[test]
fn migration_cas_and_transactions_preserve_logical_keys() {
    use starshard::{Transaction, TransactionResult};
    let map = ShardedHashMap::new(2);
    map.insert(1, 10);
    map.start_rebalance_online(4).unwrap();
    assert!(map.compare_and_swap(&1, &10, 20).is_success());
    let mut transaction = Transaction::new();
    transaction.write(1, 30);
    transaction.write(2, 40);
    assert!(matches!(
        map.execute_transaction(transaction),
        TransactionResult::Committed(())
    ));
    assert_eq!(map.len(), 2);
    let mut transaction = Transaction::new();
    transaction.remove(1);
    map.execute_transaction(transaction);
    assert_eq!(map.get(&1), None);
    assert_eq!(map.len(), 1);
    assert!(map.compare_and_remove(&2, &40));
    assert!(map.is_empty());
}

#[cfg(feature = "lifecycle")]
#[test]
fn retain_and_drain_visit_unmigrated_entries() {
    let map = ShardedHashMap::new(2);
    map.batch_insert((0..20).map(|key| (key, key)));
    map.start_rebalance_online(8).unwrap();
    map.retain(|key, _| key % 2 == 0);
    assert_eq!(map.len(), 10);
    assert_eq!(map.drain().count(), 10);
    assert_eq!(map.rebalance_status().state, "idle");
    for key in 0..20 {
        assert_eq!(map.get(&key), None);
    }
    assert!(map.is_empty());
}

#[test]
fn cow_writes_do_not_clone_values_and_shared_hits_reuse_storage() {
    struct Counted(Arc<AtomicUsize>);
    impl Clone for Counted {
        fn clone(&self) -> Self {
            self.0.fetch_add(1, Ordering::Relaxed);
            Self(self.0.clone())
        }
    }
    let copies = Arc::new(AtomicUsize::new(0));
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cow);
    map.batch_insert((0..64).map(|key| (key, Counted(copies.clone()))));
    copies.store(0, Ordering::Relaxed);
    map.insert(0, Counted(copies.clone()));
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    let first = map.shared_snapshot();
    assert_eq!(copies.load(Ordering::Relaxed), 64);
    assert!(Arc::ptr_eq(&first, &map.shared_snapshot()));
    assert_eq!(copies.load(Ordering::Relaxed), 64);
    let _keys: Vec<_> = map.keys().collect();
    assert_eq!(copies.load(Ordering::Relaxed), 64);
}

#[test]
fn cow_snapshot_matches_completed_concurrent_writes() {
    let map = ShardedHashMap::with_snapshot_mode(4, SnapshotMode::Cow);
    map.batch_insert((0..32).map(|key| (key, 0)));
    map.start_rebalance_online(8).unwrap();
    std::thread::scope(|scope| {
        for key in 0..8 {
            let map = &map;
            scope.spawn(move || {
                for value in 0..100 {
                    map.insert(key, value);
                }
            });
        }
        scope.spawn(|| {
            for _ in 0..20 {
                let _ = map.shared_snapshot();
            }
        });
    });
    let snapshot = map.shared_snapshot();
    for key in 0..8 {
        assert_eq!(
            snapshot.iter().find(|(k, _)| *k == key).map(|(_, v)| *v),
            Some(99)
        );
    }
}

#[test]
fn borrowed_misses_do_not_allocate_and_projection_does_not_clone() {
    let map = ShardedHashMap::<String, Vec<u8>>::new(64);
    assert_eq!(map.get_borrowed("absent"), None);
    assert!(!map.contains_borrowed("absent"));
    assert_eq!(map.initialized_shards(), 0);
    map.insert("key".into(), vec![1, 2, 3]);
    assert_eq!(map.read_with("key", Vec::len), Some(3));
    assert_eq!(map.remove_borrowed("key"), Some(vec![1, 2, 3]));
}

#[cfg(feature = "advanced")]
#[test]
fn snapshot_versions_identify_data_not_snapshot_calls() {
    let map = ShardedHashMap::new(2);
    map.insert(1, 10);
    let first = map.versioned_snapshot();
    assert_eq!(first.version(), map.versioned_snapshot().version());
    assert!(map.snapshot_at_version(first.version()).is_some());
    map.insert(2, 20);
    assert!(map.snapshot_at_version(first.version()).is_none());
    assert!(map.versioned_snapshot().version() > first.version());
}

#[test]
fn concurrent_clear_keeps_completed_length_exact() {
    let map = ShardedHashMap::new(16);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..200 {
                map.clear();
            }
        });
        for worker in 0..4 {
            let map = &map;
            scope.spawn(move || {
                for key in 0..1000 {
                    map.insert(worker * 1000 + key, key);
                }
            });
        }
    });
    assert_eq!(map.len(), map.iter().count());
}

#[test]
fn panicking_retain_predicate_preserves_completed_length() {
    let map = ShardedHashMap::new(1);
    map.batch_insert((0..32).map(|key| (key, key)));
    let calls = AtomicUsize::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        map.retain(|_, _| {
            assert!(
                calls.fetch_add(1, Ordering::Relaxed) < 5,
                "intentional predicate panic"
            );
            false
        })
    }));
    assert!(result.is_err());
    assert_eq!(map.len(), map.iter().count());
}

#[test]
fn batch_producers_can_take_map_snapshots_before_routing_is_locked() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let map = ShardedHashMap::<usize, usize>::new(2);
        let added = map.batch_insert((0..4).map(|key| (key, map.iter().count())));
        let removed = map.batch_remove((0..4).inspect(|_| {
            let _ = map.shared_snapshot();
        }));
        sender.send((added, removed, map.len())).unwrap();
    });
    assert_eq!(
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        (4, 4, 0)
    );
    worker.join().unwrap();
}
