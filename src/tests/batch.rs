use super::*;

/* ==================== Sync Batch/Compute/Iteration Tests ==================== */

#[test]
fn batch_insert_new_entries() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    let entries = vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)];
    let inserted = m.batch_insert(entries);
    assert_eq!(inserted, 3);
    assert_eq!(m.len(), 3);
    assert_eq!(m.get(&"b".into()), Some(2));
}

#[test]
fn batch_insert_with_replacements() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);

    let entries = vec![
        ("a".into(), 10), // replacement
        ("c".into(), 3),  // new
    ];
    let inserted = m.batch_insert(entries);
    assert_eq!(inserted, 1); // only new entries count
    assert_eq!(m.len(), 3);
    assert_eq!(m.get(&"a".into()), Some(10));
}

#[test]
fn batch_remove() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);
    m.insert("c".into(), 3);

    let keys = vec!["a".into(), "b".into(), "d".into()];
    let removed = m.batch_remove(keys);
    assert_eq!(removed, 2);
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&"c".into()), Some(3));
}

#[test]
fn batch_get() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 1);
    m.insert("c".into(), 3);

    let keys = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let results = m.batch_get(&keys);
    assert_eq!(results, vec![Some(1), None, Some(3)]);
}

#[test]
fn compute_if_present_exists() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 10);

    let result = m.compute_if_present(&"a".into(), |v| Some(v * 2));
    assert_eq!(result, Some(20));
    assert_eq!(m.get(&"a".into()), Some(20));
    assert_eq!(m.len(), 1);
}

#[test]
fn compute_if_present_absent() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    let result = m.compute_if_present(&"a".into(), |v| Some(v * 2));
    assert_eq!(result, None);
    assert_eq!(m.len(), 0);
}

#[test]
fn compute_if_present_remove() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 10);
    m.insert("b".into(), 20);

    let result = m.compute_if_present(&"a".into(), |_v| None);
    assert_eq!(result, None);
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&"b".into()), Some(20));
}

#[test]
fn compute_if_absent_exists() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 10);

    let result = m.compute_if_absent("a".into(), || 20);
    assert_eq!(result, 10);
    assert_eq!(m.len(), 1);
}

#[test]
fn compute_if_absent_inserts() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    let result = m.compute_if_absent("a".into(), || 20);
    assert_eq!(result, 20);
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&"a".into()), Some(20));
}

#[test]
fn retain_filter() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..10 {
        m.insert(format!("k{i}"), i);
    }
    assert_eq!(m.len(), 10);

    m.retain(|_, v| v % 2 == 0); // keep only even values
    assert_eq!(m.len(), 5);
    assert_eq!(m.get(&"k2".into()), Some(2));
    assert_eq!(m.get(&"k3".into()), None);
}

#[test]
fn keys_iteration() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);
    m.insert("c".into(), 3);

    let mut keys: Vec<_> = m.keys().collect();
    keys.sort();
    assert_eq!(keys, vec!["a", "b", "c"]);
}

#[test]
fn values_iteration() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 10);
    m.insert("b".into(), 20);
    m.insert("c".into(), 30);

    let mut values: Vec<_> = m.values().collect();
    values.sort();
    assert_eq!(values, vec![10, 20, 30]);
}

#[test]
fn shard_stats_basic() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..10 {
        m.insert(format!("k{i}"), i);
    }

    let stats = m.shard_stats();
    assert_eq!(stats.total, 4);
    assert!(stats.initialized > 0);
    assert!(stats.max_load > 0);
    assert!(stats.avg_load > 0.0);
}

#[test]
fn shard_stats_empty_count() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("k".into(), 1);
    m.remove(&"k".into());

    let stats = m.shard_stats();
    assert_eq!(stats.initialized, 1);
    assert_eq!(stats.empty, 1);
}

#[test]
fn shard_utilization() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(16);
    m.insert("a".into(), 1);

    let util = m.shard_utilization();
    assert!(util > 0.0 && util <= 100.0);
}

/* ==================== Async Batch/Compute/Iteration Tests ==================== */

#[cfg(feature = "async")]
#[tokio::test]
async fn async_batch_insert() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    let entries = vec![("a".into(), 1), ("b".into(), 2)];
    let inserted = m.batch_insert(entries).await;
    assert_eq!(inserted, 2);
    assert_eq!(m.len().await, 2);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_batch_remove() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("a".into(), 1).await;
    m.insert("b".into(), 2).await;
    m.insert("c".into(), 3).await;

    let removed = m.batch_remove(vec!["a".into(), "b".into()]).await;
    assert_eq!(removed, 2);
    assert_eq!(m.len().await, 1);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_batch_get() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("a".into(), 1).await;
    m.insert("c".into(), 3).await;

    let keys = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let results = m.batch_get(&keys).await;
    assert_eq!(results, vec![Some(1), None, Some(3)]);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_compute_if_present() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("a".into(), 10).await;

    let result = m.compute_if_present(&"a".into(), |v| Some(v * 2)).await;
    assert_eq!(result, Some(20));
    assert_eq!(m.get(&"a".into()).await, Some(20));
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_compute_if_absent() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    let result = m.compute_if_absent("a".into(), || 20).await;
    assert_eq!(result, 20);
    assert_eq!(m.len().await, 1);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_retain() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    for i in 0..10 {
        m.insert(format!("k{i}"), i).await;
    }

    m.retain(|_, v| v % 2 == 0).await;
    assert_eq!(m.len().await, 5);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_keys() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("a".into(), 1).await;
    m.insert("b".into(), 2).await;

    let mut keys = m.keys().await;
    keys.sort();
    assert_eq!(keys, vec!["a", "b"]);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_values() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("a".into(), 1).await;
    m.insert("b".into(), 2).await;

    let mut values = m.values().await;
    values.sort();
    assert_eq!(values, vec![1, 2]);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_shard_stats() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    for i in 0..10 {
        m.insert(format!("k{i}"), i).await;
    }

    let stats = m.shard_stats().await;
    assert_eq!(stats.total, 4);
    assert!(stats.initialized > 0);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_shard_stats_empty_count() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("k".into(), 1).await;
    m.remove(&"k".into()).await;

    let stats = m.shard_stats().await;
    assert_eq!(stats.initialized, 1);
    assert_eq!(stats.empty, 1);
}

/* ==================== Lifecycle Tests ==================== */

#[cfg(feature = "lifecycle")]
#[test]
fn lifecycle_memory_stats_and_drain() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);
    m.insert("c".into(), 3);

    let memory = m.memory_stats();
    assert!(memory.shards_allocated > 0);
    assert!(memory.total_capacity > 0);
    assert!(memory.load_factor > 0.0);

    let drained: Vec<_> = m.drain().collect();
    assert_eq!(drained.len(), 3);
    assert_eq!(m.len(), 0);
}

#[cfg(all(feature = "async", feature = "lifecycle"))]
#[tokio::test]
async fn async_lifecycle_memory_load_and_drain() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("a".into(), 1).await;
    m.insert("b".into(), 2).await;
    m.insert("c".into(), 3).await;

    let loads = m.per_shard_load().await;
    assert!(!loads.is_empty());

    let memory = m.memory_stats().await;
    assert!(memory.shards_allocated > 0);
    assert!(memory.total_capacity > 0);
    assert!(memory.load_factor > 0.0);

    let drained: Vec<_> = m.drain().await.collect();
    assert_eq!(drained.len(), 3);
    assert_eq!(m.len().await, 0);
}
