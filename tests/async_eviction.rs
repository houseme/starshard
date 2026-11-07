#![cfg(all(feature = "async", feature = "lifecycle"))]

use starshard::{
    AsyncShardedHashMap, EvictionConfig, EvictionError, EvictionOptions, EvictionPolicy,
    EvictionSchedulerError, EvictionWorkerStop, SnapshotMode,
};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0, |value, byte| {
            value.wrapping_mul(256).wrapping_add(u64::from(*byte))
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
type Map = AsyncShardedHashMap<u64, u64, Identity>;
fn config(limit: usize) -> EvictionConfig {
    EvictionConfig {
        policy: EvictionPolicy::Lru,
        max_entries: Some(limit),
        max_items_per_shard: None,
        ttl: None,
        check_interval: Duration::from_millis(5),
        background_enabled: false,
    }
}
fn map(limit: usize) -> Map {
    Map::try_with_manual_eviction(
        2,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(config(limit)),
    )
    .unwrap()
}
fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

struct HeldRead {
    release: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl HeldRead {
    async fn acquire(map: Map, key: u64) -> Self {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let gate = release.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let worker = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(map.read_with(&key, |_| {
                    entered_tx.send(()).unwrap();
                    let (lock, changed) = &*gate;
                    let mut ready = lock.lock().unwrap();
                    while !*ready {
                        ready = changed.wait(ready).unwrap();
                    }
                }));
        });
        entered_rx.await.unwrap();
        Self {
            release,
            worker: Some(worker),
        }
    }
}
impl Drop for HeldRead {
    fn drop(&mut self) {
        let (lock, changed) = &*self.release;
        *lock.lock().unwrap() = true;
        changed.notify_one();
        self.worker.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn cancellation_after_insert_keeps_pending_until_explicit_repair() {
    let map = map(1);
    map.insert(1, 10).await;
    let old_view = map.snapshot_view().await;
    let held = HeldRead::acquire(map.clone(), 1).await;
    let mut inserting = Box::pin(map.insert(2, 20));
    assert!(poll_once(inserting.as_mut()).is_pending());
    drop(inserting);
    assert_eq!(map.len().await, 2);
    assert!(map.eviction_runtime_stats().pending);
    assert!(map.eviction_runtime_stats().dirty);
    drop(held);
    assert_eq!(map.len().await, 2);
    assert_eq!(map.enforce_eviction_limits().await, 1);
    assert_eq!(map.len().await, 1);
    assert!(!map.eviction_runtime_stats().pending);
    assert!(!map.eviction_runtime_stats().dirty);
    assert_eq!(old_view.len(), 1);
    assert_eq!(map.metrics_snapshot().total_inserts, 2);
    assert_eq!(map.metrics_snapshot().evictions, 1);
}

#[tokio::test]
async fn callback_panic_is_isolated_after_committed_eviction() {
    let map = map(1);
    map.set_evict_callback(|_, _| panic!("expected callback panic"));
    map.insert(1, 10).await;
    map.insert(2, 20).await;
    assert_eq!(map.get(&1).await, None);
    assert_eq!(map.get(&2).await, Some(20));
    assert_eq!(map.eviction_runtime_stats().callback_panics, 1);
    assert_eq!(map.metrics_snapshot().evictions, 1);
    assert_eq!(map.metrics_snapshot().total_removes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reentrant_callback_runs_without_map_locks_and_suppresses_nested_dispatch() {
    let map = Arc::new(map(1));
    let weak = Arc::downgrade(&map);
    map.set_evict_callback(move |_, _| {
        let map = weak.upgrade().unwrap();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(map.insert(3, 30));
        });
    });
    map.insert(1, 10).await;
    tokio::time::timeout(Duration::from_secs(2), map.insert(2, 20))
        .await
        .unwrap();
    assert_eq!(map.len().await, 1);
    assert_eq!(map.get(&3).await, Some(30));
    assert_eq!(map.eviction_runtime_stats().suppressed_callbacks, 1);
    assert_eq!(map.metrics_snapshot().evictions, 2);
    assert_eq!(map.eviction_runtime_stats().callback_panics, 0);
}

#[tokio::test]
async fn last_external_clone_drop_stops_periodic_worker() {
    let map = map(2);
    let handle = map.spawn_eviction_task().unwrap();
    let last = map.clone();
    drop(map);
    assert!(last.eviction_worker_status().running);
    drop(last);
    let status = tokio::time::timeout(Duration::from_secs(2), handle.join())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.running);
    assert_eq!(status.stop_reason, Some(EvictionWorkerStop::OwnersDropped));
}

#[test]
fn automatic_constructor_requires_runtime_but_manual_does_not() {
    let mut automatic = config(2);
    automatic.background_enabled = true;
    let result = AsyncShardedHashMap::<u64, u64>::try_with_eviction(2, automatic);
    assert!(matches!(
        result,
        Err(EvictionError::Scheduler(
            EvictionSchedulerError::RuntimeUnavailable
        ))
    ));
    let _manual = map(2);
}

#[test]
fn automatic_constructor_rejects_runtime_without_time_driver() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut automatic = config(2);
        automatic.background_enabled = true;
        let result = AsyncShardedHashMap::<u64, u64>::try_with_eviction(2, automatic);
        assert!(matches!(
            result,
            Err(EvictionError::Scheduler(
                EvictionSchedulerError::TimeDriverUnavailable
            ))
        ));
    });
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn replicated_mutations_reject_configured_eviction_even_without_quorum() {
    let map = map(1);
    map.insert(1, 10).await;
    assert!(matches!(
        map.insert_replicated(2, 20).await,
        Err(starshard::ReplicaError::Rejected(_))
    ));
    assert!(matches!(
        map.remove_replicated(&1).await,
        Err(starshard::ReplicaError::Rejected(_))
    ));
    let metrics = map.metrics_snapshot();
    assert_eq!(metrics.total_inserts, 1);
    assert_eq!(metrics.total_removes, 0);
    assert_eq!(metrics.evictions, 0);
    assert_eq!(map.get(&1).await, Some(10));
    assert_eq!(map.get(&2).await, None);
}

