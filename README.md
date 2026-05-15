# Starshard

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

English | [简体中文](README_CN.md)

<b>Starshard</b>: a high-performance, lazily sharded concurrent HashMap for Rust.

<code>Sync + Async + Optional Rayon + Optional Serde + Lifecycle + Advanced Features</code>

---

## Status

Production-ready (v2.1+). API stability prioritized.

## Motivation

You often need a fast concurrent map:

- Standard single `RwLock<HashMap<..>>` becomes contended under mixed read/write load.
- Fully lock-free or CHM designs can add memory + complexity cost.
- Sharding with lazy initialization offers a pragmatic middle ground.

Starshard focuses on:

1. Minimal uncontended overhead.
2. Lazy shard allocation (memory proportional to actually touched shards).
3. Atomic cached length.
4. Snapshot iteration (parallel if `rayon`).
5. Symmetric sync / async APIs.
6. Extensible design (lifecycle management, advanced concurrency).

---

## Features

| Feature     | Description                                          | Notes                          |
|-------------|------------------------------------------------------|--------------------------------|
| `async`     | Adds `AsyncShardedHashMap` (Tokio `RwLock`)          | Independent of `rayon`         |
| `rayon`     | Parallel snapshot flatten for large iteration        | Used internally; API unchanged |
| `serde`     | Serialize/Deserialize (sync) + async snapshot helper | Hasher not persisted           |
| `lifecycle` | Lifecycle utilities + primitives (`per_shard_load`, `memory_stats`, `drain`, metrics/eviction config types) | Consolidated v0.9+ features |
| `advanced`  | Transactions, CAS, Replication, Diagnostics          | Consolidated v1.0 features     |
| (none)      | Pure sync core + Batch Ops                           | Lowest dependency surface      |

Enable all in docs.rs via:

```toml
[package.metadata.docs.rs]
all-features = true
```

Internal layout in `v2.1.0`:

- `src/core/sync_impl.rs`
- `src/core/async_impl.rs`
- `src/core/types.rs`
- `src/core/helpers.rs`
- `src/serde/sync_serde.rs`
- `src/serde/async_snapshot.rs`

---

## Installation

```toml
[dependencies]
starshard = { version = "2.1.0", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# or minimal:
# starshard = "2.1.0"
```

`serde_json` (tests / examples):

```toml
[dev-dependencies]
serde_json = "1"
```

---

## Quick Start (Sync)

```rust
use starshard::ShardedHashMap;
use rustc_hash::FxBuildHasher;

let map: ShardedHashMap<String, i32, FxBuildHasher> = ShardedHashMap::new(64);
map.insert("a".into(), 1);
assert_eq!(map.get(&"a".into()), Some(1));
assert_eq!(map.len(), 1);
```

### Custom Hasher (defense against adversarial keys)

```rust
use starshard::ShardedHashMap;
use std::collections::hash_map::RandomState;

let secure = ShardedHashMap::<String, u64, RandomState>
::with_shards_and_hasher(128, RandomState::default ());
secure.insert("k".into(), 7);
```

---

## Async Usage

```rust
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let m: AsyncShardedHashMap<String, u32> = AsyncShardedHashMap::new(64);
    m.insert("x".into(), 42).await;
    assert_eq!(m.get(&"x".into()).await, Some(42));
}
```

## Adaptive Rebalance (`v2.1+`)

