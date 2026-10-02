#![cfg(feature = "async")]

use starshard::{AsyncShardedHashMap, RebalanceOptions, SnapshotMode};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0, |hash, byte| {
            hash.wrapping_mul(256).wrapping_add(u64::from(*byte))
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

fn map(shards: usize) -> Map {
    Map::with_shards_and_hasher(shards, Identity::default())
}

fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

/// Hold a shard through a public conditional update without blocking the test's
/// executor. The release guard also unblocks the worker if an assertion panics.
struct HeldShard {
    release: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HeldShard {
    async fn acquire(map: Map, key: u64) -> Self {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = release.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let worker = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(map.compute_if_present(&key, |value| {
                    entered_tx.send(()).unwrap();
                    let (lock, changed) = &*worker_release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = changed.wait(released).unwrap();
                    }
                    Some(value)
                }));
        });
        entered_rx.await.unwrap();
        Self {
            release,
            worker: Some(worker),
        }
    }
}

impl Drop for HeldShard {
    fn drop(&mut self) {
        let (lock, changed) = &*self.release;
        *lock.lock().unwrap() = true;
        changed.notify_one();
        self.worker.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn cancelled_migration_keeps_source_reachable() {
    let map = map(2);
    map.batch_insert([(0, 10), (1, 20)]).await;
    map.start_rebalance_online(4).await.unwrap();
    let held = HeldShard::acquire(map.clone(), 0).await;
    let mut migration = Box::pin(map.advance_rebalance(1));
    assert!(poll_once(migration.as_mut()).is_pending());
    drop(migration);
    drop(held);
    while map.advance_rebalance(1).await != 0 {}
    assert_eq!(map.batch_get(&[0, 1]).await, vec![Some(10), Some(20)]);
    assert_eq!(map.len().await, 2);
}

#[tokio::test]
async fn cancelled_retain_commits_length_before_waiting_for_next_shard() {
    let map = map(256);
    map.batch_insert((0..256).map(|key| (key, key))).await;
    // More shard locks than Tokio's cooperative budget forces a suspension
    // between committed shards, even though routing excludes other writers.
    tokio::task::yield_now().await;
    let mut retain = Box::pin(map.retain(|_, _| false));
    assert!(poll_once(retain.as_mut()).is_pending());
    drop(retain);
    let actual = map.iter().await.len();
    assert!(actual > 0 && actual < 256);
    assert_eq!(map.len().await, actual);
}

#[tokio::test]
async fn panicking_retain_keeps_length_and_snapshot_version_in_sync() {
    let map = map(1);
    map.batch_insert([(0, 10), (1, 20), (2, 30)]).await;
    let _cached = map.shared_snapshot().await;
    let worker_map = map.clone();
    let worker = tokio::spawn(async move {
        let visited = AtomicUsize::new(0);
        worker_map
            .retain(|_, _| {
                assert_ne!(
                    visited.fetch_add(1, Ordering::Relaxed),
                    1,
                    "predicate panic"
                );
                false
            })
            .await;
    });
    assert!(worker.await.unwrap_err().is_panic());
    assert_eq!(map.len().await, 2);
    assert_eq!(map.shared_snapshot().await.len(), 2);
}

#[tokio::test]
async fn pending_get_uses_new_routing_after_shrink() {
    let map = map(2);
    map.batch_insert([(0, 10), (1, 20)]).await;
    let held = HeldShard::acquire(map.clone(), 0).await;
    let mut resize = Box::pin(map.rebalance_to(1, RebalanceOptions::default()));
    assert!(poll_once(resize.as_mut()).is_pending());
    let mut read = Box::pin(map.get(&1));
    assert!(poll_once(read.as_mut()).is_pending());
    drop(held);
    resize.await.unwrap();
    assert_eq!(read.await, Some(20));
    assert_eq!(map.shard_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_advances_preserve_every_entry_and_reader_reachability() {
    let map = map(8);
    map.batch_insert((0..256).map(|key| (key, key + 10))).await;
    map.start_rebalance_online(16).await.unwrap();
    let reader_map = map.clone();
    let reader = tokio::spawn(async move {
        for _ in 0..128 {
            assert_eq!(reader_map.get(&0).await, Some(10));
            tokio::task::yield_now().await;
        }
    });
    let mut workers = Vec::new();
    for _ in 0..8 {
        let map = map.clone();
        workers.push(tokio::spawn(async move { map.advance_rebalance(1).await }));
    }
    let mut advanced = 0;
    for worker in workers {
        advanced += worker.await.unwrap();
    }
    reader.await.unwrap();
    assert_eq!(advanced, 8);
    assert_eq!(map.rebalance_status().state, "idle");
    assert_eq!(map.len().await, 256);
    for key in 0..256 {
        assert_eq!(map.get(&key).await, Some(key + 10));
    }
}

#[tokio::test]
async fn every_snapshot_mode_includes_both_generations_and_reuses_shared_cache() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
            2,
            Identity::default(),
            mode,
        );
        map.batch_insert([(0_u64, 10), (1, 20)]).await;
        map.start_rebalance_online(4).await.unwrap();
        map.insert(0, 30).await;
        let mut entries = map.iter().await;
        entries.sort_unstable();
        assert_eq!(entries, vec![(0, 30), (1, 20)]);
        let first = map.shared_snapshot().await;
        let second = map.shared_snapshot().await;
        assert_eq!(Arc::ptr_eq(&first, &second), mode != SnapshotMode::Clone);
        map.insert(0, 40).await;
        assert!(first.contains(&(0, 30)));
        assert!(map.shared_snapshot().await.contains(&(0, 40)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_initialization_is_atomic() {
    let map = map(2);
    map.start_rebalance_online(4).await.unwrap();
    let initializations = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..16 {
        let map = map.clone();
        let initializations = initializations.clone();
        workers.push(tokio::spawn(async move {
            map.get_or_insert_with(1, || initializations.fetch_add(1, Ordering::Relaxed) as u64)
                .await
        }));
    }
    for worker in workers {
        assert_eq!(worker.await.unwrap(), 0);
    }
    assert_eq!(initializations.load(Ordering::Relaxed), 1);
    assert_eq!(map.len().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_conditional_updates_do_not_lose_increments() {
    let map = map(2);
    map.insert(1, 0).await;
    map.start_rebalance_online(4).await.unwrap();
    let mut workers = Vec::new();
    for _ in 0..16 {
        let map = map.clone();
        workers.push(tokio::spawn(async move {
            for _ in 0..32 {
                map.compute_if_present(&1, |value| Some(value + 1)).await;
            }
        }));
    }
    for worker in workers {
        worker.await.unwrap();
    }
    assert_eq!(map.get(&1).await, Some(512));
    assert_eq!(map.len().await, 1);
}

#[tokio::test]
async fn borrowed_queries_do_not_materialize_empty_shards() {
    let map = AsyncShardedHashMap::<String, u64>::new(8);
    assert_eq!(map.get_borrowed("missing").await, None);
    assert!(!map.contains_borrowed("missing").await);
    assert_eq!(map.initialized_shards().await, 0);
    map.insert("present".to_owned(), 10).await;
    map.start_rebalance_online(16).await.unwrap();
    assert_eq!(map.get_borrowed("present").await, Some(10));
    assert!(map.contains_borrowed("present").await);
    assert_eq!(map.initialized_shards().await, 0);
    assert_eq!(map.remove_borrowed("present").await, Some(10));
    assert_eq!(map.len().await, 0);
}

struct Counted(Arc<AtomicUsize>);

impl Clone for Counted {
    fn clone(&self) -> Self {
        self.0.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

#[tokio::test]
async fn writes_do_not_copy_cow_shards_and_clone_snapshots_copy_values_once() {
    let clones = Arc::new(AtomicUsize::new(0));
    let cow = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cow);
    cow.batch_insert((0..64).map(|key| (key, Counted(clones.clone()))))
        .await;
    clones.store(0, Ordering::Relaxed);
    cow.insert(0, Counted(clones.clone())).await;
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    cow.keys().await;
    assert_eq!(cow.read_with(&0, |_| 7).await, Some(7));
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    let first = cow.shared_snapshot().await;
    assert_eq!(clones.load(Ordering::Relaxed), 64);
    let second = cow.shared_snapshot().await;
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(clones.load(Ordering::Relaxed), 64);

    let plain = AsyncShardedHashMap::new(1);
    plain
        .batch_insert((0..64).map(|key| (key, Counted(clones.clone()))))
        .await;
    clones.store(0, Ordering::Relaxed);
    assert_eq!(plain.iter().await.len(), 64);
    assert_eq!(clones.load(Ordering::Relaxed), 64);
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn migration_cas_and_transactions_update_existing_logical_keys() {
    use starshard::{Transaction, TransactionResult};
    let map = map(2);
    map.batch_insert([(0, 10), (1, 20), (2, 30)]).await;
    map.start_rebalance_online(4).await.unwrap();
    assert!(map.compare_and_swap(&0, &10, 11).await.is_success());
    assert!(map.compare_and_remove(&1, &20).await);
    let mut transaction = Transaction::new();
    transaction.write(2, 31);
    transaction.remove(0);
    assert!(matches!(
        map.execute_transaction(transaction).await,
        TransactionResult::Committed(())
    ));
    assert_eq!(map.len().await, 1);
    assert_eq!(map.iter().await, vec![(2, 31)]);
    let mut remove = Transaction::new();
    remove.remove(2);
    map.execute_transaction(remove).await;
    assert!(map.is_empty().await);
    assert_eq!(map.get(&2).await, None);
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn snapshot_versions_identify_mutations_and_reject_old_data() {
    let map = map(2);
    map.insert(1, 10).await;
    let first = map.versioned_snapshot().await;
    assert!(map.snapshot_at_version(first.version()).await.is_some());
    let repeat = map.versioned_snapshot().await;
    assert_eq!(first.version(), repeat.version());
    map.insert(1, 20).await;
    assert!(map.snapshot_at_version(first.version()).await.is_none());
    let current = map.versioned_snapshot().await;
    assert!(current.version() > first.version());
    assert_eq!(current.len(), 1);
    assert_eq!(map.get(&1).await, Some(20));
}

#[cfg(feature = "lifecycle")]
#[tokio::test]
async fn migration_retain_and_drain_include_previous_entries() {
    let map = map(2);
    map.batch_insert([(0, 10), (1, 20), (2, 30)]).await;
    map.start_rebalance_online(4).await.unwrap();
    map.retain(|key, _| *key != 1).await;
    assert_eq!(map.len().await, 2);
    let mut drained: Vec<_> = map.drain().await.collect();
    drained.sort_unstable();
    assert_eq!(drained, vec![(0, 10), (2, 30)]);
    assert!(map.is_empty().await);
    assert_eq!(map.get(&0).await, None);
    assert_eq!(map.get(&2).await, None);
    assert_eq!(map.rebalance_status().state, "idle");
}

#[derive(Default)]
struct HashPanic {
    calls: AtomicUsize,
    panic_at: AtomicUsize,
}

impl HashPanic {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            panic_at: AtomicUsize::new(usize::MAX),
        })
    }

    fn arm(&self, call: usize) {
        self.calls.store(0, Ordering::Relaxed);
        self.panic_at.store(call, Ordering::Relaxed);
    }

    fn disarm(&self) {
        self.panic_at.store(usize::MAX, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct PanicKey {
    value: u64,
    state: Arc<HashPanic>,
}

impl PartialEq for PanicKey {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for PanicKey {}

impl std::hash::Hash for PanicKey {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        let call = self.state.calls.fetch_add(1, Ordering::Relaxed);
        assert_ne!(
            call,
            self.state.panic_at.load(Ordering::Relaxed),
            "injected key hash panic"
        );
        hasher.write_u64(self.value);
    }
}

#[tokio::test]
async fn migration_hash_panic_preserves_data_and_can_resume() {
    let state = HashPanic::new();
    let key = |value| PanicKey {
        value,
        state: state.clone(),
    };
    let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        1,
        Identity::default(),
        SnapshotMode::Cached,
    );
    map.batch_insert((0..8).map(|value| (key(value), value)))
        .await;
    map.start_rebalance_online(2).await.unwrap();
    map.insert(key(8), 8).await;
    let cached = map.shared_snapshot().await;
    // Hash all eight source keys for target discovery, stage one entry, then
    // panic while processing another entry. Old drain-based migration lost data.
    state.arm(10);
    let worker_map = map.clone();
    let result = tokio::spawn(async move { worker_map.advance_rebalance(1).await }).await;
    assert!(result.unwrap_err().is_panic());
    state.disarm();
    assert_eq!(map.len().await, 9);
    assert_eq!(map.rebalance_status().moved_shards, 0);
    assert_eq!(map.iter().await.len(), cached.len());
    for value in 0..9 {
        assert_eq!(map.get(&key(value)).await, Some(value));
    }
    assert_eq!(map.advance_rebalance(1).await, 1);
    assert_eq!(map.rebalance_status().state, "idle");
    assert_eq!(map.iter().await.len(), 9);
}

#[tokio::test]
async fn full_rebalance_hash_panic_preserves_both_generations_and_can_resume() {
    let state = HashPanic::new();
    let key = |value| PanicKey {
        value,
        state: state.clone(),
    };
    let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        1,
        Identity::default(),
        SnapshotMode::Cached,
    );
    map.batch_insert((0..8).map(|value| (key(value), value)))
        .await;
    map.start_rebalance_online(2).await.unwrap();
    map.insert(key(8), 8).await;
    let cached = map.shared_snapshot().await;
    state.arm(2);
    let worker_map = map.clone();
    let result = tokio::spawn(async move {
        worker_map
            .rebalance_to(4, RebalanceOptions::default())
            .await
    })
    .await;
    assert!(result.unwrap_err().is_panic());
    state.disarm();
    assert_eq!(map.shard_count(), 2);
    assert_eq!(map.len().await, 9);
    assert_eq!(map.iter().await.len(), cached.len());
    for value in 0..9 {
        assert_eq!(map.get(&key(value)).await, Some(value));
    }
    let report = map
        .rebalance_to(4, RebalanceOptions::default())
        .await
        .unwrap();
    assert_eq!(report.moved_entries, 9);
    assert_eq!(map.shard_count(), 4);
    assert_eq!(map.rebalance_status().state, "idle");
    assert_eq!(map.iter().await.len(), 9);
}

fn snapshot_from_lazy_producer(map: Map) {
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(map.shared_snapshot());
        completed_tx.send(()).unwrap();
    });
    completed_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("lazy producers must run before the batch acquires routing");
    worker.join().unwrap();
}

#[tokio::test]
async fn batch_lazy_producers_can_read_the_same_map() {
    let map = map(2);
    let inserted = map
        .batch_insert(std::iter::once_with(|| {
            snapshot_from_lazy_producer(map.clone());
            (0, 10)
        }))
        .await;
    assert_eq!(inserted, 1);
    let removed = map
        .batch_remove(std::iter::once_with(|| {
            snapshot_from_lazy_producer(map.clone());
            0
        }))
        .await;
    assert_eq!(removed, 1);
    assert!(map.is_empty().await);
}
