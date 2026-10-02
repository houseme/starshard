#![cfg(all(feature = "async", feature = "advanced"))]

use std::{
    future::{Future, poll_fn},
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use starshard::{AsyncShardedHashMap, QuorumConfig, Replica, ReplicaError, ReplicationOp};
use tokio::sync::{Notify, Semaphore};

type Map = AsyncShardedHashMap<u64, u64>;

#[derive(Default)]
struct HealthyReplica {
    operations: Mutex<Vec<ReplicationOp<u64, u64>>>,
}

#[async_trait::async_trait]
impl Replica<u64, u64> for HealthyReplica {
    async fn replicate(&self, op: ReplicationOp<u64, u64>) -> Result<(), ReplicaError> {
        self.operations.lock().unwrap().push(op);
        Ok(())
    }

    async fn fetch_state(&self) -> Result<Vec<(u64, u64)>, ReplicaError> {
        Ok(vec![])
    }
}

struct FailedReplica;

#[async_trait::async_trait]
impl Replica<u64, u64> for FailedReplica {
    async fn replicate(&self, _: ReplicationOp<u64, u64>) -> Result<(), ReplicaError> {
        Err(ReplicaError::ConnectionFailed)
    }

    async fn fetch_state(&self) -> Result<Vec<(u64, u64)>, ReplicaError> {
        Err(ReplicaError::ConnectionFailed)
    }
}

struct PanickingReplica;

#[async_trait::async_trait]
impl Replica<u64, u64> for PanickingReplica {
    async fn replicate(&self, _: ReplicationOp<u64, u64>) -> Result<(), ReplicaError> {
        panic!("injected replica panic");
    }

    async fn fetch_state(&self) -> Result<Vec<(u64, u64)>, ReplicaError> {
        Ok(vec![])
    }
}

#[derive(Default)]
struct DeadlineOrderReplica {
    events: Mutex<Vec<&'static str>>,
    started: Notify,
    finished: Notify,
}

struct RecordCancellation<'a>(&'a Mutex<Vec<&'static str>>);

impl Drop for RecordCancellation<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("first dropped");
    }
}

#[async_trait::async_trait]
impl Replica<u64, u64> for DeadlineOrderReplica {
    async fn replicate(&self, op: ReplicationOp<u64, u64>) -> Result<(), ReplicaError> {
        if matches!(op, ReplicationOp::Insert { value: 10, .. }) {
            let _record = RecordCancellation(&self.events);
            self.events.lock().unwrap().push("first started");
            self.started.notify_one();
            std::future::pending::<()>().await;
        }
        self.events.lock().unwrap().push("second started");
        self.finished.notify_one();
        Ok(())
    }

    async fn fetch_state(&self) -> Result<Vec<(u64, u64)>, ReplicaError> {
        Ok(vec![])
    }
}

struct GatedReplica {
    gate: Semaphore,
    started: Notify,
    dropped: Semaphore,
    operations: Mutex<Vec<ReplicationOp<u64, u64>>>,
}

impl GatedReplica {
    fn new() -> Self {
        Self {
            gate: Semaphore::new(0),
            started: Notify::new(),
            dropped: Semaphore::new(0),
            operations: Mutex::new(vec![]),
        }
    }
}

struct Completion<'a>(&'a Semaphore);

impl Drop for Completion<'_> {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

#[async_trait::async_trait]
impl Replica<u64, u64> for GatedReplica {
    async fn replicate(&self, op: ReplicationOp<u64, u64>) -> Result<(), ReplicaError> {
        let _completion = Completion(&self.dropped);
        self.started.notify_one();
        self.gate.acquire().await.unwrap().forget();
        self.operations.lock().unwrap().push(op);
        Ok(())
    }

