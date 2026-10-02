use starshard::{ShardedHashMap, SnapshotMode};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_mul(256).wrapping_add(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Identity = BuildHasherDefault<IdentityHasher>;

#[derive(Clone, Copy)]
enum KeyPanic {
    Hash,
    Eq,
}

#[derive(Default)]
struct Faults {
    hash_calls: AtomicUsize,
    hash_armed: AtomicBool,
    eq_armed: AtomicBool,
    drop_armed: AtomicBool,
}

impl Faults {
    fn arm(&self, kind: KeyPanic) {
        match kind {
            KeyPanic::Hash => {
                self.hash_calls.store(0, Ordering::Relaxed);
                self.hash_armed.store(true, Ordering::Relaxed);
            }
            KeyPanic::Eq => self.eq_armed.store(true, Ordering::Relaxed),
        }
    }
}

struct FaultKey {
    id: u64,
    faults: Arc<Faults>,
    owns_drop: bool,
}

impl FaultKey {
    fn new(id: u64) -> Self {
        Self {
            id,
            faults: Arc::default(),
            owns_drop: true,
        }
    }
}

impl Clone for FaultKey {
    fn clone(&self) -> Self {
        // Snapshot and query copies must not trigger the stored key's destructor.
        Self {
            id: self.id,
            faults: self.faults.clone(),
            owns_drop: false,
        }
    }
}

impl Hash for FaultKey {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        if self.faults.hash_armed.load(Ordering::Relaxed)
            && self.faults.hash_calls.fetch_add(1, Ordering::Relaxed) == 1
            && self.faults.hash_armed.swap(false, Ordering::Relaxed)
        {
            // Routing hashes each batch input first; fail inside the shard map.
            panic!("injected shard hash panic");
        }
        // Force collisions so Eq faults do not depend on hash-table capacity.
        hasher.write_u64(0);
    }
}

impl PartialEq for FaultKey {
    fn eq(&self, other: &Self) -> bool {
        assert!(
            !self.faults.eq_armed.swap(false, Ordering::Relaxed)
                && !other.faults.eq_armed.swap(false, Ordering::Relaxed),
            "injected key equality panic"
        );
        self.id == other.id
    }
}

impl Eq for FaultKey {}

impl Drop for FaultKey {
    fn drop(&mut self) {
        assert!(
            !self.owns_drop || !self.faults.drop_armed.swap(false, Ordering::Relaxed),
            "injected stored key destructor panic"
        );
    }
}

struct FaultValue {
    value: u64,
    armed: Arc<AtomicBool>,
    owns_drop: bool,
}

impl FaultValue {
    fn new(value: u64) -> Self {
        Self {
            value,
            armed: Arc::default(),
            owns_drop: true,
        }
    }
}

impl Clone for FaultValue {
    fn clone(&self) -> Self {
        Self {
            value: self.value,
            armed: self.armed.clone(),
            owns_drop: false,
        }
    }
}

impl Drop for FaultValue {
    fn drop(&mut self) {
        assert!(
            !self.owns_drop || !self.armed.swap(false, Ordering::Relaxed),
            "injected replaced value destructor panic"
        );
    }
}

fn contents(snapshot: &[(FaultKey, u64)]) -> Vec<(u64, u64)> {
    let mut entries: Vec<_> = snapshot
        .iter()
        .map(|(key, value)| (key.id, *value))
        .collect();
    entries.sort_unstable();
    entries
}

#[test]
fn batch_insert_panic_publishes_completed_entries() {
    for kind in [KeyPanic::Hash, KeyPanic::Eq] {
        let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        map.insert(FaultKey::new(0), 10);
        let old = map.shared_snapshot();
        let failing = FaultKey::new(2);
        failing.faults.arm(kind);
        let result = catch_unwind(AssertUnwindSafe(|| {
            map.batch_insert([(FaultKey::new(1), 11), (failing, 12)])
        }));
        assert!(result.is_err());
        assert_eq!(map.len(), 2);
        assert_eq!(contents(&map.shared_snapshot()), vec![(0, 10), (1, 11)]);
        assert_eq!(contents(&old), vec![(0, 10)]);
    }
}

#[test]
fn batch_remove_panic_publishes_completed_removals() {
    for kind in [KeyPanic::Hash, KeyPanic::Eq] {
        let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        let failing = FaultKey::new(2);
        map.batch_insert([
            (FaultKey::new(0), 10),
            (FaultKey::new(1), 11),
            (FaultKey::new(2), 12),
        ]);
        let old = map.shared_snapshot();
        failing.faults.arm(kind);
        let result = catch_unwind(AssertUnwindSafe(|| {
            map.batch_remove([FaultKey::new(1), failing])
        }));
        assert!(result.is_err());
        assert_eq!(map.len(), 2);
        assert_eq!(contents(&map.shared_snapshot()), vec![(0, 10), (2, 12)]);
        assert_eq!(old.len(), 3);
    }
}

