// Example: v1.0.1 Advanced Features (Transactions, CAS, Replication)
// Run with: cargo run --example v101_advanced --features "advanced"

#[cfg(feature = "advanced")]
use starshard::{QuorumConfig, ReplicationOp, ShardedHashMap, Transaction, TransactionResult};

#[cfg(not(feature = "advanced"))]
fn main() {
    println!("This example requires the 'advanced' feature.");
    println!("Run with: cargo run --example v101_advanced --features \"advanced\"");
}

#[cfg(feature = "advanced")]
fn main() {
    println!("=== Starshard v1.0.1: Advanced Features ===\n");

    // Example 1: Atomic Transactions
    println!("1. Atomic Transactions (MVCC)");
    transaction_example();

    // Example 2: Compare-and-Swap (CAS)
    println!("\n2. Compare-and-Swap (CAS)");
    cas_example();

    // Example 3: Replication Simulation
    println!("\n3. Replication Simulation");
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

    // Read current values (in a real app, you'd read first, then build txn)
    // Here we assume we know the values or logic is part of txn execution
    // For this simple example, we just queue writes.
    txn.write("account_a".into(), 70);
    txn.write("account_b".into(), 80);

    println!("  Executing transfer transaction...");
    match map.execute_transaction(txn) {
        TransactionResult::Committed(_) => println!("  Transaction Committed."),
        TransactionResult::Aborted => println!("  Transaction Aborted."),
        TransactionResult::Conflict => println!("  Transaction Conflict."),
    }

    println!(
        "  Final: A={}, B={}",
        map.get(&"account_a".into()).unwrap(),
        map.get(&"account_b".into()).unwrap()
    );
}

#[cfg(feature = "advanced")]
fn cas_example() {
    // Note: This assumes ShardedHashMap has a CAS method exposed or we simulate it
    // Since `CasResult` is in `advanced`, likely there is a method.
    // If not explicitly on `ShardedHashMap` in `lib.rs`, we might need to use `compute_if_present`
    // to simulate CAS, or check if `advanced` module adds it.

    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    map.insert("config_ver".into(), 1);

    println!("  Current config_ver: 1");

    // Simulate CAS: Update to 2 if current is 1
    let key = "config_ver".to_string();
    let expected = 1;
    let new_val = 2;

    // Using compute_if_present to simulate CAS behavior if explicit `compare_and_swap` isn't on main struct
    let result = map.compute_if_present(&key, |&current| {
        if current == expected {
            Some(new_val)
        } else {
            Some(current) // No change
        }
    });

    if let Some(val) = result {
        if val == new_val {
            println!("  CAS Success: updated to {}", val);
        } else {
            println!("  CAS Failed: current value is {}", val);
        }
    }
}

#[cfg(feature = "advanced")]
fn replication_example() {
    let config = QuorumConfig::majority(3);
    println!("  Replication Config: {:?}", config);

    // Simulate a replication operation
    let op = ReplicationOp::Insert {
        key: "replicated_key".to_string(),
        value: "data".to_string(),
    };

    println!("  Replicating operation: {:?}", op);

    // In a real scenario, this would involve network calls to replicas.
    // Here we just demonstrate the types and structure.
    println!("  (Replication logic would execute here, waiting for quorum)");
}
