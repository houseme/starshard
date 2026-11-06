use starshard::{RebalanceError, RebalanceOptions, ShardedHashMap};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
type Map = ShardedHashMap<u64, u64, Identity>;
fn map(shards: usize) -> Map {
    Map::with_shards_and_hasher(shards, Identity::default())
}
fn options(entries: usize, nanoseconds: u64) -> RebalanceOptions {
    RebalanceOptions {
        background: true,
        batch_size: entries,
        max_pause_ns: nanoseconds,
    }
}

#[test]
fn bounded_batches_pause_resume_and_preserve_legacy_shard_counts() {
    let map = map(1);
    map.batch_insert((0..10_000).map(|key| (key, key)));
    map.start_rebalance_online(8).unwrap();
    let batch = map.advance_rebalance_with_options(&options(7, 0));
    assert_eq!(batch.moved_entries, 7);
    assert_eq!(batch.completed_shards, 0);
    assert!(batch.budget_exhausted);
    assert_eq!(map.rebalance_status().moved_shards, 0);
    assert_eq!(map.len(), 10_000);
    assert!(map.pause_rebalance());
    assert_eq!(
        map.advance_rebalance_with_options(&options(7, 0))
            .moved_entries,
        0
    );
    assert_eq!(map.advance_rebalance(1), 0);
    assert!(map.resume_rebalance());
    assert_eq!(map.advance_rebalance(1), 1);
    assert_eq!(map.rebalance_status().state, "idle");
    let telemetry = map.rebalance_telemetry();
    assert_eq!(telemetry.moved_entries, 10_000);
    assert_eq!(telemetry.completed_shards, 1);
    assert!(telemetry.complete);
    assert!(telemetry.batches >= 2);
    for key in 0..10_000 {
        assert_eq!(map.get(&key), Some(key));
    }
}

#[test]
fn cooperative_budget_reports_unavoidable_overrun_and_progress() {
    let map = map(1);
    map.batch_insert((0..32).map(|key| (key, key)));
    map.start_rebalance_online(2).unwrap();
    let batch = map.advance_rebalance_with_options(&options(32, 1));
    assert_eq!(batch.moved_entries, 1);
    assert!(batch.budget_exhausted);
    assert!(batch.elapsed_ns > 1);
    assert_eq!(map.rebalance_telemetry().budget_overruns, 1);
    assert_eq!(map.len(), 32);
    assert_eq!(map.advance_rebalance(1), 1);
}

#[test]
fn supervised_worker_cancels_paused_generation_and_can_restart() {
    let map = map(1);
    map.batch_insert((0..128).map(|key| (key, key)));
    map.start_rebalance_online(4).unwrap();
    map.pause_rebalance();
    let worker = map.spawn_rebalance(4, options(3, 0)).unwrap();
    assert!(matches!(
        map.spawn_rebalance(4, options(3, 0)),
        Err(RebalanceError::WorkerAlreadyRunning)
    ));
    worker.cancel();
    worker.cancel();
    assert!(!worker.join().unwrap().completed);
    assert!(!map.rebalance_telemetry().worker_running);
    assert_eq!(map.len(), 128);
    assert_eq!(map.rebalance_status().state, "migrating");
    let worker = map.spawn_rebalance(4, options(3, 0)).unwrap();
    map.resume_rebalance();
    assert!(worker.join().unwrap().completed);
    assert_eq!(map.iter().count(), 128);
}

#[test]
fn no_op_worker_reports_completed_current_layout() {
    let map = map(4);
    let outcome = map
        .spawn_rebalance(4, options(1, 0))
        .unwrap()
        .join()
        .unwrap();
    assert!(outcome.completed);
    assert_eq!(outcome.report.from_shards, 4);
    assert_eq!(outcome.report.to_shards, 4);
    assert_eq!(outcome.report.moved_entries, 0);
}

#[test]
fn manual_rebalance_supersedes_paused_worker_without_following_new_generation() {
    let map = map(1);
    map.batch_insert((0..64).map(|key| (key, key)));
    map.start_rebalance_online(4).unwrap();
    map.pause_rebalance();
    let generation = map.rebalance_telemetry().generation;
    let worker = map.spawn_rebalance(4, options(1, 0)).unwrap();
    map.rebalance_to(8, RebalanceOptions::default()).unwrap();
    assert!(!worker.join().unwrap().completed);
    assert!(map.rebalance_telemetry().generation > generation);
    assert_eq!(map.shard_count(), 8);
    assert_eq!(map.iter().count(), 64);
}

