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

Production-ready (`v2.2.1`).

Roadmap capabilities shipped in `v2.2.1`:
- Adaptive shard expansion and rebalance (stop-the-world + online incremental).
- Snapshot modes (`Clone`, `Cached`, `Cow`) with epoch-based cache invalidation.

## Installation

```toml
[dependencies]
starshard = { version = "2.2.1", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# minimal:
# starshard = "2.2.1"
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
| `rayon` | parallel snapshot flatten in iteration | large snapshot/scan workloads |
| `serde` | sync serialize/deserialize + async serializable snapshot helper | persistence/export |
| `lifecycle` | `per_shard_load`, `memory_stats`, `drain`, lifecycle structs | observability and maintenance |
| `advanced` | transaction/CAS/replication/diagnostic APIs | advanced concurrency and control planes |

## Quick Start (Sync)

```rust
use starshard::ShardedHashMap;

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
m.insert("k1".into(), 10);
assert_eq!(m.get(&"k1".into()), Some(10));
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
    assert_eq!(m.get(&"k1".into()).await, Some(10));
}
```

## Common Operations (Cheat Sheet)

| Goal | Sync API | Async API |
|---|---|---|
| insert/update | `insert(k, v)` | `insert(k, v).await` |
| read | `get(&k)` | `get(&k).await` |
| delete | `remove(&k)` | `remove(&k).await` |
| batch insert | `batch_insert(items)` | `batch_insert(items).await` |
| batch read | `batch_get(&keys)` | `batch_get(&keys).await` |
| conditional update | `compute_if_present(&k, f)` | `compute_if_present(&k, f).await` |
| conditional insert | `compute_if_absent(k, f)` | `compute_if_absent(k, f).await` |
| metrics/introspection | `shard_stats()` / `memory_stats()` | `shard_stats().await` / `memory_stats().await` |

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
- migration is finalized when `advance_rebalance(...)` drains all source shards.

## Snapshot Modes (`v2.2.1`)

`SnapshotMode` lets you pick snapshot behavior per workload:

- `Clone`: always rebuild snapshot entries (default, lowest write-path overhead).
- `Cached`: reuse cached snapshot while no writes occur.
- `Cow`: maintain per-shard COW snapshot views for snapshot-heavy workloads.

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
- Global iteration/snapshot is shard-snapshot based, not a global serializable transaction.
- During online rebalance, active-first + previous-fallback keeps key reachability.

## Performance Notes

- Sharding reduces contention versus a single `RwLock<HashMap<..>>` under mixed load.
- Lazy shard allocation keeps memory proportional to touched shards.
- `rayon` improves large snapshot flatten throughput when scan size is high.
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

## Validation

Before release or upgrade verification:

```bash
cargo fmt --all
cargo test --all-features
cargo check --all-features
```

## Current Limits

- Not lock-free; hot-shard writer pressure can still serialize.
- Snapshot operations still materialize `Vec<(K, V)>` as output format.
- `RebalanceOptions` fields `background`, `batch_size`, `max_pause_ns` are forward-compatible placeholders in `v2.2.1`.

## License

Dual license:
- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)

You may choose either license.

## Disclaimer

- Benchmarks and throughput notes in this README are indicative, not guaranteed.
- Always validate behavior, latency, and memory usage under your own workload before production rollout.
