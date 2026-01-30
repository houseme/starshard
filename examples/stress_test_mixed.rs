// Example: Mixed Workload Stress Test (High Contention)
// Run with: cargo run --example stress_test_mixed --release

use starshard::ShardedHashMap;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

const NUM_THREADS: usize = 16;
const OPS_PER_THREAD: usize = 50_000;
const KEY_SPACE: usize = 100; // Small key space to force contention

fn main() {
    println!("=== Starshard Mixed Workload Stress Test (High Contention) ===");
    println!("Threads: {}", NUM_THREADS);
    println!("Ops/Thread: {}", OPS_PER_THREAD);
    println!("Key Space: {}", KEY_SPACE);
    println!("Total Ops: {}", NUM_THREADS * OPS_PER_THREAD);

    let map: ShardedHashMap<usize, usize> = ShardedHashMap::new(16); // Fewer shards to increase contention
    let barrier = Arc::new(Barrier::new(NUM_THREADS));
    let mut handles = Vec::new();

    let start = Instant::now();

    for t_id in 0..NUM_THREADS {
        let map = map.clone();
        let barrier = barrier.clone();

        handles.push(thread::spawn(move || {
            barrier.wait();

            for i in 0..OPS_PER_THREAD {
                // Hot keys
                let key = if i % 100 == 0 {
                    0 // Super hot key
                } else {
                    (t_id * OPS_PER_THREAD + i) % KEY_SPACE
                };

                // 80% Read, 20% Write
                if i % 5 == 0 {
                    map.insert(key, i);
                } else {
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
