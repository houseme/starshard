use starshard::{ShardedHashMap, SnapshotMode};
use std::time::Instant;

fn run_workload(mode: SnapshotMode) -> u128 {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::with_snapshot_mode(64, mode);
    for i in 0..50_000 {
        map.insert(format!("seed_{i}"), i);
    }

    let started = Instant::now();
    for round in 0..200 {
        for i in 0..25 {
            let key = format!("rw_{}", (round * 25 + i) % 20_000);
            map.insert(key, (round * 25 + i) as i32);
        }
        let _ = map.iter().count();
    }
    started.elapsed().as_millis()
}

fn main() {
    let clone_ms = run_workload(SnapshotMode::Clone);
    let cached_ms = run_workload(SnapshotMode::Cached);
    let cow_ms = run_workload(SnapshotMode::Cow);

    println!("mixed workload elapsed(ms): clone={clone_ms}, cached={cached_ms}, cow={cow_ms}");
}