#[tokio::test]
async fn clear_retain_and_drain_remove_policy_metadata_without_eviction_callbacks() {
    let map = map(8);
    map.batch_insert((0..6).map(|key| (key, key))).await;
    map.start_rebalance_online(4).await.unwrap();
    map.retain(|key, _| *key < 3).await;
    assert_eq!(map.eviction_runtime_stats().tracked_entries, 3);
    assert_eq!(map.metrics_snapshot().total_removes, 3);
    let drained: Vec<_> = map.drain().await.collect();
    assert_eq!(drained.len(), 3);
    assert_eq!(map.eviction_runtime_stats().tracked_entries, 0);
    assert_eq!(map.metrics_snapshot().total_removes, 6);
    map.batch_insert([(0, 0), (1, 1)]).await;
    map.clear().await;
    assert_eq!(map.metrics_snapshot().total_removes, 8);
    assert_eq!(map.metrics_snapshot().evictions, 0);
    assert_eq!(map.eviction_runtime_stats().tracked_entries, 0);
    assert!(!map.eviction_runtime_stats().dirty);
}

struct HashGate {
    armed: std::sync::atomic::AtomicBool,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: Mutex<bool>,
    changed: Condvar,
}
#[derive(Clone)]
struct GatedKey {
    id: u64,
    gate: Arc<HashGate>,
}
impl PartialEq for GatedKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for GatedKey {}
impl std::hash::Hash for GatedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if self.id == 0
            && self
                .gate
                .armed
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.gate
                .entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let mut released = self.gate.released.lock().unwrap();
            while !*released {
                released = self.gate.changed.wait(released).unwrap();
            }
        }
        state.write_u64(self.id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_shard_maintenance_does_not_block_an_unrelated_reader() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let gate = Arc::new(HashGate {
        armed: std::sync::atomic::AtomicBool::new(false),
        entered: Mutex::new(Some(entered_tx)),
        released: Mutex::new(false),
        changed: Condvar::new(),
    });
    let key = |id| GatedKey {
        id,
        gate: gate.clone(),
    };
    let mut per_shard = config(1);
    per_shard.max_entries = None;
    per_shard.max_items_per_shard = Some(1);
    let map = AsyncShardedHashMap::try_with_manual_eviction(
        2,
        Identity::default(),
        SnapshotMode::Clone,
        EvictionOptions::new(per_shard),
    )
    .unwrap();
    map.insert(key(0), 0_u64).await;
    map.insert(key(1), 10).await;
    // Inserting key 2 does not hash resident key 0: its shard already has spare
    // capacity. The gate is reached when maintenance routes policy candidates.
    gate.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let writing = map.clone();
    let new_key = key(2);
    let writer = tokio::spawn(async move { writing.insert(new_key, 20).await });
    entered_rx.await.unwrap();
    let unrelated = tokio::time::timeout(Duration::from_secs(1), map.get(&key(1))).await;
    *gate.released.lock().unwrap() = true;
    gate.changed.notify_one();
    writer.await.unwrap();
    assert_eq!(unrelated.unwrap(), Some(10));
    assert_eq!(map.len().await, 2);
    assert_eq!(map.get(&key(2)).await, Some(20));
}

