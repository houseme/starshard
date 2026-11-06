use starshard::{ShardedHashMap, SnapshotMode};

fn main() {
    let map: ShardedHashMap<String, i32> =
        ShardedHashMap::with_snapshot_mode(32, SnapshotMode::Cow);
    for i in 0..10_000 {
        map.insert(format!("key_{i}"), i);
    }

    let before = map.snapshot_view();
    map.remove(&"key_1".to_string());
    map.insert("key_10_001".into(), 10_001);
    let after = map.snapshot_view();
    assert!(before.iter().any(|(key, _)| key == "key_1"));
    assert!(after.iter().all(|(key, _)| key != "key_1"));
    assert!(after.iter().any(|(key, _)| key == "key_10_001"));
    assert!(after.version() > before.version());

    println!(
        "cow views before={}, after={}, version={}",
        before.len(),
        after.len(),
        after.version()
    );
}
