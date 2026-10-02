#![cfg(feature = "async")]

use starshard::{AsyncShardedHashMap, RebalanceOptions, SnapshotMode};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
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
    Map::with_shards_and_hasher_and_snapshot_mode(2, Identity::default(), mode)
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
    fn new(armed: bool) -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (entered, receiver) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                armed: AtomicBool::new(armed),
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
}

/// Release a deliberately paused worker even when an assertion fails, so these
/// tests fail instead of hanging when a snapshot unexpectedly needs routing.write.
struct PausedWorker {
    pause: Arc<Pause>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PausedWorker {
    fn start(pause: Arc<Pause>, operation: impl FnOnce() + Send + 'static) -> Self {
        Self {
            pause,
            worker: Some(std::thread::spawn(operation)),
        }
    }
}

impl Drop for PausedWorker {
    fn drop(&mut self) {
        *self.pause.released.lock().unwrap() = true;
        self.pause.changed.notify_one();
        self.worker.take().unwrap().join().unwrap();
    }
}

async fn hold_writer(map: Map) -> PausedWorker {
    let (pause, entered) = Pause::new(true);
    let worker_pause = pause.clone();
    let worker = PausedWorker::start(pause, move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(map.compute_if_present(&0, |value| {
                worker_pause.wait_if_armed();
                Some(value + 1)
            }));
    });
    entered.await.unwrap();
    worker
}

#[tokio::test]
async fn cached_snapshot_hits_complete_while_a_writer_holds_shared_routing() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        map.batch_insert([(0, 10), (1, 20)]).await;
        let cached = map.shared_snapshot().await;
        let writer = hold_writer(map.clone()).await;

        let mut snapshot = Box::pin(map.shared_snapshot());
        let std::task::Poll::Ready(hit) = poll_once(snapshot.as_mut()) else {
            panic!("a valid cached snapshot waited for an unrelated writer");
        };
        assert!(Arc::ptr_eq(&cached, &hit));
        drop(snapshot);

        let mut entries = Box::pin(map.iter());
        let std::task::Poll::Ready(entries) = poll_once(entries.as_mut()) else {
            panic!("a cached owned snapshot waited for an unrelated writer");
        };
        assert_eq!(entries, *cached);

        drop(writer);
        let current = map.shared_snapshot().await;
        assert!(!Arc::ptr_eq(&cached, &current));
        assert!(current.contains(&(0, 11)));
        assert!(cached.contains(&(0, 10)));
    }
}

#[tokio::test]
async fn invalidated_snapshot_waits_for_writers_and_can_be_cancelled() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        map.batch_insert([(0, 10), (1, 20)]).await;
        let cached = map.shared_snapshot().await;
        let writer = hold_writer(map.clone()).await;
        // This disjoint write completes while the first writer still holds
        // shared routing, making the previously published cache obsolete.
        map.insert(1, 30).await;
        let mut snapshot = Box::pin(map.shared_snapshot());
        assert!(poll_once(snapshot.as_mut()).is_pending());
        drop(snapshot);
        drop(writer);

        let current = map.shared_snapshot().await;
        assert!(current.contains(&(0, 11)));
        assert!(current.contains(&(1, 30)));
        assert!(!Arc::ptr_eq(&cached, &current));
        assert!(Arc::ptr_eq(&current, &map.shared_snapshot().await));
    }
}

#[tokio::test]
async fn repeated_cached_snapshots_allow_other_tasks_to_progress() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        map.insert(0, 10).await;
        map.shared_snapshot().await;
        let (start, started) = tokio::sync::oneshot::channel();
        let (done, mut completed) = tokio::sync::oneshot::channel();
        let writer_map = map.clone();
        let writer = tokio::spawn(async move {
            started.await.unwrap();
            writer_map.insert(1, 20).await;
            done.send(()).unwrap();
        });
        start.send(()).unwrap();

        // This current-thread task never explicitly yields. Cached snapshots
        // must preserve Tokio's cooperative budget so the writer gets to run.
        let mut progressed = false;
        for _ in 0..4096 {
            map.shared_snapshot().await;
            if completed.try_recv().is_ok() {
                progressed = true;
                break;
            }
        }
        assert!(progressed, "ready cached snapshots starved the writer");
        writer.await.unwrap();
        assert_eq!(map.get(&1).await, Some(20));
    }
}

struct PausedClone {
    value: u64,
    pause: Option<Arc<Pause>>,
}

impl Clone for PausedClone {
    fn clone(&self) -> Self {
        if let Some(pause) = &self.pause {
            pause.wait_if_armed();
        }
        Self {
            value: self.value,
            pause: self.pause.clone(),
        }
    }
}

