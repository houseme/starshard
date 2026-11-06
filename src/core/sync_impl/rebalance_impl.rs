//! Bounded migration and supervised background execution.
use super::*;
use crate::rebalance::{BatchTimer, WorkerControl, WorkerLease, elapsed_ns, move_one};
use std::time::Instant;

impl<K, V, S> ShardedHashMap<K, V, S>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
    S: BuildHasher + Clone + Send + Sync,
{
    /// Compatibility status; a paused migration remains `migrating`.
    pub fn rebalance_status(&self) -> RebalanceStatus {
        self.rebalance_tracker.snapshot()
    }
    /// Detailed counters for the current or last completed generation.
    pub fn rebalance_telemetry(&self) -> RebalanceTelemetry {
        self.rebalance_tracker.telemetry()
    }
    /// Pause after any in-flight batch commits. Data remains readable and writable.
    pub fn pause_rebalance(&self) -> bool {
        let _routing = std_write_guard(&self.routing_lock, "pause_rebalance");
        self.rebalance_tracker.pause(true)
    }
    /// Allow manual or supervised advancement to resume.
    pub fn resume_rebalance(&self) -> bool {
        self.rebalance_tracker.pause(false)
    }
    /// Start online migration without moving entries. Existing migrations are retained.
    pub fn start_rebalance_online(&self, new_shard_count: usize) -> Result<(), ShardCountError> {
        let target = strict_shard_count(new_shard_count, MAX_SHARDS)?;
        let _routing = std_write_guard(&self.routing_lock, "routing_rebalance");
        if self.rebalance_tracker.is_migrating() || target == self.shard_count() {
            return Ok(());
        }
        let mut active = std_write_guard(&self.shards, "shards");
        let mut previous = std_write_guard(&self.previous_shards, "previous_shards");
        let current = active.len();
        *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
        self.previous_shard_count.store(current, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker
            .begin_with_counts(current, target, self.len());
        tracing::info!(
            from_shards = current,
            to_shards = target,
            "rebalance started"
        );
        Ok(())
    }
    /// Complete up to `max_shards` source slots, preserving the original shard-count contract.
    /// Use `advance_rebalance_with_options` for a bounded entry/time batch.
    pub fn advance_rebalance(&self, max_shards: usize) -> usize {
        self.advance_rebalance_inner(max_shards, usize::MAX, 0, None, None)
            .completed_shards
    }
    /// Move at most `batch_size.max(1)` entries, checking the pause budget at
    /// each safe commit boundary. A single user operation may exceed the budget.
    pub fn advance_rebalance_with_options(
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
    }
    fn advance_rebalance_inner(
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
        let _routing = std_write_guard(&self.routing_lock, "routing_rebalance");
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
        while report.completed_shards < max_shards && report.moved_entries < max_entries {
            if control.is_some_and(WorkerControl::cancelled) {
                break;
            }
            let index = self.rebalance_tracker.snapshot().moved_shards;
            let source = std_read_guard(&self.previous_shards, "migration_source_directory")
                .as_ref()
                .and_then(|slots| slots.get(index))
                .and_then(Clone::clone);
            if loaded_index != Some(index) {
                if let Some(source) = source.as_ref() {
                    let source = std_read_guard(source, "migration_keys");
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
                let target = self.get_or_init_shard(target_index);
                let mut target = std_write_guard(&target, "migration_target");
                let mut source = std_write_guard(&source, "migration_source");
                // Resolve COW clones before detaching data; move_one prepares
                // destination growth and hashing before its synchronous commit.
                let target_map = &mut **target;
                let source_map = &mut **source;
                // Empty targets can reserve without hashing resident keys.
                // Uniform routing avoids repeated growth copies; skewed or
                // already populated targets still use move_one's safe fallback.
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
            let empty = std_read_guard(&self.previous_shards, "migration_empty_directory")
                .as_ref()
                .and_then(|slots| slots.get(index))
                .and_then(|slot| slot.as_ref())
                .is_none_or(|source| std_read_guard(source, "migration_empty").is_empty());
            if empty {
                retired_sources.reserve(1);
                if let Some(source) = std_write_guard(&self.previous_shards, "migration_remove")
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
                *std_write_guard(&self.previous_shards, "migration_finish") = None;
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
            "rebalance batch committed"
        );
        drop(timer);
        drop(_routing);
        drop(retired_sources);
        drop(retired_entries);
        drop(work_keys);
        report
    }
    /// Spawn one supervised worker for the current/new migration. Dropping the
    /// handle cancels future batches; join observes termination and user panics.
    pub fn spawn_rebalance(
        &self,
        target: usize,
        options: RebalanceOptions,
    ) -> Result<RebalanceHandle, RebalanceError>
    where
        K: 'static,
        V: 'static,
        S: 'static,
    {
        let target = strict_shard_count(target, MAX_SHARDS)?;
        // Clone user-provided hasher state before reserving worker ownership.
        let map = self.clone();
        // Starting and reserving a generation share routing exclusion. A
        // concurrent manual rebalance cannot change the target between them.
        let (telemetry, already_complete, from_shards) = {
            let _routing = std_write_guard(&self.routing_lock, "spawn_rebalance");
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
                let mut active = std_write_guard(&self.shards, "spawn_rebalance_active");
                let mut previous =
                    std_write_guard(&self.previous_shards, "spawn_rebalance_previous");
                *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
                self.previous_shard_count.store(current, Ordering::Relaxed);
                self.shard_count.store(target, Ordering::Relaxed);
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
        let worker_control = control.clone();
        let worker_tracker = tracker.clone();
        let generation = telemetry.generation;
        let lease = WorkerLease {
            tracker: tracker.clone(),
            generation,
        };
        let result = std::thread::Builder::new()
            .name("starshard-rebalance".into())
            .spawn(move || {
                let _lease = lease;
                let started = Instant::now();
                let mut moved = 0;
                let mut completed = already_complete;
                let mut effective = options.batch_size.max(1);
                while worker_tracker.wait_ready(generation, &worker_control) {
                    let batch = map.advance_rebalance_inner(
                        usize::MAX,
                        effective,
                        options.max_pause_ns,
                        Some(generation),
                        Some(&worker_control),
                    );
                    moved += batch.moved_entries;
                    if batch.complete {
                        completed = true;
                        break;
                    }
                    if options.max_pause_ns != 0 && batch.elapsed_ns > options.max_pause_ns {
                        effective = (effective / 2).max(1);
                        std::thread::sleep(std::time::Duration::from_nanos(
                            (batch.elapsed_ns - options.max_pause_ns).min(1_000_000),
                        ));
                    } else {
                        effective = effective.saturating_add(1).min(options.batch_size.max(1));
                    }
                    std::thread::yield_now();
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
        match result {
            Ok(worker) => Ok(RebalanceHandle {
                control,
                tracker,
                worker: Some(worker),
            }),
            Err(error) => {
                tracker.release_worker(generation);
                Err(RebalanceError::Spawn(error))
            }
        }
    }

    /// Completes a rebalance before returning. With `background = false` the
    /// replacement generation is staged atomically under exclusive routing.
    /// With `background = true`, entry/time budgets apply to cooperative batches;
    /// this method still waits for completion. Use `spawn_rebalance` for a worker.
    pub fn rebalance_to(
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
                    let _routing = std_write_guard(&self.routing_lock, "cooperative_rebalance");
                    let current = self.shard_count();
                    if self.rebalance_tracker.is_migrating() {
                        false
                    } else if current == target {
                        true
                    } else {
                        let mut active = std_write_guard(&self.shards, "cooperative_active");
                        let mut previous =
                            std_write_guard(&self.previous_shards, "cooperative_previous");
                        *previous = Some(std::mem::replace(&mut *active, vec![None; target]));
                        self.previous_shard_count.store(current, Ordering::Relaxed);
                        self.shard_count.store(target, Ordering::Relaxed);
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
                self.resume_rebalance();
                moved += self.advance_rebalance_with_options(&options).moved_entries;
                std::thread::yield_now();
            }
            return Ok(RebalanceReport {
                from_shards: from,
                to_shards: target,
                moved_entries: moved,
                elapsed_ms: started.elapsed().as_millis(),
            });
        }
        let _routing = std_write_guard(&self.routing_lock, "routing_rebalance");
        let current = self.shard_count();
        let started = Instant::now();
        if target == current && !self.rebalance_tracker.is_migrating() {
            return Ok(RebalanceReport {
                from_shards: current,
                to_shards: target,
                moved_entries: 0,
                elapsed_ms: 0,
            });
        }
        let mut maps: Vec<Option<HashMap<K, V, S>>> = (0..target).map(|_| None).collect();
        let mut moved = 0;
        for shard in self.all_shards() {
            for (key, value) in std_read_guard(&shard, "rebalance_source").iter() {
                let index = (self.hasher.hash_one(key) % target as u64) as usize;
                let map =
                    maps[index].get_or_insert_with(|| HashMap::with_hasher(self.hasher.clone()));
                if let hashbrown::hash_map::Entry::Vacant(entry) = map.entry(key.clone()) {
                    entry.insert(value.clone());
                    moved += 1;
                }
            }
        }
        let slots = maps
            .into_iter()
            .map(|map| map.map(|m| Arc::new(StdRwLock::new(ShardData::new(m, self.snapshot_mode)))))
            .collect();
        let old_active = std::mem::replace(
            &mut *std_write_guard(&self.shards, "rebalance_publish"),
            slots,
        );
        let old_previous = std_write_guard(&self.previous_shards, "rebalance_previous").take();
        self.previous_shard_count.store(0, Ordering::Relaxed);
        self.shard_count.store(target, Ordering::Relaxed);
        self.rebalance_tracker
            .finish_staged(current, target, moved, started);
        drop(_routing);
        drop(old_active);
        drop(old_previous);
        Ok(RebalanceReport {
            from_shards: current,
            to_shards: target,
            moved_entries: moved,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}
