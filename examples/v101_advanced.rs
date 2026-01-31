// Example: v1.0.1 Advanced Features (Transactions, CAS, Snapshots, Replication)
// Run with: cargo run --example v101_advanced --features "advanced"

#[cfg(feature = "advanced")]
use starshard::{CasResult, QuorumConfig, ShardedHashMap, Transaction, TransactionResult};

#[cfg(not(feature = "advanced"))]
fn main() {
    println!("This example requires the 'advanced' feature.");
    println!("Run with: cargo run --example v101_advanced --features \"advanced\"");
}

#[cfg(feature = "advanced")]
fn main() {
    println!("=== Starshard v1.0.0: Advanced Features ===\n");

    // Example 1: Atomic Transactions
    println!("1. Atomic Transactions (MVCC)");
    transaction_example();

    // Example 2: Compare-and-Swap (CAS)
    println!("\n2. Compare-and-Swap (CAS)");
    cas_example();

    // Example 3: Copy-on-Write Snapshots
    println!("\n3. Copy-on-Write Snapshots");
    snapshot_example();

    // Example 4: Versioned Snapshots
    println!("\n4. Versioned Snapshots (Time-Travel)");
    versioned_snapshot_example();

    // Example 5: Lock Profiling
    println!("\n5. Lock Profiling");
    profiling_example();

    // Example 6: Replication Simulation
    println!("\n6. Replication Configuration");
    replication_example();
}

#[cfg(feature = "advanced")]
fn transaction_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    // Initial state
    map.insert("account_a".into(), 100);
    map.insert("account_b".into(), 50);

    println!("  Initial: A=100, B=50");

    // Transaction: Transfer 30 from A to B
    let mut txn = Transaction::new();
    txn.write("account_a".into(), 70);
    txn.write("account_b".into(), 80);

    println!("  Executing transfer transaction...");
    match map.execute_transaction(txn) {
        TransactionResult::Committed(_) => println!("  ✓ Transaction Committed."),
        TransactionResult::Aborted => println!("  ✗ Transaction Aborted."),
        TransactionResult::Conflict => println!("  ✗ Transaction Conflict."),
    }

    println!(
        "  Final: A={}, B={}",
        map.get(&"account_a".into()).unwrap(),
        map.get(&"account_b".into()).unwrap()
    );
}

#[cfg(feature = "advanced")]
fn cas_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    map.insert("config_ver".into(), 1);

    println!("  Current config_ver: 1");

    // CAS: Update to 2 if current is 1
    let result = map.compare_and_swap(&"config_ver".into(), &1, 2);

    match result {
        CasResult::Success(val) => {
            println!("  ✓ CAS Success: updated to {}", val);
            println!(
                "  Current value: {}",
                map.get(&"config_ver".into()).unwrap()
            );
        }
        CasResult::Failure(current) => {
            println!("  ✗ CAS Failed: current value is {}", current);
        }
    }

    // CAS with wrong expected value
    let result2 = map.compare_and_swap(&"config_ver".into(), &99, 100);
    match result2 {
        CasResult::Success(_) => println!("  ✓ Second CAS succeeded (unexpected)"),
        CasResult::Failure(current) => {
            println!("  ✓ Second CAS failed as expected (current={})", current)
        }
    }

    // Compare and remove
    println!("\n  Testing compare_and_remove:");
    map.insert("temp".into(), 42);
    let removed = map.compare_and_remove(&"temp".into(), &42);
    println!("  Removed temp (expected 42): {}", removed);

    let not_removed = map.compare_and_remove(&"temp".into(), &42);
    println!("  Try remove again: {}", not_removed);
}

#[cfg(feature = "advanced")]
fn snapshot_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    map.insert("x".into(), 10);
    map.insert("y".into(), 20);
    map.insert("z".into(), 30);

    println!("  Creating CoW snapshot...");
    let snapshot = map.cow_snapshot();
    println!("  Snapshot contains {} entries", snapshot.len());

    // Modify map after snapshot
    map.insert("w".into(), 40);
    map.remove(&"x".into());

    println!("  Modified map (added w, removed x)");
    println!("  Map now has {} entries", map.len());
    println!("  Snapshot still has {} entries (isolated)", snapshot.len());

    println!("  Snapshot contents:");
    for (k, v) in snapshot.iter() {
        println!("    {} = {}", k, v);
    }
}

#[cfg(feature = "advanced")]
fn versioned_snapshot_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    println!("  Creating initial versioned snapshot...");
    let snap1 = map.versioned_snapshot();
    println!("  Snapshot v{} (empty)", snap1.version());

    map.insert("a".into(), 1);
    map.insert("b".into(), 2);

    let snap2 = map.versioned_snapshot();
    println!("  Snapshot v{} (2 entries)", snap2.version());

    map.insert("c".into(), 3);
    map.remove(&"a".into());

    let snap3 = map.versioned_snapshot();
    println!("  Snapshot v{} (2 entries, modified)", snap3.version());

    println!(
        "  Version ordering: v{} < v{} < v{}",
        snap1.version(),
        snap2.version(),
        snap3.version()
    );

    println!("  Snapshot v{} age: {:?}", snap2.version(), snap2.age());

    // Try to get snapshot at specific version
    if let Some(snap) = map.snapshot_at_version(snap3.version()) {
        println!("  ✓ Retrieved snapshot at version {}", snap.version());
    }
}

#[cfg(feature = "advanced")]
fn profiling_example() {
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    println!("  Enabling lock profiling...");
    map.enable_profiling(true);

    // Perform some operations
    for i in 0..20 {
        map.insert(format!("key_{}", i), i);
    }

    let profiles = map.lock_profiles();
    println!("  Collected profiles for {} shards", profiles.len());

    for profile in profiles.iter().take(3) {
        println!(
            "    Shard {}: reads={}, writes={}, contention={}",
            profile.shard_id, profile.reads, profile.writes, profile.contention_count
        );
    }

    map.enable_profiling(false);
    println!("  ✓ Profiling disabled");
}

#[cfg(feature = "advanced")]
fn replication_example() {
    let config = QuorumConfig::majority(3);

    println!("  Quorum config: {} replicas", config.replica_count);
    println!("  Write quorum: {}", config.write_quorum);
    println!("  Read quorum: {}", config.read_quorum);
    println!("  Valid: {}", config.is_valid());

    let strict_config = QuorumConfig::strict(5);
    println!("\n  Strict config (all replicas):");
    println!(
        "  Write/Read quorum: {}/{}",
        strict_config.write_quorum, strict_config.read_quorum
    );

    println!("  Note: Async replication requires 'async' feature");
    println!("  See test_v100_advanced.rs for full replication examples");
}