#[derive(Clone)]
struct FaultKey {
    id: u64,
    panic_hash: Arc<AtomicBool>,
}
impl Hash for FaultKey {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        assert!(
            !self.panic_hash.swap(false, Ordering::Relaxed),
            "injected worker hash panic"
        );
        hasher.write_u64(self.id);
    }
}
impl PartialEq for FaultKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for FaultKey {}

#[test]
fn worker_panic_releases_ownership_and_data_remains_resumable() {
    let panic_hash = Arc::new(AtomicBool::new(false));
    let map =
        ShardedHashMap::<FaultKey, u64, Identity>::with_shards_and_hasher(1, Identity::default());
    map.insert(
        FaultKey {
            id: 1,
            panic_hash: panic_hash.clone(),
        },
        10,
    );
    map.start_rebalance_online(2).unwrap();
    panic_hash.store(true, Ordering::Relaxed);
    let worker = map.spawn_rebalance(2, options(1, 0)).unwrap();
    assert!(matches!(worker.join(), Err(RebalanceError::WorkerPanicked)));
    assert!(!map.rebalance_telemetry().worker_running);
    assert_eq!(map.len(), 1);
    assert!(
        map.spawn_rebalance(2, options(1, 0))
            .unwrap()
            .join()
            .unwrap()
            .completed
    );
    assert_eq!(map.get(&FaultKey { id: 1, panic_hash }), Some(10));
}

