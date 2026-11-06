#![cfg(feature = "serde")]

use serde::{Deserialize, Serialize, Serializer};
use starshard::{ShardedHashMap, SnapshotMode};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_mul(256).wrapping_add(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type Identity = BuildHasherDefault<IdentityHasher>;

struct Tracked {
    value: u64,
    copies: Arc<AtomicUsize>,
    fail_serialization: bool,
}

impl Tracked {
    fn new(value: u64, copies: &Arc<AtomicUsize>) -> Self {
        Self {
            value,
            copies: copies.clone(),
            fail_serialization: false,
        }
    }
}

impl Clone for Tracked {
    fn clone(&self) -> Self {
        self.copies.fetch_add(1, Ordering::Relaxed);
        Self {
            value: self.value,
            copies: self.copies.clone(),
            fail_serialization: self.fail_serialization,
        }
    }
}

impl PartialEq for Tracked {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for Tracked {}

impl Hash for Tracked {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.value.hash(hasher);
    }
}

impl Serialize for Tracked {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.fail_serialization {
            return Err(serde::ser::Error::custom("injected serialization failure"));
        }
        serializer.serialize_u64(self.value)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMap {
    shard_count: usize,
    entries: Vec<(u64, u64)>,
}

fn sorted(mut entries: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    entries.sort_unstable();
    entries
}

fn array_entries(value: &impl Serialize) -> Vec<(u64, u64)> {
    sorted(serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap())
}

#[test]
fn sync_json_preserves_wire_format_and_custom_hasher_round_trip() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = ShardedHashMap::<u64, u64, Identity>::with_shards_and_hasher_and_snapshot_mode(
            4,
            Identity::default(),
            mode,
        );
        assert_eq!(
            serde_json::to_value(&map).unwrap(),
            serde_json::json!({"shard_count": 4, "entries": []})
        );
        map.batch_insert((0..8).map(|key| (key, key + 10)));
        let json = serde_json::to_string(&map).unwrap();
        let wire: WireMap = serde_json::from_str(&json).unwrap();
        assert_eq!(wire.shard_count, 4);
        assert_eq!(
            sorted(wire.entries),
            (0..8).map(|key| (key, key + 10)).collect::<Vec<_>>()
        );
        let restored: ShardedHashMap<u64, u64, Identity> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.shard_count(), 4);
        assert_eq!(restored.len(), 8);
        for key in 0..8 {
            assert_eq!(restored.get(&key), Some(key + 10));
        }
    }
}

#[test]
fn sync_cow_serialization_borrows_both_generations_and_retains_old_values() {
    let map = ShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        2,
        Identity::default(),
        SnapshotMode::Cow,
    );
    let key_copies = Arc::new(AtomicUsize::new(0));
    let value_copies = Arc::new(AtomicUsize::new(0));
    map.batch_insert((0..8).map(|key| {
        (
            Tracked::new(key, &key_copies),
            Tracked::new(key + 10, &value_copies),
        )
    }));
    map.start_rebalance_online(4).unwrap();
    map.insert(
        Tracked::new(0, &key_copies),
        Tracked::new(100, &value_copies),
    );
    key_copies.store(0, Ordering::Relaxed);
    value_copies.store(0, Ordering::Relaxed);

    let json = serde_json::to_string(&map).unwrap();
    let wire: WireMap = serde_json::from_str(&json).unwrap();
    let mut expected: Vec<_> = (0..8).map(|key| (key, key + 10)).collect();
    expected[0].1 = 100;
    assert_eq!(wire.shard_count, 4);
    assert_eq!(sorted(wire.entries), expected);
    let old = map.snapshot_view();
    assert_eq!(array_entries(&old), expected);
    assert_eq!(key_copies.load(Ordering::Relaxed), 0);
    assert_eq!(value_copies.load(Ordering::Relaxed), 0);

    map.insert(
        Tracked::new(1, &key_copies),
        Tracked::new(200, &value_copies),
    );
    map.remove(&Tracked::new(2, &key_copies));
    while map.advance_rebalance(1) != 0 {}
    assert_eq!(array_entries(&old), expected);
    expected[1].1 = 200;
    expected.remove(2);
    let current: WireMap = serde_json::from_str(&serde_json::to_string(&map).unwrap()).unwrap();
    assert_eq!(sorted(current.entries), expected);
}

