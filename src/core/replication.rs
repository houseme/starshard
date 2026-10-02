//! Ordered, bounded primary-first replication and early write acknowledgements.

use super::*;
use tokio::{
    sync::{OwnedMutexGuard, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout_at},
};

/// Shared by map clones, without keeping a map alive from a coordinator task.
#[derive(Default)]
pub(crate) struct ReplicationState {
    order: Arc<TokioMutex<()>>,
    worker: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl ReplicationState {
    fn start(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        // Register under the lock so an immediately completed worker cannot
        // race a subsequent registration and overwrite its live task handle.
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        *worker = Some(tokio::spawn(work));
    }
}

impl Drop for ReplicationState {
    fn drop(&mut self) {
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            worker.abort();
        }
    }
}

fn invalid_configuration(reason: String) -> ReplicaError {
    ReplicaError::Rejected(format!("invalid replication configuration: {reason}"))
}

fn validate_topology<K: Send, V: Send>(
    replicas: &[Arc<dyn Replica<K, V>>],
    config: &QuorumConfig,
) -> Result<(), ReplicaError> {
    if !config.is_valid() {
        return Err(invalid_configuration(
            "quorums must fit a nonempty topology, writes need a majority, and timeout must be nonzero"
                .into(),
        ));
    }
    if replicas.len().checked_add(1) != Some(config.replica_count) {
        return Err(invalid_configuration(
            "replica_count must equal the remote replica count plus one primary".into(),
        ));
    }
    if replicas.iter().enumerate().any(|(index, replica)| {
        replicas[..index]
            .iter()
            .any(|other| Arc::ptr_eq(replica, other))
    }) {
        return Err(invalid_configuration(
            "the same replica handle cannot be counted more than once".into(),
        ));
    }
    Ok(())
}

fn acknowledge(
    sender: &mut Option<oneshot::Sender<Result<(), ReplicaError>>>,
    result: Result<(), ReplicaError>,
) {
    if let Some(sender) = sender.take() {
        // A cancelled caller does not abandon already dispatched replica work.
        let _ = sender.send(result);
    }
}

async fn replicate_to_quorum<K, V>(
    replicas: Vec<Arc<dyn Replica<K, V>>>,
    operation: ReplicationOp<K, V>,
    quorum: usize,
    deadline: Instant,
    order: OwnedMutexGuard<()>,
    sender: oneshot::Sender<Result<(), ReplicaError>>,
) where
    K: Clone + Send + 'static,
    V: Clone + Send + 'static,
{
    let mut sender = Some(sender);
    let mut tasks = JoinSet::new();
    for replica in replicas {
        let operation = operation.clone();
        tasks.spawn(async move { replica.replicate(operation).await });
    }

    // The local application completed before this coordinator was started.
    let mut acknowledged = 1;
    if acknowledged >= quorum {
        acknowledge(&mut sender, Ok(()));
    }
    while !tasks.is_empty() {
        match timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(Ok(Ok(())))) => acknowledged += 1,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                acknowledge(&mut sender, Err(ReplicaError::Timeout));
                // Drop all pending replica futures before releasing ordering.
                tasks.shutdown().await;
                break;
            }
        }
        if acknowledged >= quorum {
            acknowledge(&mut sender, Ok(()));
        } else if acknowledged + tasks.len() < quorum {
            acknowledge(&mut sender, Err(ReplicaError::QuorumFailed));
        }
    }
    if sender.is_some() {
        acknowledge(&mut sender, Err(ReplicaError::QuorumFailed));
    }
    // Keep subsequent replicated local writes behind the complete fanout,
    // including cancellation cleanup, even after acknowledging early success.
    drop(order);
}

