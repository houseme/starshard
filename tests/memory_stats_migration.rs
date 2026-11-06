#![cfg(all(feature = "async", feature = "lifecycle"))]

use starshard::{AsyncShardedHashMap, ShardedHashMap};
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0, |hash, byte| {
            hash.wrapping_mul(256).wrapping_add(u64::from(*byte))
        });
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Identity = BuildHasherDefault<IdentityHasher>;
type SyncMap = ShardedHashMap<u64, u64, Identity>;
type AsyncMap = AsyncShardedHashMap<u64, u64, Identity>;

async fn assert_matching_memory(sync: &SyncMap, asynchronous: &AsyncMap) {
    let sync_stats = sync.memory_stats();
    let async_stats = asynchronous.memory_stats().await;
    assert_eq!(async_stats.shards_allocated, sync_stats.shards_allocated);
    assert_eq!(async_stats.total_capacity, sync_stats.total_capacity);
    assert_eq!(async_stats.load_factor, sync_stats.load_factor);
    assert_eq!(asynchronous.len().await, sync.len());
    let expected_load = if async_stats.total_capacity == 0 {
        0.0
    } else {
        sync.len() as f64 / async_stats.total_capacity as f64
    };
    assert_eq!(async_stats.load_factor, expected_load);
}

async fn check_migration(from: usize, to: usize) {
    let sync = SyncMap::with_shards_and_hasher(from, Identity::default());
    let asynchronous = AsyncMap::with_shards_and_hasher(from, Identity::default());
    assert_matching_memory(&sync, &asynchronous).await;

    sync.batch_insert((0..32).map(|key| (key, key)));
    asynchronous
        .batch_insert((0..32).map(|key| (key, key)))
        .await;
    assert_matching_memory(&sync, &asynchronous).await;
    let baseline = sync.memory_stats();

    sync.start_rebalance_online(to).unwrap();
    asynchronous.start_rebalance_online(to).await.unwrap();
    // All live entries and allocations initially remain in the previous
    // generation, despite the active-only distribution APIs reporting zero.
    assert_eq!(asynchronous.shard_stats().await.initialized, 0);
    assert!(asynchronous.per_shard_load().await.is_empty());
    assert_matching_memory(&sync, &asynchronous).await;
    let started = asynchronous.memory_stats().await;
    assert_eq!(started.shards_allocated, baseline.shards_allocated);
    assert_eq!(started.total_capacity, baseline.total_capacity);
    assert!(started.total_capacity > 0);
    assert!(started.load_factor > 0.0);

    assert_eq!(sync.advance_rebalance(1), 1);
    assert_eq!(asynchronous.advance_rebalance(1).await, 1);
    assert_matching_memory(&sync, &asynchronous).await;
    let partial = asynchronous.memory_stats().await;
    let active = asynchronous.shard_stats().await;
    assert!(active.initialized > 0);
    assert!(partial.shards_allocated > active.initialized);

    assert_eq!(sync.advance_rebalance(from), from - 1);
    assert_eq!(asynchronous.advance_rebalance(from).await, from - 1);
    assert_eq!(asynchronous.rebalance_status().state, "idle");
    assert_matching_memory(&sync, &asynchronous).await;
    let completed = asynchronous.memory_stats().await;
    assert_eq!(
        completed.shards_allocated,
        asynchronous.initialized_shards().await
    );
    assert_eq!(asynchronous.len().await, 32);
}

#[tokio::test]
async fn memory_stats_include_both_generations_while_growing() {
    check_migration(4, 8).await;
}

#[tokio::test]
async fn memory_stats_include_both_generations_while_shrinking() {
    check_migration(8, 4).await;
}
