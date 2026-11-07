//! Supervised periodic maintenance with ownership separate from map storage.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// A failure to start or join periodic eviction work.
#[derive(Debug)]
#[non_exhaustive]
pub enum EvictionSchedulerError {
    /// The interval is zero or cannot be represented by the monotonic clock.
    InvalidInterval,
    /// Another worker is registered for this map's shared state.
    WorkerAlreadyRunning,
    /// Starting an asynchronous worker requires an entered Tokio runtime.
    RuntimeUnavailable,
    /// The entered Tokio runtime has no usable time driver.
    TimeDriverUnavailable,
    /// The operating system could not create a maintenance thread.
    Spawn(std::io::Error),
    /// A maintenance operation panicked.
    WorkerPanicked,
    /// The runtime shut down or cancelled a task without a requested stop.
    WorkerCancelled,
}

impl fmt::Display for EvictionSchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInterval => {
                f.write_str("eviction interval must be nonzero and representable")
            }
            Self::WorkerAlreadyRunning => f.write_str("an eviction worker is already running"),
            Self::RuntimeUnavailable => {
                f.write_str("a Tokio runtime is required for eviction scheduling")
            }
            Self::TimeDriverUnavailable => {
                f.write_str("eviction scheduling requires a Tokio time driver")
            }
            Self::Spawn(error) => write!(f, "cannot start eviction worker: {error}"),
            Self::WorkerPanicked => f.write_str("eviction worker panicked"),
            Self::WorkerCancelled => {
                f.write_str("eviction worker task was cancelled by its runtime")
            }
        }
    }
}

impl std::error::Error for EvictionSchedulerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            _ => None,
        }
    }
}

/// Why a worker was asked to stop, or why it terminated unexpectedly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvictionWorkerStop {
    /// Its handle requested cancellation.
    Cancelled,
    /// The last external map owner was dropped.
    OwnersDropped,
    /// A maintenance operation panicked.
    Panicked,
    /// A task was dropped by its runtime without an explicit stop request.
    RuntimeStopped,
    /// Thread creation failed before its worker could start.
    SpawnFailed,
}

/// Progress for the current or most recently finished eviction worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvictionWorkerStatus {
    /// Monotonically advancing worker registration identifier.
    pub generation: u64,
    /// Whether the worker still owns its registration, including cancellation cleanup.
    pub running: bool,
    /// Maintenance calls that returned normally.
    pub ticks: u64,
    /// Evictions reported by completed ticks; partial cancelled ticks are excluded.
    pub evicted: usize,
    /// Requested or final stop reason; it may be set before `running` becomes false.
    pub stop_reason: Option<EvictionWorkerStop>,
}

#[derive(Default)]
struct Registration {
    generation: u64,
    active: Option<Weak<Control>>,
    last: EvictionWorkerStatus,
}

/// Shared registration only; it never owns map storage or a worker join handle.
#[derive(Default)]
pub(crate) struct SchedulerRegistry {
    state: Mutex<Registration>,
}

