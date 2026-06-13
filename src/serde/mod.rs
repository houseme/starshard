//! Serde support for Starshard maps.
//!
//! - [`sync_serde`] — `Serialize` / `Deserialize` impls for `ShardedHashMap`.
//! - [`async_snapshot`] — serializable snapshot wrapper for
//!   `AsyncShardedHashMap` (behind `async` + `serde` features).

use super::*;

pub(crate) mod sync_serde;

#[cfg(feature = "async")]
pub(crate) mod async_snapshot;

#[cfg(all(feature = "async", feature = "serde"))]
pub use async_snapshot::AsyncShardedHashMapSnapshot;