#[tokio::test]
async fn valid_cached_snapshots_bypass_exclusive_routing_but_stale_ones_wait() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        for invalidated in [false, true] {
            let map = map(mode);
            let (pause, entered) = Pause::new(false);
            map.insert(
                0,
                PausedClone {
                    value: 10,
                    pause: Some(pause.clone()),
                },
            )
            .await;
            let cached = map.shared_snapshot().await;
            if invalidated {
                map.insert(
                    1,
                    PausedClone {
                        value: 20,
                        pause: None,
                    },
                )
                .await;
            }
            // Rebalancing holds routing exclusively while cloning existing
            // values. Its directory changes leave the logical entries intact.
            pause.armed.store(true, Ordering::Relaxed);
            let worker_map = map.clone();
            let worker = PausedWorker::start(pause, move || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(worker_map.rebalance_to(4, RebalanceOptions::default()))
                    .unwrap();
            });
            entered.await.unwrap();

            let mut snapshot = Box::pin(map.shared_snapshot());
            let result = poll_once(snapshot.as_mut());
            if invalidated {
                assert!(result.is_pending());
            } else {
                let std::task::Poll::Ready(hit) = result else {
                    panic!("valid immutable cache should not wait for a routing change");
                };
                assert!(Arc::ptr_eq(&cached, &hit));
            }
            drop(snapshot);
            drop(worker);

            let current = map.shared_snapshot().await;
            assert_eq!(current.len(), 1 + usize::from(invalidated));
            assert!(
                current
                    .iter()
                    .any(|(key, value)| *key == 0 && value.value == 10)
            );
            if invalidated {
                assert!(
                    current
                        .iter()
                        .any(|(key, value)| *key == 1 && value.value == 20)
                );
            }
        }
    }
}

#[tokio::test]
async fn owned_cached_snapshot_clones_after_releasing_routing() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let (pause, entered) = Pause::new(false);
        map.insert(
            0,
            PausedClone {
                value: 10,
                pause: Some(pause.clone()),
            },
        )
        .await;
        let cached = map.shared_snapshot().await;
        pause.armed.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        let worker = PausedWorker::start(pause, move || {
            let entries = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(worker_map.iter());
            assert_eq!(entries[0].1.value, 10);
        });
        entered.await.unwrap();

        let mut write = Box::pin(map.insert(
            0,
            PausedClone {
                value: 20,
                pause: None,
            },
        ));
        assert!(poll_once(write.as_mut()).is_ready());
        drop(write);
        drop(worker);
        assert_eq!(cached[0].1.value, 10);
        assert_eq!(map.get(&0).await.unwrap().value, 20);
    }
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn all_versioned_snapshot_hits_share_the_validated_epoch() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        map.batch_insert([(0, 10), (1, 20)]).await;
        let version = map.versioned_snapshot().await.version();
        let writer = hold_writer(map.clone()).await;

        let mut cow = Box::pin(map.cow_snapshot());
        let std::task::Poll::Ready(cow) = poll_once(cow.as_mut()) else {
            panic!("a valid COW snapshot waited for an unrelated writer");
        };
        assert_eq!(cow.version(), version);
        assert!(cow.iter().any(|entry| *entry == (0, 10)));

        let mut isolated = Box::pin(map.versioned_snapshot());
        let std::task::Poll::Ready(isolated) = poll_once(isolated.as_mut()) else {
            panic!("a valid versioned snapshot waited for an unrelated writer");
        };
        assert_eq!(isolated.version(), version);

        let mut requested = Box::pin(map.snapshot_at_version(version));
        let std::task::Poll::Ready(Some(requested)) = poll_once(requested.as_mut()) else {
            panic!("a valid requested snapshot waited for an unrelated writer");
        };
        assert_eq!(requested.version(), version);

        drop(writer);
        assert!(map.snapshot_at_version(version).await.is_none());
        let current = map.cow_snapshot().await;
        assert_eq!(current.version(), version + 1);
        assert!(current.iter().any(|entry| *entry == (0, 11)));
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
async fn retired_cached_snapshot_is_destroyed_after_releasing_routing() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let (pause, entered) = Pause::new(false);
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
        // Only the invalidated cache still owns a value using this pause.
        pause.armed.store(true, Ordering::Relaxed);
        let worker_map = map.clone();
        let worker = PausedWorker::start(pause, move || {
            let snapshot = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(worker_map.shared_snapshot());
            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot[0].1.value, 20);
        });
        entered.await.unwrap();

        let mut write = Box::pin(map.insert(
            1,
            PausedDrop {
                value: 30,
                pause: None,
            },
        ));
        assert!(poll_once(write.as_mut()).is_ready());
        drop(write);
        drop(worker);

        let current = map.shared_snapshot().await;
        assert!(
            current
                .iter()
                .any(|(key, value)| *key == 1 && value.value == 30)
        );
    }
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn cached_snapshots_never_publish_partial_transaction_data() {
    use starshard::{Transaction, TransactionResult};

    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        for blocked_key in 0..2 {
            let map = map(mode);
            let (pause, entered) = Pause::new(false);
            for key in 0..2 {
                map.insert(
                    key,
                    PausedDrop {
                        value: 0,
                        pause: (key == blocked_key).then(|| pause.clone()),
                    },
                )
                .await;
            }
            let cached = map.cow_snapshot().await;
            pause.armed.store(true, Ordering::Relaxed);
            let worker_map = map.clone();
            let worker = PausedWorker::start(pause, move || {
                let mut transaction = Transaction::new();
                for key in 0..2 {
                    transaction.write(
                        key,
                        PausedDrop {
                            value: 1,
                            pause: None,
                        },
                    );
                }
                let result = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(worker_map.execute_transaction(transaction));
                assert!(matches!(result, TransactionResult::Committed(())));
            });
            entered.await.unwrap();

            let mut snapshot = Box::pin(map.cow_snapshot());
            let result = poll_once(snapshot.as_mut());
            if blocked_key == 0 {
                let std::task::Poll::Ready(snapshot) = result else {
                    panic!("cache should remain valid before the first mutation publication");
                };
                assert_eq!(snapshot.version(), cached.version());
                assert!(snapshot.iter().all(|(_, value)| value.value == 0));
            } else {
                assert!(result.is_pending());
            }
            drop(snapshot);
            drop(worker);

            let current = map.cow_snapshot().await;
            assert_eq!(current.version(), cached.version() + 2);
            assert!(current.iter().all(|(_, value)| value.value == 1));
        }
    }
}

