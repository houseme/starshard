//! Version 1.0.0: Advanced Transactions, CAS Operations, and Distributed Support
//!
//! This module provides:
//! - MVCC-based atomic transactions
//! - Compare-and-swap (CAS) operations
//! - Copy-on-write snapshots for read-heavy workloads
//! - Distributed replication framework
//! - Lock profiling and diagnostics

use std::sync::Arc;

/// Result of a transaction execution.
#[derive(Debug, Clone)]
pub enum TransactionResult<T> {
    /// Transaction committed successfully with result
    Committed(T),
    /// Transaction aborted due to conflict
    Aborted,
    /// Transaction rolled back
    Conflict,
}

/// A single transaction operation.
#[derive(Debug, Clone)]
pub enum TxnOp<K, V> {
    /// Read a key
    Read(K),
    /// Write a key-value pair
    Write(K, V),
    /// Remove a key
    Remove(K),
}

/// Transaction context for coordinated multi-key operations.
pub struct Transaction<K, V> {
    /// Operations within transaction
    pub(crate) ops: Vec<TxnOp<K, V>>,
    /// Read set for conflict detection
    pub(crate) read_set: Vec<K>,
    /// Write set for conflict detection
    pub(crate) write_set: Vec<K>,
    /// Transaction version (epoch)
    #[allow(dead_code)]
    pub(crate) version: u64,
}

impl<K: Clone, V: Clone> Transaction<K, V> {
    /// Create new transaction
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Self {
        Self {
            ops: Vec::new(),
            read_set: Vec::new(),
            write_set: Vec::new(),
            version: 0,
        }
    }

    /// Add a read operation
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub fn read(&mut self, key: K) {
        self.read_set.push(key.clone());
        self.ops.push(TxnOp::Read(key));
    }

    /// Add a write operation
    #[tracing::instrument(skip(self, key, value), level = "trace")]
    pub fn write(&mut self, key: K, value: V) {
        self.write_set.push(key.clone());
        self.ops.push(TxnOp::Write(key, value));
    }

    /// Add a remove operation
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub fn remove(&mut self, key: K) {
        self.write_set.push(key.clone());
        self.ops.push(TxnOp::Remove(key));
    }

    /// Get operation count
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Check if transaction is empty
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

impl<K: Clone, V: Clone> Default for Transaction<K, V> {
    #[tracing::instrument(level = "trace")]
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a compare-and-swap operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasResult<V> {
    /// Swap succeeded
    Success(V),
    /// Swap failed - current value was different
    Failure(V),
}

impl<V> CasResult<V> {
    /// Check if operation succeeded
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn is_success(&self) -> bool {
        matches!(self, CasResult::Success(_))
    }

    /// Extract the value
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn into_value(self) -> V {
        match self {
            CasResult::Success(v) | CasResult::Failure(v) => v,
        }
    }
}

/// Copy-on-write snapshot for minimal locking during reads.
pub struct CowSnapshot<K, V> {
    /// Immutable data snapshot
    pub(crate) data: std::sync::Arc<Vec<(K, V)>>,
    /// Version number for consistency
    pub(crate) version: u64,
}

impl<K: Clone, V: Clone> CowSnapshot<K, V> {
    /// Create new CoW snapshot
    #[tracing::instrument(skip(data), level = "trace")]
    pub fn new(data: Vec<(K, V)>, version: u64) -> Self {
        Self {
            data: std::sync::Arc::new(data),
            version,
        }
    }

    /// Create snapshot from pre-built shared data.
    #[tracing::instrument(skip(data), level = "trace")]
    pub fn from_arc(data: std::sync::Arc<Vec<(K, V)>>, version: u64) -> Self {
        Self { data, version }
    }

    /// Get snapshot version
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Iterate over snapshot data
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn iter(&self) -> impl Iterator<Item = &(K, V)> {
        self.data.iter()
    }

