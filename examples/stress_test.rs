// Example: Stress Test
// Run with: cargo run --example stress_test --release

use starshard::ShardedHashMap;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

const NUM_THREADS: usize = 8;
const OPS_PER_THREAD: usize = 100_000;
const KEY_SPACE: usize = 10_000;

fn main() {
    println!("=== Starshard Stress Test ===");
    println!("Threads: {}", NUM_THREADS);
    println!("Ops/Thread: {}", OPS_PER_THREAD);
    println!("Key Space: {}", KEY_SPACE);
    println!("Total Ops: {}", NUM_THREADS * OPS_PER_THREAD);

    let map: ShardedHashMap<usize, usize> = ShardedHashMap::new(64);
    let barrier = Arc::new(Barrier::new(NUM_THREADS));
    let mut handles = Vec::new();

    let start = Instant::now();

    for t_id in 0..NUM_THREADS {
        let map = map.clone();
        let barrier = barrier.clone();

        handles.push(thread::spawn(move || {
            barrier.wait();

            for i in 0..OPS_PER_THREAD {
                let key = (t_id * OPS_PER_THREAD + i) % KEY_SPACE;

                // Mix of operations
                if i % 10 == 0 {
                    // Write
                    map.insert(key, i);
                } else if i % 10 == 1 {
                    // Remove
                    map.remove(&key);
                } else {
                    // Read
                    map.get(&key);
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let duration = start.elapsed();
    println!("\nTime: {:?}", duration);
    println!(
        "Throughput: {:.2} Mops/sec",
        (NUM_THREADS * OPS_PER_THREAD) as f64 / duration.as_secs_f64() / 1_000_000.0
    );
    println!("Final Map Size: {}", map.len());
}