#[cfg(feature = "advanced")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_cached_snapshot_versions_match_their_data() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        map.insert(0, 0).await;
        let writer_map = map.clone();
        let writer = tokio::spawn(async move {
            for value in 1..=256 {
                writer_map.insert(0, value).await;
                tokio::task::yield_now().await;
            }
        });
        for _ in 0..256 {
            let snapshot = map.cow_snapshot().await;
            let (_, value) = snapshot.iter().next().unwrap();
            assert_eq!(snapshot.version(), value + 1);
            tokio::task::yield_now().await;
        }
        writer.await.unwrap();
        let current = map.cow_snapshot().await;
        assert_eq!(current.version(), 257);
        assert_eq!(current.iter().next(), Some(&(0, 256)));
    }
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn repeated_obsolete_version_queries_allow_other_tasks_to_progress() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        map.insert(0, 10).await;
        let obsolete = map.versioned_snapshot().await.version();
        map.insert(1, 20).await;
        let (start, started) = tokio::sync::oneshot::channel();
        let (done, mut completed) = tokio::sync::oneshot::channel();
        let writer_map = map.clone();
        let writer = tokio::spawn(async move {
            started.await.unwrap();
            writer_map.insert(2, 30).await;
            done.send(()).unwrap();
        });
        start.send(()).unwrap();

        // Fast rejection never acquires routing or rebuilds a snapshot, but it
        // must still cooperate with another task on the current-thread runtime.
        let mut progressed = false;
        for _ in 0..4096 {
            assert!(map.snapshot_at_version(obsolete).await.is_none());
            if completed.try_recv().is_ok() {
                progressed = true;
                break;
            }
        }
        assert!(progressed, "obsolete-version queries starved the writer");
        writer.await.unwrap();
        assert_eq!(map.get(&2).await, Some(30));
    }
}

#[cfg(feature = "advanced")]
#[tokio::test]
async fn rejecting_an_obsolete_snapshot_version_does_not_clone_values() {
    use std::sync::atomic::AtomicUsize;

    struct Counted(Arc<AtomicUsize>);

    impl Clone for Counted {
        fn clone(&self) -> Self {
            self.0.fetch_add(1, Ordering::Relaxed);
            Self(self.0.clone())
        }
    }

    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let clones = Arc::new(AtomicUsize::new(0));
        map.insert(0, Counted(clones.clone())).await;
        let version = map.versioned_snapshot().await.version();
        map.insert(1, Counted(clones.clone())).await;
        clones.store(0, Ordering::Relaxed);
        assert!(map.snapshot_at_version(version).await.is_none());
        assert_eq!(clones.load(Ordering::Relaxed), 0);
    }
}
