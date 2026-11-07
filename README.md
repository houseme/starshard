# Starshard

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

English | [简体中文](README_CN.md)

Starshard is a high-performance, lazily sharded concurrent `HashMap` for Rust.

It is designed for real production workloads where you need:
- predictable lock behavior,
- lower write contention than a single global lock,
- optional async parity,
- and explicit control over rebalance and snapshot trade-offs.

## Status

Current crate version: `2.4.1`. Release changes are listed in [CHANGELOG.md](CHANGELOG.md#241---2026-11-07).

`2.4.1` fixes additional panic-recovery boundaries in migration, mutations, and replica fanout, plus migration statistics. See the [2.4.0 patch upgrade notes](MIGRATION-1X-TO-2X.md#46-upgrading-from-240-to-241).

Roadmap capabilities shipped through `v2.4.0`:
- Adaptive shard expansion and rebalance (stop-the-world + online incremental).
- Snapshot modes (`Clone`, `Cached`, `Cow`) with epoch-based cache invalidation.
- Patch-level dependency hygiene: the `async` feature no longer forces Tokio's multi-thread runtime.
- Entry-style and get-or-create APIs for atomic per-key initialization in sync and async maps.
- Migration-safe conditional operations, CAS, transactions, and complete snapshots of both shard generations.
- Shared snapshot handles and borrowed-key reads, with rebuilds that allow ordinary readers to progress.
- Per-shard batch metadata publication and primary-inclusive, bounded, ordered replication.

Upgrading from 2.3.x: snapshot versions now identify committed data, and invalid replication topologies are rejected. Review the [2.4 upgrade notes](MIGRATION-1X-TO-2X.md#45-upgrading-from-23x-to-240) before using advanced APIs. Performance improvements are supported by operation/allocation counts; workload-specific throughput and p99 still require measurement.

## Installation

```toml
[dependencies]
starshard = { version = "2.4.1", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# minimal:
# starshard = "2.4.1"
```

## 5-Minute Path

1. Start with default sync map: `ShardedHashMap::new(64)`.
2. If you are in Tokio runtime, switch to `AsyncShardedHashMap`.
3. If snapshots are frequent, try `SnapshotMode::Cached` first, then `SnapshotMode::Cow`.
4. If shard count is user-driven or external-input-driven, use strict constructors (`try_with_*`).

Migration guide:
- [1.x to 2.x Usage Differences](MIGRATION-1X-TO-2X.md)

## Feature Flags

| Feature | What you get | Typical use |
|---|---|---|
| `async` | `AsyncShardedHashMap` (Tokio `RwLock`) | async services and workers |
| `rayon` | large sync snapshots; parallel `IterBuilder` filtering with `lifecycle` | large scans and expensive predicates |
| `serde` | sync serialize/deserialize + async serializable snapshot helper | persistence/export |
| `lifecycle` | capacity policies/workers, operation metrics, `drain`, introspection | bounded caches and maintenance |
| `advanced` | transaction/CAS/replication/diagnostic APIs | advanced concurrency and control planes |

## Quick Start (Sync)

```rust
use starshard::ShardedHashMap;

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
m.insert("k1".into(), 10);
assert_eq!(m.get_borrowed("k1"), Some(10));
assert_eq!(m.len(), 1);
```

## Quick Start (Async)

```rust
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;

    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(64);
    m.insert("k1".into(), 10).await;
    assert_eq!(m.get_borrowed("k1").await, Some(10));
}
```

## Common Operations (Cheat Sheet)

| Goal | Sync API | Async API |
|---|---|---|
| insert/update | `insert(k, v)` | `insert(k, v).await` |
| read | `get(&k)` | `get(&k).await` |
| borrowed-key read | `get_borrowed(q)` | `get_borrowed(q).await` |
| delete | `remove(&k)` | `remove(&k).await` |
| entry insert/update | `entry(k).or_insert_with(f)` | `entry(k).await.or_insert_with(f).await` |
| get or insert | `get_or_insert_with(k, f)` | `get_or_insert_with(k, f).await` |
| batch insert | `batch_insert(items)` | `batch_insert(items).await` |
| batch read | `batch_get(&keys)` | `batch_get(&keys).await` |
| conditional update | `compute_if_present(&k, f)` | `compute_if_present(&k, f).await` |
| conditional insert | `compute_if_absent(k, f)` | `compute_if_absent(k, f).await` |
| metrics/introspection | `shard_stats()` / `memory_stats()` | `shard_stats().await` / `memory_stats().await` |

For borrowed keys, use `get_borrowed`, `contains_borrowed`, and `remove_borrowed` (for example, an `&str` query against `String` keys). Existing `get`, `contains`, and `remove` retain their `&K` signatures. Reads still return cloned values.

Entry-style insert/update:

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, usize> = ShardedHashMap::new(16);
let value = map
    .entry("read-version".to_string())
    .and_modify(|count| *count += 1)
    .or_insert_with(|| 1);
assert_eq!(value, 1);
```

Atomic get-or-create helper:

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, Vec<u64>> = ShardedHashMap::new(16);
let lane = map.get_or_insert_with("read-version".to_string(), Vec::new);
assert!(lane.is_empty());
```

## Capacity Eviction (Unreleased)

`lifecycle` provides the new capacity maintenance and operation metrics. Functional regression checks have passed, and workload performance acceptance remains open.

Configure LRU/LFU with `EvictionConfig`, or select FIFO through the separate `CapacityPolicy` and `EvictionOptions` selector. Global `max_entries` and per-shard `max_items_per_shard` are mutually exclusive, and zero is a valid limit. Configuration is installed at construction and shared by map clones.

This example uses manual scheduling with post-mutation capacity enforcement and an estimator-based budget for live entries.

```rust
use std::collections::hash_map::RandomState;
use starshard::{CapacityPolicy, EvictionConfig, EvictionOptions, ShardedHashMap, SnapshotMode};

let options = EvictionOptions::new(EvictionConfig {
    max_entries: Some(2),
    background_enabled: false,
    ..EvictionConfig::default()
})
.with_policy(CapacityPolicy::Fifo)
.with_memory_budget(1024, |key: &String, value: &Vec<u8>| key.len() + value.len());
let map = ShardedHashMap::try_with_manual_eviction(
    4, RandomState::new(), SnapshotMode::Clone, options,
).unwrap();
map.insert("first".into(), vec![1]);
map.insert("second".into(), vec![2]);
map.insert("third".into(), vec![3]);
assert_eq!(map.len(), 2);
assert_eq!(map.get_borrowed("first"), None);
```

The weight budget sums the estimator's per-entry values. The estimator runs inside the mutation's lock scope and must return stable weights for stable entry state. Cow view retention and allocator overhead require separate memory measurement. FIFO preserves insertion age across replacement, while LRU/LFU record logical accesses. Snapshot scans and physical migration preserve policy ranking.

`background_enabled = true` starts a periodic worker in `try_with_eviction` / `try_with_eviction_options`. Sync maps use a thread. Async maps require an entered Tokio runtime with its time driver enabled. This example uses `async` + `lifecycle` and a direct Tokio dependency with `rt` and `time`.

```rust
use std::time::Duration;
use starshard::{AsyncShardedHashMap, EvictionConfig};

let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_time()
    .build()
    .unwrap();
runtime.block_on(async {
    let map = AsyncShardedHashMap::<u64, u64>::try_with_eviction(
        4, EvictionConfig::auto_lru(2, Duration::from_secs(60)),
    ).unwrap();
    map.batch_insert([(1, 10), (2, 20), (3, 30)]).await;
    assert!(map.len().await <= 2);
});
```

`try_with_manual_eviction` accepts `background_enabled=false` and borrowed sync keys/values. Worker-capable constructors require `'static` data. `with_eviction` panics on invalid configuration or startup failure, and the `try_*` constructors expose typed errors. `spawn_eviction_task()` explicitly starts one worker and returns a handle with `cancel()` and `join()` (`join().await` for async). Automatic constructors retain their handle in external ownership. Dropping the last external map clone stops periodic work. Cancellation preserves committed removals. `eviction_worker_status()` exposes progress and termination reasons.

Completed mutations, explicit `enforce_eviction_limits()` calls and periodic ticks enforce the limits. Concurrent, cancelled, panicking or topology-changing operations can leave `pending` / `dirty` maintenance for a subsequent mutation or pass. The argument to `evict(target_count)` / `evict_lru(target_count)` specifies the desired remaining count, and the return value reports removals. Unconfigured maps return zero. Per-shard limits use active routing across both generations. Maintenance can wait on routing and data locks.

`set_evict_callback` runs synchronously after removal, length/version updates, eviction counters and release of map/metadata locks. Callback panics are isolated. Nested callback dispatch on the same thread is suppressed and counted, while nested mutations still enforce capacity. A callback that continually reinserts entries can leave pending maintenance. `clear_evict_callback` removes the shared callback.

Enabling `lifecycle` activates per-element operation counters on ordinary maps too, with atomic-counter overhead. `metrics_snapshot()` / `reset_metrics()` cover logical queries and mutations, with a separate capacity-eviction counter. Concurrent snapshots and resets sample counters independently, so exact accounting checks require quiescence. `eviction_runtime_stats()` also exposes pending repair, estimated weight, maintenance duration and callback diagnostics.

Capacity constructors return typed errors for `TimeToLive`, `Custom` and a populated `ttl`. Expiry remains tracked under roadmap F. Configured capacity maps return `ReplicaError::Rejected` from `insert_replicated` / `remove_replicated`. Ordinary local mutations are available for cache writes.

## Constructor Strategy

Choose by strictness and control level:

- Backward-compatible clamping:
  - `with_shards_and_hasher(...)`
  - `with_shards_and_hasher_capped(...)`
- Strict validation (returns `ShardCountError`):
  - `try_with_shards_and_hasher(...)`
  - `try_with_shards_and_hasher_capped(...)`
- Snapshot mode aware:
  - `with_snapshot_mode(...)`
  - `with_shards_and_hasher_and_snapshot_mode(...)`
  - `with_shards_and_hasher_capped_and_snapshot_mode(...)`

## Adaptive Rebalance (`v2.2.1`)

### Stop-the-world rebalance

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
let report = m.rebalance_to(32, RebalanceOptions::default()).unwrap();
assert_eq!(report.from_shards, 8);
assert_eq!(report.to_shards, 32);
```

### Online incremental rebalance

```rust
use starshard::ShardedHashMap;

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.start_rebalance_online(32).unwrap();

while m.rebalance_status().state == "migrating" {
    m.advance_rebalance(2);
}

assert_eq!(m.rebalance_status().state, "idle");
```

Semantics:
- writes route to active shards immediately,
- reads fall back to previous shards while migration is in progress,
- migration is finalized when `advance_rebalance(...)` drains all source shards,
- each advance call pauses map operations while moving up to the requested number of source shards; large source shards can still cause long pauses. This is not a time or entry-count budget.

### Budgeted batches and supervised workers

`advance_rebalance_with_options(&options)` limits entries moved per batch using `batch_size`, and checks `max_pause_ns` at safe commit boundaries. User Hash/Clone, shard copying, and lock waits can exceed that budget. `rebalance_telemetry()` reports measured intervals and overruns.

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let map = ShardedHashMap::<u64, u64>::new(8);
map.batch_insert((0..1_000).map(|key| (key, key)));
let worker = map.spawn_rebalance(32, RebalanceOptions {
    background: true,
    batch_size: 64,
    max_pause_ns: 100_000,
}).unwrap();
assert!(worker.join().unwrap().completed);
```

`pause_rebalance()` pauses at a batch boundary, and `resume_rebalance()` resumes. Worker handles support `cancel()` and `join()`, and request cancellation when dropped. Already moved entries remain reachable. Resume with manual advancement or a new worker. Async counterparts use `.await`, and dropping an async handle also aborts pending awaits.

`rebalance_to()` returns after completion. Setting `background = true` selects cooperative entry batches. Use `spawn_rebalance()` to obtain a supervised worker handle. Workers reduce subsequent batch sizes after budget overruns. Load-triggered automatic scaling remains in roadmap G.

## Snapshot Modes (`v2.2.1`)

`SnapshotMode` lets you pick snapshot behavior per workload:

- `Clone`: rebuild snapshot entries on each request (default).
- `Cached`: reuse a versioned, shared whole-map snapshot until the next write.
- `Cow`: stores immutable shard allocations behind shared handles. `snapshot_view()` captures shard handles without cloning keys or values. A write copies a touched shard while a view still retains it.

Valid `shared_snapshot()` cache hits validate the data epoch under a short cache lock. Missing snapshots use a builder gate and ordered shard read locks to capture a consistent view. Cow materializes owned entries after releasing map locks. Clone/Cached materialize entries while holding shard read locks. Async cache reads preserve Tokio's cooperative task budget.

`snapshot_view()` returns a `SnapshotView` with borrowed iteration, a data version, and the active shard count captured together. Cow views retain shard allocations, so memory usage depends on retained versions and subsequent writes. Releasing a view allows its old allocations to be reclaimed. In Clone/Cached modes, this API copies initialized shards when capturing a view.

`iter()` returns owned entries. `shared_snapshot()` returns a materialized `Arc<Vec<(K, V)>>` and caches it in Cached/Cow modes. With `advanced`, `cow_snapshot()` wraps that materialized representation and its version. These APIs copy entries when materializing a fresh snapshot. Use `snapshot_view()` with Cow for capture and traversal that borrow the shard entries.

```rust
use starshard::{ShardedHashMap, SnapshotMode};

let clone_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Clone);
let cached_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cached);
let cow_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cow);
```

### Mode selection guide

| Workload profile | Recommended mode |
|---|---|
| high write + low snapshot frequency | `Clone` |
| medium write + medium snapshot frequency | `Cached` |
| frequent borrowed snapshots or serialization | `Cow` with `snapshot_view()` |
| repeated materialized snapshots between writes | `Cached` with `shared_snapshot()` |

## Consistency Model

- Per-shard operations are linearizable for that shard.
- Snapshot rebuilding retains directory and ordered shard read locks, producing a stable view of both active and previous shards once all guards are held. Ordinary reads can proceed subject to lock fairness; queued writers or cold-shard initialization can still delay later readers. A matching immutable cache is returned without routing access; its data and epoch are captured together.
- During online rebalance, active-first + previous-fallback keeps key reachability.
- Transactions acquire participating shard write locks in a fixed order. They are pessimistic transactions; read/write sets are not an MVCC conflict detector. Callbacks invoked by locked operations must not reenter the same map. Eviction notifications run after lock release and follow the separate reentrancy rules above.
- If user code panics during a transaction, already-applied operations are not rolled back; length and cached-snapshot metadata track the actual stored contents.

## Performance Notes

- Use the contention benchmarks to choose a shard count for your thread count and key distribution.
- Lazy shard allocation keeps memory proportional to touched shards.
- `IterBuilder::parallel(true)` uses Rayon for filtering at least 1024 inputs when enabled. This initial cutoff is not a measured optimum. Filtering precedes the result limit and preserves input order; filter calls can run concurrently, while `for_each` callbacks remain sequential. Small inputs and builds without `rayon` use sequential filtering.
- Use `get_or_insert_with` or `entry(...).or_insert_with(...)` for atomic get-or-create paths instead of external check-then-insert locks.
- For snapshot-heavy services, test `Cached` and `Cow` with your real key distribution.
- Stable-layout batches publish length and version changes once per touched shard, including during unwinding. Snapshot versions are change tokens, not per-operation counters.

## Serde Semantics

- Sync map supports direct `Serialize` / `Deserialize`.
- Hasher state is not persisted; rebuild uses `S::default()`.
- Async map uses `async_snapshot_serializable().await` for serialization. Sync serialization and the async wrapper stream borrowed entries from a captured view. Cow capture and serialization avoid cloning keys and values. The existing object/array formats are retained.

## Examples and Benchmarks

Examples:
- `examples/v210_rebalance.rs`
- `examples/snapshot_mode_clone_demo.rs`
- `examples/snapshot_mode_cached_demo.rs`
- `examples/snapshot_mode_cow_demo.rs`
- `examples/mixed_workload_snapshot_tradeoff_demo.rs`

Benchmark entry:
- `benches/bench_main.rs`

The benchmark suite pre-generates keys and reuses contention workers. Timed contention batches include barrier synchronization, amortized over 1000 operations per worker. It samples 1/2/4/8/16 threads, a 90% hot-key distribution, 16-byte/4-KiB values, 10%/50% writes, and all snapshot modes without running the full Cartesian product. Owned iteration and shared snapshot handles are measured separately.

```bash
cargo bench --bench bench_main -- concurrent_mixed
cargo bench --bench bench_main -- snapshot_modes
cargo bench --bench bench_main -- shared_snapshot
cargo bench --bench bench_main -- concurrent_read_paths
cargo bench --bench bench_main -- concurrent_cached_snapshot
cargo bench --all-features --bench bench_main -- snapshot_rebuild
cargo bench --all-features --bench bench_main -- batch_mutations
```

Criterion reports batch timing and throughput here. These benchmarks do not measure individual-operation p99 latency, allocations, peak memory, or migration pause bounds; those require separate workload instrumentation.

## Validation

Before release or upgrade verification:

```bash
cargo fmt --all
cargo test --all-features
cargo check --all-features
```

## Current Limits

- Not lock-free; hot-shard writer pressure can still serialize.
- Snapshot capture acquires routing and ordered shard read locks. Cow views release map locks before traversal, and keep old shard allocations alive. Writes to retained shards incur shard-level copy costs, including user-defined Clone work. Materialized snapshots additionally allocate a whole-map vector.
- Capacity eviction requires explicit configuration and converges at maintenance boundaries. Estimated entry weights exclude allocator overhead and retained Cow versions. TTL/custom policy execution remains unsupported; see [capacity eviction](#capacity-eviction-unreleased).
- Lock timing instrumentation is not implemented; `lock_profiles()` does not provide measured contention data.
- Migration time budgets are checked at safe entry boundaries. User Hash/Clone/Drop and lock waits can cause overruns. Tune batches using measured `budget_overruns` and `max_pause_ns`. Background workers migrate to caller-selected targets.

## License

Dual license:
- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)

You may choose either license.

## Disclaimer

- Benchmarks and throughput notes in this README are indicative, not guaranteed.
- Always validate behavior, latency, and memory usage under your own workload before production rollout.
