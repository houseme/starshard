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
