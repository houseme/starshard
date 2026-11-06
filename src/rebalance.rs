//! Rebalance types and internal progress tracking.
//!
//! This module provides the public data models for shard rebalancing:
//!
//! - [`RebalanceOptions`] — configuration knobs for rebalance execution.
//! - [`RebalanceReport`] — summary produced after a stop-the-world rebalance.
//! - [`RebalanceStatus`] — live progress snapshot during online migration.
//!
//! It also contains the internal [`RebalanceTracker`], a synchronized state machine
//! that coordinates migration progress between `start_rebalance_online`,
//! `advance_rebalance`, and read/write paths that need to consult previous
//! shards during migration.

use std::sync::Mutex;

/// Rebalance execution options.
#[derive(Clone, Debug)]
pub struct RebalanceOptions {
    /// Reserved for future online/background mode.
    pub background: bool,
    /// Reserved for future batched migration mode.
    pub batch_size: usize,
    /// Reserved for future pause-budget mode.
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
    /// Number of moved entries.
    pub moved_entries: usize,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u128,
}

/// Rebalance runtime status snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct RebalanceStatus {
    /// Current state label: `idle` or `migrating`.
    pub state: &'static str,
    /// Progress in range `[0.0, 1.0]`.
    pub progress: f64,
    /// Number of completed shard migrations.
    pub moved_shards: usize,
    /// Number of total source shards for current migration.
    pub total_shards: usize,
}

/// State machine tracking online rebalance progress.
///
/// The tracker has two states: **idle** and **migrating**.  During migration
/// it records how many source shards have been drained so far.  Read and write
/// paths consult `is_migrating()` to decide whether to fall back to
/// `previous_shards` for keys that have not yet been moved.
#[derive(Debug)]
pub(crate) struct RebalanceTracker {
    // Publish the generation and its counters together. The guard never
    // escapes a tracker method or overlaps acquisition of another map lock.
    state: Mutex<Progress>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Progress {
    migrating: bool,
    moved_shards: usize,
    total_shards: usize,
}

impl RebalanceTracker {
    /// Creates a new tracker in the idle state.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Progress::default()),
        }
    }

    /// Transitions to the migrating state and records the number of source shards.
    pub(crate) fn begin(&self, total_shards: usize) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Progress {
            migrating: true,
            moved_shards: 0,
            total_shards,
        };
    }

    /// Increments the completed-shard counter (called after each shard drain).
    pub(crate) fn step(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .moved_shards += 1;
    }

    /// Returns `true` if an online rebalance is currently in progress.
    pub(crate) fn is_migrating(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .migrating
    }

    /// Transitions back to idle and resets all counters.
    pub(crate) fn finish(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Progress::default();
    }

    /// Returns a point-in-time snapshot of the current rebalance progress.
    pub(crate) fn snapshot(&self) -> RebalanceStatus {
        let state = *self.state.lock().unwrap_or_else(|e| e.into_inner());
        let moved = state.moved_shards;
        let total = state.total_shards;
        let progress = if total == 0 {
            0.0
        } else {
            moved as f64 / total as f64
        };
        RebalanceStatus {
            state: if state.migrating { "migrating" } else { "idle" },
            progress,
            moved_shards: moved,
            total_shards: total,
        }
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
