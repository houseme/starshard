use super::*;

#[cfg(feature = "async")]

#[tokio::test]
async fn async_basic() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
    assert!(m.insert("a".into(), 1).await.is_none());
    assert_eq!(m.get(&"a".into()).await, Some(1));
    assert_eq!(m.len().await, 1);
    assert_eq!(m.remove(&"a".into()).await, Some(1));
    assert!(m.is_empty().await);
}

#[tokio::test]
async fn async_constructor_clamps_to_default_cap() {
    let m: AsyncShardedHashMap<String, i32> =
        AsyncShardedHashMap::with_shards_and_hasher(usize::MAX, FxBuildHasher);
    assert_eq!(m.shard_count(), MAX_SHARDS);
}

#[tokio::test]
async fn async_constructor_supports_custom_cap() {
    let m: AsyncShardedHashMap<String, i32> =
        AsyncShardedHashMap::with_shards_and_hasher_capped(10_000, FxBuildHasher, 256);
    assert_eq!(m.shard_count(), 256);
}

#[tokio::test]
async fn async_try_constructor_rejects_oversized_shards() {
    let err = match AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher(
        MAX_SHARDS + 1,
        FxBuildHasher,
    ) {
        Ok(_) => panic!("expected oversized shard_count to return error"),
        Err(err) => err,
    };
    assert_eq!(err.requested(), MAX_SHARDS + 1);
    assert_eq!(err.max_allowed(), MAX_SHARDS);
}

#[tokio::test]
async fn async_try_constructor_custom_cap_success_and_rejection() {
    let ok = AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
        48,
        FxBuildHasher,
        64,
    )
    .expect("expected shard_count within custom cap to succeed");
    assert_eq!(ok.shard_count(), 48);

    let err = match AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
        65,
        FxBuildHasher,
        64,
    ) {
        Ok(_) => panic!("expected shard_count above custom cap to fail"),
        Err(err) => err,
    };
    assert_eq!(err.requested(), 65);
    assert_eq!(err.max_allowed(), 64);
}

#[tokio::test]
async fn async_try_constructor_custom_cap_zero_normalizes_to_one() {
    let ok =
        AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(1, FxBuildHasher, 0)
            .expect("expected shard_count=1 to pass when cap=0 normalizes to 1");
    assert_eq!(ok.shard_count(), 1);

    let err = match AsyncShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
        2,
        FxBuildHasher,
        0,
    ) {
        Ok(_) => panic!("expected shard_count=2 to fail when cap=0 normalizes to 1"),
        Err(err) => err,
    };
    assert_eq!(err.requested(), 2);
    assert_eq!(err.max_allowed(), 1);
}

#[tokio::test]
async fn async_contains() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
    assert!(!m.contains(&"a".into()).await);
    m.insert("a".into(), 1).await;
    assert!(m.contains(&"a".into()).await);
    m.insert("b".into(), 2).await;
    assert!(m.contains(&"b".into()).await);
    m.remove(&"a".into()).await;
    assert!(!m.contains(&"a".into()).await);
    assert!(m.contains(&"b".into()).await);
}

#[tokio::test]
async fn async_rebalance_stop_the_world() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    for i in 0..200 {
        m.insert(format!("k{i}"), i).await;
    }
    let before_len = m.len().await;
    let report = m
        .rebalance_to(64, RebalanceOptions::default())
        .await
        .expect("async rebalance should succeed");
    assert_eq!(report.from_shards, 4);
    assert_eq!(report.to_shards, 64);
    assert_eq!(report.moved_entries, before_len);
    assert_eq!(m.shard_count(), 64);
    assert_eq!(m.len().await, before_len);
    for i in 0..200 {
        assert_eq!(m.get(&format!("k{i}")).await, Some(i));
    }
}

#[tokio::test]
async fn async_rebalance_rejects_oversized_target() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    let err = m
        .rebalance_to(MAX_SHARDS + 1, RebalanceOptions::default())
        .await
        .expect_err("oversized rebalance target should fail");
    assert_eq!(err.requested(), MAX_SHARDS + 1);
    assert_eq!(err.max_allowed(), MAX_SHARDS);
}

#[tokio::test]
async fn async_rebalance_status_is_idle_after_rebalance() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("k".into(), 1).await;
    let status_before = m.rebalance_status();
    assert_eq!(status_before.state, "idle");
    m.rebalance_to(16, RebalanceOptions::default())
        .await
        .expect("rebalance should succeed");
    let status_after = m.rebalance_status();
    assert_eq!(status_after.state, "idle");
    assert_eq!(status_after.total_shards, 0);
    assert_eq!(status_after.moved_shards, 0);
}

#[tokio::test]
async fn async_online_rebalance_incremental_migration() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    for i in 0..120 {
        m.insert(format!("k{i}"), i).await;
    }
    assert_eq!(m.len().await, 120);
    m.start_rebalance_online(32)
        .await
        .expect("online rebalance start should succeed");
    assert_eq!(m.rebalance_status().state, "migrating");
    assert_eq!(m.get(&"k3".to_string()).await, Some(3));

    assert_eq!(m.remove(&"k42".to_string()).await, Some(42));
    assert_eq!(m.insert("hot".to_string(), 888).await, None);

    while m.rebalance_status().state == "migrating" {
        let advanced = m.advance_rebalance(2).await;
        assert!(advanced <= 2);
        if advanced == 0 {
            break;
        }
    }

    let status = m.rebalance_status();
    assert_eq!(status.state, "idle");
    assert_eq!(m.shard_count(), 32);
    assert_eq!(m.get(&"k42".to_string()).await, None);
    assert_eq!(m.get(&"hot".to_string()).await, Some(888));
    assert_eq!(m.len().await, 120);
}

#[tokio::test]
async fn async_rebalance_to_while_online_migrating_keeps_all_data() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    for i in 0..200 {
        m.insert(format!("k{i}"), i).await;
    }
    m.start_rebalance_online(16)
        .await
        .expect("online rebalance start should succeed");
    m.insert("hot".to_string(), 901).await;
    let report = m
        .rebalance_to(32, RebalanceOptions::default())
        .await
        .expect("stop-the-world rebalance should succeed");
    assert_eq!(report.to_shards, 32);
    assert_eq!(m.rebalance_status().state, "idle");
    assert_eq!(m.len().await, 201);
    for i in 0..200 {
        assert_eq!(m.get(&format!("k{i}")).await, Some(i));
    }
    assert_eq!(m.get(&"hot".to_string()).await, Some(901));
}

#[tokio::test]
async fn async_clear_removes_previous_fallback_entries_during_migration() {
    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(4);
    m.insert("k".to_string(), 1).await;
    m.start_rebalance_online(8)
        .await
        .expect("online rebalance start should succeed");
    m.clear().await;
    assert_eq!(m.len().await, 0);
    assert_eq!(m.get(&"k".to_string()).await, None);
    assert_eq!(m.rebalance_status().state, "idle");
}
