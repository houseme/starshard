use starshard::{ShardedHashMap, SnapshotMode};
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct Faults {
    hash: AtomicBool,
    clone: AtomicBool,
    drop: AtomicBool,
}
struct Key {
    id: u64,
    stored: bool,
    faults: Arc<Faults>,
}
impl Key {
    fn query(&self) -> Self {
        Self {
            id: self.id,
            stored: false,
            faults: self.faults.clone(),
        }
    }
}
impl Clone for Key {
    fn clone(&self) -> Self {
        assert!(
            !(self.stored && self.faults.clone.swap(false, Ordering::Relaxed)),
            "key clone fault"
        );
        Self {
            id: self.id,
            stored: self.stored,
            faults: self.faults.clone(),
        }
    }
}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        assert!(
            !(self.stored && self.faults.hash.swap(false, Ordering::Relaxed)),
            "stored key hash fault"
        );
        self.id.hash(state);
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Key {}
impl Drop for Key {
    fn drop(&mut self) {
        assert!(
            !(self.stored && self.faults.drop.swap(false, Ordering::Relaxed)),
            "stored key drop fault"
        );
    }
}

#[test]
fn promotion_hash_panic_keeps_previous_entry_reachable() {
    let faults = Arc::new(Faults::default());
    let key = Key {
        id: 1,
        stored: true,
        faults: faults.clone(),
    };
    let query = key.query();
    let map = ShardedHashMap::with_snapshot_mode(2, SnapshotMode::Cached);
    map.insert(key, 10);
    map.start_rebalance_online(4).unwrap();
    faults.hash.store(true, Ordering::Relaxed);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || map.get_or_insert_with(query.query(), || 99)
        ))
        .is_err()
    );
    assert_eq!(map.get(&query), Some(10));
    assert_eq!(map.len(), 1);
    assert_eq!(map.shared_snapshot().len(), 1);
    while map.rebalance_status().state == "migrating" {
        map.advance_rebalance(1);
    }
    assert_eq!(map.get(&query), Some(10));
}

#[test]
fn remove_key_drop_panic_updates_length_and_cached_data() {
    let faults = Arc::new(Faults::default());
    let key = Key {
        id: 1,
        stored: true,
        faults: faults.clone(),
    };
    let query = key.query();
    let map = ShardedHashMap::with_snapshot_mode(2, SnapshotMode::Cached);
    map.insert(key, 10);
    let old = map.shared_snapshot();
    faults.drop.store(true, Ordering::Relaxed);
    assert!(catch_unwind(AssertUnwindSafe(|| map.remove(&query))).is_err());
    assert_eq!(map.get(&query), None);
    assert_eq!(map.len(), 0);
    assert!(map.shared_snapshot().is_empty());
    assert_eq!(old.len(), 1);
}

struct Value {
    number: u64,
    armed: Arc<AtomicBool>,
    owns_drop: bool,
}
impl Clone for Value {
    fn clone(&self) -> Self {
        Self {
            number: self.number,
            armed: self.armed.clone(),
            owns_drop: false,
        }
    }
}
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
    }
}
impl Drop for Value {
    fn drop(&mut self) {
        assert!(
            !(self.owns_drop && self.armed.swap(false, Ordering::Relaxed)),
            "value drop fault"
        );
    }
}
fn value(number: u64) -> Value {
    Value {
        number,
        armed: Arc::new(AtomicBool::new(false)),
        owns_drop: true,
    }
}

#[test]
fn conditional_update_drop_panic_invalidates_cached_snapshot() {
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    let old = value(10);
    let armed = old.armed.clone();
    map.insert(1, old);
    let snapshot = map.shared_snapshot();
    armed.store(true, Ordering::Relaxed);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || map.compute_if_present(&1, |_| Some(value(20)))
        ))
        .is_err()
    );
    assert_eq!(map.get(&1).unwrap().number, 20);
    assert_eq!(map.shared_snapshot()[0].1.number, 20);
    assert_eq!(snapshot[0].1.number, 10);
}

