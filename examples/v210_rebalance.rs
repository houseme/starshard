use starshard::{RebalanceOptions, ShardedHashMap};

fn sync_stop_the_world_demo() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    map.insert("a".into(), 1);
    map.insert("b".into(), 2);

    let report = map
        .rebalance_to(32, RebalanceOptions::default())
        .expect("stop-the-world rebalance should succeed");
    println!(
        "sync rebalance: {} -> {}, moved={}, elapsed_ms={}",
        report.from_shards, report.to_shards, report.moved_entries, report.elapsed_ms
    );
}

fn sync_online_incremental_demo() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    for i in 0..100 {
        map.insert(format!("k{i}"), i);
    }

    map.start_rebalance_online(64)
        .expect("online rebalance start should succeed");
    while map.rebalance_status().state == "migrating" {
        map.advance_rebalance(2);
    }

    assert_eq!(map.get(&"k99".to_string()), Some(99));
    println!("online rebalance finished, shard_count={}", map.shard_count());
}

#[cfg(feature = "async")]
#[allow(dead_code)]
async fn async_stop_the_world_demo() {
    use starshard::AsyncShardedHashMap;

    let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
    map.insert("x".into(), 7).await;
    map.rebalance_to(32, RebalanceOptions::default())
        .await
        .expect("async stop-the-world rebalance should succeed");
}

fn main() {
    sync_stop_the_world_demo();
    sync_online_incremental_demo();
}