impl SchedulerRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn status(&self) -> EvictionWorkerStatus {
        let (active, last) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.active.as_ref().and_then(Weak::upgrade),
                state.last.clone(),
            )
        };
        active.map_or(last, |control| control.status())
    }

    fn claim(
        self: &Arc<Self>,
        unexpected_stop: EvictionWorkerStop,
    ) -> Result<(Arc<Control>, WorkerLease), EvictionSchedulerError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(EvictionSchedulerError::WorkerAlreadyRunning);
        }
        state.generation = state.generation.wrapping_add(1);
        let control = Arc::new(Control::new(state.generation));
        state.active = Some(Arc::downgrade(&control));
        let lease = WorkerLease {
            registry: Arc::downgrade(self),
            control: control.clone(),
            active: true,
            unexpected_stop,
        };
        Ok((control, lease))
    }

    fn cancel(&self, reason: EvictionWorkerStop) {
        let control = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(control) = control {
            control.cancel(reason);
        }
    }

    pub(crate) fn start_sync(
        self: &Arc<Self>,
        interval: Duration,
        tick: impl FnMut() -> usize + Send + 'static,
    ) -> Result<EvictionHandle, EvictionSchedulerError> {
        self.start_sync_with(interval, tick, |work| {
            std::thread::Builder::new()
                .name("starshard-eviction".into())
                .spawn(work)
        })
    }

    fn start_sync_with<F, Spawn>(
        self: &Arc<Self>,
        interval: Duration,
        mut tick: F,
        spawn: Spawn,
    ) -> Result<EvictionHandle, EvictionSchedulerError>
    where
        F: FnMut() -> usize + Send + 'static,
        Spawn: FnOnce(
            Box<dyn FnOnce() -> EvictionWorkerStatus + Send>,
        ) -> std::io::Result<std::thread::JoinHandle<EvictionWorkerStatus>>,
    {
        validate_interval(interval)?;
        let (control, lease) = self.claim(EvictionWorkerStop::SpawnFailed)?;
        let worker_control = control.clone();
        // No registry lock spans spawn: failed spawning drops the captured lease.
        let worker = spawn(Box::new(move || {
            while worker_control.wait_interval(interval) {
                let evicted = tick();
                worker_control.record_tick(evicted);
            }
            lease.complete()
        }))
        .map_err(EvictionSchedulerError::Spawn)?;
        Ok(EvictionHandle {
            control,
            worker: Some(worker),
        })
    }

    #[cfg(feature = "async")]
    pub(crate) fn start_async<F, Fut>(
        self: &Arc<Self>,
        interval: Duration,
        mut tick: F,
    ) -> Result<AsyncEvictionHandle, EvictionSchedulerError>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = usize> + Send + 'static,
    {
        validate_interval(interval)?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| EvictionSchedulerError::RuntimeUnavailable)?;
        // Tokio has no public time-driver query. Probe only its timer constructor,
        // before registering or invoking user code; preserve a typed constructor error.
        let mut delay = std::panic::catch_unwind(|| Box::pin(tokio::time::sleep(interval)))
            .map_err(|_| EvictionSchedulerError::TimeDriverUnavailable)?;
        let (control, lease) = self.claim(EvictionWorkerStop::RuntimeStopped)?;
        let worker_control = control.clone();
        // Capturing the lease before spawn also covers shutdown before the first poll.
        let worker = runtime.spawn(async move {
            loop {
                if worker_control.until_stopped(delay.as_mut()).await.is_none() {
                    break;
                }
                if let Some(evicted) = worker_control.until_stopped(tick()).await {
                    worker_control.record_tick(evicted);
                } else {
                    break;
                }
                // Delay after completion: missed periods never create catch-up bursts.
                let Some(deadline) = tokio::time::Instant::now().checked_add(interval) else {
                    break;
                };
                delay.as_mut().reset(deadline);
            }
            lease.complete()
        });
        Ok(AsyncEvictionHandle {
            control,
            worker: Some(worker),
        })
    }
}

fn validate_interval(interval: Duration) -> Result<(), EvictionSchedulerError> {
    if interval.is_zero() || Instant::now().checked_add(interval).is_none() {
        Err(EvictionSchedulerError::InvalidInterval)
    } else {
        Ok(())
    }
}

struct Control {
    progress: Mutex<EvictionWorkerStatus>,
    changed: Condvar,
    #[cfg(feature = "async")]
    notified: tokio::sync::Notify,
}

impl Control {
    fn new(generation: u64) -> Self {
        Self {
            progress: Mutex::new(EvictionWorkerStatus {
                generation,
                running: true,
                ..Default::default()
            }),
            changed: Condvar::new(),
            #[cfg(feature = "async")]
            notified: tokio::sync::Notify::new(),
        }
    }

