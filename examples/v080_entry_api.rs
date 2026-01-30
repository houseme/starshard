// Example: v0.8.0 Conditional Operations and Batch Operations
// Run with: cargo run --example v080_entry_api --all-features

use starshard::ShardedHashMap;

fn main() {
    println!("=== Starshard v0.8.0: Conditional Operations & Collection Views ===\n");

    // Example 1: Conditional operations (compute_if_present, compute_if_absent)
    println!("1. Conditional Operations");
    entry_api_example();

    // Example 2: Collection views
    println!("\n2. Collection Views (Keys & Values)");
    collection_views_example();

    // Example 3: Shard introspection
    println!("\n3. Shard Introspection");
    shard_stats_example();

    // Example 4: Batch operations
    println!("\n4. Batch Operations");
    batch_operations_example();

    // Example 5: Retention/filtering
    println!("\n5. Retention (Filter)");
    retention_example();
}

fn entry_api_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    // Insert initial value
    map.insert("counter".into(), 0);

    // Method 1: compute_if_absent for default insertion
    let val = map.compute_if_absent("counter".into(), || 999);
    println!(
        "  compute_if_absent('counter'): {} (exists, so returned existing)",
        val
    );

    let val = map.compute_if_absent("new_key".into(), || 42);
    println!(
        "  compute_if_absent('new_key'): {} (absent, so inserted new)",
        val
    );

    // Method 2: compute_if_present for in-place updates
    let updated = map.compute_if_present(&"counter".into(), |v| Some(v + 1));
    println!(
        "  compute_if_present('counter'): {:?} (incremented to {})",
        updated,
        map.get(&"counter".into()).unwrap()
    );

    // Method 3: compute_if_present with removal
    map.insert("to_remove".into(), 5);
    let removed = map.compute_if_present(&"to_remove".into(), |_v| None);
    println!("  compute_if_present with None (remove): {:?}", removed);
    println!(
        "  'to_remove' still exists: {}",
        map.contains(&"to_remove".into())
    );
}

fn batch_operations_example() {
    let map: ShardedHashMap<String, u32> = ShardedHashMap::new(8);

    // Batch insert
    let entries = vec![
        ("alice".into(), 100),
        ("bob".into(), 200),
        ("charlie".into(), 300),
        ("diana".into(), 400),
    ];
    let count = map.batch_insert(entries);
    println!("  Inserted {} entries", count);
    println!("  Map size: {}", map.len());

    // Batch get
    let keys = vec![
        "alice".to_string(),
        "bob".to_string(),
        "unknown".to_string(),
    ];
    let results = map.batch_get(&keys);
    println!("  Batch get {:?}: {:?}", keys, results);

    // Batch remove
    let to_remove = vec!["alice".into(), "bob".into()];
    let removed = map.batch_remove(to_remove);
    println!("  Removed {} entries", removed);
    println!("  Map size: {}", map.len());
}

fn collection_views_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    map.insert("x".into(), 10);
    map.insert("y".into(), 20);
    map.insert("z".into(), 30);

    // Iterate keys
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    println!("  Keys: {:?}", keys);

    // Iterate values
    let mut values: Vec<_> = map.values().collect();
    values.sort();
    println!("  Values: {:?}", values);

    // Standard iter (key-value pairs)
    let items: Vec<_> = map.iter().collect();
    println!("  Total items: {}", items.len());
}

fn shard_stats_example() {
    let map: ShardedHashMap<String, u32> = ShardedHashMap::new(16);

    // Insert some data
    for i in 0..100 {
        map.insert(format!("key_{:03}", i), i);
    }

    // Get shard statistics
    let stats = map.shard_stats();
    println!("  Shard statistics:");
    println!("    - Total slots: {}", stats.total);
    println!("    - Initialized shards: {}", stats.initialized);
    println!("    - Empty shards: {}", stats.empty);
    println!("    - Avg load: {:.2}", stats.avg_load);
    println!("    - Max load: {}", stats.max_load);
    println!("    - Utilization: {:.1}%", map.shard_utilization());
}

fn retention_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    // Insert data
    for i in 1..=20 {
        map.insert(format!("num_{}", i), i);
    }

    println!("  Before retain: {} entries", map.len());

    // Keep only even values
    map.retain(|_k, v| v % 2 == 0);

    println!("  After retain (only even): {} entries", map.len());

    // Verify
    for i in 0..=10 {
        let key = format!("num_{}", i * 2);
        if i > 0 {
            println!("  {} = {:?}", key, map.get(&key));
        }
    }
}
