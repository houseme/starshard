use starshard::{ShardedHashMap, SnapshotMode};

fn main() {
    let map: ShardedHashMap<String, i32> =
        ShardedHashMap::with_snapshot_mode(32, SnapshotMode::Cached);
    for i in 0..10_000 {
        map.insert(format!("key_{i}"), i);
    }

    // First snapshot builds cache; later snapshots hit cache until next write.
    let first = map.iter().count();
    let second = map.iter().count();
    println!("cached mode first={first}, second={second}");

    map.insert("new_key".into(), 42);
    let third = map.iter().count();
    println!("after write cache invalidated, third={third}");
}
