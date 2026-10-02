//! Publish batch mutation metadata once per shard, including during unwinding.

use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hashbrown::HashMap;

/// Accounts for changes while borrowing a map protected by its caller's write guard.
///
/// The borrow keeps metadata publication ahead of unlocking the shard. Inspecting
/// the final length also covers a key or value destructor panicking after removal.
pub(crate) struct ShardMutation<'a, K, V, S> {
    map: &'a mut HashMap<K, V, S>,
    total_len: &'a AtomicUsize,
    write_epoch: &'a AtomicU64,
    initial_len: usize,
    changed: bool,
}

impl<'a, K, V, S> ShardMutation<'a, K, V, S> {
    #[inline]
    pub(crate) fn new(
        map: &'a mut HashMap<K, V, S>,
        total_len: &'a AtomicUsize,
        write_epoch: &'a AtomicU64,
    ) -> Self {
        let initial_len = map.len();
        Self {
            map,
            total_len,
            write_epoch,
            initial_len,
            changed: false,
        }
    }
}

impl<K, V, S> ShardMutation<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    #[inline]
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        // Mark before calling user Hash/Eq/Drop: a replacement may already have
        // committed when a destructor panics. A failed Hash may conservatively
        // invalidate a snapshot even though it changed no data.
        self.changed = true;
        self.map.insert(key, value)
    }

    #[inline]
    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.remove(key)
    }
}

impl<K, V, S> Drop for ShardMutation<'_, K, V, S> {
    #[inline]
    fn drop(&mut self) {
        let final_len = self.map.len();
        if final_len > self.initial_len {
            self.total_len
                .fetch_add(final_len - self.initial_len, Ordering::Relaxed);
        } else if final_len < self.initial_len {
            self.total_len
                .fetch_sub(self.initial_len - final_len, Ordering::Relaxed);
        }
        if self.changed || final_len != self.initial_len {
            self.write_epoch.fetch_add(1, Ordering::Release);
        }
    }
}