Sync:

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.rebalance_to(32, RebalanceOptions::default()).unwrap();
```

Online incremental (sync):

```rust
let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.start_rebalance_online(32).unwrap();
while m.rebalance_status().state == "migrating" {
    m.advance_rebalance(2);
}
```

Async:

```rust
#[cfg(feature = "async")]
async fn rebalance_async(m: &starshard::AsyncShardedHashMap<String, i32>) {
    m.rebalance_to(32, starshard::RebalanceOptions::default()).await.unwrap();
}
```

---

## Parallel Iteration (`rayon`)

```rust
#[cfg(feature = "rayon")]
{
use starshard::ShardedHashMap;
let m: ShardedHashMap<String, u32> = ShardedHashMap::new(32);
for i in 0..50_000 {
m.insert(format ! ("k{i}"), i);
}
let count = m.iter().count(); // internal parallel flatten
assert_eq!(count, 50_000);
}
```

---

## Serde Semantics

Sync:

- Serialized shape: `{ "shard_count": usize, "entries": [[K,V], ...] }`.
- Hasher internal state not preserved; recreated with `S::default()`.
- Requirements: `K: Eq + Hash + Clone + Serialize + Deserialize`, `V: Clone + Serialize + Deserialize`,
  `S: BuildHasher + Default + Clone`.

Async:

- No direct `Serialize`; call:

```rust
#[cfg(all(feature = "async", feature = "serde"))]
{
let snap = async_map.async_snapshot_serializable().await;
let json = serde_json::to_string( & snap).unwrap();
}
```

- To reconstruct: create a new async map and bulk insert.

---

## Consistency Model

- Per-shard ops are linearizable w.r.t that shard.
- Global iteration builds a per-shard snapshot as each shard lock is taken (not a fully atomic global view).
- `len()` is maintained atomically (structural insert/remove only).
- Iteration after concurrent writes may omit late inserts performed after a shard snapshot was captured.

---

## Performance Notes (Indicative)

| Scenario                                              | Observation (relative)       |
|-------------------------------------------------------|------------------------------|
| Read-heavy mixed workload vs global `RwLock<HashMap>` | Reduced contention           |
| Large snapshot iteration with `rayon` (100k+)         | 3-4x speedup flattening      |
| Sparse shard usage                                    | Only touched shards allocate |
| Batch Insert/Remove                                   | Single lock per shard group + sparse grouping in v2.0.0 |
| Online rebalance                                      | Incremental shard migration with active-first reads |

Do benchmark with your own key/value distribution and CPU topology.

---

## Safety / Concurrency

- No nested multi-shard lock ordering -> avoids deadlocks.
- Each shard single `RwLock`; iteration snapshots avoid long-lived global blocking.
- Cloning values required (trade memory for contention isolation).
- Not lock-free: intense write focus on one shard can still serialize.

---

## Limitations

- Dynamic shard rebalancing supports stop-the-world + online incremental modes (`v2.1`).
- Snapshot iteration allocates intermediate vectors.
- Hasher state not serialized.
- No lock-free progress guarantees.

---

## Roadmap (Potential)

- Extended rebalance telemetry and cancellation controls.
- Zero-copy or COW snapshot mode.

---

## Core Features (v0.8+)

### Conditional Operations (Method-based, highly efficient)

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);

// Update only if key exists; single shard lock
map.compute_if_present( & "counter".into(), | v| Some(v + 1));

// Insert only if key absent; single shard lock
let val = map.compute_if_absent("new_key".into(), | | 42);

// Conditional deletion
map.compute_if_present( & "key".into(), | _v| None);
```

### Batch Operations (Optimized)

```rust
// Batch insert (amortizes shard lock acquisition - single lock per shard)
let entries = vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)];
let inserted = map.batch_insert(entries);

// Batch remove
let removed = map.batch_remove(vec!["a".into(), "b".into()]);

// Batch get
let keys = vec!["a", "b", "c"];
let results = map.batch_get( & keys);
```

### Collection Views & Filtering

```rust
// Iterate all keys
let all_keys: Vec<_ > = map.keys().collect();

// Iterate all values
let all_values: Vec<_ > = map.values().collect();

// Standard iteration
for (k, v) in map.iter() {
println ! ("{}: {}", k, v);
}

// Retention filtering
map.retain( | k, v| v > & 10);  // Keep only entries where v > 10
```

### Shard Introspection

```rust
// Get shard distribution statistics
let stats = map.shard_stats();
println!("Total slots: {}", stats.total);
println!("Initialized shards: {}", stats.initialized);
println!("Avg load: {:.2}", stats.avg_load);

// Get utilization percentage
let util = map.shard_utilization();
println!("Utilization: {:.1}%", util);
```

---

## Lifecycle Features (v0.9+)

Enable via `features = ["lifecycle"]`.

- **Per-shard introspection**: `per_shard_load()` (sync + async)
- **Memory-oriented stats**: `memory_stats()` (sync + async)
- **Bulk drain**: `drain()` (sync + async)
- **Lifecycle primitives**: `EvictionConfig`, `EvictionPolicy`, `AtomicMetrics`, `IterBuilder`

```rust
#[cfg(feature = "lifecycle")]
{
    use starshard::ShardedHashMap;
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(16);
    map.insert("a".into(), 1);
    map.insert("b".into(), 2);

    let _loads = map.per_shard_load();
    let _memory = map.memory_stats();
    let drained: Vec<_> = map.drain().collect();
    assert_eq!(drained.len(), 2);
}
```