#[test]
fn cooperative_rebalance_to_completes_before_returning() {
    let map = map(2);
    map.batch_insert((0..100).map(|key| (key, key)));
    let report = map.rebalance_to(8, options(5, 0)).unwrap();
    assert_eq!(report.moved_entries, 100);
    assert_eq!(map.shard_count(), 8);
    assert_eq!(map.rebalance_status().state, "idle");
    assert!(map.rebalance_telemetry().batches >= 20);
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use starshard::AsyncShardedHashMap;
    type AsyncMap = AsyncShardedHashMap<u64, u64, Identity>;
    fn map(shards: usize) -> AsyncMap {
        AsyncMap::with_shards_and_hasher(shards, Identity::default())
    }
    fn poll_once<F: std::future::Future>(
        future: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<F::Output> {
        future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    }
    #[tokio::test]
    async fn bounded_batches_and_pause_resume_keep_all_entries() {
        let map = map(1);
        map.batch_insert((0..512).map(|key| (key, key))).await;
        map.start_rebalance_online(4).await.unwrap();
        let batch = map.advance_rebalance_with_options(&options(7, 0)).await;
        assert_eq!(batch.moved_entries, 7);
        assert_eq!(batch.completed_shards, 0);
        map.pause_rebalance().await;
        assert_eq!(map.advance_rebalance(1).await, 0);
        map.resume_rebalance().await;
        assert_eq!(map.advance_rebalance(1).await, 1);
        assert_eq!(map.rebalance_telemetry().moved_entries, 512);
        assert_eq!(map.iter().await.len(), 512);
    }
    #[tokio::test]
    async fn no_op_worker_completes_and_paused_worker_can_cancel_restart() {
        let map = map(4);
        let outcome = map
            .spawn_rebalance(4, options(1, 0))
            .await
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(outcome.completed);
        assert_eq!(outcome.report.from_shards, 4);
        map.batch_insert((0..128).map(|key| (key, key))).await;
        map.start_rebalance_online(8).await.unwrap();
        map.pause_rebalance().await;
        let worker = map.spawn_rebalance(8, options(3, 0)).await.unwrap();
        assert!(matches!(
            map.spawn_rebalance(8, options(3, 0)).await,
            Err(RebalanceError::WorkerAlreadyRunning)
        ));
        worker.cancel();
        worker.cancel();
        assert!(!worker.join().await.unwrap().completed);
        let worker = map.spawn_rebalance(8, options(3, 0)).await.unwrap();
        map.resume_rebalance().await;
        assert!(worker.join().await.unwrap().completed);
        assert_eq!(map.iter().await.len(), 128);
    }
    #[tokio::test]
    async fn dropping_join_aborts_worker_waiting_for_routing() {
        use std::sync::{Condvar, Mutex};
        let map = map(1);
        map.insert(0, 10).await;
        map.start_rebalance_online(2).await.unwrap();
        map.pause_rebalance().await;
        let worker = map.spawn_rebalance(2, options(1, 0)).await.unwrap();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let gate = release.clone();
        let held_map = map.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let holder = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(held_map.compute_if_present(&0, |value| {
                    entered_tx.send(()).unwrap();
                    let (lock, changed) = &*gate;
                    let mut ready = lock.lock().unwrap();
                    while !*ready {
                        ready = changed.wait(ready).unwrap();
                    }
                    Some(value)
                }));
        });
        entered_rx.await.unwrap();
        map.resume_rebalance().await;
        tokio::task::yield_now().await;
        let mut join = Box::pin(worker.join());
        assert!(poll_once(join.as_mut()).is_pending());
        drop(join);
        for _ in 0..20 {
            if !map.rebalance_telemetry().worker_running {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stopped = !map.rebalance_telemetry().worker_running;
        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_one();
        holder.join().unwrap();
        assert!(
            stopped,
            "dropping join must abort even while routing is held elsewhere"
        );
        assert_eq!(map.get(&0).await, Some(10));
        map.advance_rebalance(1).await;
    }
    #[tokio::test]
    async fn cancelled_partial_batch_can_resume_and_preserves_lengths() {
        let map = map(1);
        map.batch_insert((0..1024).map(|key| (key, key))).await;
        map.start_rebalance_online(4).await.unwrap();
        tokio::task::yield_now().await;
        let config = options(1024, 0);
        let mut batch = Box::pin(map.advance_rebalance_with_options(&config));
        assert!(poll_once(batch.as_mut()).is_pending());
        drop(batch);
        assert_eq!(map.len().await, 1024);
        assert_eq!(map.iter().await.len(), 1024);
        map.advance_rebalance(1).await;
        assert_eq!(map.rebalance_status().state, "idle");
        assert_eq!(map.rebalance_telemetry().moved_entries, 1024);
    }
    #[test]
    fn missing_runtime_returns_typed_error_without_starting_migration() {
        let map = map(1);
        let mut spawn = Box::pin(map.spawn_rebalance(2, options(1, 0)));
        assert!(matches!(
            poll_once(spawn.as_mut()),
            std::task::Poll::Ready(Err(RebalanceError::RuntimeUnavailable))
        ));
        assert_eq!(map.shard_count(), 1);
    }
}

struct CloneFaultHasher {
    armed: Arc<AtomicBool>,
}
impl Clone for CloneFaultHasher {
    fn clone(&self) -> Self {
        assert!(
            !self.armed.swap(false, Ordering::Relaxed),
            "injected hasher clone panic"
        );
        Self {
            armed: self.armed.clone(),
        }
    }
}
impl std::hash::BuildHasher for CloneFaultHasher {
    type Hasher = IdentityHasher;
    fn build_hasher(&self) -> Self::Hasher {
        IdentityHasher::default()
    }
}

#[test]
fn hasher_clone_panic_does_not_claim_sync_worker() {
    let armed = Arc::new(AtomicBool::new(false));
    let map = ShardedHashMap::<u64, u64, CloneFaultHasher>::with_shards_and_hasher(
        1,
        CloneFaultHasher {
            armed: armed.clone(),
        },
    );
    map.insert(1, 10);
    armed.store(true, Ordering::Relaxed);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            map.spawn_rebalance(2, options(1, 0))
        }))
        .is_err()
    );
    assert!(!map.rebalance_telemetry().worker_running);
    assert_eq!(map.shard_count(), 1);
    assert!(
        map.spawn_rebalance(2, options(1, 0))
            .unwrap()
            .join()
            .unwrap()
            .completed
    );
    assert_eq!(map.get(&1), Some(10));
}