#[test]
fn batch_remove_stored_key_drop_panic_publishes_length() {
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    let stored = FaultKey::new(1);
    let query = stored.clone();
    let armed = stored.faults.clone();
    map.insert(stored, 11);
    let old = map.shared_snapshot();
    armed.drop_armed.store(true, Ordering::Relaxed);
    let result = catch_unwind(AssertUnwindSafe(|| map.batch_remove([query])));
    assert!(result.is_err());
    assert_eq!(map.len(), 0);
    assert!(map.shared_snapshot().is_empty());
    assert_eq!(contents(&old), vec![(1, 11)]);
}

#[test]
fn batch_replacement_value_drop_panic_invalidates_cached_snapshot() {
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    let old_value = FaultValue::new(10);
    let armed = old_value.armed.clone();
    map.insert(1, old_value);
    let old = map.shared_snapshot();
    armed.store(true, Ordering::Relaxed);
    let result = catch_unwind(AssertUnwindSafe(|| {
        map.batch_insert([(1, FaultValue::new(20))])
    }));
    assert!(result.is_err());
    assert_eq!(map.len(), 1);
    assert_eq!(map.shared_snapshot()[0].1.value, 20);
    assert_eq!(old[0].1.value, 10);
}

#[test]
fn duplicate_batch_keys_preserve_counts_and_replacement_snapshots() {
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    assert_eq!(map.batch_insert([(1, 10), (1, 20), (2, 30)]), 2);
    let old = map.shared_snapshot();
    assert_eq!(map.batch_insert([(1, 40), (1, 50)]), 0);
    assert_eq!(map.len(), 2);
    assert_eq!(map.get(&1), Some(50));
    assert_eq!(
        map.shared_snapshot().iter().find(|(key, _)| *key == 1),
        Some(&(1, 50))
    );
    assert!(old.contains(&(1, 20)));
    assert_eq!(map.batch_remove([1, 1, 3]), 1);
    assert_eq!(map.len(), 1);
    assert_eq!(map.shared_snapshot().as_slice(), &[(2, 30)]);
}

#[test]
fn empty_and_miss_only_batches_reuse_cached_snapshots() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map =
            ShardedHashMap::with_shards_and_hasher_and_snapshot_mode(2, Identity::default(), mode);
        map.insert(0_u64, 10_u64);
        let cached = map.shared_snapshot();
        #[cfg(feature = "advanced")]
        let version = map.versioned_snapshot().version();

        assert_eq!(map.batch_insert([]), 0);
        assert_eq!(map.batch_remove([]), 0);
        // Exercise both an initialized shard and an uninitialized shard.
        assert_eq!(map.batch_remove([2, 3, 2]), 0);
        assert_eq!(map.len(), 1);
        assert!(Arc::ptr_eq(&cached, &map.shared_snapshot()));
        #[cfg(feature = "advanced")]
        {
            assert_eq!(map.versioned_snapshot().version(), version);
            assert!(map.snapshot_at_version(version).is_some());
        }
    }
}

