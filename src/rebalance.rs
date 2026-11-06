//! Rebalance configuration, progress, telemetry, and supervised worker handles.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Rebalance execution options.
#[derive(Clone, Debug)]
pub struct RebalanceOptions {
    /// Request cooperative batches from `rebalance_to`; it still returns only
    /// after completion. Use `spawn_rebalance` for a supervised background task.
    pub background: bool,
    /// Maximum entries moved per cooperative batch. Zero is treated as one.
    pub batch_size: usize,
    /// Cooperative pause budget, checked between entry commits; zero disables it.
    /// Lock waits and user Hash/Clone/Drop cannot be preempted. Telemetry reports
    /// observed overruns rather than promising a hard real-time bound.
    pub max_pause_ns: u64,
}
impl Default for RebalanceOptions {
    fn default() -> Self {
        Self {
            background: false,
            batch_size: 1024,
            max_pause_ns: 0,
        }
    }
}

/// Rebalance execution report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebalanceReport {
    /// Previous shard slot count.
    pub from_shards: usize,
    /// New shard slot count.
    pub to_shards: usize,
    /// Entries moved by the reported execution.
    pub moved_entries: usize,
    /// Elapsed execution time in milliseconds.
    pub elapsed_ms: u128,
}

/// Compatibility status; idle status resets its shard counters.
#[derive(Clone, Debug, PartialEq)]
pub struct RebalanceStatus {
    /// `idle` or `migrating`; a paused migration remains `migrating`.
    pub state: &'static str,
    /// Fraction of fully drained source shards.
    pub progress: f64,
    /// Fully drained source shards.
    pub moved_shards: usize,
    /// Source shard count.
    pub total_shards: usize,
}

/// Result of a bounded entry migration batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebalanceBatchReport {
    /// Entries physically promoted by this batch (excluding foreground writes).
    pub moved_entries: usize,
    /// Source slots fully drained in this batch.
    pub completed_shards: usize,
    /// Time holding routing exclusively, including shard-lock preparation.
    pub elapsed_ns: u64,
    /// Entry or time budget stopped a still-incomplete migration.
    pub budget_exhausted: bool,
    /// This migration has completed.
    pub complete: bool,
}

/// Detailed current or most recently completed migration telemetry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebalanceTelemetry {
    /// Monotonically increasing migration identifier.
    pub generation: u64,
    /// Previous shard count.
    pub from_shards: usize,
    /// Destination shard count.
    pub to_shards: usize,
    /// Logical entry count observed when migration began.
    pub initial_entries: usize,
    /// Entries moved by migration batches; foreground promotion is excluded.
    pub moved_entries: usize,
    /// Fully drained source slots, retained after completion.
    pub completed_shards: usize,
    /// Original source slot count.
    pub total_shards: usize,
    /// Published batches, including partially committed batches that unwind.
    pub batches: usize,
    /// Most recent routing-exclusive interval.
    pub last_pause_ns: u64,
    /// Maximum routing-exclusive interval.
    pub max_pause_ns: u64,
    /// Batches that exceeded their requested time budget.
    pub budget_overruns: usize,
    /// Wall time since migration began, frozen at completion.
    pub elapsed_ns: u64,
    /// Explicitly paused at a safe batch boundary.
    pub paused: bool,
    /// A supervised worker owns this generation.
    pub worker_running: bool,
    /// Whether both-generation migration has finished.
    pub complete: bool,
}

/// Outcome returned by a supervised rebalance worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebalanceOutcome {
    /// False when cancelled or superseded; remaining data stays reachable.
    pub completed: bool,
    /// Work performed by this worker before stopping.
    pub report: RebalanceReport,
}

