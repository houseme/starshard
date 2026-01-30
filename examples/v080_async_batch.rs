// Example: v0.8.0 Async API and Batch Operations
// Run with: cargo run --example v080_async_batch --all-features --no-default-features --features async,batch

#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;

    println!("=== Starshard v0.8.0: Async Batch Operations ===\n");

    async_batch_example().await;
    async_conditional_example().await;
    async_introspection_example().await;
}

#[cfg(feature = "async")]
async fn async_batch_example() {
    use starshard::AsyncShardedHashMap;

    println!("1. Async Batch Operations");

    let map: AsyncShardedHashMap<String, u32> = AsyncShardedHashMap::new(16);

    #[cfg(feature = "batch")]
    {
        // Async batch insert
        let entries = vec![
            ("task_1".into(), 100),
            ("task_2".into(), 200),
            ("task_3".into(), 300),
        ];
        let inserted = map.batch_insert(entries).await;
        println!("  Inserted {} tasks", inserted);

        // Async batch get
        let keys = vec!["task_1", "task_2"];
        let results = map.batch_get(&keys).await;
        println!("  Batch get results: {:?}", results);

        // Async batch remove
        let removed = map.batch_remove(vec!["task_1".into()]).await;
        println!("  Removed {} tasks", removed);
    }

    #[cfg(not(feature = "batch"))]
    {
        println!("  (Batch operations disabled)");
    }
}

#[cfg(feature = "async")]
async fn async_conditional_example() {
    use starshard::AsyncShardedHashMap;

    println!("\n2. Async Conditional Operations");

    let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);

    // compute_if_absent
    let value = map.compute_if_absent("counter".into(), || 0).await;
    println!("  Initial counter: {}", value);

    // compute_if_present
    let updated = map
        .compute_if_present(&"counter".into(), |v| Some(v + 1))
        .await;
    println!("  Updated counter: {:?}", updated);

    // Retain (async)
    for i in 0..10 {
        map.insert(format!("item_{}", i), i).await;
    }

    map.retain(|_k, v| v > &4).await;
    println!("  After retain(v > 4): {} items", map.len().await);
}

#[cfg(feature = "async")]
async fn async_introspection_example() {
    use starshard::AsyncShardedHashMap;

    println!("\n3. Async Introspection");

    let map: AsyncShardedHashMap<String, String> = AsyncShardedHashMap::new(32);

    // Populate
    for i in 0..50 {
        map.insert(format!("data_{:03}", i), format!("value_{}", i))
            .await;
    }

    // Async keys and values
    let all_keys = map.keys().await;
    println!("  Total keys: {}", all_keys.len());

    let all_values = map.values().await;
    println!("  Total values: {}", all_values.len());

    // Async shard stats
    let stats = map.shard_stats().await;
    println!("  Shard stats:");
    println!("    - Initialized: {} / {}", stats.initialized, stats.total);
    println!("    - Avg load: {:.2}", stats.avg_load);
    println!("    - Utilization: {:.1}%", map.shard_utilization().await);
}

#[cfg(not(feature = "async"))]
fn main() {
    println!("This example requires the 'async' feature.");
    println!("Run with: cargo run --example v080_async_batch --all-features");
}
