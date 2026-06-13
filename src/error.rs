//! Error types for Starshard constructor validation.
//!
//! Contains [`ShardCountError`], returned by strict constructors
//! (`try_with_shards_and_hasher`, `try_with_shards_and_hasher_capped`,
//! `rebalance_to`, `start_rebalance_online`) when the requested shard
//! count exceeds the configured maximum.

use std::fmt;

/// Error returned by strict shard-count constructors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardCountError {
    pub(crate) requested: usize,
    pub(crate) max_allowed: usize,
}

impl ShardCountError {
    /// Returns the requested effective shard count (after zero-normalization).
    pub fn requested(&self) -> usize {
        self.requested
    }

    /// Returns the maximum allowed shard count used for validation.
    pub fn max_allowed(&self) -> usize {
        self.max_allowed
    }
}

impl fmt::Display for ShardCountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requested shard_count {} exceeds max allowed {}",
            self.requested, self.max_allowed
        )
    }
}

impl std::error::Error for ShardCountError {}
