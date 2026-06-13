//! Shared helper utilities for shard-count normalization, validation, and
//! poison-recovery lock acquisition.
//!
//! All functions in this module are `pub(crate)` and marked `#[inline]` to
//! keep overhead minimal on hot paths.

use super::*;

/// Acquires a read lock on `lock`, recovering from poison with a `tracing::error` log.
///
/// This avoids panicking if a writer panicked while holding the lock.  The
/// poisoned guard is returned via `into_inner()` so the map remains usable.
#[inline]
pub(crate) fn std_read_guard<'a, T>(
    lock: &'a StdRwLock<T>,
    context: &'static str,
) -> StdReadGuard<'a, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(context = %context, "std rwlock poisoned (read)");
            poisoned.into_inner()
        }
    }
}

/// Acquires a write lock on `lock`, recovering from poison with a `tracing::error` log.
///
/// Identical semantics to [`std_read_guard`] but for exclusive write access.
#[inline]
pub(crate) fn std_write_guard<'a, T>(
    lock: &'a StdRwLock<T>,
    context: &'static str,
) -> StdWriteGuard<'a, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(context = %context, "std rwlock poisoned (write)");
            poisoned.into_inner()
        }
    }
}

/// Normalizes a shard count: returns [`DEFAULT_SHARDS`] when `shard_count` is zero.
///
/// This allows callers to pass `0` as a sentinel for "use the default".
#[inline]
pub(crate) fn normalized_shard_count(shard_count: usize) -> usize {
    if shard_count == 0 {
        DEFAULT_SHARDS
    } else {
        shard_count
    }
}

/// Validates that the (normalized) shard count does not exceed `max_shards`.
///
/// Returns `Err(ShardCountError)` if the count is too large, making it
/// suitable for `try_*` constructors that must not silently clamp.
#[inline]
pub(crate) fn strict_shard_count(
    shard_count: usize,
    max_shards: usize,
) -> Result<usize, ShardCountError> {
    let requested = normalized_shard_count(shard_count);
    if requested > max_shards {
        Err(ShardCountError {
            requested,
            max_allowed: max_shards,
        })
    } else {
        Ok(requested)
    }
}

/// Returns the shard count clamped to `max_shards` (infallible).
///
/// Used by `with_shards_and_hasher_capped` constructors that silently cap
/// rather than returning an error.
#[inline]
pub(crate) fn capped_shard_count(shard_count: usize, max_shards: usize) -> usize {
    let requested = normalized_shard_count(shard_count);
    requested.min(max_shards)
}