#[test]
fn sync_serializer_error_releases_temporary_cow_view() {
    let map = ShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cow);
    let key_copies = Arc::new(AtomicUsize::new(0));
    let value_copies = Arc::new(AtomicUsize::new(0));
    let mut value = Tracked::new(10, &value_copies);
    value.fail_serialization = true;
    map.insert(Tracked::new(1, &key_copies), value);
    let error = serde_json::to_string(&map).unwrap_err();
    assert!(error.to_string().contains("injected serialization failure"));

    // With no retained view, changing this shard must not copy its old entries.
    map.insert(
        Tracked::new(1, &key_copies),
        Tracked::new(20, &value_copies),
    );
    assert_eq!(key_copies.load(Ordering::Relaxed), 0);
    assert_eq!(value_copies.load(Ordering::Relaxed), 0);
    let wire: WireMap = serde_json::from_str(&serde_json::to_string(&map).unwrap()).unwrap();
    assert_eq!(wire.entries, vec![(1, 20)]);
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use starshard::{AsyncShardedHashMap, AsyncShardedHashMapSnapshot};

    #[tokio::test]
    async fn async_wrapper_preserves_array_format_in_all_modes() {
        for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
            let map = AsyncShardedHashMap::with_snapshot_mode(4, mode);
            let empty: AsyncShardedHashMapSnapshot<u64, u64> =
                map.async_snapshot_serializable().await;
            assert_eq!(serde_json::to_string(&empty).unwrap(), "[]");
            map.batch_insert((0..8).map(|key| (key, key + 10))).await;
            let snapshot: AsyncShardedHashMapSnapshot<u64, u64> =
                map.async_snapshot_serializable().await;
            assert_eq!(
                array_entries(&snapshot),
                (0..8).map(|key| (key, key + 10)).collect::<Vec<_>>()
            );
        }
    }

    #[tokio::test]
    async fn async_cow_wrapper_borrows_both_generations_and_retains_old_values() {
        let map = AsyncShardedHashMap::with_snapshot_mode(2, SnapshotMode::Cow);
        let key_copies = Arc::new(AtomicUsize::new(0));
        let value_copies = Arc::new(AtomicUsize::new(0));
        map.batch_insert((0..8).map(|key| {
            (
                Tracked::new(key, &key_copies),
                Tracked::new(key + 10, &value_copies),
            )
        }))
        .await;
        map.start_rebalance_online(4).await.unwrap();
        map.insert(
            Tracked::new(0, &key_copies),
            Tracked::new(100, &value_copies),
        )
        .await;
        key_copies.store(0, Ordering::Relaxed);
        value_copies.store(0, Ordering::Relaxed);

        let old = map.async_snapshot_serializable().await;
        let mut expected: Vec<_> = (0..8).map(|key| (key, key + 10)).collect();
        expected[0].1 = 100;
        assert_eq!(array_entries(&old), expected);
        assert_eq!(key_copies.load(Ordering::Relaxed), 0);
        assert_eq!(value_copies.load(Ordering::Relaxed), 0);

        map.insert(
            Tracked::new(1, &key_copies),
            Tracked::new(200, &value_copies),
        )
        .await;
        map.remove(&Tracked::new(2, &key_copies)).await;
        while map.advance_rebalance(1).await != 0 {}
        assert_eq!(array_entries(&old), expected);
        expected[1].1 = 200;
        expected.remove(2);
        assert_eq!(
            array_entries(&map.async_snapshot_serializable().await),
            expected
        );
    }

    #[tokio::test]
    async fn async_serializer_error_does_not_retain_a_dropped_cow_wrapper() {
        let map = AsyncShardedHashMap::with_snapshot_mode(1, SnapshotMode::Cow);
        let key_copies = Arc::new(AtomicUsize::new(0));
        let value_copies = Arc::new(AtomicUsize::new(0));
        let mut value = Tracked::new(10, &value_copies);
        value.fail_serialization = true;
        map.insert(Tracked::new(1, &key_copies), value).await;
        let snapshot = map.async_snapshot_serializable().await;
        let error = serde_json::to_string(&snapshot).unwrap_err();
        assert!(error.to_string().contains("injected serialization failure"));
        drop(snapshot);

        map.insert(
            Tracked::new(1, &key_copies),
            Tracked::new(20, &value_copies),
        )
        .await;
        assert_eq!(key_copies.load(Ordering::Relaxed), 0);
        assert_eq!(value_copies.load(Ordering::Relaxed), 0);
        assert_eq!(
            array_entries(&map.async_snapshot_serializable().await),
            vec![(1, 20)]
        );
    }

    #[tokio::test]
    async fn async_custom_hasher_view_serializes_without_changing_wrapper_api() {
        let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
            2,
            Identity::default(),
            SnapshotMode::Cow,
        );
        map.batch_insert([(0_u64, 10_u64), (1, 11)]).await;
        let snapshot = map.snapshot_view().await;
        assert_eq!(array_entries(&snapshot), vec![(0, 10), (1, 11)]);
    }
}
