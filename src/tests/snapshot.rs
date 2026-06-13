use super::*;

/* ==================== Serde Tests ==================== */

#[cfg(feature = "serde")]
#[test]
fn serde_round_trip() {
    use serde_json;
    let m: ShardedHashMap<String, u32> = ShardedHashMap::new(4);
    m.insert("x".into(), 10);
    m.insert("y".into(), 20);
    let s = serde_json::to_string(&m).unwrap();
    let de: ShardedHashMap<String, u32> = serde_json::from_str(&s).unwrap();
    assert_eq!(de.len(), 2);
    assert_eq!(de.get(&"x".into()), Some(10));
}

#[cfg(feature = "serde")]
#[test]
fn serde_deserialize_oversized_shard_count_is_clamped() {
    use serde_json;
    let json = format!(
        r#"{{"shard_count":{},"entries":[["k",1]]}}"#,
        MAX_SHARDS + 1024
    );
    let de: ShardedHashMap<String, u32> =
        serde_json::from_str(&json).expect("oversized shard_count JSON should deserialize");
    assert_eq!(de.shard_count(), MAX_SHARDS);
    assert_eq!(de.get(&"k".into()), Some(1));
}

#[cfg(all(feature = "async", feature = "serde"))]
#[tokio::test]
async fn async_snapshot_serialize() {
    use serde_json;
    let m = AsyncShardedHashMap::<u32, u32>::new(8);
    m.insert(1, 10).await;
    let snap = m.async_snapshot_serializable().await;
    let json = serde_json::to_string(&snap).unwrap();
    assert!(json.contains("[[1,10]]"));
}

/* ==================== Cached Snapshot Tests ==================== */

#[test]
fn sync_cached_snapshot_cache_invalidation() {
    let m: ShardedHashMap<String, i32> =
        ShardedHashMap::with_snapshot_mode(4, SnapshotMode::Cached);
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);

    let first = m.iter().collect::<Vec<_>>();
    let second = m.iter().collect::<Vec<_>>();
    assert_eq!(first.len(), 2);
    assert_eq!(first, second);
    let epoch = m.write_epoch.load(Ordering::Relaxed);
    assert_eq!(m.snapshot_cache_epoch.load(Ordering::Relaxed), epoch);

    m.insert("c".into(), 3);
    let cache = m.snapshot_cache.read().unwrap_or_else(|e| e.into_inner());
    assert!(cache.is_none());
}

#[test]
fn sync_cow_mode_iter_matches_writes() {
    let m: ShardedHashMap<String, i32> =
        ShardedHashMap::with_snapshot_mode(8, SnapshotMode::Cow);
    m.insert("a".into(), 1);
    m.insert("b".into(), 2);
    m.remove(&"a".into());

    let mut values = m.iter().map(|(_, v)| v).collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, vec![2]);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_cached_snapshot_cache_invalidation() {
    let m: AsyncShardedHashMap<String, i32> =
        AsyncShardedHashMap::with_snapshot_mode(4, SnapshotMode::Cached);
    m.insert("a".into(), 1).await;
    m.insert("b".into(), 2).await;

    let first = m.iter().await;
    let second = m.iter().await;
    assert_eq!(first.len(), 2);
    assert_eq!(first, second);
    let epoch = m.write_epoch.load(Ordering::Relaxed);
    assert_eq!(m.snapshot_cache_epoch.load(Ordering::Relaxed), epoch);

    m.insert("c".into(), 3).await;
    let cache = m.snapshot_cache.read().await;
    assert!(cache.is_none());
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_cow_mode_iter_matches_writes() {
    let m: AsyncShardedHashMap<String, i32> =
        AsyncShardedHashMap::with_snapshot_mode(8, SnapshotMode::Cow);
    m.insert("a".into(), 1).await;
    m.insert("b".into(), 2).await;
    m.remove(&"a".into()).await;

    let mut values = m
        .iter()
        .await
        .into_iter()
        .map(|(_, v)| v)
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, vec![2]);
}
