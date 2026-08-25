use super::*;

#[test]
fn sync_basic() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    assert!(m.insert("a".into(), 1).is_none());
    assert_eq!(m.get(&"a".into()), Some(1));
    assert_eq!(m.len(), 1);
    assert_eq!(m.remove(&"a".into()), Some(1));
    assert!(m.is_empty());
}

#[test]
fn sync_constructor_clamps_to_default_cap() {
    let m: ShardedHashMap<String, i32> =
        ShardedHashMap::with_shards_and_hasher(usize::MAX, FxBuildHasher);
    assert_eq!(m.shard_count(), MAX_SHARDS);
}

#[test]
fn sync_constructor_supports_custom_cap() {
    let m: ShardedHashMap<String, i32> =
        ShardedHashMap::with_shards_and_hasher_capped(10_000, FxBuildHasher, 128);
    assert_eq!(m.shard_count(), 128);
}

#[test]
fn sync_try_constructor_rejects_oversized_shards() {
    let err = match ShardedHashMap::<String, i32>::try_with_shards_and_hasher(
        MAX_SHARDS + 1,
        FxBuildHasher,
    ) {
        Ok(_) => panic!("expected oversized shard_count to return error"),
        Err(err) => err,
    };
    assert_eq!(err.requested(), MAX_SHARDS + 1);
    assert_eq!(err.max_allowed(), MAX_SHARDS);
}

#[test]
fn sync_try_constructor_custom_cap_success_and_rejection() {
    let ok =
        ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(32, FxBuildHasher, 64)
            .expect("expected shard_count within custom cap to succeed");
    assert_eq!(ok.shard_count(), 32);

    let err = match ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
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

#[test]
fn sync_try_constructor_custom_cap_zero_normalizes_to_one() {
    let ok = ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(1, FxBuildHasher, 0)
        .expect("expected shard_count=1 to pass when cap=0 normalizes to 1");
    assert_eq!(ok.shard_count(), 1);

    let err =
        match ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(2, FxBuildHasher, 0)
        {
            Ok(_) => panic!("expected shard_count=2 to fail when cap=0 normalizes to 1"),
            Err(err) => err,
        };
    assert_eq!(err.requested(), 2);
    assert_eq!(err.max_allowed(), 1);
}

#[test]
fn sync_contains() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
    assert!(!m.contains(&"a".into()));
    m.insert("a".into(), 1);
    assert!(m.contains(&"a".into()));
    m.insert("b".into(), 2);
    assert!(m.contains(&"b".into()));
    m.remove(&"a".into());
    assert!(!m.contains(&"a".into()));
    assert!(m.contains(&"b".into()));
}

#[test]
fn sync_entry_or_insert_and_modify() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    let inserted = m.entry("a".to_string()).or_insert_with(|| 1);
    assert_eq!(inserted, 1);
    assert_eq!(m.len(), 1);

    let value = m
        .entry("a".to_string())
        .and_modify(|v| *v += 41)
        .or_insert(7);
    assert_eq!(value, 42);
    assert_eq!(m.get(&"a".to_string()), Some(42));
    assert_eq!(m.len(), 1);
}

#[test]
fn sync_entry_reports_variants_and_removes() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

    let vacant = m.entry("missing".to_string());
    assert!(vacant.is_vacant());
    assert_eq!(vacant.key(), "missing");

    m.insert("present".to_string(), 3);
    match m.entry("present".to_string()) {
        Entry::Occupied(entry) => {
            assert_eq!(entry.get(), Some(3));
            assert_eq!(entry.remove_entry(), Some(("present".to_string(), 3)));
        }
        Entry::Vacant(_) => panic!("entry should be occupied"),
    }
    assert!(m.is_empty());
}

#[test]
fn sync_entry_or_insert_with_initializes_once_under_contention() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;

    let m: Arc<ShardedHashMap<String, i32>> = Arc::new(ShardedHashMap::new(8));
    let init_count = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();

    for _ in 0..16 {
        let map = Arc::clone(&m);
        let counter = Arc::clone(&init_count);
        workers.push(thread::spawn(move || {
            map.entry("shared".to_string()).or_insert_with(|| {
                counter.fetch_add(1, Ordering::SeqCst);
                9
            })
        }));
    }

    for worker in workers {
        assert_eq!(worker.join().expect("worker should not panic"), 9);
    }
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&"shared".to_string()), Some(9));
    assert_eq!(init_count.load(Ordering::SeqCst), 1);
}

