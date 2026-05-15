use starshard::{ShardedHashMap, SnapshotMode};

fn main() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::with_snapshot_mode(32, SnapshotMode::Clone);
    for i in 0..10_000 {
        map.insert(format!("key_{i}"), i);
    }

    let snapshot_len = map.iter().count();
    println!("clone mode snapshot len={snapshot_len}");
}