    async fn fetch_state(&self) -> Result<Vec<(u64, u64)>, ReplicaError> {
        Ok(vec![])
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("test hung")
}

#[tokio::test]
async fn strict_three_counts_the_primary_for_insert_and_remove() {
    let a = Arc::new(HealthyReplica::default());
    let b = Arc::new(HealthyReplica::default());
    let map = Map::with_replication(2, vec![a.clone(), b.clone()], QuorumConfig::strict(3));
    assert_eq!(bounded(map.insert_replicated(7, 11)).await, Ok(None));
    assert_eq!(bounded(map.remove_replicated(&7)).await, Ok(Some(11)));
    for replica in [a, b] {
        let operations = replica.operations.lock().unwrap();
        assert!(matches!(
            operations[0],
            ReplicationOp::Insert { key: 7, value: 11 }
        ));
        assert!(matches!(operations[1], ReplicationOp::Remove { key: 7 }));
    }
}

#[tokio::test]
async fn majority_succeeds_with_one_failed_remote() {
    let map = Map::with_replication(
        2,
        vec![Arc::new(FailedReplica), Arc::new(HealthyReplica::default())],
        QuorumConfig::majority(3),
    );
    assert_eq!(bounded(map.insert_replicated(1, 2)).await, Ok(None));
}

#[tokio::test]
async fn majority_returns_while_slow_fanout_continues_and_map_drop_cancels_it() {
    let slow = Arc::new(GatedReplica::new());
    let map = Map::with_replication(
        2,
        vec![slow.clone(), Arc::new(HealthyReplica::default())],
        QuorumConfig::majority(3),
    );
    assert_eq!(bounded(map.insert_replicated(1, 2)).await, Ok(None));
    bounded(slow.started.notified()).await;
    assert!(slow.operations.lock().unwrap().is_empty());
    drop(map);
    bounded(slow.dropped.acquire()).await.unwrap().forget();
}

#[tokio::test]
async fn one_map_clone_can_drop_without_cancelling_fanout_but_last_clone_cancels() {
    let slow = Arc::new(GatedReplica::new());
    let map = Map::with_replication(
        2,
        vec![slow.clone(), Arc::new(HealthyReplica::default())],
        QuorumConfig::majority(3),
    );
    let clone = map.clone();
    assert_eq!(bounded(map.insert_replicated(1, 10)).await, Ok(None));
    bounded(slow.started.notified()).await;
    drop(map);
    slow.gate.add_permits(1);
    bounded(slow.dropped.acquire()).await.unwrap().forget();
    assert_eq!(slow.operations.lock().unwrap().len(), 1);

    assert_eq!(bounded(clone.insert_replicated(1, 20)).await, Ok(Some(10)));
    bounded(slow.started.notified()).await;
    drop(clone);
    bounded(slow.dropped.acquire()).await.unwrap().forget();
    assert_eq!(slow.operations.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn expired_tail_is_dropped_before_the_next_remote_write_starts() {
    let slow = Arc::new(DeadlineOrderReplica::default());
    let mut config = QuorumConfig::majority(3);
    config.timeout = Duration::from_millis(200);
    let map = Map::with_replication(
        2,
        vec![slow.clone(), Arc::new(HealthyReplica::default())],
        config,
    );
    assert_eq!(bounded(map.insert_replicated(1, 10)).await, Ok(None));
    bounded(slow.started.notified()).await;
    // Give the queued write its own later deadline while the first remains pending.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut next = Box::pin(map.insert_replicated(1, 20));
    poll_fn(|cx| {
        assert!(next.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(bounded(next).await, Ok(Some(10)));
    bounded(slow.finished.notified()).await;
    assert_eq!(
        *slow.events.lock().unwrap(),
        vec!["first started", "first dropped", "second started"]
    );
}

#[tokio::test]
async fn replica_panic_is_a_failed_acknowledgement_and_does_not_panic_the_caller() {
    let strict =
        Map::with_replication(2, vec![Arc::new(PanickingReplica)], QuorumConfig::strict(2));
    assert_eq!(
        bounded(strict.insert_replicated(1, 2)).await,
        Err(ReplicaError::QuorumFailed)
    );
    assert_eq!(strict.get(&1).await, Some(2));
    let majority = Map::with_replication(
        2,
        vec![
            Arc::new(PanickingReplica),
            Arc::new(HealthyReplica::default()),
        ],
        QuorumConfig::majority(3),
    );
    assert_eq!(bounded(majority.insert_replicated(1, 2)).await, Ok(None));
}

#[tokio::test]
async fn replicated_operations_remain_ordered_after_early_quorum() {
    let slow = Arc::new(GatedReplica::new());
    let map = Map::with_replication(
        2,
        vec![slow.clone(), Arc::new(HealthyReplica::default())],
        QuorumConfig::majority(3),
    );
    assert_eq!(bounded(map.insert_replicated(1, 10)).await, Ok(None));
    bounded(slow.started.notified()).await;
    let clone = map.clone();
    let mut next = Box::pin(clone.insert_replicated(1, 20));
    poll_fn(|cx| {
        assert!(next.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(map.get(&1).await, Some(10));
    slow.gate.add_permits(2);
    assert_eq!(bounded(next).await, Ok(Some(10)));
    // Observe both completed fanouts before inspecting the remote operation log.
    bounded(slow.dropped.acquire()).await.unwrap().forget();
    bounded(slow.started.notified()).await;
    bounded(slow.dropped.acquire()).await.unwrap().forget();
    let operations = slow.operations.lock().unwrap();
    assert_eq!(operations.len(), 2);
    assert!(matches!(
        operations[0],
        ReplicationOp::Insert { value: 10, .. }
    ));
    assert!(matches!(
        operations[1],
        ReplicationOp::Insert { value: 20, .. }
    ));
}

#[tokio::test]
async fn timeout_is_bounded_and_does_not_roll_back_local_write() {
    let slow = Arc::new(GatedReplica::new());
    let mut config = QuorumConfig::strict(2);
    config.timeout = Duration::from_millis(50);
    let map = Map::with_replication(2, vec![slow.clone()], config);
    assert_eq!(
        bounded(map.insert_replicated(1, 2)).await,
        Err(ReplicaError::Timeout)
    );
    assert_eq!(map.get(&1).await, Some(2));
    bounded(slow.dropped.acquire()).await.unwrap().forget();
}

#[tokio::test]
async fn successful_quorum_still_cancels_unresponsive_tail_at_the_deadline() {
    let slow = Arc::new(GatedReplica::new());
    let mut config = QuorumConfig::majority(3);
    config.timeout = Duration::from_millis(50);
    let map = Map::with_replication(
        2,
        vec![slow.clone(), Arc::new(HealthyReplica::default())],
        config,
    );
    assert_eq!(bounded(map.insert_replicated(1, 2)).await, Ok(None));
    bounded(slow.started.notified()).await;
    bounded(slow.dropped.acquire()).await.unwrap().forget();
    assert!(slow.operations.lock().unwrap().is_empty());
    assert_eq!(map.get(&1).await, Some(2));
}

#[tokio::test]
async fn failed_quorum_does_not_roll_back_a_local_removal() {
    let map = Map::with_replication(2, vec![Arc::new(FailedReplica)], QuorumConfig::strict(2));
    map.insert(1, 2).await;
    assert_eq!(
        bounded(map.remove_replicated(&1)).await,
        Err(ReplicaError::QuorumFailed)
    );
    assert_eq!(map.get(&1).await, None);
}

#[tokio::test]
async fn cancelled_caller_keeps_dispatched_fanout_bounded_and_ordered() {
    let slow = Arc::new(GatedReplica::new());
    let map = Map::with_replication(2, vec![slow.clone()], QuorumConfig::strict(2));
    let mut request = Box::pin(map.insert_replicated(1, 10));
    poll_fn(|cx| {
        assert!(request.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    bounded(slow.started.notified()).await;
    drop(request);
    assert_eq!(map.get(&1).await, Some(10));
    let mut next = Box::pin(map.insert_replicated(1, 20));
    poll_fn(|cx| {
        assert!(next.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(map.get(&1).await, Some(10));
    slow.gate.add_permits(2);
    assert_eq!(bounded(next).await, Ok(Some(10)));
    let operations = slow.operations.lock().unwrap();
    assert_eq!(operations.len(), 2);
    assert!(matches!(
        operations[0],
        ReplicationOp::Insert { value: 10, .. }
    ));
    assert!(matches!(
        operations[1],
        ReplicationOp::Insert { value: 20, .. }
    ));
}

#[tokio::test]
async fn unconfigured_and_primary_only_maps_work_locally() {
    let primary = Map::try_with_replication(2, vec![], QuorumConfig::strict(1)).unwrap();
    for map in [Map::new(2), primary] {
        assert_eq!(bounded(map.insert_replicated(1, 10)).await, Ok(None));
        assert_eq!(bounded(map.remove_replicated(&1)).await, Ok(Some(10)));
    }
}

#[test]
fn invalid_topologies_are_rejected_before_construction() {
    let healthy = Arc::new(HealthyReplica::default());
    assert!(matches!(
        Map::try_with_replication(2, vec![healthy.clone()], QuorumConfig::majority(1)),
        Err(ReplicaError::Rejected(reason)) if reason.starts_with("invalid replication configuration:")
    ));
    assert!(Map::try_with_replication(2, vec![], QuorumConfig::strict(2)).is_err());
    assert!(Map::try_with_replication(2, vec![], QuorumConfig::strict(0)).is_err());
    assert!(
        Map::try_with_replication(2, vec![healthy.clone(), healthy], QuorumConfig::strict(3))
            .is_err()
    );
    for (write, read, duration) in [
        (1, 1, Duration::from_secs(1)),
        (4, 1, Duration::from_secs(1)),
        (2, 0, Duration::from_secs(1)),
        (2, 4, Duration::from_secs(1)),
        (2, 2, Duration::ZERO),
    ] {
        let config = QuorumConfig {
            replica_count: 3,
            write_quorum: write,
            read_quorum: read,
            timeout: duration,
        };
        assert!(!config.is_valid());
        assert!(
            Map::try_with_replication(
                2,
                vec![
                    Arc::new(HealthyReplica::default()),
                    Arc::new(HealthyReplica::default())
                ],
                config
            )
            .is_err()
        );
    }
}