```rust
#[cfg(all(feature = "async", feature = "lifecycle"))]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(16);
    map.insert("a".into(), 1).await;

    let _loads = map.per_shard_load().await;
    let _memory = map.memory_stats().await;
    let drained: Vec<_> = map.drain().await.collect();
    assert_eq!(drained.len(), 1);
}
```

Note: lifecycle currently exposes utility APIs and configuration primitives; it does not yet provide a built-in autonomous TTL eviction scheduler.

---

## Advanced Features (v1.0+)

Enable via `features = ["advanced"]`.

- **MVCC Transactions**: Atomic multi-key operations with conflict detection
- **Compare-And-Swap**: Lock-free coordination primitives
- **Copy-On-Write Snapshots**: Minimal-contention read optimization
- **Distributed Replication**: Quorum-based consistency framework
- **Lock Diagnostics**: Per-shard contention profiling

---

## Feature Matrix

| Feature                    | v0.7 | v0.8 | v0.9 | v1.0 | v2.0 | Status          |
|----------------------------|------|------|------|------|------|-----------------|
| Sharded HashMap (sync)     | ✅    | ✅    | ✅    | ✅    | ✅    | Stable          |
| Async (Tokio)              | ✅    | ✅    | ✅    | ✅    | ✅    | Stable          |
| Parallel iteration (rayon) | ✅    | ✅    | ✅    | ✅    | ✅    | Stable          |
| Serde (de)serialization    | ✅    | ✅    | ✅    | ✅    | ✅    | Stable          |
| Conditional Operations     | -    | ✅    | ✅    | ✅    | ✅    | Stable          |
| Batch operations           | -    | ✅    | ✅    | ✅    | ✅    | Stable          |
| Lifecycle utilities        | -    | -    | ✅    | ✅    | ✅    | Stable (lifecycle) |
| Eviction/metrics primitives| -    | -    | ✅    | ✅    | ✅    | Stable (lifecycle) |
| Transactions               | -    | -    | -    | ✅    | ✅    | Stable (advanced) |
| CAS operations             | -    | -    | -    | ✅    | ✅    | Stable (advanced) |
| Replication                | -    | -    | -    | ✅    | ✅    | Stable (advanced) |

---

## Design Sketch

```

Arc -> RwLock<Vec<Option<Arc<RwLock<HashMap<K,V,S>>>>>> + AtomicUsize(len)

```

Lazy fill of inner `Option` slot when first key hashes into shard.

---

## Examples Summary

| Goal                  | Snippet                         |
|-----------------------|---------------------------------|
| Basic sync            | see Quick Start                 |
| Async insert/get      | see Async Usage                 |
| Parallel iterate      | enable `rayon`                  |
| Serde snapshot (sync) | `serde_json::to_string(&map)`   |
| Async serde snapshot  | `async_snapshot_serializable()` |
| Custom hasher         | `with_shards_and_hasher(..)`    |
| Custom shard cap      | `with_shards_and_hasher_capped(..)` |
| Strict validation     | `try_with_shards_and_hasher(..)` |

---

## Shard Count Safety

- Infallible constructors now enforce a default cap via `MAX_SHARDS` to prevent oversized allocations.
- For custom limits, use `with_shards_and_hasher_capped(shard_count, hasher, max_shards)`.
- For strict validation (no clamping), use `try_with_shards_and_hasher(..)` or
  `try_with_shards_and_hasher_capped(..)`, which return `ShardCountError`.

---

## License

Dual license: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) (choose either).

---

## Contribution

PRs welcome: focus on correctness (tests), simplicity, and documentation clarity.  
Run:

```bash
cargo clippy --all-features -- -D warnings
cargo test --all-features
```

---

## Minimal Example (All Features)

```rust
use starshard::{ShardedHashMap, AsyncShardedHashMap};
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    let sync_map: ShardedHashMap<u64, u64> = ShardedHashMap::new(32);
    sync_map.insert(1, 10);

    #[cfg(feature = "serde")]
    {
        let json = serde_json::to_string(&sync_map).unwrap();
        let _de: ShardedHashMap<u64, u64> = serde_json::from_str(&json).unwrap();
    }

    let async_map: AsyncShardedHashMap<u64, u64> = AsyncShardedHashMap::new(32);
    async_map.insert(2, 20).await;
}
```

---

## Disclaimer

Benchmarks and behavior notes are indicative only; validate under production load patterns.
