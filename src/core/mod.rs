//! Core implementation module for Starshard.
//!
//! This `pub(crate)` module houses the concrete `ShardedHashMap` and
//! `AsyncShardedHashMap` method implementations, internal type aliases,
//! and shared helper utilities.  It is split into:
//!
//! - [`types`] — shard-level type aliases (`StdShard`, `StdShardVecArc`, etc.)
//! - [`helpers`] — lock helpers, shard-count normalization and validation
//! - [`sync_impl`] — synchronous map implementation
//! - [`async_impl`] — asynchronous map implementation (behind `async` feature)

use super::*;

pub(crate) mod helpers;
pub(crate) mod sync_impl;
pub(crate) mod types;

#[cfg(feature = "async")]
pub(crate) mod async_impl;

pub(crate) use helpers::{
    capped_shard_count, normalized_shard_count, std_read_guard, std_write_guard, strict_shard_count,
};
pub(crate) use types::{StdShard, StdShardMap, StdShardVec, StdShardVecArc};

#[cfg(feature = "async")]
pub(crate) use types::{AsyncShard, AsyncShardMap, AsyncShardVec, AsyncShardVecArc};

#[cfg(all(feature = "async", feature = "advanced"))]
pub(crate) use types::ReplicaList;