#[test]
fn sync_iteration() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("x".into(), 10);
    m.insert("y".into(), 20);
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(v.len(), 2);
}

#[test]
fn sync_rebalance_stop_the_world() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..200 {
        m.insert(format!("k{i}"), i);
    }
    let before_len = m.len();
    let report = m
        .rebalance_to(32, RebalanceOptions::default())
        .expect("sync rebalance should succeed");
    assert_eq!(report.from_shards, 4);
    assert_eq!(report.to_shards, 32);
    assert_eq!(report.moved_entries, before_len);
    assert_eq!(m.shard_count(), 32);
    assert_eq!(m.len(), before_len);
    for i in 0..200 {
        assert_eq!(m.get(&format!("k{i}")), Some(i));
    }
}

#[test]
fn sync_rebalance_rejects_oversized_target() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    let err = m
        .rebalance_to(MAX_SHARDS + 1, RebalanceOptions::default())
        .expect_err("oversized rebalance target should fail");
    assert_eq!(err.requested(), MAX_SHARDS + 1);
    assert_eq!(err.max_allowed(), MAX_SHARDS);
}

#[test]
fn sync_rebalance_status_is_idle_after_rebalance() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("k".into(), 1);
    let status_before = m.rebalance_status();
    assert_eq!(status_before.state, "idle");
    m.rebalance_to(8, RebalanceOptions::default())
        .expect("rebalance should succeed");
    let status_after = m.rebalance_status();
    assert_eq!(status_after.state, "idle");
    assert_eq!(status_after.total_shards, 0);
    assert_eq!(status_after.moved_shards, 0);
}

#[test]
fn sync_online_rebalance_incremental_migration() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..120 {
        m.insert(format!("k{i}"), i);
    }
    assert_eq!(m.len(), 120);
    m.start_rebalance_online(16)
        .expect("online rebalance start should succeed");
    assert_eq!(m.rebalance_status().state, "migrating");
    assert_eq!(m.get(&"k3".to_string()), Some(3));

    assert_eq!(m.remove(&"k42".to_string()), Some(42));
    assert_eq!(m.insert("hot".to_string(), 777), None);

    while m.rebalance_status().state == "migrating" {
        let advanced = m.advance_rebalance(2);
        assert!(advanced <= 2);
        if advanced == 0 {
            break;
        }
    }

    let status = m.rebalance_status();
    assert_eq!(status.state, "idle");
    assert_eq!(m.shard_count(), 16);
    assert_eq!(m.get(&"k42".to_string()), None);
    assert_eq!(m.get(&"hot".to_string()), Some(777));
    assert_eq!(m.len(), 120);
}

#[test]
fn sync_rebalance_to_while_online_migrating_keeps_all_data() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    for i in 0..200 {
        m.insert(format!("k{i}"), i);
    }
    m.start_rebalance_online(16)
        .expect("online rebalance start should succeed");
    m.insert("hot".to_string(), 900);
    let report = m
        .rebalance_to(32, RebalanceOptions::default())
        .expect("stop-the-world rebalance should succeed");
    assert_eq!(report.to_shards, 32);
    assert_eq!(m.rebalance_status().state, "idle");
    assert_eq!(m.len(), 201);
    for i in 0..200 {
        assert_eq!(m.get(&format!("k{i}")), Some(i));
    }
    assert_eq!(m.get(&"hot".to_string()), Some(900));
}

#[test]
fn sync_clear_removes_previous_fallback_entries_during_migration() {
    let m: ShardedHashMap<String, i32> = ShardedHashMap::new(4);
    m.insert("k".to_string(), 1);
    m.start_rebalance_online(8)
        .expect("online rebalance start should succeed");
    m.clear();
    assert_eq!(m.len(), 0);
    assert_eq!(m.get(&"k".to_string()), None);
    assert_eq!(m.rebalance_status().state, "idle");
}
