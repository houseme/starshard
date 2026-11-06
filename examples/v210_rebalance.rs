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
    println!(
        "online rebalance finished, shard_count={}",
        map.shard_count()
    );
}

fn sync_budgeted_background_demo() {
    let map = ShardedHashMap::<usize, usize>::new(2);
    map.batch_insert((0..1_000).map(|key| (key, key)));
    map.start_rebalance_online(32).unwrap();
    let options = RebalanceOptions {
        background: true,
        batch_size: 32,
        max_pause_ns: 100_000,
    };
    let batch = map.advance_rebalance_with_options(&options);
    assert!(batch.moved_entries <= 32);
    assert!(map.pause_rebalance());
    assert_eq!(
        map.advance_rebalance_with_options(&options).moved_entries,
        0
    );
    assert!(map.resume_rebalance());
    let worker = map.spawn_rebalance(32, options).unwrap();
    assert!(worker.join().unwrap().completed);
    assert_eq!(map.len(), 1_000);
    let telemetry = map.rebalance_telemetry();
    println!(
        "background batches={}, max_pause_ns={}, budget_overruns={}",
        telemetry.batches, telemetry.max_pause_ns, telemetry.budget_overruns
    );
}

#[cfg(feature = "async")]
async fn async_rebalance_demo() {
    use starshard::AsyncShardedHashMap;

    let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
    map.insert("x".into(), 7).await;
    map.rebalance_to(32, RebalanceOptions::default())
        .await
        .expect("async stop-the-world rebalance should succeed");
    let worker = map
        .spawn_rebalance(
            64,
            RebalanceOptions {
                background: true,
                batch_size: 16,
                max_pause_ns: 100_000,
            },
        )
        .await
        .unwrap();
    assert!(worker.join().await.unwrap().completed);
    assert_eq!(map.get(&"x".to_owned()).await, Some(7));
}

fn main() {
    sync_stop_the_world_demo();
    sync_online_incremental_demo();
    sync_budgeted_background_demo();
    #[cfg(feature = "async")]
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async_rebalance_demo());
}