    /// Get count of entries
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if snapshot is empty
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Replication error types.
#[derive(Debug, Clone)]
pub enum ReplicaError {
    /// Connection failed
    ConnectionFailed,
    /// Replication timeout
    Timeout,
    /// Replica rejected operation
    Rejected(String),
    /// Quorum not reached
    QuorumFailed,
}

/// Operations for replication.
#[derive(Debug, Clone)]
pub enum ReplicationOp<K, V> {
    /// Insert or update
    Insert {
        /// Key to insert or update.
        key: K,
        /// Value to store.
        value: V,
    },
    /// Remove entry
    Remove {
        /// Key to remove.
        key: K,
    },
    /// Clear all entries
    Clear,
}

/// Trait for implementing replicas.
#[async_trait::async_trait]
pub trait Replica<K, V>: Send + Sync
where
    K: Send,
    V: Send,
{
    /// Replicate an operation
    async fn replicate(&self, op: ReplicationOp<K, V>) -> Result<(), ReplicaError>;

    /// Fetch current state
    async fn fetch_state(&self) -> Result<Vec<(K, V)>, ReplicaError>;

    /// Verify replica health
    #[tracing::instrument(skip(self), level = "trace")]
    async fn health_check(&self) -> Result<bool, ReplicaError> {
        Ok(true)
    }
}

/// Lock profile for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct LockProfile {
    /// Shard identifier
    pub shard_id: usize,
    /// Number of contentions detected
    pub contention_count: u64,
    /// Average wait time in nanoseconds
    pub avg_wait_time_ns: u64,
    /// Maximum wait time in nanoseconds
    pub max_wait_time_ns: u64,
    /// Total read acquisitions
    pub reads: u64,
    /// Total write acquisitions
    pub writes: u64,
}

/// Snapshot with version for time-travel queries.
#[derive(Debug, Clone)]
pub struct IsolatedSnapshot<K, V> {
    /// Version number
    pub version: u64,
    /// Snapshot timestamp
    pub timestamp: std::time::Instant,
    /// Snapshotted data
    pub(crate) data: Arc<Vec<(K, V)>>,
}

impl<K, V> IsolatedSnapshot<K, V> {
    /// Create new isolated snapshot
    #[tracing::instrument(skip(data), level = "trace")]
    pub fn new(version: u64, data: Vec<(K, V)>) -> Self {
        Self {
            version,
            timestamp: std::time::Instant::now(),
            data: Arc::new(data),
        }
    }

    /// Get snapshot version
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Get snapshot age
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn age(&self) -> std::time::Duration {
        std::time::Instant::now().saturating_duration_since(self.timestamp)
    }

    /// Get entry count
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if empty
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Quorum-based consistency configuration.
#[derive(Debug, Clone)]
pub struct QuorumConfig {
    /// Total number of replicas (including primary)
    pub replica_count: usize,
    /// Minimum replicas that must acknowledge writes (must be > replica_count/2)
    pub write_quorum: usize,
    /// Minimum replicas for read quorum
    pub read_quorum: usize,
    /// Replication timeout
    pub timeout: std::time::Duration,
}

impl QuorumConfig {
    /// Create strict quorum config (all replicas)
    #[tracing::instrument(level = "trace")]
    pub fn strict(replica_count: usize) -> Self {
        Self {
            replica_count,
            write_quorum: replica_count,
            read_quorum: replica_count,
            timeout: std::time::Duration::from_secs(5),
        }
    }

    /// Create majority quorum config
    #[tracing::instrument(level = "trace")]
    pub fn majority(replica_count: usize) -> Self {
        let quorum = (replica_count / 2) + 1;
        Self {
            replica_count,
            write_quorum: quorum,
            read_quorum: quorum,
            timeout: std::time::Duration::from_secs(5),
        }
    }

    /// Validate configuration
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn is_valid(&self) -> bool {
        self.write_quorum > self.replica_count / 2
            && self.read_quorum > 0
            && self.write_quorum <= self.replica_count
            && self.read_quorum <= self.replica_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "async")]
    use crate::AsyncShardedHashMap;
    use crate::ShardedHashMap;

    #[test]
    fn transaction_creation() {
        let txn: Transaction<String, i32> = Transaction::new();
        assert!(txn.is_empty());
        assert_eq!(txn.len(), 0);
    }

    #[test]
    fn transaction_operations() {
        let mut txn: Transaction<String, i32> = Transaction::new();
        txn.read("a".into());
        txn.write("b".into(), 10);
        txn.remove("c".into());
        assert_eq!(txn.len(), 3);
    }

    #[test]
    fn cas_result_is_success() {
        let success: CasResult<i32> = CasResult::Success(42);
        assert!(success.is_success());

        let failure: CasResult<i32> = CasResult::Failure(42);
        assert!(!failure.is_success());
    }

    #[test]
    fn cow_snapshot_creation() {
        let data = vec![("a", 1), ("b", 2)];
        let snap = CowSnapshot::new(data, 1);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.version(), 1);
    }

    #[test]
    fn quorum_config_majority() {
        let config = QuorumConfig::majority(5);
        assert!(config.is_valid());
        assert_eq!(config.write_quorum, 3);
    }

    #[test]
    fn quorum_config_strict() {
        let config = QuorumConfig::strict(3);
        assert!(config.is_valid());
        assert_eq!(config.write_quorum, 3);
        assert_eq!(config.read_quorum, 3);
    }

    #[test]
    fn isolated_snapshot() {
        let data = vec![("a", 1), ("b", 2)];
        let snap = IsolatedSnapshot::new(1, data);
        assert_eq!(snap.version(), 1);
        assert_eq!(snap.len(), 2);
        assert!(snap.age() >= std::time::Duration::ZERO);
    }

    #[test]
    fn test_sync_transaction_execution() {
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

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn test_async_transaction_execution() {
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
}
