#![cfg(feature = "async")]

use starshard::{AsyncShardedHashMap, SnapshotMode};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
type Map<V = u64> = AsyncShardedHashMap<u64, V, Identity>;

fn map<V: Clone + Send + Sync + 'static>(mode: SnapshotMode) -> Map<V> {
    Map::with_shards_and_hasher_and_snapshot_mode(4, Identity::default(), mode)
}

fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

struct Pause {
    armed: AtomicBool,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: Mutex<bool>,
    changed: Condvar,
}

impl Pause {
    fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (entered, receiver) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                armed: AtomicBool::new(true),
                entered: Mutex::new(Some(entered)),
                released: Mutex::new(false),
                changed: Condvar::new(),
            }),
            receiver,
        )
    }

    fn wait_if_armed(&self) {
        if !self.armed.swap(false, Ordering::Relaxed) {
            return;
        }
        self.entered
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(())
            .unwrap();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_one();
    }
}

/// A failed progress assertion must still release and join the paused worker.
struct PausedWorker<T> {
    pause: Arc<Pause>,
    worker: Option<std::thread::JoinHandle<T>>,
}

impl<T: Send + 'static> PausedWorker<T> {
    fn start(pause: Arc<Pause>, operation: impl FnOnce() -> T + Send + 'static) -> Self {
        Self {
            pause,
            worker: Some(std::thread::spawn(operation)),
        }
    }

    fn finish(mut self) -> T {
        self.pause.release();
        self.worker.take().unwrap().join().unwrap()
    }
}

impl<T> Drop for PausedWorker<T> {
    fn drop(&mut self) {
        self.pause.release();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

struct ControlledValue {
    value: u64,
    clones: Arc<AtomicUsize>,
    pause: Option<Arc<Pause>>,
    panic: Option<Arc<AtomicBool>>,
}

impl ControlledValue {
    fn plain(value: u64, clones: &Arc<AtomicUsize>) -> Self {
        Self {
            value,
            clones: clones.clone(),
            pause: None,
            panic: None,
        }
    }
}

impl Clone for ControlledValue {
    fn clone(&self) -> Self {
        if let Some(pause) = &self.pause {
            pause.wait_if_armed();
        }
        if let Some(panic) = &self.panic {
            assert!(!panic.swap(false, Ordering::Relaxed), "clone panic");
        }
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            value: self.value,
            clones: self.clones.clone(),
            pause: self.pause.clone(),
            panic: self.panic.clone(),
        }
    }
}

#[tokio::test]
async fn snapshot_build_allows_reads_and_freezes_existing_and_cold_shard_writes() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let clones = Arc::new(AtomicUsize::new(0));
        let (pause, entered) = Pause::new();
        map.insert(
            0,
            ControlledValue {
                pause: Some(pause.clone()),
                ..ControlledValue::plain(10, &clones)
            },
        )
        .await;
        map.insert(1, ControlledValue::plain(20, &clones)).await;
        let worker_map = map.clone();
        let worker = PausedWorker::start(pause, move || run(worker_map.shared_snapshot()));
        entered.await.unwrap();

        let mut read = Box::pin(map.get(&1));
        let std::task::Poll::Ready(Some(value)) = poll_once(read.as_mut()) else {
            panic!("an ordinary read waited for snapshot cloning");
        };
        assert_eq!(value.value, 20);
        drop(read);
        let mut contains = Box::pin(map.contains(&0));
        assert!(matches!(
            poll_once(contains.as_mut()),
            std::task::Poll::Ready(true)
        ));
        drop(contains);

        let mut write = Box::pin(map.insert(0, ControlledValue::plain(30, &clones)));
        assert!(poll_once(write.as_mut()).is_pending());
        let mut cold_write = Box::pin(map.insert(2, ControlledValue::plain(40, &clones)));
        assert!(poll_once(cold_write.as_mut()).is_pending());
        let snapshot = worker.finish();
        assert_eq!(snapshot.len(), 2);
        assert!(
            snapshot
                .iter()
                .any(|(key, value)| *key == 0 && value.value == 10)
        );
        write.await;
        cold_write.await;
        let current = map.shared_snapshot().await;
        assert_eq!(current.len(), 3);
        assert!(
            current
                .iter()
                .any(|(key, value)| *key == 0 && value.value == 30)
        );
        assert!(
            current
                .iter()
                .any(|(key, value)| *key == 2 && value.value == 40)
        );
    }
}

