//! Core data types and configuration constants for Starshard.
//!
//! This module contains the shared types used across both synchronous and
//! asynchronous map implementations:
//!
//! - [`SnapshotMode`] — controls how iteration snapshots are produced
//!   (`Clone`, `Cached`, or `Cow`).
//! - [`ShardStats`] — runtime statistics about shard distribution and load.
//! - [`DEFAULT_SHARDS`] / [`MAX_SHARDS`] — default and upper-bound shard counts.

/// Default shard count (power-of-two not required; hashing modulo used).
pub const DEFAULT_SHARDS: usize = 64;

/// Default hard cap for shard slots in infallible constructors.
///
/// This guards against accidental or attacker-influenced oversized allocations.
/// If you need a different cap, use `with_shards_and_hasher_capped(...)` or
/// `try_with_shards_and_hasher_capped(...)`.
pub const MAX_SHARDS: usize = 262_144;

/// Snapshot construction mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SnapshotMode {
    /// Always rebuild snapshot content on demand.
    #[default]
    Clone,
    /// Reuse snapshot cache while no writes happen.
    Cached,
    /// Use per-shard copy-on-write versions for snapshot reads.
    Cow,
}

/// Statistics about shard distribution and utilization.
///
/// Fields:
/// - `initialized`: number of shards that have been allocated
/// - `total`: total configured shard slots
/// - `empty`: number of initialized shards with zero entries
/// - `avg_load`: average load across initialized shards
/// - `max_load`: maximum load in any shard
#[derive(Clone, Debug)]
pub struct ShardStats {
    /// Number of shards that have been allocated.
    pub initialized: usize,
    /// Total configured shard slots.
    pub total: usize,
    /// Number of initialized shards with zero entries.
    pub empty: usize,
    /// Average load across initialized shards.
    pub avg_load: f64,
    /// Maximum load in any shard.
    pub max_load: usize,
}

impl ShardStats {
    /// Returns shard utilization as a percentage (0-100).
    ///
    /// # Returns
    /// - `f64`: percentage of shards that have been initialized
    ///
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn utilization_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.initialized as f64 / self.total as f64) * 100.0
        }
    }
}
