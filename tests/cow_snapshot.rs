use starshard::{ShardedHashMap, SnapshotMode, SnapshotView};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(0, |hash, byte| {
            hash.wrapping_mul(256).wrapping_add(u64::from(*byte))
        });
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Identity = BuildHasherDefault<IdentityHasher>;
type Map = ShardedHashMap<u64, Value, Identity>;

struct Value {
    number: u64,
    copies: Arc<AtomicUsize>,
    panic_on_clone: Arc<AtomicBool>,
}

impl Value {
    fn new(number: u64, copies: &Arc<AtomicUsize>) -> Self {
        Self {
            number,
            copies: copies.clone(),
            panic_on_clone: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        assert!(
            !self.panic_on_clone.swap(false, Ordering::Relaxed),
            "clone panic"
        );
        self.copies.fetch_add(1, Ordering::Relaxed);
        Self {
            number: self.number,
            copies: self.copies.clone(),
            panic_on_clone: self.panic_on_clone.clone(),
        }
    }
}

fn map(mode: SnapshotMode) -> Map {
    Map::with_shards_and_hasher_and_snapshot_mode(4, Identity::default(), mode)
}

fn entries(view: &SnapshotView<u64, Value, Identity>) -> Vec<(u64, u64)> {
    let mut entries: Vec<_> = view
        .iter()
        .map(|(key, value)| (*key, value.number))
        .collect();
    entries.sort_unstable();
    entries
}

#[test]
fn view_capture_and_iteration_clone_no_cow_values() {
    for mode in [SnapshotMode::Clone, SnapshotMode::Cached, SnapshotMode::Cow] {
        let map = map(mode);
        let copies = Arc::new(AtomicUsize::new(0));
        map.insert(0, Value::new(10, &copies));
        map.insert(1, Value::new(20, &copies));
        let view = map.snapshot_view();
        assert_eq!(
            copies.load(Ordering::Relaxed),
            if mode == SnapshotMode::Cow { 0 } else { 2 }
        );
        let clone = view.clone();
        assert_eq!(entries(&clone), vec![(0, 10), (1, 20)]);
        assert_eq!(view.len(), 2);
        assert!(!view.is_empty());
        assert_eq!(view.version(), 2);
        assert_eq!(view.shard_count(), 4);
        map.insert(0, Value::new(30, &copies));
        assert_eq!(entries(&view), vec![(0, 10), (1, 20)]);
        drop(map);
        assert_eq!(entries(&clone), vec![(0, 10), (1, 20)]);
    }
}

#[test]
fn cow_writes_copy_only_retained_touched_shards_once() {
    let map = map(SnapshotMode::Cow);
    let copies = Arc::new(AtomicUsize::new(0));
    for key in [0, 4, 1] {
        map.insert(key, Value::new(key, &copies));
    }
    let old = map.snapshot_view();
    let same = old.clone();
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    map.insert(0, Value::new(10, &copies));
    assert_eq!(copies.load(Ordering::Relaxed), 2);
    map.insert(4, Value::new(40, &copies));
    assert_eq!(copies.load(Ordering::Relaxed), 2);
    map.insert(1, Value::new(11, &copies));
    assert_eq!(copies.load(Ordering::Relaxed), 3);
    assert_eq!(entries(&same), vec![(0, 0), (1, 1), (4, 4)]);
    drop(old);
    drop(same);
    let latest = map.snapshot_view();
    drop(latest);
    map.insert(0, Value::new(100, &copies));
    assert_eq!(
        copies.load(Ordering::Relaxed),
        3,
        "the map must not retain a hidden view"
    );
}

#[test]
fn legacy_cow_materialization_does_not_retain_shard_owners() {
    let map = map(SnapshotMode::Cow);
    let copies = Arc::new(AtomicUsize::new(0));
    map.insert(0, Value::new(10, &copies));
    let legacy = map.shared_snapshot();
    assert_eq!(copies.load(Ordering::Relaxed), 1);
    map.insert(0, Value::new(20, &copies));
    assert_eq!(copies.load(Ordering::Relaxed), 1);
    assert_eq!(legacy[0].1.number, 10);
    assert_eq!(entries(&map.snapshot_view()), vec![(0, 20)]);
}

#[test]
fn removing_absent_keys_never_detaches_a_retained_cow_shard() {
    let map = map(SnapshotMode::Cow);
    let copies = Arc::new(AtomicUsize::new(0));
    let value = Value::new(10, &copies);
    value.panic_on_clone.store(true, Ordering::Relaxed);
    map.insert(0, value);
    let old = map.snapshot_view();
    assert!(map.remove(&4).is_none());
    assert_eq!(map.batch_remove([4, 8]), 0);
    #[cfg(feature = "advanced")]
    {
        let mut transaction = starshard::Transaction::new();
        transaction.remove(12);
        assert!(matches!(
            map.execute_transaction(transaction),
            starshard::TransactionResult::Committed(())
        ));
    }
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    assert_eq!(map.snapshot_view().version(), old.version());
    assert_eq!(entries(&old), vec![(0, 10)]);
}

#[test]
fn cow_detach_panic_keeps_data_length_and_version() {
    let map = map(SnapshotMode::Cow);
    let copies = Arc::new(AtomicUsize::new(0));
    let value = Value::new(10, &copies);
    let panic = value.panic_on_clone.clone();
    map.insert(0, value);
    let old = map.snapshot_view();
    panic.store(true, Ordering::Relaxed);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        map.insert(0, Value::new(20, &copies));
    }));
    assert!(failed.is_err());
    let current = map.snapshot_view();
    assert_eq!(map.len(), 1);
    assert_eq!(current.version(), old.version());
    assert_eq!(entries(&current), entries(&old));
    map.insert(0, Value::new(30, &copies));
    assert_eq!(entries(&old), vec![(0, 10)]);
    assert_eq!(entries(&map.snapshot_view()), vec![(0, 30)]);
}

