# Starshard 2.x vs 1.x: Usage Differences

## 1. Scope

- 1.x line: `1.0.0` to `1.2.x`
- 2.x line: `2.0.0` to `2.4.x`

This document focuses on **usage-level** differences and migration guidance.

## 2. Quick Summary

- Most basic KV APIs remain compatible.
- The key practical changes are:
  - safer shard-count constructor behavior in `2.0.0`,
  - optional rebalance APIs in `2.1+`,
  - optional snapshot modes in `2.2+`.

## 3. High-Level Differences

| Area | 1.x | 2.x |
|---|---|---|
| Shard-count safety | very large `shard_count` could cause memory pressure | default `MAX_SHARDS` guard, plus strict constructors |
| Constructor strategy | mostly direct constructors | compatibility + capped + strict constructor paths |
| Dynamic rebalance | not available | `rebalance_to`, `start_rebalance_online`, `advance_rebalance` |
| Snapshot strategy | traditional path | `SnapshotMode::{Clone, Cached, Cow}` |
| Migration effort | - | low for default usage, incremental for advanced usage |

## 4. Detailed Usage Changes

### 4.1 Constructor and Shard Safety (`2.0.0`)

New practical behavior in 2.x:

- Compatibility constructors still work but apply cap protection for oversized requests.
- New capped constructors:
  - `with_shards_and_hasher_capped(...)`
- New strict constructors (return `ShardCountError`):
  - `try_with_shards_and_hasher(...)`
  - `try_with_shards_and_hasher_capped(...)`

Recommended for external/user-driven configs:

```rust
use starshard::ShardedHashMap;
use rustc_hash::FxBuildHasher;

let map = ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
    4096,
    FxBuildHasher,
    8192,
)?;
```

Migration note:
- If your 1.x setup implicitly relied on very large shard counts always taking effect, audit that path after upgrade.

### 4.2 Rebalance APIs (`2.1+`)

1.x has no dynamic rebalance APIs.

2.1+ adds:
- Stop-the-world:
  - `rebalance_to(new_shard_count, options)`
- Online incremental:
  - `start_rebalance_online(new_shard_count)`
  - `advance_rebalance(batch_shards)`
  - `rebalance_status()`

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.rebalance_to(32, RebalanceOptions::default())?;
```

### 4.3 Snapshot Mode Selection (`2.2+`)

2.2+ makes snapshot strategy explicit:
- `SnapshotMode::Clone` (default)
- `SnapshotMode::Cached`
- `SnapshotMode::Cow`

And adds mode-aware constructors:
- `with_snapshot_mode(...)`
- `with_shards_and_hasher_and_snapshot_mode(...)`
- `with_shards_and_hasher_capped_and_snapshot_mode(...)`

```rust
use starshard::{ShardedHashMap, SnapshotMode};

let map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cached);
```

### 4.4 Performance-Relevant Behavior

- `batch_insert` / `batch_remove` / `batch_get` received grouping-path optimizations in 2.0.
- async `compute_if_present` path was aligned to avoid unnecessary remove+reinsert.

These are mostly transparent to callers.

### 4.5 Upgrading from 2.3.x to 2.4.0

Existing CRUD and Entry signatures remain available. New APIs include `shared_snapshot()`, `get_borrowed`, `contains_borrowed`, `remove_borrowed`, and `read_with`.

- `versioned_snapshot()` and `snapshot_at_version()` use the committed data epoch. Repeated snapshots without writes have the same version; query the returned version directly, without adding one. Historical versions are not retained, and epochs are not operation counters.
- `Cached` and `Cow` share a lazy whole-map cache. Valid hits bypass routing; rebuilds use a dedicated builder gate and ordered shard read locks. Ordinary reads can proceed subject to lock fairness, while writers and topology changes may wait. Use shared handles to avoid owned-output copies.
- Transactions are pessimistic operations over ordered shard locks, not MVCC. Snapshot and migration fixes preserve the same logical keys across both generations.
- Replication counts the primary in `replica_count` and `write_quorum`: two remote replicas plus one primary use `QuorumConfig::strict(3)` or `majority(3)`. Prefer `try_with_replication` for recoverable validation; `with_replication` panics on invalid topology or configuration.
- Replicated operations use one deadline for queueing, local application, and remote fanout. Success acknowledges quorum; remaining fanout is tracked and bounded, and later replicated writes wait for its cleanup. Errors or cancellation do not roll back already-applied local or remote writes. Reads remain local; no consensus or read quorum is implemented. Configured replication requires a Tokio runtime with its time driver enabled.
- `lock_profiles()` returns no samples until real instrumentation exists. TTL/eviction configuration and standalone metrics do not enable an autonomous scheduler.

Run the feature combinations you deploy and remeasure workload-specific latency, throughput, and memory. Allocation/hash reductions are not a universal throughput or p99 guarantee.

## 5. Feature Flag Reminder

If migrating from early 1.x, re-check `Cargo.toml` feature selection:

```toml
[dependencies]
starshard = { version = "2.4.0", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
```

## 6. Migration Checklist

1. Upgrade dependency to 2.x (recommended: `2.4.x`).
2. Review constructor entry points:
   - external input -> use `try_with_*`
   - fixed internal params -> compatibility constructors are fine
3. Adopt rebalance APIs only if you need runtime shard transitions.
4. Evaluate `SnapshotMode::Cached` / `Cow` for snapshot-heavy workloads.
5. Run validation:
   - `cargo test --all-features`
   - workload-specific latency/throughput/memory checks

## 7. Compatibility Conclusion

- For basic key-value usage, migration from 1.x to 2.x is usually low-risk.
- For systems relying on extreme shard counts or advanced snapshot/rebalance behavior, perform explicit migration validation.
