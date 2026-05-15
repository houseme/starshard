// Example: v1.2.0 Lifecycle Utilities (Per-shard load, memory stats, drain)
// Run with: cargo run --example v090_lifecycle --features "lifecycle"

#[cfg(feature = "lifecycle")]
use starshard::ShardedHashMap;

#[cfg(not(feature = "lifecycle"))]
fn main() {
    println!("This example requires the 'lifecycle' feature.");
    println!("Run with: cargo run --example v090_lifecycle --features \"lifecycle\"");
}

#[cfg(feature = "lifecycle")]
fn main() {
    println!("=== Starshard v1.2.0: Lifecycle Utilities ===\n");

    println!("1. Per-shard load");
    per_shard_load_example();

    println!("\n2. Memory stats");
    memory_stats_example();

    println!("\n3. Drain operation");
    drain_example();
}

#[cfg(feature = "lifecycle")]
fn per_shard_load_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    for i in 0..20 {
        map.insert(format!("user:{i}"), i);
    }

    let mut loads = map.per_shard_load();
    loads.sort_by_key(|x| x.shard_idx);

    println!("  Initialized shards: {}", loads.len());
    for load in loads.iter().take(3) {
        println!(
            "    shard={} entries={} capacity={}",
            load.shard_idx, load.entry_count, load.capacity
        );
    }
}

#[cfg(feature = "lifecycle")]
fn memory_stats_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..100 {
        map.insert(format!("k{}", i), i);
    }

    let stats = map.memory_stats();
    println!("  shards_allocated={}", stats.shards_allocated);
    println!("  total_capacity={}", stats.total_capacity);
    println!("  load_factor={:.3}", stats.load_factor);
}

#[cfg(feature = "lifecycle")]
fn drain_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..6 {
        map.insert(format!("job-{i}"), i);
    }

    println!("  Before drain: len={}", map.len());
    let drained: Vec<_> = map.drain().collect();
    println!("  Drained {} entries", drained.len());
    println!("  After drain: len={}", map.len());
}
