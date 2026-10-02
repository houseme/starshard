use starshard::{ShardedHashMap, SnapshotMode};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(3);

type Gate = Arc<(Mutex<bool>, Condvar)>;

struct Release(Gate);
impl Release {
    fn pair() -> (Self, Gate) {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        (Self(state.clone()), state)
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
fn wait_for_release(state: &Gate) {
    let mut released = state.0.lock().unwrap();
    while !*released {
        released = state.1.wait(released).unwrap();
    }
}

#[test]
fn valid_cached_snapshot_completes_while_an_uncommitted_writer_holds_routing() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = ShardedHashMap::with_snapshot_mode(4, mode);
        map.insert(1, 10);
        let expected = map.shared_snapshot();
        let (release, gate) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::channel();
        let writer_map = map.clone();
        let writer = std::thread::spawn(move || {
            writer_map.compute_if_present(&1, |_| {
                entered_tx.send(()).unwrap();
                wait_for_release(&gate);
                Some(20)
            })
        });
        entered_rx.recv_timeout(WAIT).unwrap();
        let (result_tx, result_rx) = mpsc::channel();
        let reader_map = map.clone();
        let reader = std::thread::spawn(move || {
            let _ = result_tx.send(reader_map.shared_snapshot());
        });
        let snapshot = result_rx
            .recv_timeout(WAIT)
            .expect("a cache hit must not wait for exclusive routing");
        assert!(Arc::ptr_eq(&expected, &snapshot));
        assert_eq!(snapshot.as_slice(), &[(1, 10)]);
        drop(release);
        writer.join().unwrap();
        reader.join().unwrap();
        assert_eq!(map.shared_snapshot().as_slice(), &[(1, 20)]);
    }
}

#[test]
fn invalid_cached_snapshot_waits_for_the_writer_and_rebuilds() {
    let map = ShardedHashMap::with_snapshot_mode(4, SnapshotMode::Cached);
    map.insert(1, 10);
    let _old = map.shared_snapshot();
    map.insert(2, 30);
    let (release, gate) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::channel();
    let writer_map = map.clone();
    let writer = std::thread::spawn(move || {
        writer_map.compute_if_present(&1, |_| {
            entered_tx.send(()).unwrap();
            wait_for_release(&gate);
            Some(20)
        })
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    let (result_tx, result_rx) = mpsc::channel();
    let reader_map = map.clone();
    let reader = std::thread::spawn(move || {
        let _ = result_tx.send(reader_map.shared_snapshot());
    });
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(release);
    writer.join().unwrap();
    let snapshot = result_rx.recv_timeout(WAIT).unwrap();
    reader.join().unwrap();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(
        snapshot
            .iter()
            .find(|(key, _)| *key == 1)
            .map(|(_, value)| *value),
        Some(20)
    );
}

#[test]
fn owned_cached_iteration_clones_values_after_releasing_routing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Value {
        value: usize,
        armed: Arc<AtomicBool>,
        gate: Gate,
        entered: mpsc::Sender<()>,
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            if self.armed.swap(false, Ordering::Relaxed) {
                self.entered.send(()).unwrap();
                wait_for_release(&self.gate);
            }
            Self {
                value: self.value,
                armed: self.armed.clone(),
                gate: self.gate.clone(),
                entered: self.entered.clone(),
            }
        }
    }
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    let (release, gate) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::channel();
    let armed = Arc::new(AtomicBool::new(false));
    map.insert(
        1,
        Value {
            value: 10,
            armed: armed.clone(),
            gate: gate.clone(),
            entered: entered_tx.clone(),
        },
    );
    let _cached = map.shared_snapshot();
    armed.store(true, Ordering::Relaxed);
    let reader_map = map.clone();
    let reader = std::thread::spawn(move || {
        reader_map
            .iter()
            .map(|(_, value)| value.value)
            .collect::<Vec<_>>()
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    let (written_tx, written_rx) = mpsc::channel();
    let writer_map = map.clone();
    let writer = std::thread::spawn(move || {
        writer_map.insert(
            2,
            Value {
                value: 20,
                armed,
                gate,
                entered: entered_tx,
            },
        );
        let _ = written_tx.send(());
    });
    written_rx
        .recv_timeout(WAIT)
        .expect("copying an immutable snapshot must not block a writer");
    drop(release);
    assert_eq!(reader.join().unwrap(), vec![10]);
    writer.join().unwrap();
    assert_eq!(map.len(), 2);
}

#[cfg(feature = "advanced")]
#[test]
fn version_queries_reject_stale_epochs_without_cloning_data() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Value(Arc<AtomicUsize>);
    impl Clone for Value {
        fn clone(&self) -> Self {
            self.0.fetch_add(1, Ordering::Relaxed);
            Self(self.0.clone())
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    map.insert(1, Value(counter.clone()));
    let old = map.versioned_snapshot();
    map.insert(2, Value(counter.clone()));
    counter.store(0, Ordering::Relaxed);
    assert!(map.snapshot_at_version(old.version()).is_none());
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

#[cfg(feature = "advanced")]
#[test]
fn cached_transaction_snapshots_never_mix_two_committed_keys() {
    use starshard::Transaction;
    let map = ShardedHashMap::with_snapshot_mode(16, SnapshotMode::Cached);
    map.batch_insert([(1, 0), (2, 0)]);
    let _ = map.shared_snapshot();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for value in 1..400 {
                let mut txn = Transaction::new();
                txn.write(1, value);
                txn.write(2, value);
                map.execute_transaction(txn);
            }
        });
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..400 {
                    let snapshot = map.shared_snapshot();
                    let first = snapshot.iter().find(|(key, _)| *key == 1).unwrap().1;
                    let second = snapshot.iter().find(|(key, _)| *key == 2).unwrap().1;
                    assert_eq!(first, second);
                }
            });
        }
    });
}

