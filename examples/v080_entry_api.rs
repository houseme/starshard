// Example: v0.8.0 Entry API and Batch Operations
// Run with: cargo run --example v080_entry_api --all-features

use starshard::ShardedHashMap;

fn main() {
    println!("=== Starshard v0.8.0: Entry API & Batch Operations ===\n");

    // Example 1: Entry API for efficient updates
    println!("1. Entry API - Conditional Updates");
    entry_api_example();

    // Example 2: Batch operations
    println!("\n2. Batch Operations");
    batch_operations_example();

    // Example 3: Conditional operations
    println!("\n3. Conditional Operations");
    conditional_operations_example();

    // Example 4: Collection views
    println!("\n4. Collection Views (Keys & Values)");
    collection_views_example();

    // Example 5: Shard introspection
    println!("\n5. Shard Introspection");
    shard_stats_example();

    // Example 6: Retention/filtering
    println!("\n6. Retention (Filter)");
    retention_example();
}

fn entry_api_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    // Insert initial value
    map.insert("counter".into(), 0);

    // Use entry API to increment
    #[cfg(feature = "entry")]
    {
        use starshard::Entry;

        match map.entry("counter".into()) {
            Entry::Occupied(entry) => {
                println!("  Key exists with value: {}", entry.get());
            }
            Entry::Vacant(_) => {
                println!("  Key vacant (unexpected)");
            }
        }

        // More practical: or_insert for default values
        let val = map.entry("new_key".into()).or_insert_with(|| 42);
        println!("  Inserted 'new_key' with value: {}", val);

        // and_modify for conditional updates
        let _entry = map.entry("counter".into()).and_modify(|v| *v += 1);
        println!(
            "  Incremented 'counter' to: {}",
            map.get(&"counter".into()).unwrap()
        );
    }

    #[cfg(not(feature = "entry"))]
    {
        println!("  (Entry API disabled - enable with 'entry' feature)");
    }
}

fn batch_operations_example() {
    let map: ShardedHashMap<String, u32> = ShardedHashMap::new(8);

    #[cfg(feature = "batch")]
    {
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
        let keys = vec!["alice", "bob", "unknown"];
        let results = map.batch_get(&keys);
        println!("  Batch get {:?}: {:?}", keys, results);

        // Batch remove
        let to_remove = vec!["alice".into(), "bob".into()];
        let removed = map.batch_remove(to_remove);
        println!("  Removed {} entries", removed);
        println!("  Map size: {}", map.len());
    }

    #[cfg(not(feature = "batch"))]
    {
        println!("  (Batch operations disabled - enable with 'batch' feature)");
    }
}

fn conditional_operations_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    // compute_if_present: Update only if key exists
    let result = map.compute_if_present(&"nonexistent".into(), |v| Some(v * 2));
    println!("  compute_if_present (absent): {:?}", result);

    map.insert("score".into(), 10);
    let result = map.compute_if_present(&"score".into(), |v| Some(v * 2));
    println!("  compute_if_present (present): {:?}", result);
    println!("  score is now: {}", map.get(&"score".into()).unwrap());

    // compute_if_absent: Insert only if key absent
    let val = map.compute_if_absent("score".into(), || 999);
    println!("  compute_if_absent (present): {}", val); // Returns existing 20

    let val = map.compute_if_absent("new_score".into(), || 50);
    println!("  compute_if_absent (absent): {}", val); // Returns newly inserted 50

    // compute_if_present with removal (return None)
    map.insert("removable".into(), 5);
    let result = map.compute_if_present(&"removable".into(), |_v| None);
    println!("  compute_if_present with removal: {:?}", result);
    println!(
        "  'removable' still exists: {}",
        map.contains(&"removable".into())
    );
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