#[derive(Clone)]
struct ResourceKey {
    id: u64,
    _resource: Arc<Vec<u8>>,
}
impl std::hash::Hash for ResourceKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.id);
    }
}
impl PartialEq for ResourceKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for ResourceKey {}
impl std::borrow::Borrow<u64> for ResourceKey {
    fn borrow(&self) -> &u64 {
        &self.id
    }
}

#[tokio::test]
async fn ordinary_remove_flushes_retired_key_resources_without_capacity_eviction() {
    // Cover the no-limit and clean global fast returns, plus the stable
    // per-shard zero-eviction path. Keep each map alive through every assertion.
    for (global, per_shard) in [(None, None), (Some(4), None), (None, Some(4))] {
        let config = EvictionConfig {
            max_entries: global,
            max_items_per_shard: per_shard,
            background_enabled: false,
            ..EvictionConfig::default()
        };
        let map = AsyncShardedHashMap::<ResourceKey, u64, Identity>::try_with_manual_eviction(
            2,
            Identity::default(),
            SnapshotMode::Clone,
            EvictionOptions::new(config),
        )
        .unwrap();
        for id in 0..16 {
            let resource = Arc::new(vec![0_u8; 1024]);
            let weak = Arc::downgrade(&resource);
            map.insert(
                ResourceKey {
                    id,
                    _resource: resource,
                },
                id,
            )
            .await;
            assert_eq!(map.read_with(&id, |_| ()).await, Some(()));
            assert_eq!(map.remove_borrowed(&id).await, Some(id));
            assert!(
                weak.upgrade().is_none(),
                "retired metadata retained key {id} after remove"
            );
            assert_eq!(map.len().await, 0);
            assert_eq!(map.eviction_runtime_stats().tracked_entries, 0);
        }
        assert_eq!(map.metrics_snapshot().total_removes, 16);
        assert_eq!(map.metrics_snapshot().evictions, 0);
        assert_eq!(map.shard_count(), 2);
    }
}

#[derive(Clone)]
struct HashCountedKey {
    id: u64,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl std::hash::Hash for HashCountedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.write_u64(self.id);
    }
}
impl PartialEq for HashCountedKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for HashCountedKey {}

#[tokio::test]
async fn under_limit_per_shard_maintenance_does_not_hash_resident_keys() {
    let per_shard = EvictionConfig {
        max_items_per_shard: Some(2),
        background_enabled: false,
        ..EvictionConfig::default()
    };
    let map = AsyncShardedHashMap::<HashCountedKey, u64, Identity>::try_with_manual_eviction(
        8,
        Identity::default(),
        SnapshotMode::Cow,
        EvictionOptions::new(per_shard),
    )
    .unwrap();
    let calls: Vec<_> = (0..8)
        .map(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        .collect();
    for (id, counter) in calls.iter().enumerate() {
        map.insert(
            HashCountedKey {
                id: id as u64,
                calls: counter.clone(),
            },
            id as u64,
        )
        .await;
    }
    let retained = map.snapshot_view().await;
    for counter in &calls {
        counter.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    assert_eq!(map.enforce_eviction_limits().await, 0);
    for counter in &calls {
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    assert_eq!(retained.len(), 8);
    assert_eq!(map.len().await, 8);
}

#[tokio::test]
async fn single_global_victim_does_not_hash_unselected_resident_keys() {
    let map = AsyncShardedHashMap::<HashCountedKey, u64, Identity>::try_with_manual_eviction(
        4,
        Identity::default(),
        SnapshotMode::Clone,
        EvictionOptions::new(config(2)),
    )
    .unwrap();
    let calls: Vec<_> = (0..3)
        .map(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        .collect();
    for id in 0..2 {
        map.insert(
            HashCountedKey {
                id,
                calls: calls[id as usize].clone(),
            },
            id,
        )
        .await;
    }
    for counter in &calls {
        counter.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    map.insert(
        HashCountedKey {
            id: 2,
            calls: calls[2].clone(),
        },
        2,
    )
    .await;
    assert_eq!(calls[1].load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(map.len().await, 2);
    assert_eq!(map.metrics_snapshot().evictions, 1);
    let mut keys: Vec<_> = map
        .snapshot_view()
        .await
        .iter()
        .map(|(key, _)| key.id)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec![1, 2]);
}