#[tokio::test]
async fn concurrent_cache_misses_clone_each_value_once_and_share_the_result() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let clones = Arc::new(AtomicUsize::new(0));
        let (pause, entered) = Pause::new();
        map.insert(
            0,
            ControlledValue {
                pause: Some(pause.clone()),
                ..ControlledValue::plain(10, &clones)
            },
        )
        .await;
        let worker_map = map.clone();
        let worker = PausedWorker::start(pause, move || run(worker_map.shared_snapshot()));
        entered.await.unwrap();
        let mut cancelled = Box::pin(map.shared_snapshot());
        assert!(poll_once(cancelled.as_mut()).is_pending());
        drop(cancelled);
        let mut waiting = Box::pin(map.shared_snapshot());
        assert!(poll_once(waiting.as_mut()).is_pending());
        let first = worker.finish();
        let second = waiting.await;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(clones.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn cancelling_a_builder_releases_partial_shard_guards_and_its_gate() {
    let map = map(SnapshotMode::Cached);
    map.batch_insert([(0, 10), (1, 20)]).await;
    let (pause, entered) = Pause::new();
    let worker_map = map.clone();
    let worker_pause = pause.clone();
    let writer = PausedWorker::start(pause, move || {
        run(worker_map.compute_if_present(&1, |value| {
            worker_pause.wait_if_armed();
            Some(value + 1)
        }))
    });
    entered.await.unwrap();
    let mut cancelled = Box::pin(map.shared_snapshot());
    assert!(poll_once(cancelled.as_mut()).is_pending());
    drop(cancelled);
    let mut write = Box::pin(map.insert(0, 11));
    assert!(matches!(
        poll_once(write.as_mut()),
        std::task::Poll::Ready(Some(10))
    ));
    drop(write);
    writer.finish();
    let snapshot = map.shared_snapshot().await;
    assert!(snapshot.contains(&(0, 11)));
    assert!(snapshot.contains(&(1, 21)));
}

#[tokio::test]
async fn a_clone_panic_releases_builder_and_all_read_guards() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let clones = Arc::new(AtomicUsize::new(0));
        map.insert(0, ControlledValue::plain(10, &clones)).await;
        map.insert(
            1,
            ControlledValue {
                panic: Some(Arc::new(AtomicBool::new(true))),
                ..ControlledValue::plain(20, &clones)
            },
        )
        .await;
        let worker_map = map.clone();
        let failed = tokio::spawn(async move { worker_map.shared_snapshot().await });
        assert!(failed.await.err().unwrap().is_panic());
        map.insert(2, ControlledValue::plain(30, &clones)).await;
        assert_eq!(map.shared_snapshot().await.len(), 3);
    }
}

#[derive(Clone)]
struct PausedDrop {
    value: u64,
    pause: Option<Arc<Pause>>,
}

impl Drop for PausedDrop {
    fn drop(&mut self) {
        if let Some(pause) = &self.pause {
            pause.wait_if_armed();
        }
    }
}