/// Failure to start or join a rebalance worker.
#[derive(Debug)]
pub enum RebalanceError {
    /// Invalid requested shard count.
    InvalidShardCount(crate::ShardCountError),
    /// A supervised worker already owns the migration.
    WorkerAlreadyRunning,
    /// An active migration targets a different shard count.
    TargetMismatch {
        /// Current destination shard count.
        active: usize,
        /// Requested destination shard count.
        requested: usize,
    },
    /// The operating system rejected thread creation.
    Spawn(std::io::Error),
    /// No Tokio runtime is entered for asynchronous worker creation.
    RuntimeUnavailable,
    /// User code panicked inside the worker. Committed entries remain reachable.
    WorkerPanicked,
    /// The asynchronous task was aborted, for example by runtime shutdown.
    /// Committed entries remain reachable and migration can be resumed.
    WorkerCancelled,
}
impl std::fmt::Display for RebalanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidShardCount(error) => error.fmt(f),
            Self::WorkerAlreadyRunning => f.write_str("a rebalance worker is already running"),
            Self::TargetMismatch { active, requested } => write!(
                f,
                "active migration targets {active} shards, requested {requested}"
            ),
            Self::Spawn(error) => write!(f, "cannot start rebalance worker: {error}"),
            Self::RuntimeUnavailable => {
                f.write_str("a Tokio runtime is required to spawn rebalance")
            }
            Self::WorkerPanicked => f.write_str("rebalance worker panicked"),
            Self::WorkerCancelled => f.write_str("rebalance worker task was cancelled"),
        }
    }
}
impl std::error::Error for RebalanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidShardCount(error) => Some(error),
            Self::Spawn(error) => Some(error),
            _ => None,
        }
    }
}
impl From<crate::ShardCountError> for RebalanceError {
    fn from(error: crate::ShardCountError) -> Self {
        Self::InvalidShardCount(error)
    }
}

pub(crate) fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Default)]
struct Progress {
    migrating: bool,
    started: Option<Instant>,
    worker_generation: Option<u64>,
    telemetry: RebalanceTelemetry,
}

/// Serializes generation, pause, and progress metadata.
#[derive(Debug)]
pub(crate) struct RebalanceTracker {
    state: Mutex<Progress>,
    changed: Condvar,
    #[cfg(feature = "async")]
    notified: tokio::sync::Notify,
}
impl RebalanceTracker {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Progress::default()),
            changed: Condvar::new(),
            #[cfg(feature = "async")]
            notified: tokio::sync::Notify::new(),
        }
    }
    #[cfg(test)]
    pub(crate) fn begin(&self, total_shards: usize) {
        self.begin_with_counts(total_shards, total_shards, 0);
    }
    pub(crate) fn begin_with_counts(&self, from: usize, to: usize, initial_entries: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let generation = state.telemetry.generation.wrapping_add(1);
        state.migrating = true;
        state.started = Some(Instant::now());
        state.telemetry = RebalanceTelemetry {
            generation,
            from_shards: from,
            to_shards: to,
            initial_entries,
            total_shards: from,
            ..RebalanceTelemetry::default()
        };
        drop(state);
        self.wake();
    }
    pub(crate) fn step(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .telemetry
            .completed_shards += 1;
    }
    /// Publish a successfully staged stop-the-world generation in one update.
    /// The original routing-lock acquisition time includes copying and hashing;
    /// staging failures leave the preceding generation's telemetry untouched.
    pub(crate) fn finish_staged(
        &self,
        from: usize,
        to: usize,
        moved_entries: usize,
        started: Instant,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = elapsed_ns(started);
        let generation = state.telemetry.generation.wrapping_add(1);
        state.migrating = false;
        state.started = None;
        state.telemetry = RebalanceTelemetry {
            generation,
            from_shards: from,
            to_shards: to,
            initial_entries: moved_entries,
            moved_entries,
            completed_shards: from,
            total_shards: from,
            batches: 1,
            last_pause_ns: elapsed,
            max_pause_ns: elapsed,
            elapsed_ns: elapsed,
            complete: true,
            ..RebalanceTelemetry::default()
        };
        drop(state);
        self.wake();
    }
    pub(crate) fn moved(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .telemetry
            .moved_entries += 1;
    }
    pub(crate) fn record_batch(&self, generation: u64, elapsed: u64, budget: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.telemetry.generation != generation {
            return;
        }
        let telemetry = &mut state.telemetry;
        telemetry.batches += 1;
        telemetry.last_pause_ns = elapsed;
        telemetry.max_pause_ns = telemetry.max_pause_ns.max(elapsed);
        telemetry.budget_overruns += usize::from(budget != 0 && elapsed > budget);
    }
    pub(crate) fn is_migrating(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .migrating
    }
    pub(crate) fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.migrating = false;
        state.telemetry.paused = false;
        state.telemetry.complete = true;
        if let Some(started) = state.started.take() {
            state.telemetry.elapsed_ns = elapsed_ns(started);
        }
        drop(state);
        self.wake();
    }
    pub(crate) fn snapshot(&self) -> RebalanceStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (moved, total) = if state.migrating {
            (
                state.telemetry.completed_shards,
                state.telemetry.total_shards,
            )
        } else {
            (0, 0)
        };
        RebalanceStatus {
            state: if state.migrating { "migrating" } else { "idle" },
            progress: if total == 0 {
                0.0
            } else {
                moved as f64 / total as f64
            },
            moved_shards: moved,
            total_shards: total,
        }
    }
    pub(crate) fn telemetry(&self) -> RebalanceTelemetry {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = state.telemetry.clone();
        if let Some(started) = state.started {
            result.elapsed_ns = elapsed_ns(started);
        }
        result.worker_running = state.worker_generation == Some(result.generation);
        result
    }
    pub(crate) fn pause(&self, paused: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.migrating {
            return false;
        }
        state.telemetry.paused = paused;
        drop(state);
        self.wake();
        true
    }
    pub(crate) fn has_worker(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_generation
            .is_some()
    }
    pub(crate) fn claim_worker(&self, generation: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_generation.is_some() || state.telemetry.generation != generation {
            return false;
        }
        state.worker_generation = Some(generation);
        true
    }
    pub(crate) fn release_worker(&self, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_generation == Some(generation) {
            state.worker_generation = None;
        }
        drop(state);
        self.wake();
    }
    pub(crate) fn wake(&self) {
        // Synchronize with Condvar's unlock-and-wait to avoid lost cancellation.
        drop(self.state.lock().unwrap_or_else(|e| e.into_inner()));
        self.changed.notify_all();
        #[cfg(feature = "async")]
        self.notified.notify_waiters();
    }
    pub(crate) fn wait_ready(&self, generation: u64, control: &WorkerControl) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.migrating
            && state.telemetry.generation == generation
            && state.telemetry.paused
            && !control.cancelled()
        {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.migrating && state.telemetry.generation == generation && !control.cancelled()
    }
    #[cfg(feature = "async")]
    pub(crate) async fn wait_ready_async(&self, generation: u64, control: &WorkerControl) -> bool {
        loop {
            let notified = self.notified.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let telemetry = self.telemetry();
            if control.cancelled() || telemetry.generation != generation || !self.is_migrating() {
                return false;
            }
            if !telemetry.paused {
                return true;
            }
            notified.await;
        }
    }
}