#[test]
fn concurrent_batches_on_different_shards_accumulate_length_deltas() {
    let map = ShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Cached,
    );
    let start = Barrier::new(4);
    std::thread::scope(|scope| {
        for shard in 0..4_u64 {
            let map = &map;
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..12 {
                    assert_eq!(
                        map.batch_insert((0..64).map(|index| (shard + index * 4, round))),
                        if round == 0 { 64 } else { 32 }
                    );
                    assert_eq!(
                        map.batch_remove((0..64).step_by(2).map(|index| shard + index * 4)),
                        32
                    );
                }
            });
        }
    });
    assert_eq!(map.len(), 128);
    let snapshot = map.shared_snapshot();
    assert_eq!(snapshot.len(), 128);
    assert!(
        snapshot
            .iter()
            .all(|(key, value)| key / 4 % 2 == 1 && *value == 11)
    );
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use starshard::AsyncShardedHashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Condvar, Mutex};
    use std::task::{Context, Poll, Waker};

    type Map = AsyncShardedHashMap<u64, u64, Identity>;

    fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// Keep one shard locked on a separate runtime without blocking this test's
    /// executor. Unwinding releases the worker before joining it.
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
            let held = Self {
                release,
                worker: Some(worker),
            };
            entered_rx.await.unwrap();
            held
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

    async fn check_cancelled_batch(remove: bool) {
        let mut observed_committed_prefix = false;
        // Trying both held shards covers a committed first bucket without making
        // the test depend on the internal bucket HashMap's iteration order.
        for held_index in 0..2 {
            let map = Map::with_shards_and_hasher_and_snapshot_mode(
                2,
                Identity::default(),
                SnapshotMode::Cached,
            );
            map.batch_insert([(0, 10), (1, 11)]).await;
            if remove {
                map.batch_insert([(2, 12), (3, 13)]).await;
            }
            let initial_len = map.len().await;
            let cached = map.shared_snapshot().await;
            #[cfg(feature = "advanced")]
            let version = map.versioned_snapshot().await.version();
            let held = HeldShard::acquire(map.clone(), held_index).await;

            // Refresh Tokio's cooperative budget so this poll reaches the held
            // shard rather than stopping at an unrelated cooperative yield.
            tokio::task::yield_now().await;
            let mut batch: Pin<Box<dyn Future<Output = usize> + '_>> = if remove {
                Box::pin(map.batch_remove([2, 3]))
            } else {
                Box::pin(map.batch_insert([(2, 12), (3, 13)]))
            };
            assert!(poll_once(batch.as_mut()).is_pending());

            let unblocked_key = 3 - held_index;
            let unblocked_value = map.get(&unblocked_key).await;
            let committed = if remove {
                unblocked_value.is_none()
            } else {
                unblocked_value.is_some()
            };
            observed_committed_prefix |= committed;
            let expected_len = if remove {
                initial_len - usize::from(committed)
            } else {
                initial_len + usize::from(committed)
            };
            assert_eq!(map.len().await, expected_len);

            #[cfg(feature = "advanced")]
            if committed {
                let mut stale = Box::pin(map.snapshot_at_version(version));
                assert!(
                    matches!(poll_once(stale.as_mut()), Poll::Ready(None)),
                    "the completed bucket must invalidate its snapshot before the next lock"
                );
            }

            drop(batch);
            drop(held);
            assert_eq!(map.len().await, expected_len);
            assert_eq!(map.get(&unblocked_key).await, unblocked_value);
            let blocked_key = held_index + 2;
            assert_eq!(
                map.get(&blocked_key).await,
                remove.then_some(blocked_key + 10)
            );
            assert_eq!(map.get(&0).await, Some(10));
            assert_eq!(map.get(&1).await, Some(11));
            assert_eq!(map.shared_snapshot().await.len(), expected_len);
            assert_eq!(cached.len(), initial_len);
        }
        assert!(
            observed_committed_prefix,
            "at least one held-shard case must cancel after committing a bucket"
        );
    }

    #[tokio::test]
    async fn cancelled_batch_insert_preserves_completed_shard_metadata() {
        check_cancelled_batch(false).await;
    }

    #[tokio::test]
    async fn cancelled_batch_remove_preserves_completed_shard_metadata() {
        check_cancelled_batch(true).await;
    }

    #[tokio::test]
    async fn batch_insert_panic_publishes_completed_entries() {
        for kind in [KeyPanic::Hash, KeyPanic::Eq] {
            let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
            map.insert(FaultKey::new(0), 10).await;
            let old = map.shared_snapshot().await;
            let failing = FaultKey::new(2);
            failing.faults.arm(kind);
            let worker = map.clone();
            let result = tokio::spawn(async move {
                worker
                    .batch_insert([(FaultKey::new(1), 11), (failing, 12)])
                    .await
            })
            .await;
            assert!(result.unwrap_err().is_panic());
            assert_eq!(map.len().await, 2);
            assert_eq!(
                contents(&map.shared_snapshot().await),
                vec![(0, 10), (1, 11)]
            );
            assert_eq!(contents(&old), vec![(0, 10)]);
        }
    }

    #[tokio::test]
    async fn batch_remove_panic_publishes_completed_removals() {
        for kind in [KeyPanic::Hash, KeyPanic::Eq] {
            let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
            let failing = FaultKey::new(2);
            map.batch_insert([
                (FaultKey::new(0), 10),
                (FaultKey::new(1), 11),
                (FaultKey::new(2), 12),
            ])
            .await;
            let old = map.shared_snapshot().await;
            failing.faults.arm(kind);
            let worker = map.clone();
            let result =
                tokio::spawn(async move { worker.batch_remove([FaultKey::new(1), failing]).await })
                    .await;
            assert!(result.unwrap_err().is_panic());
            assert_eq!(map.len().await, 2);
            assert_eq!(
                contents(&map.shared_snapshot().await),
                vec![(0, 10), (2, 12)]
            );
            assert_eq!(old.len(), 3);
        }
    }

    #[tokio::test]
    async fn batch_remove_stored_key_drop_panic_publishes_length() {
        let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        let stored = FaultKey::new(1);
        let query = stored.clone();
        let armed = stored.faults.clone();
        map.insert(stored, 11).await;
        let old = map.shared_snapshot().await;
        armed.drop_armed.store(true, Ordering::Relaxed);
        let worker = map.clone();
        let result = tokio::spawn(async move { worker.batch_remove([query]).await }).await;
        assert!(result.unwrap_err().is_panic());
        assert_eq!(map.len().await, 0);
        assert!(map.shared_snapshot().await.is_empty());
        assert_eq!(contents(&old), vec![(1, 11)]);
    }

    #[tokio::test]
    async fn batch_replacement_value_drop_panic_invalidates_cached_snapshot() {
        let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        let old_value = FaultValue::new(10);
        let armed = old_value.armed.clone();
        map.insert(1, old_value).await;
        let old = map.shared_snapshot().await;
        armed.store(true, Ordering::Relaxed);
        let worker = map.clone();
        let result =
            tokio::spawn(async move { worker.batch_insert([(1, FaultValue::new(20))]).await })
                .await;
        assert!(result.unwrap_err().is_panic());
        assert_eq!(map.len().await, 1);
        assert_eq!(map.shared_snapshot().await[0].1.value, 20);
        assert_eq!(old[0].1.value, 10);
    }

    #[tokio::test]
    async fn duplicate_batch_keys_preserve_counts_and_replacement_snapshots() {
        let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        assert_eq!(map.batch_insert([(1, 10), (1, 20), (2, 30)]).await, 2);
        let old = map.shared_snapshot().await;
        assert_eq!(map.batch_insert([(1, 40), (1, 50)]).await, 0);
        assert_eq!(map.len().await, 2);
        assert_eq!(map.get(&1).await, Some(50));
        assert_eq!(
            map.shared_snapshot()
                .await
                .iter()
                .find(|(key, _)| *key == 1),
            Some(&(1, 50))
        );
        assert!(old.contains(&(1, 20)));
        assert_eq!(map.batch_remove([1, 1, 3]).await, 1);
        assert_eq!(map.len().await, 1);
        assert_eq!(map.shared_snapshot().await.as_slice(), &[(2, 30)]);
    }

    #[tokio::test]
    async fn empty_and_miss_only_batches_reuse_cached_snapshots() {
        for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
            let map = Map::with_shards_and_hasher_and_snapshot_mode(2, Identity::default(), mode);
            map.insert(0, 10).await;
            let cached = map.shared_snapshot().await;
            #[cfg(feature = "advanced")]
            let version = map.versioned_snapshot().await.version();

            assert_eq!(map.batch_insert([]).await, 0);
            assert_eq!(map.batch_remove([]).await, 0);
            assert_eq!(map.batch_remove([2, 3, 2]).await, 0);
            assert_eq!(map.len().await, 1);
            assert!(Arc::ptr_eq(&cached, &map.shared_snapshot().await));
            #[cfg(feature = "advanced")]
            {
                assert_eq!(map.versioned_snapshot().await.version(), version);
                assert!(map.snapshot_at_version(version).await.is_some());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_batches_on_different_shards_accumulate_length_deltas() {
        let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
            4,
            Identity::default(),
            SnapshotMode::Cached,
        );
        let start = Arc::new(tokio::sync::Barrier::new(4));
        let mut workers = Vec::new();
        for shard in 0..4_u64 {
            let map = map.clone();
            let start = start.clone();
            workers.push(tokio::spawn(async move {
                start.wait().await;
                for round in 0..12 {
                    assert_eq!(
                        map.batch_insert((0..64).map(|index| (shard + index * 4, round)))
                            .await,
                        if round == 0 { 64 } else { 32 }
                    );
                    assert_eq!(
                        map.batch_remove((0..64).step_by(2).map(|index| shard + index * 4))
                            .await,
                        32
                    );
                }
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
        assert_eq!(map.len().await, 128);
        let snapshot = map.shared_snapshot().await;
        assert_eq!(snapshot.len(), 128);
        assert!(
            snapshot
                .iter()
                .all(|(key, value)| key / 4 % 2 == 1 && *value == 11)
        );
    }
}
