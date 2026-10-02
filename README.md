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

Current crate version: `2.3.0`. Unreleased fixes and API changes are listed in [CHANGELOG.md](CHANGELOG.md).

Roadmap capabilities shipped through `v2.3.0`:
- Adaptive shard expansion and rebalance (stop-the-world + online incremental).
- Snapshot modes (`Clone`, `Cached`, `Cow`) with epoch-based cache invalidation.
- Patch-level dependency hygiene: the `async` feature no longer forces Tokio's multi-thread runtime.
- Entry-style and get-or-create APIs for atomic per-key initialization in sync and async maps.

## Installation

```toml
[dependencies]
starshard = { version = "2.3.0", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# minimal:
# starshard = "2.3.0"
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
| `lifecycle` | `per_shard_load`, `memory_stats`, `drain`, lifecycle structs | observability and maintenance |
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

## Snapshot Modes (`v2.2.1`)

`SnapshotMode` lets you pick snapshot behavior per workload:

- `Clone`: rebuild snapshot entries on each request (default).
- `Cached`: reuse a versioned, shared whole-map snapshot until the next write.
- `Cow`: currently uses the same lazy shared snapshot cache as `Cached`. Writes invalidate the cache without cloning an entire shard; the next snapshot request rebuilds it.

Valid `Cached`/`Cow` hits validate the data epoch under a short cache lock, without acquiring routing. Missing or stale snapshots still require an exclusive rebuild. Cached reads can finish during topology-only migration; they are not lock-free. Async cache reads preserve Tokio's cooperative task budget.

`iter()` returns owned entries and therefore still clones entries from a shared cache, after releasing routing. Replaced cached data is also destroyed outside routing. Use `shared_snapshot()` to obtain an `Arc<Vec<(K, V)>>` and read repeated snapshots without copying their entries. Existing snapshot handles remain immutable after later writes. With `advanced`, `cow_snapshot()` also exposes a shared view and its data version.

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
| low write + high snapshot frequency | `Cow` or `Cached` |

## Consistency Model

- Per-shard operations are linearizable for that shard.
- Snapshot rebuilding excludes concurrent mutation and directory changes, producing a stable view of both active and previous shards. A matching immutable cache is returned without routing access; its data and epoch are captured together.
- During online rebalance, active-first + previous-fallback keeps key reachability.
- Transactions acquire participating shard write locks in a fixed order. They are pessimistic transactions; read/write sets are not an MVCC conflict detector. Callbacks must not reenter the same map while its locks are held.

## Performance Notes

- Use the contention benchmarks to choose a shard count for your thread count and key distribution.
- Lazy shard allocation keeps memory proportional to touched shards.
- `IterBuilder::parallel(true)` uses Rayon for filtering at least 1024 inputs when enabled. This initial cutoff is not a measured optimum. Filtering precedes the result limit and preserves input order; filter calls can run concurrently, while `for_each` callbacks remain sequential. Small inputs and builds without `rayon` use sequential filtering.
- Use `get_or_insert_with` or `entry(...).or_insert_with(...)` for atomic get-or-create paths instead of external check-then-insert locks.
- For snapshot-heavy services, test `Cached` and `Cow` with your real key distribution.

## Serde Semantics

- Sync map supports direct `Serialize` / `Deserialize`.
- Hasher state is not persisted; rebuild uses `S::default()`.
- Async map uses `async_snapshot_serializable().await` helper for serialization.

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
- Snapshot cache misses materialize a whole-map `Vec<(K, V)>` while mutation is paused; `shared_snapshot()` amortizes this cost only while no write invalidates the cache.
- `EvictionConfig`, eviction policies, and `AtomicMetrics` are standalone types. Maps do not run an autonomous TTL/LRU/LFU scheduler or automatically increment those operation counters. `memory_stats()` and `per_shard_load()` report map state.
- Lock timing instrumentation is not implemented; `lock_profiles()` does not provide measured contention data.
- `RebalanceOptions` fields `background`, `batch_size`, `max_pause_ns` are forward-compatible placeholders in `v2.x`; they do not start a background task or enforce a pause budget.

## License

Dual license:
- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)

You may choose either license.

## Disclaimer

- Benchmarks and throughput notes in this README are indicative, not guaranteed.
- Always validate behavior, latency, and memory usage under your own workload before production rollout.