#[cfg(feature = "async")]
#[tokio::test]
async fn hasher_clone_panic_does_not_claim_async_worker() {
    let armed = Arc::new(AtomicBool::new(false));
    let map = starshard::AsyncShardedHashMap::<u64, u64, CloneFaultHasher>::with_shards_and_hasher(
        1,
        CloneFaultHasher {
            armed: armed.clone(),
        },
    );
    map.insert(1, 10).await;
    let attempting = map.clone();
    armed.store(true, Ordering::Relaxed);
    let attempt = tokio::spawn(async move {
        let worker = attempting.spawn_rebalance(2, options(1, 0)).await;
        assert!(worker.is_ok());
    });
    assert!(attempt.await.unwrap_err().is_panic());
    assert!(!map.rebalance_telemetry().worker_running);
    assert_eq!(map.shard_count(), 1);
    assert!(
        map.spawn_rebalance(2, options(1, 0))
            .await
            .unwrap()
            .join()
            .await
            .unwrap()
            .completed
    );
    assert_eq!(map.get(&1).await, Some(10));
}

struct TimedClone {
    clone_ns: Arc<std::sync::atomic::AtomicU64>,
    fail: Arc<AtomicBool>,
}
impl Clone for TimedClone {
    fn clone(&self) -> Self {
        let started = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        self.clone_ns
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        assert!(
            !self.fail.load(Ordering::Relaxed),
            "injected staging clone panic"
        );
        Self {
            clone_ns: self.clone_ns.clone(),
            fail: self.fail.clone(),
        }
    }
}

fn assert_staged_telemetry(map_telemetry: starshard::RebalanceTelemetry, clone_ns: u64) {
    assert!(clone_ns > 0);
    assert!(map_telemetry.elapsed_ns >= clone_ns);
    assert!(map_telemetry.last_pause_ns >= clone_ns);
    assert_eq!(map_telemetry.last_pause_ns, map_telemetry.max_pause_ns);
    assert_eq!(map_telemetry.elapsed_ns, map_telemetry.last_pause_ns);
    assert_eq!(map_telemetry.batches, 1);
    assert_eq!(map_telemetry.moved_entries, 1);
    assert_eq!(map_telemetry.completed_shards, 1);
    assert_eq!(map_telemetry.from_shards, 1);
    assert_eq!(map_telemetry.to_shards, 2);
    assert!(map_telemetry.complete);
}

#[test]
fn staged_sync_telemetry_includes_clone_time_and_ignores_failed_staging() {
    let clone_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fail = Arc::new(AtomicBool::new(false));
    let map = ShardedHashMap::new(1);
    map.insert(
        0,
        TimedClone {
            clone_ns: clone_ns.clone(),
            fail: fail.clone(),
        },
    );
    let previous = map.rebalance_telemetry();
    fail.store(true, Ordering::Relaxed);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            map.rebalance_to(2, RebalanceOptions::default())
        }))
        .is_err()
    );
    assert_eq!(map.rebalance_telemetry(), previous);
    assert_eq!(map.shard_count(), 1);
    fail.store(false, Ordering::Relaxed);
    clone_ns.store(0, Ordering::Relaxed);
    let report = map.rebalance_to(2, RebalanceOptions::default()).unwrap();
    assert_eq!(report.moved_entries, 1);
    assert_staged_telemetry(map.rebalance_telemetry(), clone_ns.load(Ordering::Relaxed));
}

#[cfg(feature = "async")]
#[tokio::test]
async fn staged_async_telemetry_includes_clone_time_and_ignores_failed_staging() {
    let clone_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fail = Arc::new(AtomicBool::new(false));
    let map = starshard::AsyncShardedHashMap::new(1);
    map.insert(
        0,
        TimedClone {
            clone_ns: clone_ns.clone(),
            fail: fail.clone(),
        },
    )
    .await;
    let previous = map.rebalance_telemetry();
    fail.store(true, Ordering::Relaxed);
    let attempting = map.clone();
    assert!(
        tokio::spawn(async move {
            attempting
                .rebalance_to(2, RebalanceOptions::default())
                .await
        })
        .await
        .unwrap_err()
        .is_panic()
    );
    assert_eq!(map.rebalance_telemetry(), previous);
    assert_eq!(map.shard_count(), 1);
    fail.store(false, Ordering::Relaxed);
    clone_ns.store(0, Ordering::Relaxed);
    let report = map
        .rebalance_to(2, RebalanceOptions::default())
        .await
        .unwrap();
    assert_eq!(report.moved_entries, 1);
    assert_staged_telemetry(map.rebalance_telemetry(), clone_ns.load(Ordering::Relaxed));
}

