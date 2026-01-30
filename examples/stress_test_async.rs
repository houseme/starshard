// Example: Async Stress Test
// Run with: cargo run --example stress_test_async --features "async" --release

#[cfg(feature = "async")]
use starshard::AsyncShardedHashMap;
#[cfg(feature = "async")]
use std::sync::Arc;
#[cfg(feature = "async")]
use std::time::Instant;
#[cfg(feature = "async")]
use tokio::sync::Barrier;

const NUM_TASKS: usize = 8;
const OPS_PER_TASK: usize = 100_000;
const KEY_SPACE: usize = 10_000;

#[cfg(not(feature = "async"))]
fn main() {
    println!("This example requires the 'async' feature.");
    println!("Run with: cargo run --example stress_test_async --features \"async\" --release");
}

#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    println!("=== Starshard Async Stress Test ===");
    println!("Tasks: {}", NUM_TASKS);
    println!("Ops/Task: {}", OPS_PER_TASK);
    println!("Key Space: {}", KEY_SPACE);
    println!("Total Ops: {}", NUM_TASKS * OPS_PER_TASK);

    let map: AsyncShardedHashMap<usize, usize> = AsyncShardedHashMap::new(64);
    let barrier = Arc::new(Barrier::new(NUM_TASKS));
    let mut handles = Vec::new();

    let start = Instant::now();

    for t_id in 0..NUM_TASKS {
        let map = map.clone();
        let barrier = barrier.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for i in 0..OPS_PER_TASK {
                let key = (t_id * OPS_PER_TASK + i) % KEY_SPACE;

                // Mix of operations
                if i % 10 == 0 {
                    // Write
                    map.insert(key, i).await;
                } else if i % 10 == 1 {
                    // Remove
                    map.remove(&key).await;
                } else {
                    // Read
                    map.get(&key).await;
                }
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let duration = start.elapsed();
    println!("\nTime: {:?}", duration);
    println!(
        "Throughput: {:.2} Mops/sec",
        (NUM_TASKS * OPS_PER_TASK) as f64 / duration.as_secs_f64() / 1_000_000.0
    );
    println!("Final Map Size: {}", map.len().await);
}
