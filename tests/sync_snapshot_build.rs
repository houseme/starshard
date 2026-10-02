use starshard::{ShardedHashMap, SnapshotMode};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(3);
#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0, |n, byte| {
            n.wrapping_mul(256).wrapping_add(u64::from(*byte))
        });
    }
    fn write_usize(&mut self, value: usize) {
        self.0 = value as u64;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
type Identity = BuildHasherDefault<IdentityHasher>;
struct Gate {
    armed: AtomicBool,
    entered: mpsc::Sender<()>,
    released: Mutex<bool>,
    ready: Condvar,
}
impl Gate {
    fn wait(&self) {
        if self.armed.swap(false, Ordering::Relaxed) {
            let _ = self.entered.send(());
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.ready.wait(released).unwrap();
            }
        }
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.ready.notify_all();
    }
}
fn gate() -> (Arc<Gate>, Release, mpsc::Receiver<()>) {
    let (entered, receiver) = mpsc::channel();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(true),
        entered,
        released: Mutex::new(false),
        ready: Condvar::new(),
    });
    (gate.clone(), Release(gate), receiver)
}
struct Value {
    number: usize,
    gate: Option<Arc<Gate>>,
    copies: Arc<AtomicUsize>,
}
impl Clone for Value {
    fn clone(&self) -> Self {
        self.copies.fetch_add(1, Ordering::Relaxed);
        if let Some(gate) = &self.gate {
            gate.wait();
        }
        Self {
            number: self.number,
            gate: self.gate.clone(),
            copies: self.copies.clone(),
        }
    }
}

#[test]
fn rebuilding_allows_ordinary_reads_and_freezes_existing_and_cold_writes() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map =
            ShardedHashMap::<usize, Value, Identity>::with_shards_and_hasher_and_snapshot_mode(
                4,
                Identity::default(),
                mode,
            );
        let copies = Arc::new(AtomicUsize::new(0));
        let (gate, release, entered) = gate();
        map.insert(
            0,
            Value {
                number: 10,
                gate: Some(gate),
                copies: copies.clone(),
            },
        );
        map.insert(
            1,
            Value {
                number: 20,
                gate: None,
                copies: copies.clone(),
            },
        );
        let builder_map = map.clone();
        let builder = std::thread::spawn(move || builder_map.shared_snapshot());
        entered.recv_timeout(WAIT).unwrap();
        let reader_map = map.clone();
        let (read, read_result) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = read.send(reader_map.read_with(&1, |value| value.number));
        });
        assert_eq!(
            read_result
                .recv_timeout(WAIT)
                .expect("snapshot rebuilding blocked an ordinary reader"),
            Some(20)
        );
        reader.join().unwrap();
        let (written, writes) = mpsc::channel();
        let workers: Vec<_> = [1, 2]
            .into_iter()
            .map(|key| {
                let map = map.clone();
                let copies = copies.clone();
                let written = written.clone();
                std::thread::spawn(move || {
                    map.insert(
                        key,
                        Value {
                            number: 30,
                            gate: None,
                            copies,
                        },
                    );
                    let _ = written.send(key);
                })
            })
            .collect();
        assert!(matches!(
            writes.recv_timeout(Duration::from_millis(30)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(release);
        let snapshot = builder.join().unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(snapshot.len(), 2);
        assert_eq!(
            snapshot.iter().find(|(key, _)| *key == 1).unwrap().1.number,
            20
        );
        assert!(snapshot.iter().all(|(key, _)| *key != 2));
        assert_eq!(map.len(), 3);
    }
}

#[test]
fn concurrent_cache_misses_share_one_build_and_one_clone_pass() {
    for mode in [SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = ShardedHashMap::with_snapshot_mode(1, mode);
        let copies = Arc::new(AtomicUsize::new(0));
        let (gate, release, entered) = gate();
        map.insert(
            0,
            Value {
                number: 0,
                gate: Some(gate),
                copies: copies.clone(),
            },
        );
        for key in 1..32 {
            map.insert(
                key,
                Value {
                    number: key,
                    gate: None,
                    copies: copies.clone(),
                },
            );
        }
        let first_map = map.clone();
        let first = std::thread::spawn(move || first_map.shared_snapshot());
        entered.recv_timeout(WAIT).unwrap();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let map = map.clone();
                std::thread::spawn(move || map.shared_snapshot())
            })
            .collect();
        drop(release);
        let snapshot = first.join().unwrap();
        for worker in workers {
            assert!(Arc::ptr_eq(&snapshot, &worker.join().unwrap()));
        }
        assert_eq!(copies.load(Ordering::Relaxed), 32);
    }
}

#[test]
fn clone_panic_releases_builder_and_source_reads() {
    struct PanicClone(Arc<AtomicBool>);
    impl Clone for PanicClone {
        fn clone(&self) -> Self {
            assert!(
                !self.0.swap(false, Ordering::Relaxed),
                "deliberate clone failure"
            );
            Self(self.0.clone())
        }
    }
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cached);
    map.insert(0, PanicClone(Arc::new(AtomicBool::new(true))));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.shared_snapshot())).is_err()
    );
    map.insert(0, PanicClone(Arc::new(AtomicBool::new(false))));
    assert_eq!(map.shared_snapshot().len(), 1);
}

#[cfg(feature = "advanced")]
#[test]
fn all_shard_reads_preserve_transaction_cuts_during_promotion() {
    use starshard::Transaction;
    let map = ShardedHashMap::<usize, usize, Identity>::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Clone,
    );
    map.batch_insert([(0, 0), (1, 0)]);
    map.start_rebalance_online(8).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for value in 1..200 {
                let mut txn = Transaction::new();
                txn.write(0, value);
                txn.write(1, value);
                map.execute_transaction(txn);
            }
        });
        scope.spawn(|| {
            while map.rebalance_status().state == "migrating" {
                map.advance_rebalance(1);
            }
        });
        for _ in 0..200 {
            let snapshot = map.shared_snapshot();
            assert_eq!(snapshot.len(), 2);
            assert_eq!(snapshot[0].1, snapshot[1].1);
        }
    });
    assert_eq!(map.len(), 2);
}

#[test]
fn large_parallel_snapshots_include_both_generations_exactly_once() {
    let map =
        ShardedHashMap::<usize, usize, Identity>::with_shards_and_hasher(64, Identity::default());
    map.batch_insert((0..8192).map(|key| (key, key)));
    map.start_rebalance_online(128).unwrap();
    for key in 0..64 {
        map.insert(key, key + 1);
    }
    let mut snapshot: Vec<_> = map.iter().collect();
    snapshot.sort_unstable_by_key(|pair| pair.0);
    assert_eq!(snapshot.len(), 8192);
    for (key, value) in snapshot {
        assert_eq!(value, if key < 64 { key + 1 } else { key });
    }
    assert_eq!(map.keys().count(), 8192);
    assert_eq!(map.values().count(), 8192);
}