/// Publishes interval telemetry on normal exit and unwinding before routing unlock.
pub(crate) struct BatchTimer<'a> {
    tracker: &'a RebalanceTracker,
    generation: u64,
    pub(crate) started: Instant,
    budget: u64,
}
impl<'a> BatchTimer<'a> {
    pub(crate) fn new(tracker: &'a RebalanceTracker, generation: u64, budget: u64) -> Self {
        Self {
            tracker,
            generation,
            started: Instant::now(),
            budget,
        }
    }
}
impl Drop for BatchTimer<'_> {
    fn drop(&mut self) {
        self.tracker
            .record_batch(self.generation, elapsed_ns(self.started), self.budget);
    }
}

pub(crate) struct WorkerControl {
    cancelled: AtomicBool,
}
impl WorkerControl {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
        }
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
pub(crate) struct WorkerLease {
    pub(crate) tracker: Arc<RebalanceTracker>,
    pub(crate) generation: u64,
}
impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.tracker.release_worker(self.generation);
    }
}

/// Supervised thread worker. Drop requests cancellation without joining.
/// Cancellation is observed at a batch boundary; remaining data stays reachable.
pub struct RebalanceHandle {
    pub(crate) control: Arc<WorkerControl>,
    pub(crate) tracker: Arc<RebalanceTracker>,
    pub(crate) worker: Option<std::thread::JoinHandle<RebalanceOutcome>>,
}
impl RebalanceHandle {
    /// Idempotently request cancellation; use join to observe termination.
    pub fn cancel(&self) {
        self.control.cancelled.store(true, Ordering::Release);
        self.tracker.wake();
    }
    /// Wait for completion or cancellation, surfacing a worker panic.
    pub fn join(mut self) -> Result<RebalanceOutcome, RebalanceError> {
        self.worker
            .take()
            .expect("worker handle is consumed once")
            .join()
            .map_err(|_| RebalanceError::WorkerPanicked)
    }
}
impl Drop for RebalanceHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Supervised Tokio worker. Drop requests cancellation and aborts pending awaits.
#[cfg(feature = "async")]
pub struct AsyncRebalanceHandle {
    pub(crate) control: Arc<WorkerControl>,
    pub(crate) tracker: Arc<RebalanceTracker>,
    pub(crate) worker: Option<tokio::task::JoinHandle<RebalanceOutcome>>,
}
#[cfg(feature = "async")]
impl AsyncRebalanceHandle {
    /// Idempotently request cancellation at the next safe batch boundary.
    pub fn cancel(&self) {
        self.control.cancelled.store(true, Ordering::Release);
        self.tracker.wake();
    }
    /// Await completion or cooperative cancellation. Runtime shutdown or task
    /// abortion returns `WorkerCancelled`; a user panic returns `WorkerPanicked`.
    pub async fn join(mut self) -> Result<RebalanceOutcome, RebalanceError> {
        let result = self
            .worker
            .as_mut()
            .expect("worker handle is consumed once")
            .await;
        self.worker.take();
        result.map_err(|error| {
            if error.is_panic() {
                RebalanceError::WorkerPanicked
            } else {
                RebalanceError::WorkerCancelled
            }
        })
    }
}
#[cfg(feature = "async")]
impl Drop for AsyncRebalanceHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