    fn status(&self) -> EvictionWorkerStatus {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn cancel(&self, reason: EvictionWorkerStop) {
        let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        if progress.running && progress.stop_reason.is_none() {
            progress.stop_reason = Some(reason);
        }
        drop(progress);
        self.changed.notify_all();
        #[cfg(feature = "async")]
        self.notified.notify_waiters();
    }

    fn record_tick(&self, evicted: usize) {
        let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        progress.ticks = progress.ticks.saturating_add(1);
        progress.evicted = progress.evicted.saturating_add(evicted);
    }

    fn wait_interval(&self, interval: Duration) -> bool {
        let progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let (progress, timeout) = self
            .changed
            .wait_timeout_while(progress, interval, |p| p.stop_reason.is_none())
            .unwrap_or_else(|e| e.into_inner());
        timeout.timed_out() && progress.stop_reason.is_none()
    }

    #[cfg(feature = "async")]
    async fn until_stopped<T>(&self, work: impl std::future::Future<Output = T>) -> Option<T> {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        let notified = self.notified.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let mut work = std::pin::pin!(work);
        poll_fn(|cx| {
            if self.status().stop_reason.is_some() {
                return Poll::Ready(None);
            }
            if notified.as_mut().poll(cx).is_ready() {
                return Poll::Ready(None);
            }
            work.as_mut().poll(cx).map(Some)
        })
        .await
    }
}

struct WorkerLease {
    registry: Weak<SchedulerRegistry>,
    control: Arc<Control>,
    active: bool,
    unexpected_stop: EvictionWorkerStop,
}

impl WorkerLease {
    fn finish(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let status = {
            let mut progress = self
                .control
                .progress
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            progress.running = false;
            if std::thread::panicking() {
                progress.stop_reason = Some(EvictionWorkerStop::Panicked);
            } else if progress.stop_reason.is_none() {
                progress.stop_reason = Some(self.unexpected_stop);
            }
            progress.clone()
        };
        if let Some(registry) = self.registry.upgrade() {
            let mut state = registry.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.generation == status.generation {
                state.active = None;
                state.last = status;
            }
        }
    }
    fn complete(mut self) -> EvictionWorkerStatus {
        self.finish();
        self.control.status()
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A supervised periodic maintenance thread. Drop requests cancellation, without joining.
/// A running synchronous maintenance call finishes before cancellation is observed.
pub struct EvictionHandle {
    control: Arc<Control>,
    worker: Option<std::thread::JoinHandle<EvictionWorkerStatus>>,
}

impl EvictionHandle {
    /// Request cancellation and wake an idle worker immediately.
    pub fn cancel(&self) {
        self.control.cancel(EvictionWorkerStop::Cancelled);
    }

    /// Wait for termination. Call [`Self::cancel`] first to stop a periodic worker.
    /// Returns an error if the maintenance operation panicked.
    pub fn join(mut self) -> Result<EvictionWorkerStatus, EvictionSchedulerError> {
        self.worker
            .take()
            .expect("worker handle is consumed once")
            .join()
            .map_err(|_| EvictionSchedulerError::WorkerPanicked)
    }
}

impl Drop for EvictionHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A supervised Tokio maintenance task. Drop requests cancellation and aborts pending work.
/// Cancellation never rolls back evictions that have already committed.
#[cfg(feature = "async")]
pub struct AsyncEvictionHandle {
    control: Arc<Control>,
    worker: Option<tokio::task::JoinHandle<EvictionWorkerStatus>>,
}

#[cfg(feature = "async")]
impl AsyncEvictionHandle {
    /// Request cooperative cancellation, including a pending maintenance future.
    pub fn cancel(&self) {
        self.control.cancel(EvictionWorkerStop::Cancelled);
    }

    /// Await termination, retaining abort ownership if this join future is dropped.
    /// Explicit cancellation returns its final status; runtime cancellation and
    /// maintenance panics produce distinct errors.
    pub async fn join(mut self) -> Result<EvictionWorkerStatus, EvictionSchedulerError> {
        let result = self
            .worker
            .as_mut()
            .expect("worker handle is consumed once")
            .await;
        self.worker.take();
        match result {
            Ok(status) => Ok(status),
            Err(error) if error.is_panic() => Err(EvictionSchedulerError::WorkerPanicked),
            Err(_) => match self.control.status().stop_reason {
                Some(EvictionWorkerStop::Cancelled | EvictionWorkerStop::OwnersDropped) => {
                    Ok(self.control.status())
                }
                _ => Err(EvictionSchedulerError::WorkerCancelled),
            },
        }
    }
}

#[cfg(feature = "async")]
impl Drop for AsyncEvictionHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

enum AutomaticHandle {
    Sync(EvictionHandle),
    #[cfg(feature = "async")]
    Async(AsyncEvictionHandle),
}

/// Owned only by external map clones, never by a maintenance or rebalance worker.
pub(crate) struct SchedulerOwner {
    registry: Arc<SchedulerRegistry>,
    automatic: Mutex<Option<AutomaticHandle>>,
}

impl SchedulerOwner {
    pub(crate) fn new(registry: Arc<SchedulerRegistry>) -> Self {
        Self {
            registry,
            automatic: Mutex::new(None),
        }
    }
    pub(crate) fn retain_sync(&self, handle: EvictionHandle) {
        let old = self
            .automatic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(AutomaticHandle::Sync(handle));
        drop(old);
    }
    #[cfg(feature = "async")]
    pub(crate) fn retain_async(&self, handle: AsyncEvictionHandle) {
        let old = self
            .automatic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(AutomaticHandle::Async(handle));
        drop(old);
    }
}

impl Drop for SchedulerOwner {
    fn drop(&mut self) {
        self.registry.cancel(EvictionWorkerStop::OwnersDropped);
        // Drop outside the mutex; async handle destruction also aborts pending awaits.
        let automatic = self
            .automatic
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match automatic {
            Some(AutomaticHandle::Sync(handle)) => drop(handle),
            #[cfg(feature = "async")]
            Some(AutomaticHandle::Async(handle)) => drop(handle),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const IDLE: Duration = Duration::from_secs(3600);
    const TICK: Duration = Duration::from_millis(1);
    const DEADLINE: Duration = Duration::from_secs(3);

    #[test]
    fn intervals_are_validated_without_claiming_a_worker() {
        let registry = Arc::new(SchedulerRegistry::new());
        for interval in [Duration::ZERO, Duration::MAX] {
            assert!(matches!(
                registry.start_sync(interval, || 0),
                Err(EvictionSchedulerError::InvalidInterval)
            ));
        }
        assert_eq!(registry.status(), EvictionWorkerStatus::default());
    }

    #[test]
    fn duplicate_start_is_rejected_until_cancelled_worker_is_joined() {
        let registry = Arc::new(SchedulerRegistry::new());
        let handle = registry.start_sync(IDLE, || 0).unwrap();
        assert!(matches!(
            registry.start_sync(IDLE, || 0),
            Err(EvictionSchedulerError::WorkerAlreadyRunning)
        ));
        handle.cancel();
        let status = handle.join().unwrap();
        assert!(!status.running);
        assert_eq!(status.stop_reason, Some(EvictionWorkerStop::Cancelled));
        assert_eq!(status.ticks, 0);
        let second = registry.start_sync(IDLE, || 0).unwrap();
        second.cancel();
        assert_eq!(second.join().unwrap().generation, status.generation + 1);
    }

    #[test]
    fn last_external_owner_stops_worker_even_when_explicit_handle_survives() {
        let registry = Arc::new(SchedulerRegistry::new());
        let owner = Arc::new(SchedulerOwner::new(registry.clone()));
        let other = owner.clone();
        let handle = registry.start_sync(IDLE, || 0).unwrap();
        drop(owner);
        assert_eq!(registry.status().stop_reason, None);
        drop(other);
        assert_eq!(
            handle.join().unwrap().stop_reason,
            Some(EvictionWorkerStop::OwnersDropped)
        );
    }

    struct DropSignal(mpsc::Sender<()>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn automatic_handle_does_not_form_an_ownership_cycle() {
        let registry = Arc::new(SchedulerRegistry::new());
        let owner = Arc::new(SchedulerOwner::new(registry.clone()));
        let (tx, rx) = mpsc::channel();
        let signal = DropSignal(tx);
        let handle = registry
            .start_sync(IDLE, move || {
                let _keep = &signal;
                0
            })
            .unwrap();
        owner.retain_sync(handle);
        drop(owner);
        rx.recv_timeout(DEADLINE)
            .expect("worker retained its captured state after owner drop");
        assert!(!registry.status().running);
        assert_eq!(
            registry.status().stop_reason,
            Some(EvictionWorkerStop::OwnersDropped)
        );
    }

    #[test]
    fn failed_spawn_releases_registration_before_returning_error() {
        let registry = Arc::new(SchedulerRegistry::new());
        let result = registry.start_sync_with(
            IDLE,
            || 0,
            |work| {
                drop(work);
                Err(std::io::Error::other("injected spawn failure"))
            },
        );
        assert!(matches!(result, Err(EvictionSchedulerError::Spawn(_))));
        assert!(!registry.status().running);
        assert_eq!(
            registry.status().stop_reason,
            Some(EvictionWorkerStop::SpawnFailed)
        );
        let handle = registry.start_sync(IDLE, || 0).unwrap();
        handle.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn tick_runs_outside_registry_lock_and_publishes_completed_work() {
        let registry = Arc::new(SchedulerRegistry::new());
        let tick_registry = registry.clone();
        let handle = registry
            .start_sync(TICK, move || {
                assert!(tick_registry.status().running);
                tick_registry.cancel(EvictionWorkerStop::Cancelled);
                7
            })
            .unwrap();
        let status = handle.join().unwrap();
        assert_eq!(status.ticks, 1);
        assert_eq!(status.evicted, 7);
    }

    #[test]
    fn maintenance_panic_is_observable_and_releases_registration() {
        let registry = Arc::new(SchedulerRegistry::new());
        let handle = registry
            .start_sync(TICK, || panic!("injected maintenance panic"))
            .unwrap();
        assert!(matches!(
            handle.join(),
            Err(EvictionSchedulerError::WorkerPanicked)
        ));
        assert_eq!(
            registry.status().stop_reason,
            Some(EvictionWorkerStop::Panicked)
        );
        let retry = registry.start_sync(IDLE, || 0).unwrap();
        retry.cancel();
        retry.join().unwrap();
    }

    #[cfg(feature = "async")]
    mod asynchronous {
        use super::*;
        use std::future::{Future, pending, poll_fn};
        use std::task::Poll;
        use tokio::sync::oneshot;

        #[test]
        fn missing_runtime_and_time_driver_are_typed_errors() {
            let registry = Arc::new(SchedulerRegistry::new());
            assert!(matches!(
                registry.start_async(IDLE, || async { 0 }),
                Err(EvictionSchedulerError::RuntimeUnavailable)
            ));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let _entered = runtime.enter();
            assert!(matches!(
                registry.start_async(IDLE, || async { 0 }),
                Err(EvictionSchedulerError::TimeDriverUnavailable)
            ));
            assert_eq!(registry.status(), EvictionWorkerStatus::default());
        }

        #[test]
        fn runtime_shutdown_releases_an_unpolled_worker_lease() {
            let registry = Arc::new(SchedulerRegistry::new());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let handle = {
                let _entered = runtime.enter();
                registry.start_async(IDLE, || async { 0 }).unwrap()
            };
            drop(runtime);
            assert!(!registry.status().running);
            assert_eq!(
                registry.status().stop_reason,
                Some(EvictionWorkerStop::RuntimeStopped)
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            assert!(matches!(
                runtime.block_on(handle.join()),
                Err(EvictionSchedulerError::WorkerCancelled)
            ));
            let retry = registry.start_sync(IDLE, || 0).unwrap();
            retry.cancel();
            retry.join().unwrap();
        }

        struct AsyncDropSignal(Option<oneshot::Sender<()>>);
        impl Drop for AsyncDropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        #[tokio::test]
        async fn cancelling_an_active_tick_drops_its_future_and_keeps_partial_counts_separate() {
            let registry = Arc::new(SchedulerRegistry::new());
            let (started, entered) = oneshot::channel();
            let (dropped, finished) = oneshot::channel();
            let mut signals = Some((started, dropped));
            let handle = registry
                .start_async(TICK, move || {
                    let (started, dropped) = signals.take().unwrap();
                    async move {
                        let _guard = AsyncDropSignal(Some(dropped));
                        let _ = started.send(());
                        pending::<usize>().await
                    }
                })
                .unwrap();
            tokio::time::timeout(DEADLINE, entered)
                .await
                .unwrap()
                .unwrap();
            handle.cancel();
            let status = tokio::time::timeout(DEADLINE, handle.join())
                .await
                .unwrap()
                .unwrap();
            finished.await.unwrap();
            assert_eq!(status.stop_reason, Some(EvictionWorkerStop::Cancelled));
            assert_eq!(status.ticks, 0);
        }

        #[tokio::test]
        async fn dropping_join_future_retains_abort_ownership() {
            let registry = Arc::new(SchedulerRegistry::new());
            let (started, entered) = oneshot::channel();
            let (dropped, finished) = oneshot::channel();
            let mut signals = Some((started, dropped));
            let handle = registry
                .start_async(TICK, move || {
                    let (started, dropped) = signals.take().unwrap();
                    async move {
                        let _guard = AsyncDropSignal(Some(dropped));
                        let _ = started.send(());
                        pending::<usize>().await
                    }
                })
                .unwrap();
            tokio::time::timeout(DEADLINE, entered)
                .await
                .unwrap()
                .unwrap();
            let mut joining = Box::pin(handle.join());
            let first_poll = poll_fn(|cx| Poll::Ready(joining.as_mut().poll(cx))).await;
            assert!(first_poll.is_pending());
            drop(joining);
            tokio::time::timeout(DEADLINE, finished)
                .await
                .unwrap()
                .unwrap();
            assert!(!registry.status().running);
            let next = registry.start_async(IDLE, || async { 0 }).unwrap();
            next.cancel();
            next.join().await.unwrap();
        }

        #[tokio::test]
        async fn automatic_owner_drop_aborts_pending_work_without_retaining_state() {
            let registry = Arc::new(SchedulerRegistry::new());
            let owner = Arc::new(SchedulerOwner::new(registry.clone()));
            let (dropped, finished) = oneshot::channel();
            let guard = AsyncDropSignal(Some(dropped));
            let handle = registry
                .start_async(IDLE, move || {
                    let _keep = &guard;
                    async { 0 }
                })
                .unwrap();
            owner.retain_async(handle);
            drop(owner);
            tokio::time::timeout(DEADLINE, finished)
                .await
                .unwrap()
                .unwrap();
            assert!(!registry.status().running);
            assert_eq!(
                registry.status().stop_reason,
                Some(EvictionWorkerStop::OwnersDropped)
            );
        }

        #[tokio::test]
        async fn manual_worker_observes_last_owner_drop_and_rejects_duplicates() {
            let registry = Arc::new(SchedulerRegistry::new());
            let owner = Arc::new(SchedulerOwner::new(registry.clone()));
            let other = owner.clone();
            let handle = registry.start_async(IDLE, || async { 0 }).unwrap();
            assert!(matches!(
                registry.start_async(IDLE, || async { 0 }),
                Err(EvictionSchedulerError::WorkerAlreadyRunning)
            ));
            drop(owner);
            assert_eq!(registry.status().stop_reason, None);
            drop(other);
            let status = tokio::time::timeout(DEADLINE, handle.join())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(status.stop_reason, Some(EvictionWorkerStop::OwnersDropped));
        }

        #[tokio::test]
        async fn task_panic_is_distinct_from_runtime_cancellation() {
            let registry = Arc::new(SchedulerRegistry::new());
            let handle = registry
                .start_async(TICK, || async { panic!("injected tick panic") })
                .unwrap();
            assert!(matches!(
                handle.join().await,
                Err(EvictionSchedulerError::WorkerPanicked)
            ));
            assert!(!registry.status().running);
            assert_eq!(
                registry.status().stop_reason,
                Some(EvictionWorkerStop::Panicked)
            );
        }
    }
}