#[test]
fn cow_views_preserve_both_generations_through_promotion_migration_and_clear() {
    let map = map(SnapshotMode::Cow);
    let copies = Arc::new(AtomicUsize::new(0));
    for key in 0..8 {
        map.insert(key, Value::new(key, &copies));
    }
    let before = map.snapshot_view();
    map.start_rebalance_online(8).unwrap();
    let migrating = map.snapshot_view();
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    assert_eq!(migrating.version(), before.version());
    assert_eq!(migrating.shard_count(), 8);
    map.insert(0, Value::new(100, &copies));
    map.remove(&1);
    while map.advance_rebalance(1) != 0 {}
    assert_eq!(map.snapshot_view().len(), 7);
    map.clear();
    assert!(map.snapshot_view().is_empty());
    let expected: Vec<_> = (0..8).map(|key| (key, key)).collect();
    assert_eq!(entries(&before), expected);
    assert_eq!(entries(&migrating), expected);
}

#[cfg(feature = "advanced")]
#[test]
fn cow_views_survive_transaction_conditional_and_batch_mutations() {
    use starshard::{Transaction, TransactionResult};
    let map = ShardedHashMap::<u64, u64, Identity>::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Cow,
    );
    map.batch_insert((0..8).map(|key| (key, key)));
    let view = map.snapshot_view();
    let mut transaction = Transaction::new();
    transaction.write(0, 100);
    transaction.remove(1);
    assert!(matches!(
        map.execute_transaction(transaction),
        TransactionResult::Committed(())
    ));
    map.compare_and_swap(&2, &2, 200);
    assert!(map.compare_and_remove(&3, &3));
    map.compute_if_present(&4, |_| Some(400));
    map.batch_remove([5]);
    map.retain(|key, _| *key != 6);
    let mut original: Vec<_> = view.iter().map(|(key, value)| (*key, *value)).collect();
    original.sort_unstable();
    assert_eq!(original, (0..8).map(|key| (key, key)).collect::<Vec<_>>());
    assert_eq!(map.len(), 4);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_cow_views_capture_without_clones_and_copy_only_touched_shards() {
    use starshard::AsyncShardedHashMap;
    let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Cow,
    );
    let copies = Arc::new(AtomicUsize::new(0));
    for key in [0, 4, 1] {
        map.insert(key, Value::new(key, &copies)).await;
    }
    let old = map.snapshot_view().await;
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    map.insert(0, Value::new(10, &copies)).await;
    assert_eq!(copies.load(Ordering::Relaxed), 2);
    map.insert(4, Value::new(40, &copies)).await;
    assert_eq!(copies.load(Ordering::Relaxed), 2);
    map.start_rebalance_online(8).await.unwrap();
    let migrating = map.snapshot_view().await;
    assert_eq!(migrating.shard_count(), 8);
    map.insert(1, Value::new(11, &copies)).await;
    while map.advance_rebalance(1).await != 0 {}
    map.clear().await;
    assert_eq!(entries(&old), vec![(0, 0), (1, 1), (4, 4)]);
    assert_eq!(entries(&migrating), vec![(0, 10), (1, 1), (4, 40)]);
    assert!(map.snapshot_view().await.is_empty());
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_clear_prepares_all_cow_copies_before_removing_any_entry() {
    use starshard::AsyncShardedHashMap;
    let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Cow,
    );
    let copies = Arc::new(AtomicUsize::new(0));
    map.insert(0, Value::new(10, &copies)).await;
    let value = Value::new(20, &copies);
    let panic = value.panic_on_clone.clone();
    map.insert(1, value).await;
    let old = map.snapshot_view().await;
    panic.store(true, Ordering::Relaxed);
    let worker_map = map.clone();
    assert!(
        tokio::spawn(async move { worker_map.clear().await })
            .await
            .unwrap_err()
            .is_panic()
    );
    assert_eq!(map.len().await, 2);
    let current = map.snapshot_view().await;
    assert_eq!(current.version(), old.version());
    assert_eq!(entries(&current), entries(&old));
    map.clear().await;
    assert_eq!(entries(&old), vec![(0, 10), (1, 20)]);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_removing_absent_keys_never_detaches_a_retained_cow_shard() {
    use starshard::AsyncShardedHashMap;
    let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Cow,
    );
    let copies = Arc::new(AtomicUsize::new(0));
    let value = Value::new(10, &copies);
    value.panic_on_clone.store(true, Ordering::Relaxed);
    map.insert(0, value).await;
    let old = map.snapshot_view().await;
    assert!(map.remove(&4).await.is_none());
    assert_eq!(map.batch_remove([4, 8]).await, 0);
    #[cfg(feature = "advanced")]
    {
        let mut transaction = starshard::Transaction::new();
        transaction.remove(12);
        assert!(matches!(
            map.execute_transaction(transaction).await,
            starshard::TransactionResult::Committed(())
        ));
    }
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    assert_eq!(map.snapshot_view().await.version(), old.version());
    assert_eq!(entries(&old), vec![(0, 10)]);
}

#[cfg(all(feature = "async", feature = "lifecycle"))]
#[tokio::test]
async fn async_drain_prepares_all_cow_copies_before_removing_any_entry() {
    use starshard::AsyncShardedHashMap;
    let map = AsyncShardedHashMap::with_shards_and_hasher_and_snapshot_mode(
        4,
        Identity::default(),
        SnapshotMode::Cow,
    );
    let copies = Arc::new(AtomicUsize::new(0));
    map.insert(0, Value::new(10, &copies)).await;
    let value = Value::new(20, &copies);
    let panic = value.panic_on_clone.clone();
    map.insert(1, value).await;
    let old = map.snapshot_view().await;
    panic.store(true, Ordering::Relaxed);
    let worker_map = map.clone();
    let failed = tokio::spawn(async move { worker_map.drain().await });
    assert!(failed.await.err().unwrap().is_panic());
    assert_eq!(map.len().await, 2);
    assert_eq!(map.snapshot_view().await.version(), old.version());
    assert_eq!(map.drain().await.count(), 2);
    assert!(map.is_empty().await);
    assert_eq!(entries(&old), vec![(0, 10), (1, 20)]);
}
