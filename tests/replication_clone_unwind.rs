#![cfg(all(feature = "async", feature = "advanced"))]

use starshard::{AsyncShardedHashMap, QuorumConfig, Replica, ReplicaError, ReplicationOp};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

const WAIT: Duration = Duration::from_secs(5);

struct CloneGate {
    clones: AtomicUsize,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    released: Mutex<bool>,
    ready: Condvar,
}

struct Release(Arc<CloneGate>);

impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.ready.notify_all();
    }
}

struct Value {
    number: u64,
    gate: Option<Arc<CloneGate>>,
}

impl Clone for Value {
    fn clone(&self) -> Self {
        if let Some(gate) = &self.gate
            // The first copy applies locally; the fourth prepares remote three.
            && gate.clones.fetch_add(1, Ordering::Relaxed) == 3
        {
            gate.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let released = gate.released.lock().unwrap();
            let (released, timeout) = gate
                .ready
                .wait_timeout_while(released, WAIT * 2, |released| !*released)
                .unwrap();
            let was_released = *released;
            drop(released);
            assert!(was_released && !timeout.timed_out(), "clone gate timed out");
            panic!("injected fanout clone panic");
        }
        Self {
            number: self.number,
            gate: self.gate.clone(),
        }
    }
}

#[derive(Default)]
struct Observed {
    started: Notify,
    values: Mutex<Vec<u64>>,
}

struct RecordingReplica(Arc<Observed>);

#[async_trait::async_trait]
impl Replica<u64, Value> for RecordingReplica {
    async fn replicate(&self, op: ReplicationOp<u64, Value>) -> Result<(), ReplicaError> {
        if let ReplicationOp::Insert { value, .. } = op {
            self.0.values.lock().unwrap().push(value.number);
            self.0.started.notify_one();
        }
        Ok(())
    }

    async fn fetch_state(&self) -> Result<Vec<(u64, Value)>, ReplicaError> {
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloning_failure_dispatches_no_partial_fanout_and_releases_ordering() {
    let (entered, waiting) = oneshot::channel();
    let gate = Arc::new(CloneGate {
        clones: AtomicUsize::new(0),
        entered: Mutex::new(Some(entered)),
        released: Mutex::new(false),
        ready: Condvar::new(),
    });
    // Release the deliberately blocked worker even when an assertion fails.
    let release = Release(gate.clone());
    let observed = Arc::new(Observed::default());
    let map = AsyncShardedHashMap::<u64, Value>::with_replication(
        2,
        vec![
            Arc::new(RecordingReplica(observed.clone())),
            Arc::new(RecordingReplica(observed.clone())),
            Arc::new(RecordingReplica(observed.clone())),
        ],
        QuorumConfig::strict(4),
    );
    let writer_map = map.clone();
    let failed = tokio::spawn(async move {
        writer_map
            .insert_replicated(
                1,
                Value {
                    number: 10,
                    gate: Some(gate),
                },
            )
            .await
    });
    tokio::time::timeout(WAIT, waiting).await.unwrap().unwrap();
    // A third replica forces Tokio's first local LIFO task into the stealable
    // queue before cloning blocks. Another worker can then expose partial fanout.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), observed.started.notified())
            .await
            .is_err(),
        "remote work began before all operation copies were prepared"
    );
    assert!(observed.values.lock().unwrap().is_empty());
    drop(release);
    assert!(matches!(
        tokio::time::timeout(WAIT, failed).await.unwrap().unwrap(),
        Err(ReplicaError::QuorumFailed)
    ));
    assert_eq!(map.get(&1).await.unwrap().number, 10);

    let previous = tokio::time::timeout(
        WAIT,
        map.insert_replicated(
            1,
            Value {
                number: 20,
                gate: None,
            },
        ),
    )
    .await
    .expect("cloning panic did not release replication ordering")
    .unwrap();
    assert_eq!(previous.unwrap().number, 10);
    assert_eq!(map.get(&1).await.unwrap().number, 20);
    assert_eq!(*observed.values.lock().unwrap(), vec![20, 20, 20]);
}
