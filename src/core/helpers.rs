use super::*;

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

#[inline]
pub(crate) fn normalized_shard_count(shard_count: usize) -> usize {
    if shard_count == 0 {
        DEFAULT_SHARDS
    } else {
        shard_count
    }
}

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

#[inline]
pub(crate) fn capped_shard_count(shard_count: usize, max_shards: usize) -> usize {
    let requested = normalized_shard_count(shard_count);
    requested.min(max_shards)
}
