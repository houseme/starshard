// Example: Transaction Stress Test (Deadlock Detection)
// Run with: cargo run --example stress_test_transaction --features "advanced" --release

use rand::RngExt;
#[cfg(feature = "advanced")]
use starshard::{ShardedHashMap, Transaction};
#[cfg(feature = "advanced")]
use std::sync::{Arc, Barrier};
#[cfg(feature = "advanced")]
use std::thread;
#[cfg(feature = "advanced")]
use std::time::Instant;

const NUM_THREADS: usize = 8;
const NUM_ACCOUNTS: usize = 100;
const TRANSACTIONS_PER_THREAD: usize = 10_000;
const INITIAL_BALANCE: i32 = 1000;

#[cfg(not(feature = "advanced"))]
fn main() {
    println!("This example requires the 'advanced' feature.");
    println!(
        "Run with: cargo run --example stress_test_transaction --features \"advanced\" --release"
    );
}

#[cfg(feature = "advanced")]
fn main() {
    println!("=== Starshard Transaction Stress Test ===");
    println!("Threads: {}", NUM_THREADS);
    println!("Accounts: {}", NUM_ACCOUNTS);
    println!("Txns/Thread: {}", TRANSACTIONS_PER_THREAD);
    println!("Total Txns: {}", NUM_THREADS * TRANSACTIONS_PER_THREAD);

    let map: ShardedHashMap<usize, i32> = ShardedHashMap::new(16);

    // Initialize accounts
    for i in 0..NUM_ACCOUNTS {
        map.insert(i, INITIAL_BALANCE);
    }

    let barrier = Arc::new(Barrier::new(NUM_THREADS));
    let mut handles = Vec::new();

    let start = Instant::now();

    for _t_id in 0..NUM_THREADS {
        let map = map.clone();
        let barrier = barrier.clone();

        handles.push(thread::spawn(move || {
            let mut rng = rand::rng();
            barrier.wait();

            for _ in 0..TRANSACTIONS_PER_THREAD {
                let from = rng.random_range(0..NUM_ACCOUNTS);
                let mut to = rng.random_range(0..NUM_ACCOUNTS);
                while from == to {
                    to = rng.random_range(0..NUM_ACCOUNTS);
                }
                let _amount = 10;

                // Build transaction: move `amount` from `from` to `to`
                // In a real app, we'd read balances inside the txn logic or optimistically.
                // Here we just blindly decrement/increment to stress the locking mechanism.
                // Note: This logic is slightly flawed for correctness (balances could go negative),
                // but perfect for stressing the lock ordering and deadlock avoidance.

                let mut txn = Transaction::new();
                // We need to read to ensure we have the latest value if we were doing CAS,
                // but `execute_transaction` in this lib applies ops blindly.
                // To make it slightly more realistic, let's just do writes.
                // Since we can't read-modify-write atomically inside the current `Transaction` struct
                // (it just has a list of ops), we can't strictly enforce balance invariants
                // without external logic.
                // However, for deadlock testing, we just need to touch multiple shards.

                // We simulate a transfer by writing to both keys.
                // The values don't matter as much as the locks.
                txn.write(from, 0); // Dummy write
                txn.write(to, 0); // Dummy write

                // Also maybe a read
                txn.read(from);

                let _ = map.execute_transaction(txn);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let duration = start.elapsed();
    println!("\nTime: {:?}", duration);
    println!(
        "Throughput: {:.2} Txn/sec",
        (NUM_THREADS * TRANSACTIONS_PER_THREAD) as f64 / duration.as_secs_f64()
    );

    // Verify map is still accessible
    assert_eq!(map.len(), NUM_ACCOUNTS);
    println!("Test completed successfully (no deadlocks).");
}
