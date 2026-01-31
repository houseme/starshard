//! Tests for Version 1.0.0 Advanced Features
//!
//! This test suite covers:
//! - Atomic transactions (MVCC-based)
//! - Compare-and-swap (CAS) operations
//! - Copy-on-write (CoW) snapshots
//! - Versioned snapshots and time-travel queries
//! - Lock profiling and diagnostics
//! - Distributed replication framework

#[cfg(feature = "advanced")]
mod advanced_tests {
    use starshard::{
        ShardedHashMap, Transaction, TransactionResult,
        advanced::{QuorumConfig, Replica, ReplicaError, ReplicationOp},
    };
    use std::sync::Arc;

    // ========== Transaction Tests ==========

    #[test]
    fn test_transaction_commit_basic() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("a".into(), 1);
        map.insert("b".into(), 2);

        let mut txn = Transaction::new();
        txn.write("a".into(), 10);
        txn.write("c".into(), 30);
        txn.remove("b".into());

        let result = map.execute_transaction(txn);
        assert!(matches!(result, TransactionResult::Committed(())));

        assert_eq!(map.get(&"a".into()), Some(10));
        assert_eq!(map.get(&"b".into()), None);
        assert_eq!(map.get(&"c".into()), Some(30));
    }

    #[test]
    fn test_transaction_multi_shard() {
        let map: ShardedHashMap<u64, String> = ShardedHashMap::new(16);

        // Insert keys that likely map to different shards
        for i in 0..100 {
            map.insert(i, format!("value_{}", i));
        }

        let mut txn = Transaction::new();
        txn.write(5, "updated_5".into());
        txn.write(50, "updated_50".into());
        txn.write(95, "updated_95".into());
        txn.remove(10);
        txn.remove(60);

        let result = map.execute_transaction(txn);
        assert!(matches!(result, TransactionResult::Committed(())));

        assert_eq!(map.get(&5), Some("updated_5".into()));
        assert_eq!(map.get(&50), Some("updated_50".into()));
        assert_eq!(map.get(&95), Some("updated_95".into()));
        assert_eq!(map.get(&10), None);
        assert_eq!(map.get(&60), None);
    }

    #[test]
    fn test_transaction_read_operations() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("x".into(), 100);

        let mut txn = Transaction::new();
        txn.read("x".into());
        txn.write("y".into(), 200);

        let result = map.execute_transaction(txn);
        assert!(matches!(result, TransactionResult::Committed(())));

        assert_eq!(map.get(&"x".into()), Some(100));
        assert_eq!(map.get(&"y".into()), Some(200));
    }

    #[test]
    fn test_transaction_empty() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        let txn: Transaction<String, i32> = Transaction::new();

        let result = map.execute_transaction(txn);
        assert!(matches!(result, TransactionResult::Committed(())));
    }

    // ========== CAS Tests ==========

    #[test]
    fn test_cas_success() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("counter".into(), 1);

        let result = map.compare_and_swap(&"counter".into(), &1, 2);
        assert!(result.is_success());
        assert_eq!(result.into_value(), 2);
        assert_eq!(map.get(&"counter".into()), Some(2));
    }

    #[test]
    fn test_cas_failure_wrong_value() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("counter".into(), 1);

        let result = map.compare_and_swap(&"counter".into(), &5, 2);
        assert!(!result.is_success());
        assert_eq!(result.into_value(), 1); // Returns current value
        assert_eq!(map.get(&"counter".into()), Some(1)); // Unchanged
    }

    #[test]
    fn test_cas_failure_key_missing() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

        let result = map.compare_and_swap(&"missing".into(), &1, 2);
        assert!(!result.is_success());
    }

    #[test]
    fn test_cas_multiple_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("shared".into(), 0);

        let map_clone = map.clone();
        let success_count = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..10 {
            let map_ref = map_clone.clone();
            let counter = Arc::clone(&success_count);

            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    loop {
                        let current = map_ref.get(&"shared".into()).unwrap_or(0);
                        let result =
                            map_ref.compare_and_swap(&"shared".into(), &current, current + 1);
                        if result.is_success() {
                            counter.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        // Retry on failure
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let final_value = map.get(&"shared".into()).unwrap();
        assert_eq!(final_value, 1000);
        assert_eq!(success_count.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn test_compare_and_remove_success() {
        let map: ShardedHashMap<String, String> = ShardedHashMap::new(8);
        map.insert("key1".into(), "value1".into());

        let removed = map.compare_and_remove(&"key1".into(), &"value1".into());
        assert!(removed);
        assert_eq!(map.get(&"key1".into()), None);
    }

    #[test]
    fn test_compare_and_remove_failure() {
        let map: ShardedHashMap<String, String> = ShardedHashMap::new(8);
        map.insert("key1".into(), "value1".into());

        let removed = map.compare_and_remove(&"key1".into(), &"wrong_value".into());
        assert!(!removed);
        assert_eq!(map.get(&"key1".into()), Some("value1".into()));
    }

    #[test]
    fn test_compare_and_remove_missing_key() {
        let map: ShardedHashMap<String, String> = ShardedHashMap::new(8);

        let removed = map.compare_and_remove(&"missing".into(), &"value".into());
        assert!(!removed);
    }

    // ========== CoW Snapshot Tests ==========

    #[test]
    fn test_cow_snapshot_creation() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("a".into(), 1);
        map.insert("b".into(), 2);
        map.insert("c".into(), 3);

        let snapshot = map.cow_snapshot();
        assert_eq!(snapshot.len(), 3);
        // version() returns u64, which is always >= 0
    }

    #[test]
    fn test_cow_snapshot_isolation() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("x".into(), 10);
        map.insert("y".into(), 20);

        let snapshot = map.cow_snapshot();
        let snap_len = snapshot.len();

        // Modify map after snapshot
        map.insert("z".into(), 30);
        map.remove(&"x".into());

        // Snapshot should be unchanged
        assert_eq!(snapshot.len(), snap_len);
        assert_eq!(map.len(), 2); // Map has changed
    }

    #[test]
    fn test_cow_snapshot_iteration() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("a".into(), 1);
        map.insert("b".into(), 2);
        map.insert("c".into(), 3);

        let snapshot = map.cow_snapshot();
        let mut items: Vec<_> = snapshot.iter().map(|(k, v)| (k.clone(), *v)).collect();
        items.sort();

        assert_eq!(items.len(), 3);
        assert!(items.contains(&("a".into(), 1)));
        assert!(items.contains(&("b".into(), 2)));
        assert!(items.contains(&("c".into(), 3)));
    }

    #[test]
    fn test_cow_snapshot_empty_map() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        let snapshot = map.cow_snapshot();

        assert!(snapshot.is_empty());
        assert_eq!(snapshot.len(), 0);
    }

    // ========== Versioned Snapshot Tests ==========

    #[test]
    fn test_versioned_snapshot_creation() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("a".into(), 1);

        let snap = map.versioned_snapshot();
        // version() returns u64, which is always >= 0
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn test_versioned_snapshot_ordering() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

        let snap1 = map.versioned_snapshot();
        map.insert("x".into(), 10);
        let snap2 = map.versioned_snapshot();
        map.insert("y".into(), 20);
        let snap3 = map.versioned_snapshot();

        assert!(snap1.version() < snap2.version());
        assert!(snap2.version() < snap3.version());
    }

    #[test]
    fn test_snapshot_at_version_hit() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("a".into(), 1);

        // Create a snapshot which increments version
        let snap1 = map.versioned_snapshot();
        let v1 = snap1.version();

        // Now try to get snapshot at the CURRENT version (which is v1 + 1)
        // Since versioned_snapshot incremented it
        // We should check if we can get a snapshot at the current internal version
        let current_internal_version = v1 + 1;
        let snap = map.snapshot_at_version(current_internal_version);

        assert!(snap.is_some());
        if let Some(s) = snap {
            assert_eq!(s.version(), current_internal_version);
        }
    }

    #[test]
    fn test_snapshot_at_version_miss() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

        let snap = map.snapshot_at_version(9999);
        assert!(snap.is_none());
    }

    #[test]
    fn test_snapshot_age() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        let snap = map.versioned_snapshot();

        std::thread::sleep(std::time::Duration::from_millis(10));

        let age = snap.age();
        assert!(age >= std::time::Duration::from_millis(10));
    }

    // ========== Lock Profiling Tests ==========

    #[test]
    fn test_enable_profiling() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

        map.enable_profiling(true);
        let profiles = map.lock_profiles();
        // With no initialized shards, should return empty or just initialized ones
        assert!(profiles.len() <= 8);

        map.enable_profiling(false);
        let profiles_disabled = map.lock_profiles();
        assert_eq!(profiles_disabled.len(), 0);
    }

    #[test]
    fn test_lock_profiles_structure() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.enable_profiling(true);

        // Initialize some shards
        map.insert("a".into(), 1);
        map.insert("b".into(), 2);

        let profiles = map.lock_profiles();
        for profile in profiles {
            assert!(profile.shard_id < 8);
            assert_eq!(profile.contention_count, 0);
            assert_eq!(profile.reads, 0);
            assert_eq!(profile.writes, 0);
        }
    }

    // ========== Integration Tests ==========

    #[test]
    fn test_cas_with_transaction() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
        map.insert("x".into(), 5);

        // Use CAS to update
        let cas_result = map.compare_and_swap(&"x".into(), &5, 10);
        assert!(cas_result.is_success());

        // Use transaction to update
        let mut txn = Transaction::new();
        txn.write("x".into(), 20);
        txn.write("y".into(), 30);

        let txn_result = map.execute_transaction(txn);
        assert!(matches!(txn_result, TransactionResult::Committed(())));

        assert_eq!(map.get(&"x".into()), Some(20));
        assert_eq!(map.get(&"y".into()), Some(30));
    }

    #[test]
    fn test_snapshot_after_transaction() {
        let map: ShardedHashMap<String, i32> = ShardedHashMap::new(8);

        let snap1 = map.versioned_snapshot();
        assert_eq!(snap1.len(), 0);

        let mut txn = Transaction::new();
        txn.write("a".into(), 1);
        txn.write("b".into(), 2);
        map.execute_transaction(txn);

        let snap2 = map.versioned_snapshot();
        assert_eq!(snap2.len(), 2);
        assert!(snap2.version() > snap1.version());
    }

    #[test]
    fn test_cow_snapshot_minimal_locking() {
        use std::sync::Arc as StdArc;
        use std::sync::Barrier;
        use std::thread;

        let map: ShardedHashMap<u64, u64> = ShardedHashMap::new(16);

        // Populate map
        for i in 0..1000 {
            map.insert(i, i * 2);
        }

        let map_clone = map.clone();
        let map_clone2 = map.clone();

        // Use a barrier to ensure reader takes snapshot before writer starts
        let barrier = StdArc::new(Barrier::new(2));
        let barrier_clone = StdArc::clone(&barrier);

        // Reader thread using CoW snapshot
        let reader = thread::spawn(move || {
            let snapshot = map_clone.cow_snapshot();
            barrier.wait(); // Signal that snapshot is taken
            let sum: u64 = snapshot.iter().map(|(_, v)| v).sum();
            sum
        });

        // Writer thread
        let writer = thread::spawn(move || {
            barrier_clone.wait(); // Wait for snapshot to be taken
            for i in 1000..1100 {
                map_clone2.insert(i, i * 2);
            }
        });

        let reader_sum = reader.join().unwrap();
        writer.join().unwrap();

        // Reader got a consistent snapshot of original 1000 entries
        let expected_sum: u64 = (0..1000).map(|i| i * 2).sum();
        assert_eq!(reader_sum, expected_sum);
    }

    // ========== Async Tests ==========

    #[cfg(feature = "async")]
    mod async_tests {
        use super::*;
        use starshard::AsyncShardedHashMap;
        use std::sync::Arc;

        #[tokio::test]
        async fn test_async_transaction_commit() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.insert("a".into(), 1).await;
            map.insert("b".into(), 2).await;

            let mut txn = Transaction::new();
            txn.write("a".into(), 10);
            txn.write("c".into(), 30);
            txn.remove("b".into());

            let result = map.execute_transaction(txn).await;
            assert!(matches!(result, TransactionResult::Committed(())));

            assert_eq!(map.get(&"a".into()).await, Some(10));
            assert_eq!(map.get(&"b".into()).await, None);
            assert_eq!(map.get(&"c".into()).await, Some(30));
        }

        #[tokio::test]
        async fn test_async_cas_success() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.insert("counter".into(), 1).await;

            let result = map.compare_and_swap(&"counter".into(), &1, 2).await;
            assert!(result.is_success());
            assert_eq!(map.get(&"counter".into()).await, Some(2));
        }

        #[tokio::test]
        async fn test_async_cas_failure() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.insert("counter".into(), 1).await;

            let result = map.compare_and_swap(&"counter".into(), &5, 2).await;
            assert!(!result.is_success());
            assert_eq!(map.get(&"counter".into()).await, Some(1));
        }

        #[tokio::test]
        async fn test_async_compare_and_remove() {
            let map: AsyncShardedHashMap<String, String> = AsyncShardedHashMap::new(8);
            map.insert("key1".into(), "value1".into()).await;

            let removed = map
                .compare_and_remove(&"key1".into(), &"value1".into())
                .await;
            assert!(removed);
            assert_eq!(map.get(&"key1".into()).await, None);
        }

        #[tokio::test]
        async fn test_async_cow_snapshot() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.insert("a".into(), 1).await;
            map.insert("b".into(), 2).await;

            let snapshot = map.cow_snapshot().await;
            assert_eq!(snapshot.len(), 2);

            map.insert("c".into(), 3).await;
            assert_eq!(snapshot.len(), 2); // Snapshot unchanged
        }

        #[tokio::test]
        async fn test_async_versioned_snapshot() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);

            let snap1 = map.versioned_snapshot().await;
            map.insert("x".into(), 10).await;
            let snap2 = map.versioned_snapshot().await;

            assert!(snap1.version() < snap2.version());
        }

        #[tokio::test]
        async fn test_async_snapshot_at_version() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.insert("a".into(), 1).await;

            let snap1 = map.versioned_snapshot().await;
            let v1 = snap1.version();

            // Current internal version is now v1 + 1 after versioned_snapshot incremented it
            let current_internal_version = v1 + 1;
            let snap = map.snapshot_at_version(current_internal_version).await;

            assert!(snap.is_some());
        }

        #[tokio::test]
        async fn test_async_lock_profiles() {
            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.enable_profiling(true);

            map.insert("a".into(), 1).await;

            let profiles = map.lock_profiles().await;
            assert!(profiles.len() <= 8);
        }

        // ========== Replication Tests ==========

        struct MockReplica {
            operations: Arc<std::sync::Mutex<Vec<String>>>,
        }

        impl MockReplica {
            fn new() -> Self {
                Self {
                    operations: Arc::new(std::sync::Mutex::new(Vec::new())),
                }
            }

            fn get_operations(&self) -> Vec<String> {
                self.operations.lock().unwrap().clone()
            }
        }

        #[async_trait::async_trait]
        impl Replica<String, i32> for MockReplica {
            async fn replicate(&self, op: ReplicationOp<String, i32>) -> Result<(), ReplicaError> {
                let mut ops = self.operations.lock().unwrap();
                match op {
                    ReplicationOp::Insert { ref key, ref value } => {
                        ops.push(format!("INSERT:{}={}", key, value));
                    }
                    ReplicationOp::Remove { ref key } => {
                        ops.push(format!("REMOVE:{}", key));
                    }
                    ReplicationOp::Clear => {
                        ops.push("CLEAR".to_string());
                    }
                }
                Ok(())
            }

            async fn fetch_state(&self) -> Result<Vec<(String, i32)>, ReplicaError> {
                Ok(Vec::new())
            }

            async fn health_check(&self) -> Result<bool, ReplicaError> {
                Ok(true)
            }
        }

        #[tokio::test]
        async fn test_with_replication() {
            let replica1 = Arc::new(MockReplica::new());
            let replica2 = Arc::new(MockReplica::new());

            let replicas: Vec<Arc<dyn Replica<String, i32>>> = vec![
                replica1.clone() as Arc<dyn Replica<String, i32>>,
                replica2.clone() as Arc<dyn Replica<String, i32>>,
            ];

            let config = QuorumConfig::majority(3); // 2 replicas + 1 primary
            let map: AsyncShardedHashMap<String, i32, rustc_hash::FxBuildHasher> =
                AsyncShardedHashMap::with_replication(8, replicas, config);

            assert_eq!(map.len().await, 0);
        }

        #[tokio::test]
        async fn test_insert_replicated() {
            let replica1 = Arc::new(MockReplica::new());
            let replica2 = Arc::new(MockReplica::new());

            let replicas: Vec<Arc<dyn Replica<String, i32>>> = vec![
                replica1.clone() as Arc<dyn Replica<String, i32>>,
                replica2.clone() as Arc<dyn Replica<String, i32>>,
            ];

            let config = QuorumConfig::majority(3);
            let map: AsyncShardedHashMap<String, i32, rustc_hash::FxBuildHasher> =
                AsyncShardedHashMap::with_replication(8, replicas, config);

            let result = map.insert_replicated("key1".into(), 100).await;
            assert!(result.is_ok());
            assert_eq!(map.get(&"key1".into()).await, Some(100));

            // Check replicas received the operation
            let ops1 = replica1.get_operations();
            let ops2 = replica2.get_operations();
            assert_eq!(ops1.len(), 1);
            assert_eq!(ops2.len(), 1);
            assert!(ops1[0].contains("INSERT:key1=100"));
            assert!(ops2[0].contains("INSERT:key1=100"));
        }

        #[tokio::test]
        async fn test_remove_replicated() {
            let replica1 = Arc::new(MockReplica::new());

            let replicas: Vec<Arc<dyn Replica<String, i32>>> =
                vec![replica1.clone() as Arc<dyn Replica<String, i32>>];

            // With 1 replica, we need a config that requires quorum of 1
            // majority(1) = (1/2) + 1 = 0 + 1 = 1
            let config = QuorumConfig::majority(1);
            let map: AsyncShardedHashMap<String, i32, rustc_hash::FxBuildHasher> =
                AsyncShardedHashMap::with_replication(8, replicas, config);

            map.insert("key1".into(), 100).await;
            let result = map.remove_replicated(&"key1".into()).await;

            assert!(result.is_ok());
            let ops = replica1.get_operations();
            assert!(ops.iter().any(|op| op.contains("REMOVE:key1")));
        }

        #[tokio::test]
        async fn test_replication_quorum_config() {
            let config1 = QuorumConfig::majority(5);
            assert!(config1.is_valid());
            assert_eq!(config1.write_quorum, 3);
            assert_eq!(config1.read_quorum, 3);

            let config2 = QuorumConfig::strict(3);
            assert!(config2.is_valid());
            assert_eq!(config2.write_quorum, 3);
            assert_eq!(config2.read_quorum, 3);
        }

        #[tokio::test]
        async fn test_concurrent_cas_operations() {
            use tokio::task;

            let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(8);
            map.insert("shared".into(), 0).await;

            let mut tasks = vec![];
            for _ in 0..10 {
                let map_clone = map.clone();
                let task = task::spawn(async move {
                    for _ in 0..10 {
                        loop {
                            let current = map_clone.get(&"shared".into()).await.unwrap_or(0);
                            let result = map_clone
                                .compare_and_swap(&"shared".into(), &current, current + 1)
                                .await;
                            if result.is_success() {
                                break;
                            }
                        }
                    }
                });
                tasks.push(task);
            }

            for task in tasks {
                task.await.unwrap();
            }

            let final_value = map.get(&"shared".into()).await.unwrap();
            assert_eq!(final_value, 100);
        }
    }
}