#[tokio::test]
async fn retired_snapshot_destruction_does_not_hold_the_builder_gate() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let (pause, entered) = Pause::new();
        pause.armed.store(false, Ordering::Relaxed);
        map.insert(
            0,
            PausedDrop {
                value: 10,
                pause: Some(pause.clone()),
            },
        )
        .await;
        map.shared_snapshot().await;
        map.insert(
            0,
            PausedDrop {
                value: 20,
                pause: None,
            },
        )
        .await;
        // Only the invalidated cache still owns the value whose Drop pauses.
        pause.armed.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        let worker = PausedWorker::start(pause, move || run(worker_map.shared_snapshot()));
        entered.await.unwrap();
        map.insert(
            1,
            PausedDrop {
                value: 30,
                pause: None,
            },
        )
        .await;
        let mut snapshot = Box::pin(map.shared_snapshot());
        let std::task::Poll::Ready(current) = poll_once(snapshot.as_mut()) else {
            panic!("a retired value destructor blocked the next snapshot build");
        };
        assert_eq!(current.len(), 2);
        assert!(
            current
                .iter()
                .any(|(key, value)| *key == 1 && value.value == 30)
        );
        drop(snapshot);
        let previous = worker.finish();
        assert_eq!(previous.len(), 1);
        assert_eq!(previous[0].1.value, 20);
    }
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn transaction_promotion_waits_for_a_complete_snapshot_of_both_generations() {
    use starshard::{Transaction, TransactionResult};

    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let clones = Arc::new(AtomicUsize::new(0));
        let (pause, entered) = Pause::new();
        pause.armed.store(false, Ordering::Relaxed);
        map.insert(
            0,
            ControlledValue {
                pause: Some(pause.clone()),
                ..ControlledValue::plain(10, &clones)
            },
        )
        .await;
        map.insert(1, ControlledValue::plain(10, &clones)).await;
        map.start_rebalance_online(8).await.unwrap();
        // Promote only one key, leaving an actual two-generation snapshot.
        map.insert(
            0,
            ControlledValue {
                pause: Some(pause.clone()),
                ..ControlledValue::plain(10, &clones)
            },
        )
        .await;
        // Materialize the second destination without promoting key 1, so the
        // transaction must wait on shard guards rather than a cold directory.
        map.insert(9, ControlledValue::plain(0, &clones)).await;
        map.remove(&9).await;
        pause.armed.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        let worker = PausedWorker::start(pause, move || run(worker_map.shared_snapshot()));
        entered.await.unwrap();
        let mut transaction = Transaction::new();
        transaction.write(0, ControlledValue::plain(20, &clones));
        transaction.write(1, ControlledValue::plain(20, &clones));
        let mut transaction = Box::pin(map.execute_transaction(transaction));
        assert!(poll_once(transaction.as_mut()).is_pending());
        let snapshot = worker.finish();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().all(|(_, value)| value.value == 10));
        assert!(matches!(
            transaction.await,
            TransactionResult::Committed(())
        ));
        let current = map.shared_snapshot().await;
        assert_eq!(current.len(), 2);
        assert!(current.iter().all(|(_, value)| value.value == 20));
        assert_eq!(current.iter().filter(|(key, _)| *key == 0).count(), 1);
        assert_eq!(current.iter().filter(|(key, _)| *key == 1).count(), 1);
    }
}

struct CountedKey {
    key: u64,
    hashes: Arc<AtomicUsize>,
    clones: Arc<AtomicUsize>,
}

impl PartialEq for CountedKey {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for CountedKey {}

impl Hash for CountedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hashes.fetch_add(1, Ordering::Relaxed);
        self.key.hash(state);
    }
}

impl Clone for CountedKey {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            key: self.key,
            hashes: self.hashes.clone(),
            clones: self.clones.clone(),
        }
    }
}

#[tokio::test]
async fn migrating_snapshot_projections_do_not_hash_or_clone_unused_columns() {
    let hashes = Arc::new(AtomicUsize::new(0));
    let key_clones = Arc::new(AtomicUsize::new(0));
    let value_clones = Arc::new(AtomicUsize::new(0));
    let map = AsyncShardedHashMap::with_shards_and_hasher(4, Identity::default());
    let key = |key| CountedKey {
        key,
        hashes: hashes.clone(),
        clones: key_clones.clone(),
    };
    for index in 0..8 {
        map.insert(key(index), ControlledValue::plain(index, &value_clones))
            .await;
    }
    map.start_rebalance_online(8).await.unwrap();
    map.insert(key(0), ControlledValue::plain(100, &value_clones))
        .await;
    hashes.store(0, Ordering::Relaxed);
    key_clones.store(0, Ordering::Relaxed);
    value_clones.store(0, Ordering::Relaxed);

    assert_eq!(map.keys().await.len(), 8);
    assert_eq!(key_clones.load(Ordering::Relaxed), 8);
    assert_eq!(value_clones.load(Ordering::Relaxed), 0);
    assert_eq!(map.values().await.len(), 8);
    assert_eq!(key_clones.load(Ordering::Relaxed), 8);
    assert_eq!(value_clones.load(Ordering::Relaxed), 8);
    let entries = map.iter().await;
    assert_eq!(entries.len(), 8);
    assert!(
        entries
            .iter()
            .any(|(key, value)| key.key == 0 && value.value == 100)
    );
    assert_eq!(key_clones.load(Ordering::Relaxed), 16);
    assert_eq!(value_clones.load(Ordering::Relaxed), 16);
    assert_eq!(hashes.load(Ordering::Relaxed), 0);
}