#[cfg(feature = "async")]
#[test]
fn runtime_shutdown_reports_cancelled_and_releases_unpolled_worker() {
    let first_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (map, worker) = first_runtime.block_on(async {
        let map = starshard::AsyncShardedHashMap::<u64, u64>::new(1);
        map.insert(1, 10).await;
        let worker = map.spawn_rebalance(2, options(1, 0)).await.unwrap();
        // No yield follows spawn: the current-thread runtime has not polled
        // the worker, but its captured lease must still be released on shutdown.
        (map, worker)
    });
    assert!(map.rebalance_telemetry().worker_running);
    drop(first_runtime);
    assert!(!map.rebalance_telemetry().worker_running);

    let second_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    second_runtime.block_on(async {
        assert!(matches!(
            worker.join().await,
            Err(RebalanceError::WorkerCancelled)
        ));
        assert_eq!(map.rebalance_status().state, "migrating");
        assert_eq!(map.get(&1).await, Some(10));
        assert_eq!(map.len().await, 1);
        let resumed = map.spawn_rebalance(2, options(1, 0)).await.unwrap();
        assert!(resumed.join().await.unwrap().completed);
        assert_eq!(map.rebalance_status().state, "idle");
        assert!(!map.rebalance_telemetry().worker_running);
        assert_eq!(map.iter().await, vec![(1, 10)]);
    });
}

struct CloneCountedValue {
    number: u64,
    clones: Arc<std::sync::atomic::AtomicUsize>,
}
impl Clone for CloneCountedValue {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            number: self.number,
            clones: self.clones.clone(),
        }
    }
}

#[test]
fn uniform_bounded_migration_reserves_empty_targets_without_value_clones() {
    for mode in [
        starshard::SnapshotMode::Clone,
        starshard::SnapshotMode::Cached,
        starshard::SnapshotMode::Cow,
    ] {
        let clones = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let map =
            ShardedHashMap::with_shards_and_hasher_and_snapshot_mode(32, Identity::default(), mode);
        map.batch_insert((0_u64..16_384).map(|key| {
            (
                key,
                CloneCountedValue {
                    number: key,
                    clones: clones.clone(),
                },
            )
        }));
        clones.store(0, Ordering::Relaxed);
        map.start_rebalance_online(64).unwrap();
        while map.rebalance_status().state == "migrating" {
            let batch = map.advance_rebalance_with_options(&options(31, 0));
            assert!(batch.moved_entries <= 31);
        }
        assert_eq!(clones.load(Ordering::Relaxed), 0, "{mode:?}");
        assert_eq!(map.len(), 16_384);
        assert_eq!(map.rebalance_telemetry().moved_entries, 16_384);
        assert_eq!(map.read_with(&0_u64, |value| value.number), Some(0));
        assert_eq!(
            map.read_with(&16_383_u64, |value| value.number),
            Some(16_383)
        );
    }
}

#[cfg(feature = "async")]
#[tokio::test]
async fn uniform_async_bounded_migration_reserves_empty_targets_without_value_clones() {
    for mode in [
        starshard::SnapshotMode::Clone,
        starshard::SnapshotMode::Cached,
        starshard::SnapshotMode::Cow,
    ] {
        let clones = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let map = starshard::AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
            32,
            Identity::default(),
            mode,
        );
        map.batch_insert((0_u64..16_384).map(|key| {
            (
                key,
                CloneCountedValue {
                    number: key,
                    clones: clones.clone(),
                },
            )
        }))
        .await;
        clones.store(0, Ordering::Relaxed);
        map.start_rebalance_online(64).await.unwrap();
        while map.rebalance_status().state == "migrating" {
            let batch = map.advance_rebalance_with_options(&options(31, 0)).await;
            assert!(batch.moved_entries <= 31);
        }
        assert_eq!(clones.load(Ordering::Relaxed), 0, "{mode:?}");
        assert_eq!(map.len().await, 16_384);
        assert_eq!(map.rebalance_telemetry().moved_entries, 16_384);
        assert_eq!(map.read_with(&0_u64, |value| value.number).await, Some(0));
        assert_eq!(
            map.read_with(&16_383_u64, |value| value.number).await,
            Some(16_383)
        );
    }
}