#[test]
fn retired_cached_values_are_destroyed_after_releasing_routing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    type DropPause = (Arc<AtomicBool>, Gate, mpsc::Sender<()>);
    #[derive(Clone)]
    struct Value {
        pause: Option<DropPause>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            if let Some((armed, gate, entered)) = &self.pause
                && armed.swap(false, Ordering::Relaxed)
            {
                let _ = entered.send(());
                wait_for_release(gate);
            }
        }
    }
    let map = ShardedHashMap::with_snapshot_mode(2, SnapshotMode::Cached);
    let (release, gate) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::channel();
    let armed = Arc::new(AtomicBool::new(false));
    map.insert(
        1,
        Value {
            pause: Some((armed.clone(), gate, entered_tx)),
        },
    );
    drop(map.shared_snapshot());
    drop(map.insert(1, Value { pause: None }));
    armed.store(true, Ordering::Relaxed);
    let capturing_map = map.clone();
    let capture = std::thread::spawn(move || capturing_map.shared_snapshot());
    entered_rx.recv_timeout(WAIT).unwrap();
    let (written_tx, written_rx) = mpsc::channel();
    let writer_map = map.clone();
    let writer = std::thread::spawn(move || {
        writer_map.insert(2, Value { pause: None });
        let _ = written_tx.send(());
    });
    written_rx
        .recv_timeout(WAIT)
        .expect("retiring snapshot data must not block routing");
    drop(release);
    capture.join().unwrap();
    writer.join().unwrap();
}

#[test]
fn cached_snapshot_hits_bypass_topology_only_exclusive_routing() {
    use starshard::RebalanceOptions;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[derive(Clone)]
    struct Pause {
        armed: Arc<AtomicBool>,
        gate: Gate,
        entered: mpsc::Sender<()>,
    }
    struct Value {
        value: u64,
        pause: Pause,
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            if self.pause.armed.swap(false, Ordering::Relaxed) {
                self.pause.entered.send(()).unwrap();
                wait_for_release(&self.pause.gate);
            }
            Self {
                value: self.value,
                pause: self.pause.clone(),
            }
        }
    }
    for stale in [false, true] {
        let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
        let (release, gate) = Release::pair();
        let (entered, waiting) = mpsc::channel();
        let pause = Pause {
            armed: Arc::new(AtomicBool::new(false)),
            gate,
            entered,
        };
        map.insert(
            0,
            Value {
                value: 10,
                pause: pause.clone(),
            },
        );
        let cached = map.shared_snapshot();
        if stale {
            map.insert(
                1,
                Value {
                    value: 20,
                    pause: pause.clone(),
                },
            );
        }
        pause.armed.store(true, Ordering::Relaxed);
        let migration_map = map.clone();
        let migration = std::thread::spawn(move || {
            migration_map
                .rebalance_to(2, RebalanceOptions::default())
                .unwrap()
        });
        waiting.recv_timeout(WAIT).unwrap();
        let (result, received) = mpsc::channel();
        let snapshot_map = map.clone();
        let snapshot = std::thread::spawn(move || {
            let _ = result.send(snapshot_map.shared_snapshot());
        });
        if stale {
            assert!(matches!(
                received.recv_timeout(Duration::from_millis(30)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
        } else {
            let hit = received
                .recv_timeout(WAIT)
                .expect("valid immutable cache must not wait for directory migration");
            assert!(Arc::ptr_eq(&cached, &hit));
        }
        drop(release);
        migration.join().unwrap();
        if stale {
            assert_eq!(received.recv_timeout(WAIT).unwrap().len(), 2);
        }
        snapshot.join().unwrap();
        assert_eq!(map.shard_count(), 2);
    }
}
