use starshard::{ShardedHashMap, SnapshotMode};

fn main() {
    let map: ShardedHashMap<String, i32> =
        ShardedHashMap::with_snapshot_mode(32, SnapshotMode::Cow);
    for i in 0..10_000 {
        map.insert(format!("key_{i}"), i);
    }

    let before = map.iter().count();
    map.remove(&"key_1".to_string());
    map.insert("key_10_001".into(), 10_001);
    let after = map.iter().count();

    println!("cow mode before={before}, after={after}");
}