#[test]
fn replacing_key_drop_panic_publishes_new_value() {
    let faults = Arc::new(Faults::default());
    let key = Key {
        id: 1,
        stored: true,
        faults: faults.clone(),
    };
    let query = key.query();
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    map.insert(key, 10);
    let cached = map.shared_snapshot();
    faults.drop.store(true, Ordering::Relaxed);
    assert!(
        catch_unwind(AssertUnwindSafe(|| map.insert(
            Key {
                id: 1,
                stored: true,
                faults
            },
            20
        )))
        .is_err()
    );
    assert_eq!(map.get(&query), Some(20));
    assert_eq!(map.shared_snapshot()[0].1, 20);
    assert_eq!(cached[0].1, 10);
}

#[cfg(feature = "lifecycle")]
#[test]
fn promotion_growth_hash_panic_preserves_both_generations() {
    let source_faults = Arc::new(Faults::default());
    let key = Key {
        id: 1,
        stored: true,
        faults: source_faults.clone(),
    };
    let query = key.query();
    let map = ShardedHashMap::with_snapshot_mode(2, SnapshotMode::Cached);
    map.insert(key, 10);
    map.start_rebalance_online(1).unwrap();
    let active_faults = Arc::new(Faults::default());
    let mut ids = Vec::new();
    for id in 2..100 {
        map.insert(
            Key {
                id,
                stored: true,
                faults: active_faults.clone(),
            },
            id,
        );
        ids.push(id);
        let stats = map.per_shard_load();
        if stats[0].entry_count == stats[0].capacity {
            break;
        }
    }
    assert_eq!(
        map.per_shard_load()[0].entry_count,
        map.per_shard_load()[0].capacity
    );
    active_faults.hash.store(true, Ordering::Relaxed);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || map.get_or_insert_with(query.query(), || 99)
        ))
        .is_err()
    );
    assert_eq!(map.len(), 1 + ids.len());
    assert_eq!(map.get(&query), Some(10));
    for id in ids {
        assert_eq!(
            map.get(&Key {
                id,
                stored: false,
                faults: active_faults.clone()
            }),
            Some(id)
        );
    }
    source_faults.clone.store(true, Ordering::Relaxed);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || map.get_or_insert_with(query.query(), || 99)
        ))
        .is_err()
    );
    assert_eq!(map.get(&query), Some(10));
}

