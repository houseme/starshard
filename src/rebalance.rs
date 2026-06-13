//! Rebalance types and internal progress tracking.
//!
//! This module provides the public data models for shard rebalancing:
//!
//! - [`RebalanceOptions`] — configuration knobs for rebalance execution.
//! - [`RebalanceReport`] — summary produced after a stop-the-world rebalance.
//! - [`RebalanceStatus`] — live progress snapshot during online migration.
//!
//! It also contains the internal [`RebalanceTracker`], an atomic state machine
//! that coordinates migration progress between `start_rebalance_online`,
//! `advance_rebalance`, and read/write paths that need to consult previous
//! shards during migration.

use std::sync::atomic::{AtomicUsize, Ordering};

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

pub(crate) const REBALANCE_STATE_IDLE: usize = 0;
pub(crate) const REBALANCE_STATE_MIGRATING: usize = 1;

/// Atomic state machine tracking online rebalance progress.
///
/// The tracker has two states: **idle** and **migrating**.  During migration
/// it records how many source shards have been drained so far.  Read and write
/// paths consult `is_migrating()` to decide whether to fall back to
/// `previous_shards` for keys that have not yet been moved.
#[derive(Debug)]
pub(crate) struct RebalanceTracker {
    state: AtomicUsize,
    moved_shards: AtomicUsize,
    total_shards: AtomicUsize,
}

impl RebalanceTracker {
    /// Creates a new tracker in the idle state.
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(REBALANCE_STATE_IDLE),
            moved_shards: AtomicUsize::new(0),
            total_shards: AtomicUsize::new(0),
        }
    }

    /// Transitions to the migrating state and records the number of source shards.
    pub(crate) fn begin(&self, total_shards: usize) {
        self.total_shards.store(total_shards, Ordering::Relaxed);
        self.moved_shards.store(0, Ordering::Relaxed);
        self.state
            .store(REBALANCE_STATE_MIGRATING, Ordering::Relaxed);
    }

    /// Increments the completed-shard counter (called after each shard drain).
    pub(crate) fn step(&self) {
        self.moved_shards.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns `true` if an online rebalance is currently in progress.
    pub(crate) fn is_migrating(&self) -> bool {
        self.state.load(Ordering::Relaxed) == REBALANCE_STATE_MIGRATING
    }

    /// Transitions back to idle and resets all counters.
    pub(crate) fn finish(&self) {
        self.state.store(REBALANCE_STATE_IDLE, Ordering::Relaxed);
        self.total_shards.store(0, Ordering::Relaxed);
        self.moved_shards.store(0, Ordering::Relaxed);
    }

    /// Returns a point-in-time snapshot of the current rebalance progress.
    pub(crate) fn snapshot(&self) -> RebalanceStatus {
        let state_num = self.state.load(Ordering::Relaxed);
        let moved = self.moved_shards.load(Ordering::Relaxed);
        let total = self.total_shards.load(Ordering::Relaxed);
        let progress = if total == 0 {
            0.0
        } else {
            moved as f64 / total as f64
        };
        RebalanceStatus {
            state: if state_num == REBALANCE_STATE_MIGRATING {
                "migrating"
            } else {
                "idle"
            },
            progress,
            moved_shards: moved,
            total_shards: total,
        }
    }
}