impl<K, V, S> AsyncShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Create a map with a validated replica topology.
    ///
    /// `replicas` contains remote replicas only. `replica_count` includes the
    /// primary. An empty remote list is valid only with `replica_count = 1`.
    /// Duplicate handles are rejected; callers must also ensure that distinct
    /// handles represent distinct remote nodes.
    ///
    /// # Errors
    /// Returns [`ReplicaError::Rejected`] for invalid quorum
    /// values, a zero timeout, or a topology/count mismatch, with an
    /// `invalid replication configuration` explanation.
    #[tracing::instrument(skip(replicas, quorum_config), level = "trace")]
    pub fn try_with_replication(
        shard_count: usize,
        replicas: Vec<Arc<dyn Replica<K, V>>>,
        quorum_config: QuorumConfig,
    ) -> Result<Self, ReplicaError>
    where
        S: Default,
    {
        validate_topology(&replicas, &quorum_config)?;
        let map = Self::with_shards_and_hasher(shard_count, S::default());
        *std_write_guard(&map.replicas, "with_replication") = replicas;
        *std_write_guard(&map.quorum_config, "with_replication_quorum") = Some(quorum_config);
        Ok(map)
    }

    /// Create a map with replication support.
    ///
    /// # Panics
    /// Panics for invalid configuration or topology. Use
    /// [`Self::try_with_replication`] to handle configuration errors.
    #[tracing::instrument(skip(replicas, quorum_config), level = "trace")]
    pub fn with_replication(
        shard_count: usize,
        replicas: Vec<Arc<dyn Replica<K, V>>>,
        quorum_config: QuorumConfig,
    ) -> Self
    where
        S: Default,
    {
        Self::try_with_replication(shard_count, replicas, quorum_config)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    /// Insert locally and wait for a primary-inclusive write quorum.
    ///
    /// The operation has one deadline, including waiting behind earlier
    /// replicated writes. It returns as soon as quorum is acknowledged; the
    /// remaining remote calls continue in a tracked task until completion or
    /// the deadline. Subsequent replicated writes wait for that fanout, keeping
    /// replica calls ordered across map clones. Dropping the last map cancels
    /// pending replica work.
    ///
    /// An error or caller cancellation does not roll back an applied local
    /// write or any remote write. Ordinary `insert`, `remove`, and reads remain
    /// local and do not participate in this ordering or quorum protocol. This
    /// API does not provide consensus, failover, or quorum reads.
    /// Configured replication requires a Tokio runtime with its time driver
    /// enabled, and replica futures must yield cooperatively.
    #[tracing::instrument(skip(self, key, value), level = "trace")]
    pub async fn insert_replicated(&self, key: K, value: V) -> Result<Option<V>, ReplicaError> {
        let config = std_read_guard(&self.quorum_config, "insert_replicated_quorum").clone();
        let Some(config) = config else {
            return Ok(self.insert(key, value).await);
        };
        let deadline = Instant::now().checked_add(config.timeout).ok_or_else(|| {
            invalid_configuration("timeout exceeds the supported clock range".into())
        })?;
        let order = timeout_at(
            deadline,
            Arc::clone(&self.replication_state.order).lock_owned(),
        )
        .await
        .map_err(|_| ReplicaError::Timeout)?;
        let old = timeout_at(deadline, self.insert(key.clone(), value.clone()))
            .await
            .map_err(|_| ReplicaError::Timeout)?;
        self.dispatch_replication(
            ReplicationOp::Insert { key, value },
            config,
            deadline,
            order,
        )
        .await?;
        Ok(old)
    }

    /// Remove locally and wait for a primary-inclusive write quorum.
    ///
    /// Has the same deadline, ordering, background fanout, and cancellation
    /// semantics as [`Self::insert_replicated`]. Failure does not undo the
    /// local removal or completed remote removals.
    #[tracing::instrument(skip(self, key), level = "trace")]
    pub async fn remove_replicated(&self, key: &K) -> Result<Option<V>, ReplicaError> {
        let config = std_read_guard(&self.quorum_config, "remove_replicated_quorum").clone();
        let Some(config) = config else {
            return Ok(self.remove(key).await);
        };
        let deadline = Instant::now().checked_add(config.timeout).ok_or_else(|| {
            invalid_configuration("timeout exceeds the supported clock range".into())
        })?;
        let order = timeout_at(
            deadline,
            Arc::clone(&self.replication_state.order).lock_owned(),
        )
        .await
        .map_err(|_| ReplicaError::Timeout)?;
        let old = timeout_at(deadline, self.remove(key))
            .await
            .map_err(|_| ReplicaError::Timeout)?;
        self.dispatch_replication(
            ReplicationOp::Remove { key: key.clone() },
            config,
            deadline,
            order,
        )
        .await?;
        Ok(old)
    }

    async fn dispatch_replication(
        &self,
        operation: ReplicationOp<K, V>,
        config: QuorumConfig,
        deadline: Instant,
        order: OwnedMutexGuard<()>,
    ) -> Result<(), ReplicaError> {
        let replicas = std_read_guard(&self.replicas, "dispatch_replication").clone();
        let (sender, receiver) = oneshot::channel();
        self.replication_state.start(replicate_to_quorum(
            replicas,
            operation,
            config.write_quorum,
            deadline,
            order,
            sender,
        ));
        receiver.await.unwrap_or(Err(ReplicaError::QuorumFailed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn queued_deadline_expires_without_applying_a_local_insert_or_removal() {
        let mut config = QuorumConfig::strict(1);
        config.timeout = Duration::from_millis(20);
        let map = AsyncShardedHashMap::<u64, u64>::with_replication(2, vec![], config);
        map.insert(1, 10).await;
        // Hold the ordering permit to isolate queue timeout from replica cleanup
        // and local shard locking; neither request can start local application.
        let _in_flight = map.replication_state.order.lock().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), map.insert_replicated(2, 20))
                .await
                .expect("queued insert did not honor its deadline"),
            Err(ReplicaError::Timeout)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), map.remove_replicated(&1))
                .await
                .expect("queued removal did not honor its deadline"),
            Err(ReplicaError::Timeout)
        );
        assert_eq!(map.get(&1).await, Some(10));
        assert_eq!(map.get(&2).await, None);
    }
}
