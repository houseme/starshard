//! Publish mutation metadata before shard unlock, including during unwinding.

use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hashbrown::HashMap;

/// Prepare the destination before detaching a previous-generation entry.
/// Both maps must be write-locked in active-before-previous order by the caller.
pub(crate) fn promote_entry<K, V, S, Q>(
    active: &mut HashMap<K, V, S>,
    previous: &mut HashMap<K, V, S>,
    key: &Q,
) where
    K: Eq + Hash + Clone + Borrow<Q>,
    V: Clone,
    S: BuildHasher + Clone,
    Q: Eq + Hash + ?Sized,
{
    let Some((stored_key, _)) = previous.get_key_value(key) else {
        return;
    };
    let destination_key = stored_key.clone();
    // Growth can hash existing keys and panic. Stage that work when necessary
    // so neither live generation is altered before destination preparation.
    let mut staged = if active.len() == active.capacity() {
        let mut map = active.clone();
        map.reserve(1);
        Some(map)
    } else {
        None
    };
    let destination = staged
        .as_mut()
        .unwrap_or(&mut *active)
        .entry(destination_key);
    // With one spare slot, VacantEntry insertion does not grow or rehash.
    let Some((retired_key, value)) = previous.remove_entry(key) else {
        return;
    };
    let retired_value = match destination {
        hashbrown::hash_map::Entry::Vacant(entry) => {
            entry.insert(value);
            None
        }
        hashbrown::hash_map::Entry::Occupied(_) => Some(value),
    };
    // Publish before dropping any user-owned retired key/value/map. A panic
    // during destruction therefore leaves the promoted value reachable.
    if let Some(staged) = staged {
        let retired_map = std::mem::replace(active, staged);
        drop(retired_map);
    }
    drop(retired_key);
    drop(retired_value);
}

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

    /// Detach the stored pair so eviction callbacks and destructors run after unlock.
    #[cfg(all(feature = "lifecycle", feature = "async"))]
    #[inline]
    pub(crate) fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.remove_entry(key)
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
