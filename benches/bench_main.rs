use criterion::{Criterion, criterion_group, criterion_main};
use starshard::ShardedHashMap;
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

criterion_group!(benches, bench_insert, bench_get, bench_concurrent_mixed);
criterion_main!(benches);