#[cfg(feature = "advanced")]
#[test]
fn cas_and_transaction_drop_panics_publish_actual_state() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        for transaction in [false, true] {
            let map = ShardedHashMap::with_snapshot_mode(1, mode);
            let old = value(10);
            let armed = old.armed.clone();
            map.insert(1, old);
            let snapshot = map.shared_snapshot();
            armed.store(true, Ordering::Relaxed);
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    if transaction {
                        let mut t = starshard::Transaction::new();
                        t.write(1, value(20));
                        map.execute_transaction(t);
                    } else {
                        map.compare_and_swap(&1, &value(10), value(20));
                    }
                }))
                .is_err()
            );
            assert_eq!(map.get(&1).unwrap().number, 20);
            assert_eq!(map.shared_snapshot()[0].1.number, 20);
            assert_eq!(map.len(), 1);
            assert_eq!(snapshot[0].1.number, 10);
        }
        let map = ShardedHashMap::with_snapshot_mode(1, mode);
        let old = value(10);
        let armed = old.armed.clone();
        map.insert(1, old);
        let _snapshot = map.shared_snapshot();
        armed.store(true, Ordering::Relaxed);
        assert!(catch_unwind(AssertUnwindSafe(|| map.compare_and_remove(&1, &value(10)))).is_err());
        assert_eq!(map.len(), 0);
        assert!(map.shared_snapshot().is_empty());
    }
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use starshard::AsyncShardedHashMap;
    #[tokio::test]
    async fn promotion_hash_and_clone_panics_keep_previous_data() {
        for clone_fault in [false, true] {
            let faults = Arc::new(Faults::default());
            let key = Key {
                id: 1,
                stored: true,
                faults: faults.clone(),
            };
            let query = key.query();
            let map = AsyncShardedHashMap::with_snapshot_mode(2, SnapshotMode::Cached);
            map.insert(key, 10).await;
            map.start_rebalance_online(4).await.unwrap();
            if clone_fault {
                faults.clone.store(true, Ordering::Relaxed);
            } else {
                faults.hash.store(true, Ordering::Relaxed);
            }
            let m = map.clone();
            let q = query.query();
            assert!(
                tokio::spawn(async move {
                    m.get_or_insert_with(q, || 99).await;
                })
                .await
                .unwrap_err()
                .is_panic()
            );
            assert_eq!(map.get(&query).await, Some(10));
            assert_eq!(map.len().await, 1);
            assert_eq!(map.shared_snapshot().await.len(), 1);
            while map.rebalance_status().state == "migrating" {
                map.advance_rebalance(1).await;
            }
            assert_eq!(map.get(&query).await, Some(10));
        }
    }
    #[tokio::test]
    async fn key_drop_panics_keep_len_and_cache_current() {
        for removing in [false, true] {
            let faults = Arc::new(Faults::default());
            let key = Key {
                id: 1,
                stored: true,
                faults: faults.clone(),
            };
            let query = key.query();
            let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
            map.insert(key, 10).await;
            let old = map.shared_snapshot().await;
            faults.drop.store(true, Ordering::Relaxed);
            let m = map.clone();
            let q = query.query();
            assert!(
                tokio::spawn(async move {
                    if removing {
                        m.remove(&q).await;
                    } else {
                        m.insert(
                            Key {
                                id: 1,
                                stored: true,
                                faults,
                            },
                            20,
                        )
                        .await;
                    }
                })
                .await
                .unwrap_err()
                .is_panic()
            );
            assert_eq!(map.len().await, usize::from(!removing));
            if removing {
                assert!(map.shared_snapshot().await.is_empty());
            } else {
                assert_eq!(map.shared_snapshot().await[0].1, 20);
            }
            assert_eq!(old[0].1, 10);
        }
    }
    #[tokio::test]
    async fn conditional_replacement_drop_panic_publishes_new_data() {
        let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        let old = value(10);
        let armed = old.armed.clone();
        map.insert(1, old).await;
        let before = map.shared_snapshot().await;
        armed.store(true, Ordering::Relaxed);
        let m = map.clone();
        assert!(
            tokio::spawn(async move {
                m.compute_if_present(&1, |_| Some(value(20))).await;
            })
            .await
            .unwrap_err()
            .is_panic()
        );
        assert_eq!(map.get(&1).await.unwrap().number, 20);
        assert_eq!(map.shared_snapshot().await[0].1.number, 20);
        assert_eq!(before[0].1.number, 10);
    }
    #[cfg(feature = "advanced")]
    #[tokio::test]
    async fn cas_and_transaction_panics_publish_changes() {
        for transaction in [false, true] {
            let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cow);
            let old = value(10);
            let armed = old.armed.clone();
            map.insert(1, old).await;
            let before = map.shared_snapshot().await;
            armed.store(true, Ordering::Relaxed);
            let m = map.clone();
            assert!(
                tokio::spawn(async move {
                    if transaction {
                        let mut t = starshard::Transaction::new();
                        t.write(1, value(20));
                        m.execute_transaction(t).await;
                    } else {
                        m.compare_and_swap(&1, &value(10), value(20)).await;
                    }
                })
                .await
                .unwrap_err()
                .is_panic()
            );
            assert_eq!(map.get(&1).await.unwrap().number, 20);
            assert_eq!(map.shared_snapshot().await[0].1.number, 20);
            assert_eq!(before[0].1.number, 10);
        }
    }
}
