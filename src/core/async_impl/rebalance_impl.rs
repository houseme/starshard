//! Bounded migration and supervised Tokio execution.
use super::*;
use crate::rebalance::{BatchTimer, WorkerControl, WorkerLease, elapsed_ns, move_one};
use std::time::Instant;

impl<K, V, S> AsyncShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Compatibility status; a paused migration remains `migrating`.
    pub fn rebalance_status(&self) -> RebalanceStatus {
        self.rebalance_tracker.snapshot()
    }
    /// Current or last completed migration counters.
    pub fn rebalance_telemetry(&self) -> RebalanceTelemetry {
        self.rebalance_tracker.telemetry()
    }
    /// Pause after any currently publishing batch commits.
    pub async fn pause_rebalance(&self) -> bool {
        let _routing = self.routing_lock.write().await;
        self.rebalance_tracker.pause(true)
    }
    /// Resume a paused manual or supervised migration.
    pub async fn resume_rebalance(&self) -> bool {
        self.rebalance_tracker.pause(false)
    }
    /// Start online migration without moving entries. Existing migrations are retained.
    pub async fn start_rebalance_online(
        &self,
        new_shard_count: usize,
    ) -> Result<(), ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let _rebalance = self.rebalance_lock.lock().await;
        let _routing = self.routing_lock.write().await;
        if self.rebalance_tracker.is_migrating() || target == self.shard_count() {
            return Ok(());
        }
        let mut active = self.shards.write().await;
        let mut previous = self.previous_shards.write().await;
        let current = active.len();
        *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
        self.previous_shard_count.store(current, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        #[cfg(feature = "lifecycle")]
        self.lifecycle.mark_pending();
        self.rebalance_tracker.begin_with_counts(
            current,
            target,
            self.total_len.load(Ordering::Relaxed),
        );
        tracing::info!(
            from_shards = current,
            to_shards = target,
            "async rebalance started"
        );
        Ok(())
    }
    /// Complete up to `max_shards` source slots, preserving the original contract.
    pub async fn advance_rebalance(&self, max_shards: usize) -> usize {
        self.advance_rebalance_inner(max_shards, usize::MAX, 0, None, None)
            .await
            .completed_shards
    }
    /// Move a bounded number of entries and cooperatively check the pause budget
    /// at safe commit boundaries. Cancellation retains all committed data.
    pub async fn advance_rebalance_with_options(
        &self,
        options: &RebalanceOptions,
    ) -> RebalanceBatchReport {
        self.advance_rebalance_inner(
            usize::MAX,
            options.batch_size.max(1),
            options.max_pause_ns,
            None,
            None,
        )
        .await
    }
    async fn advance_rebalance_inner(
        &self,
        max_shards: usize,
        max_entries: usize,
        pause_ns: u64,
        expected: Option<u64>,
        control: Option<&WorkerControl>,
    ) -> RebalanceBatchReport {
        let mut work_keys = Vec::new();
        let mut loaded_index = None;
        let mut retired_entries = Vec::new();
        let mut retired_sources = Vec::new();
        let _rebalance = self.rebalance_lock.lock().await;
        let _routing = self.routing_lock.write().await;
        let telemetry = self.rebalance_tracker.telemetry();
        let mut report = RebalanceBatchReport::default();
        if expected.is_some_and(|generation| generation != telemetry.generation)
            || telemetry.paused
            || control.is_some_and(WorkerControl::cancelled)
            || max_shards == 0
        {
            return report;
        }
        if !self.rebalance_tracker.is_migrating() {
            report.complete = true;
            return report;
        }
        let timer = BatchTimer::new(&self.rebalance_tracker, telemetry.generation, pause_ns);
        // Both directory guards are acquired before any destructive commit.
        let mut active = self.shards.write().await;
        let mut previous = self.previous_shards.write().await;
        while report.completed_shards < max_shards && report.moved_entries < max_entries {
            if control.is_some_and(WorkerControl::cancelled) {
                break;
            }
            let index = self.rebalance_tracker.snapshot().moved_shards;
            let source = previous
                .as_ref()
                .and_then(|slots| slots.get(index))
                .and_then(Clone::clone);
            if loaded_index != Some(index) {
                if let Some(source) = source.as_ref() {
                    let source = source.read().await;
                    for key in source.keys().take(max_entries - report.moved_entries) {
                        work_keys.push(key.clone());
                        if pause_ns != 0 && elapsed_ns(timer.started) >= pause_ns {
                            break;
                        }
                    }
                }
                // Allocate retirement storage once for the prepared keys,
                // before committing any entry from this source batch.
                retired_entries.reserve(work_keys.len());
                loaded_index = Some(index);
            }
            let key = work_keys.pop();
            if let (Some(source), Some(key)) = (source, key) {
                let target_index = self.shard_index(&key);
                let target = active[target_index]
                    .get_or_insert_with(|| {
                        Arc::new(TokioRwLock::new(ShardData::new(
                            HashMap::with_hasher(self.hasher.clone()),
                            self.snapshot_mode,
                        )))
                    })
                    .clone();
                let mut target = target.write_owned().await;
                let mut source = source.write_owned().await;
                let target_map = &mut **target;
                let source_map = &mut **source;
                // Reserve empty targets before detaching any source entry.
                // No resident Hash or value Clone is needed for this reservation.
                if target_map.capacity() == 0 {
                    target_map.reserve(
                        telemetry
                            .initial_entries
                            .div_ceil(telemetry.to_shards)
                            .max(1),
                    );
                }
                retired_entries.push(move_one(target_map, source_map, key));
                self.rebalance_tracker.moved();
                report.moved_entries += 1;
            }
            // No source detachment precedes an await. Empty-source observation
            // can suspend safely: all entries are still represented in one map.
            let empty = if let Some(source) = previous
                .as_ref()
                .and_then(|slots| slots.get(index))
                .and_then(|slot| slot.as_ref())
            {
                source.read().await.is_empty()
            } else {
                true
            };
            if empty {
                retired_sources.reserve(1);
                if let Some(source) = previous
                    .as_mut()
                    .and_then(|slots| slots.get_mut(index))
                    .and_then(Option::take)
                {
                    retired_sources.push(source);
                }
                self.rebalance_tracker.step();
                report.completed_shards += 1;
            }
            let status = self.rebalance_tracker.snapshot();
            if status.moved_shards == status.total_shards {
                *previous = None;
                self.previous_shard_count.store(0, Ordering::Relaxed);
                self.rebalance_tracker.finish();
                report.complete = true;
                break;
            }
            if (!empty && work_keys.is_empty())
                || (pause_ns != 0 && elapsed_ns(timer.started) >= pause_ns)
            {
                report.budget_exhausted = true;
                break;
            }
        }
        report.elapsed_ns = elapsed_ns(timer.started);
        report.budget_exhausted |= !report.complete && report.moved_entries >= max_entries;
        tracing::debug!(
            generation = telemetry.generation,
            moved_entries = report.moved_entries,
            completed_shards = report.completed_shards,
            pause_ns = report.elapsed_ns,
            budget_exhausted = report.budget_exhausted,
            "async rebalance batch committed"
        );
        drop(previous);
        drop(active);
        drop(timer);
        drop(_routing);
        drop(_rebalance);
        drop(retired_sources);
        drop(retired_entries);
        drop(work_keys);
        report
    }
    /// Start a supervised Tokio task. A runtime must be entered; no implicit
    /// runtime is created. Dropping the handle stops pending work safely.
    pub async fn spawn_rebalance(
        &self,
        target: usize,
        options: RebalanceOptions,
    ) -> Result<AsyncRebalanceHandle, RebalanceError>
    where
        S: 'static,
    {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RebalanceError::RuntimeUnavailable)?;
        let target = strict_shard_count(target, MAX_SHARDS)?;
        // Clone user-provided hasher state before reserving worker ownership.
        let map = self.clone_for_worker();
        // Starting and reserving a generation share routing exclusion. A
        // concurrent manual rebalance cannot change the target between them.
        let (telemetry, already_complete, from_shards) = {
            let _rebalance = self.rebalance_lock.lock().await;
            let _routing = self.routing_lock.write().await;
            if self.rebalance_tracker.has_worker() {
                return Err(RebalanceError::WorkerAlreadyRunning);
            }
            let migrating = self.rebalance_tracker.is_migrating();
            let current = self.shard_count();
            if migrating && current != target {
                return Err(RebalanceError::TargetMismatch {
                    active: current,
                    requested: target,
                });
            }
            let already_complete = !migrating && current == target;
            if !migrating && !already_complete {
                let mut active = self.shards.write().await;
                let mut previous = self.previous_shards.write().await;
                *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
                self.previous_shard_count.store(current, Ordering::Relaxed);
                self.shard_count.store(target, Ordering::Relaxed);
                #[cfg(feature = "lifecycle")]
                self.lifecycle.mark_pending();
                self.rebalance_tracker.begin_with_counts(
                    current,
                    target,
                    self.total_len.load(Ordering::Relaxed),
                );
            }
            let telemetry = self.rebalance_tracker.telemetry();
            if !self.rebalance_tracker.claim_worker(telemetry.generation) {
                return Err(RebalanceError::WorkerAlreadyRunning);
            }
            let from = if already_complete {
                current
            } else {
                telemetry.from_shards
            };
            (telemetry, already_complete, from)
        };
        let tracker = self.rebalance_tracker.clone();
        let control = Arc::new(WorkerControl::new());
        let worker_tracker = tracker.clone();
        let worker_control = control.clone();
        let generation = telemetry.generation;
        // Construct lease before spawning: even cancellation before the first
        // task poll releases ownership when the captured future is destroyed.
        let lease = WorkerLease {
            tracker: worker_tracker.clone(),
            generation,
        };
        let worker = runtime.spawn(async move {
            let _lease = lease;
            let started = Instant::now();
            let mut moved = 0;
            let mut completed = already_complete;
            let mut effective = options.batch_size.max(1);
            while worker_tracker
                .wait_ready_async(generation, &worker_control)
                .await
            {
                let batch = map
                    .advance_rebalance_inner(
                        usize::MAX,
                        effective,
                        options.max_pause_ns,
                        Some(generation),
                        Some(&worker_control),
                    )
                    .await;
                moved += batch.moved_entries;
                if batch.complete {
                    completed = true;
                    break;
                }
                if options.max_pause_ns != 0 && batch.elapsed_ns > options.max_pause_ns {
                    effective = (effective / 2).max(1);
                    for _ in 0..4 {
                        tokio::task::yield_now().await;
                    }
                } else {
                    effective = effective.saturating_add(1).min(options.batch_size.max(1));
                }
                tokio::task::yield_now().await;
            }
            let final_state = worker_tracker.telemetry();
            RebalanceOutcome {
                completed: completed
                    || (final_state.generation == generation && final_state.complete),
                report: RebalanceReport {
                    from_shards,
                    to_shards: target,
                    moved_entries: moved,
                    elapsed_ms: started.elapsed().as_millis(),
                },
            }
        });
        Ok(AsyncRebalanceHandle {
            control,
            tracker,
            worker: Some(worker),
        })
    }

    /// Complete migration before returning. `background = false` stages a full
    /// replacement under exclusive routing. `background = true` applies entry/time
    /// budgets between cooperative batches; use `spawn_rebalance` for a worker.
    #[tracing::instrument(skip(self, options), level = "trace")]
    pub async fn rebalance_to(
        &self,
        new_shard_count: usize,
        options: RebalanceOptions,
    ) -> Result<RebalanceReport, ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        if options.background {
            let from = self.shard_count();
            let started = Instant::now();
            let mut moved = 0;
            loop {
                let complete = {
                    let _rebalance = self.rebalance_lock.lock().await;
                    let _routing = self.routing_lock.write().await;
                    let current = self.shard_count();
                    if self.rebalance_tracker.is_migrating() {
                        false
                    } else if current == target {
                        true
                    } else {
                        let mut active = self.shards.write().await;
                        let mut previous = self.previous_shards.write().await;
                        *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
                        self.previous_shard_count.store(current, Ordering::Relaxed);
                        self.shard_count.store(target, Ordering::Relaxed);
                        #[cfg(feature = "lifecycle")]
                        self.lifecycle.mark_pending();
                        self.rebalance_tracker.begin_with_counts(
                            current,
                            target,
                            self.total_len.load(Ordering::Relaxed),
                        );
                        false
                    }
                };
                if complete {
                    break;
                }
                self.resume_rebalance().await;
                moved += self
                    .advance_rebalance_with_options(&options)
                    .await
                    .moved_entries;
                tokio::task::yield_now().await;
            }
            return Ok(RebalanceReport {
                from_shards: from,
                to_shards: target,
                moved_entries: moved,
                elapsed_ms: started.elapsed().as_millis(),
            });
        }
        let _rebalance = self.rebalance_lock.lock().await;
        let _routing = self.routing_lock.write().await;
        let current = self.shard_count();
        if target == current && !self.rebalance_tracker.is_migrating() {
            return Ok(RebalanceReport {
                from_shards: current,
                to_shards: target,
                moved_entries: 0,
                elapsed_ms: 0,
            });
        }
        let started = Instant::now();
        let mut active = self.shards.write().await;
        let mut previous = self.previous_shards.write().await;
        let mut guards = Vec::new();
        if let Some(slots) = previous.as_ref() {
            for shard in slots.iter().flatten() {
                guards.push(shard.clone().read_owned().await);
            }
        }
        for shard in active.iter().flatten() {
            guards.push(shard.clone().read_owned().await);
        }
        let mut destinations: Vec<Option<HashMap<K, V, S>>> = (0..target).map(|_| None).collect();
        // Stage the entire new generation before replacing either directory.
        // Hash/Clone/Eq panics or cancellation leave both old generations intact.
        // Previous values are copied first so active values win any ties.
        for source in &guards {
            for (key, value) in source.iter() {
                let index = (self.hasher.hash_one(key) % target as u64) as usize;
                destinations[index]
                    .get_or_insert_with(|| HashMap::with_hasher(self.hasher.clone()))
                    .insert(key.clone(), value.clone());
            }
        }
        let moved_entries = destinations.iter().flatten().map(HashMap::len).sum();
        let replacements = destinations
            .into_iter()
            .map(|map| {
                map.map(|map| Arc::new(TokioRwLock::new(ShardData::new(map, self.snapshot_mode))))
            })
            .collect();
        let retired_active = std::mem::replace(&mut *active, replacements);
        let retired_previous = previous.take();
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        #[cfg(feature = "lifecycle")]
        self.lifecycle.mark_pending();
        self.total_len.store(moved_entries, Ordering::Relaxed);
        self.rebalance_tracker
            .finish_staged(current, target, moved_entries, started);
        drop(guards);
        drop(previous);
        drop(active);
        drop(_routing);
        drop(_rebalance);
        drop(retired_previous);
        drop(retired_active);
        tracing::info!(
            from_shards = current,
            to_shards = target,
            moved_entries,
            background = options.background,
            batch_size = options.batch_size,
            max_pause_ns = options.max_pause_ns,
            "async stop-the-world rebalance completed"
        );
        Ok(RebalanceReport {
            from_shards: current,
            to_shards: target,
            moved_entries,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}
