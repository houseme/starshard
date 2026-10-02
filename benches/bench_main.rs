use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use starshard::{ShardedHashMap, SnapshotMode};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const KEY_COUNT: usize = 8192;
const OPS_PER_WORKER: usize = 1000;

fn keys() -> Vec<String> {
    (0..KEY_COUNT).map(|index| format!("key_{index}")).collect()
}

fn bench_insert(c: &mut Criterion) {
    let entries: Vec<_> = keys()
        .into_iter()
        .enumerate()
        .map(|(i, k)| (k, i))
        .collect();
    let mut group = c.benchmark_group("insert");
    group.throughput(Throughput::Elements(KEY_COUNT as u64));
    group.bench_function("new_keys", |b| {
        b.iter_batched(
            || (ShardedHashMap::new(64), entries.clone()),
            |(map, entries)| {
                for (key, value) in entries {
                    black_box(map.insert(key, value));
                }
                // Defer map destruction until Criterion's unmeasured cleanup.
                map
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let keys = keys();
    let map = ShardedHashMap::new(64);
    for (index, key) in keys.iter().enumerate() {
        map.insert(key.clone(), index);
    }
    let mut group = c.benchmark_group("get");
    group.throughput(Throughput::Elements(KEY_COUNT as u64));
    group.bench_function("existing_keys", |b| {
        b.iter(|| {
            for key in &keys {
                black_box(map.get(black_box(key)));
            }
        });
    });
    group.finish();
}

#[derive(Clone, Copy)]
struct MixedWorkload {
    threads: usize,
    hot_keys: bool,
    value_bytes: usize,
    write_percent: usize,
}

// Workers and key schedules are created before measurement. Each timed batch
// includes two barriers, amortized over OPS_PER_WORKER operations per thread.
struct MixedWorkers {
    start: Arc<Barrier>,
    finished: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl MixedWorkers {
    fn new(workload: MixedWorkload) -> Self {
        let keys = Arc::new(keys());
        let value = Arc::new(vec![42_u8; workload.value_bytes]);
        let map = Arc::new(ShardedHashMap::new(64));
        for key in keys.iter() {
            map.insert(key.clone(), value.as_ref().clone());
        }
        let start = Arc::new(Barrier::new(workload.threads + 1));
        let finished = Arc::new(Barrier::new(workload.threads + 1));
        let stop = Arc::new(AtomicBool::new(false));
        let handles = (0..workload.threads)
            .map(|worker| {
                let map = map.clone();
                let keys = keys.clone();
                let value = value.clone();
                let start = start.clone();
                let finished = finished.clone();
                let stop = stop.clone();
                let schedule: Vec<_> = (0..OPS_PER_WORKER)
                    .map(|operation| {
                        let key_index = operation * 4051 + worker * 131;
                        let key_index = if workload.hot_keys && operation % 10 != 0 {
                            key_index % 64
                        } else {
                            key_index % KEY_COUNT
                        };
                        (key_index, operation % 100 < workload.write_percent)
                    })
                    .collect();
                thread::spawn(move || {
                    loop {
                        start.wait();
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        for &(key_index, write) in &schedule {
                            let key = &keys[key_index];
                            if write {
                                black_box(map.insert(key.clone(), value.as_ref().clone()));
                            } else {
                                black_box(map.get(key));
                            }
                        }
                        finished.wait();
                    }
                })
            })
            .collect();
        Self {
            start,
            finished,
            stop,
            handles,
        }
    }

    fn measure(&self, iterations: u64) -> Duration {
        let start = Instant::now();
        for _ in 0..iterations {
            self.start.wait();
            self.finished.wait();
        }
        start.elapsed()
    }
}

impl Drop for MixedWorkers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.start.wait();
        for handle in self.handles.drain(..) {
            handle.join().expect("benchmark worker panicked");
        }
    }
}

fn bench_concurrent_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_mixed");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(1));
    // Sample axes separately instead of an expensive Cartesian product.
    let workloads = [1, 2, 4, 8, 16]
        .into_iter()
        .map(|threads| MixedWorkload {
            threads,
            hot_keys: false,
            value_bytes: 16,
            write_percent: 10,
        })
        .chain([
            MixedWorkload {
                threads: 8,
                hot_keys: true,
                value_bytes: 16,
                write_percent: 10,
            },
            MixedWorkload {
                threads: 8,
                hot_keys: false,
                value_bytes: 4096,
                write_percent: 10,
            },
            MixedWorkload {
                threads: 8,
                hot_keys: false,
                value_bytes: 16,
                write_percent: 50,
            },
        ]);
    for workload in workloads {
        let distribution = if workload.hot_keys {
            "hot90"
        } else {
            "uniform"
        };
        let name = format!(
            "t{}_{}_v{}_w{}",
            workload.threads, distribution, workload.value_bytes, workload.write_percent,
        );
        group.throughput(Throughput::Elements(
            (workload.threads * OPS_PER_WORKER) as u64,
        ));
        group.bench_function(name, |b| {
            let workers = MixedWorkers::new(workload);
            b.iter_custom(|iterations| workers.measure(iterations));
        });
    }
    group.finish();
}

fn bench_snapshot_modes(c: &mut Criterion) {
    let keys = keys();
    let modes = [
        ("clone", SnapshotMode::Clone),
        ("cached", SnapshotMode::Cached),
        ("cow", SnapshotMode::Cow),
    ];
    let mut group = c.benchmark_group("snapshot_modes");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(1));
    for (mode_name, mode) in modes {
        for (profile, writes, snapshots) in [
            ("low_write", 1, 8),
            ("mixed", 32, 4),
            ("high_write", 256, 1),
        ] {
            let map = ShardedHashMap::with_snapshot_mode(64, mode);
            for (index, key) in keys.iter().enumerate() {
                map.insert(key.clone(), index);
            }
            let mut tick = 0;
            group.bench_function(BenchmarkId::new(profile, mode_name), |b| {
                b.iter(|| {
                    for _ in 0..writes {
                        black_box(map.insert(keys[tick % KEY_COUNT].clone(), tick));
                        tick = tick.wrapping_add(1);
                    }
                    for _ in 0..snapshots {
                        // iter() includes the public owned-output copy cost.
                        black_box(map.iter().count());
                    }
                });
            });
        }
    }
    group.finish();
}

fn bench_shared_snapshots(c: &mut Criterion) {
    let keys = keys();
    let mut group = c.benchmark_group("shared_snapshot");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(1));
    for (name, mode) in [
        ("clone", SnapshotMode::Clone),
        ("cached", SnapshotMode::Cached),
        ("cow", SnapshotMode::Cow),
    ] {
        let map = ShardedHashMap::with_snapshot_mode(64, mode);
        for key in &keys {
            map.insert(key.clone(), vec![42_u8; 256]);
        }
        black_box(map.shared_snapshot());
        group.bench_function(name, |b| {
            b.iter(|| black_box(map.shared_snapshot()));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_get,
    bench_concurrent_mixed,
    bench_snapshot_modes,
    bench_shared_snapshots,
);
criterion_main!(benches);