/// Removed payloads are owned by the batch before map locks, so they are dropped
/// after locks are released, including when a later key's Hash/Clone panics.
pub(crate) struct RetiredEntry<K, V, S> {
    _key: K,
    _query: K,
    _value: Option<V>,
    _map: Option<hashbrown::HashMap<K, V, S>>,
}

pub(crate) fn move_one<K, V, S>(
    active: &mut hashbrown::HashMap<K, V, S>,
    source: &mut hashbrown::HashMap<K, V, S>,
    key: K,
) -> RetiredEntry<K, V, S>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
    S: std::hash::BuildHasher + Clone,
{
    let destination_key = key.clone();
    let mut staged = if active.len() == active.capacity() {
        let mut map = active.clone();
        map.reserve(1);
        Some(map)
    } else {
        None
    };
    let destination = staged
        .as_mut()
        .unwrap_or(&mut *active)
        .entry(destination_key);
    let (retired_key, value) = source
        .remove_entry(&key)
        .expect("routing and shard locks pin selected source key");
    let retired_value = match destination {
        hashbrown::hash_map::Entry::Vacant(entry) => {
            entry.insert(value);
            None
        }
        hashbrown::hash_map::Entry::Occupied(_) => Some(value),
    };
    let retired_map = staged.map(|replacement| std::mem::replace(active, replacement));
    RetiredEntry {
        _key: retired_key,
        _query: key,
        _value: retired_value,
        _map: retired_map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn concurrent_status_reads_never_mix_migration_generations() {
        let tracker = RebalanceTracker::new();
        let start = Barrier::new(5);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                for _ in 0..2000 {
                    for total in [64, 1] {
                        tracker.begin(total);
                        for _ in 0..total {
                            tracker.step();
                        }
                        tracker.finish();
                    }
                }
            });
            for _ in 0..4 {
                scope.spawn(|| {
                    start.wait();
                    for _ in 0..40_000 {
                        let status = tracker.snapshot();
                        assert!(status.moved_shards <= status.total_shards, "{status:?}");
                        assert!((0.0..=1.0).contains(&status.progress), "{status:?}");
                        if status.state == "idle" {
                            assert_eq!(status.moved_shards, 0);
                            assert_eq!(status.total_shards, 0);
                            assert_eq!(status.progress, 0.0);
                        } else {
                            assert_eq!(status.state, "migrating");
                            assert!([1, 64].contains(&status.total_shards));
                            assert_eq!(
                                status.progress,
                                status.moved_shards as f64 / status.total_shards as f64
                            );
                        }
                    }
                });
            }
        });
        assert!(!tracker.is_migrating());
        assert_eq!(tracker.snapshot().total_shards, 0);
    }
}
