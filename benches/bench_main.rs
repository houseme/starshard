use criterion::{Criterion, criterion_group, criterion_main};
use starshard::{ShardedHashMap, SnapshotMode};
use std::hint::black_box;
use std::sync::Arc;
use std::thread;

fn bench_insert(c: &mut Criterion) {
    c.bench_function("insert_100k", |b| {
        b.iter(|| {
            let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
            for i in 0..100_000 {
                map.insert(format!("key_{}", i), i);
            }
        })
    });
}

fn bench_get(c: &mut Criterion) {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
    for i in 0..100_000 {
        map.insert(format!("key_{}", i), i);
    }

    c.bench_function("get_100k", |b| {
        b.iter(|| {
            for i in 0..100_000 {
                black_box(map.get(&format!("key_{}", i)));
            }
        })
    });
}

fn bench_concurrent_mixed(c: &mut Criterion) {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
    for i in 0..100_000 {
        map.insert(format!("key_{}", i), i);
    }
    let map = Arc::new(map);

    c.bench_function("concurrent_mixed_8_threads", |b| {
        b.iter(|| {
            let mut handles = vec![];
            for t in 0..8 {
                let map = map.clone();
                handles.push(thread::spawn(move || {
                    for i in 0..10_000 {
                        let key = format!("key_{}", (t * 10000 + i) % 100_000);
                        if i % 10 == 0 {
                            map.insert(key, i);
                        } else {
                            black_box(map.get(&key));
                        }
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        })
    });
}

fn bench_snapshot_modes_low_write_high_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_mode_low_write_high_snapshot");
    let modes = [
        ("clone", SnapshotMode::Clone),
        ("cached", SnapshotMode::Cached),
        ("cow", SnapshotMode::Cow),
    ];

    for (name, mode) in modes {
        group.bench_function(name, |b| {
            let map: ShardedHashMap<String, i32> = ShardedHashMap::with_snapshot_mode(64, mode);
            for i in 0..50_000 {
                map.insert(format!("key_{i}"), i);
            }
            let mut write_tick = 0usize;
            b.iter(|| {
                for _ in 0..200 {
                    let snapshot_len = map.iter().count();
                    black_box(snapshot_len);
                }
                let key = format!("warm_key_{}", write_tick % 1024);
                map.insert(key, write_tick as i32);
                write_tick += 1;
            })
        });
    }
    group.finish();
}

fn bench_snapshot_modes_mid_write_mid_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_mode_mid_write_mid_snapshot");
    let modes = [
        ("clone", SnapshotMode::Clone),
        ("cached", SnapshotMode::Cached),
        ("cow", SnapshotMode::Cow),
    ];

    for (name, mode) in modes {
        group.bench_function(name, |b| {
            let map: ShardedHashMap<String, i32> = ShardedHashMap::with_snapshot_mode(64, mode);
            for i in 0..50_000 {
                map.insert(format!("key_{i}"), i);
            }
            let mut tick = 0usize;
            b.iter(|| {
                for _ in 0..20 {
                    for _ in 0..20 {
                        let key = format!("rw_key_{}", tick % 10_000);
                        map.insert(key, tick as i32);
                        tick += 1;
                    }
                    black_box(map.iter().count());
                }
            })
        });
    }
    group.finish();
}

fn bench_snapshot_modes_high_write_low_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_mode_high_write_low_snapshot");
    let modes = [
        ("clone", SnapshotMode::Clone),
        ("cached", SnapshotMode::Cached),
        ("cow", SnapshotMode::Cow),
    ];

    for (name, mode) in modes {
        group.bench_function(name, |b| {
            let map: ShardedHashMap<String, i32> = ShardedHashMap::with_snapshot_mode(64, mode);
            for i in 0..50_000 {
                map.insert(format!("key_{i}"), i);
            }
            let mut tick = 0usize;
            b.iter(|| {
                for _ in 0..2_000 {
                    let key = format!("hot_key_{}", tick % 20_000);
                    map.insert(key, tick as i32);
                    tick += 1;
                }
                black_box(map.iter().count());
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_get,
    bench_concurrent_mixed,
    bench_snapshot_modes_low_write_high_snapshot,
    bench_snapshot_modes_mid_write_mid_snapshot,
    bench_snapshot_modes_high_write_low_snapshot
);
criterion_main!(benches);
