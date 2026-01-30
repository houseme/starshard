# Starshard

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

English | [简体中文](README_CN.md)

<b>Starshard</b>: a high-performance, lazily sharded concurrent HashMap for Rust.

<code>Sync + Async + Optional Rayon + Optional Serde</code>

---

## Status

Early stage. API may still evolve (semantic stability prioritized; minor naming changes possible).

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
6. Extensible design (future: rebalancing, eviction, metrics).

---

## Features

| Feature | Description                                          | Notes                          |
|---------|------------------------------------------------------|--------------------------------|
| `async` | Adds `AsyncShardedHashMap` (Tokio `RwLock`)          | Independent of `rayon`         |
| `rayon` | Parallel snapshot flatten for large iteration        | Used internally; API unchanged |
| `serde` | Serialize/Deserialize (sync) + async snapshot helper | Hasher not persisted           |
| (none)  | Pure sync core                                       | Lowest dependency surface      |

Enable all in docs.rs via:

```toml
[package.metadata.docs.rs]
all-features = true
```

---

## Installation

```toml
[dependencies]
starshard = { version = "0.7", features = ["async", "rayon", "serde"] }
# or minimal:
# starshard = "0.7"
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

Do benchmark with your own key/value distribution and CPU topology.

---

## Safety / Concurrency

- No nested multi-shard lock ordering -> avoids deadlocks.
- Each shard single `RwLock`; iteration snapshots avoid long-lived global blocking.
- Cloning values required (trade memory for contention isolation).
- Not lock-free: intense write focus on one shard can still serialize.

---

## Limitations

- No dynamic shard rebalancing.
- No eviction / TTL.
- Snapshot iteration allocates intermediate vectors.
- Hasher state not serialized.
- No lock-free progress guarantees.

---

## Roadmap (Potential)

- Optional adaptive shard expansion / rebalancing.
- Per-shard eviction strategies (LRU / segmented).
- Metrics hooks (pre/post op instrumentation).
- Batched multi-insert API.
- Zero-copy or COW snapshot mode.

---

## Version 0.8.0 Features (Entry API & Batch Operations)

### Entry API (Idiomatic Updates)

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);

// Efficient in-place update without re-locking
match map.entry("counter".into()) {
    starshard::Entry::Occupied(mut entry) => {
        let val = entry.get();
        entry.insert(val + 1);
    }
    starshard::Entry::Vacant(entry) => {
        entry.insert(0);
    }
}
```

### Batch Operations

```rust
// Batch insert (amortizes shard lock acquisition)
let entries = vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)];
let inserted = map.batch_insert(entries);

// Batch remove
let removed = map.batch_remove(vec!["a".into(), "b".into()]);

// Batch get
let keys = vec!["a", "b", "c"];
let results = map.batch_get(&keys);
```

### Conditional Operations

```rust
// Update only if present
map.compute_if_present(&"key".into(), |v| Some(v * 2));

// Insert only if absent
let val = map.compute_if_absent("key".into(), || 42);
```

### Collection Views & Introspection

```rust
// Iterate keys/values separately
for key in map.keys() {
    println!("Key: {}", key);
}

for value in map.values() {
    println!("Value: {}", value);
}

// Shard statistics
let stats = map.shard_stats();
println!("Initialized shards: {}", stats.initialized);
println!("Avg load: {:.2}", stats.avg_load);
println!("Utilization: {:.1}%", map.shard_utilization());

// Retain entries matching predicate
map.retain(|k, v| v > &10);
```

---

## Version 0.9.0 Features (Eviction, Metrics, Advanced Iteration)

**Planned features**:

- **TTL & Eviction**: LRU/LFU/custom eviction policies with background cleanup
- **Metrics Hooks**: Production observability with hit rates, latency tracking
- **Advanced Iteration**: Filter + limit + parallel control builders
- **Drain Operations**: Efficient bulk removal

See [ROADMAP.md](ROADMAP.md) for detailed specification.

---

## Version 1.0.0 Features (Transactions, CAS, Replication)

**Planned features**:

- **MVCC Transactions**: Atomic multi-key operations with conflict detection
- **Compare-And-Swap**: Lock-free coordination primitives
- **Copy-On-Write Snapshots**: Minimal-contention read optimization
- **Distributed Replication**: Quorum-based consistency framework
- **Lock Diagnostics**: Per-shard contention profiling

See [ROADMAP.md](ROADMAP.md) for detailed specification.

---

## Feature Matrix

| Feature                    | v0.7 | v0.8 | v0.9 | v1.0 | Status     |
|----------------------------|------|------|------|------|------------|
| Sharded HashMap (sync)     | ✅    | ✅    | ✅    | ✅    | Stable     |
| Async (Tokio)              | ✅    | ✅    | ✅    | ✅    | Stable     |
| Parallel iteration (rayon) | ✅    | ✅    | ✅    | ✅    | Stable     |
| Serde (de)serialization    | ✅    | ✅    | ✅    | ✅    | Stable     |
| Entry API                  | -    | ✅    | ✅    | ✅    | New in 0.8 |
| Batch operations           | -    | ✅    | ✅    | ✅    | New in 0.8 |
| TTL/Eviction               | -    | -    | ✅    | ✅    | Planned    |
| Metrics                    | -    | -    | ✅    | ✅    | Planned    |
| Transactions               | -    | -    | -    | ✅    | Planned    |
| CAS operations             | -    | -    | -    | ✅    | Planned    |
| Replication                | -    | -    | -    | ✅    | Planned    |

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