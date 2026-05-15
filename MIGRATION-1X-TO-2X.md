# Starshard 2.x vs 1.x: Usage Differences

## 1. Scope

- 1.x line: `1.0.0` to `1.2.x`
- 2.x line: `2.0.0` to `2.2.x`

This document focuses on **usage-level** differences and migration guidance.

## 2. Quick Summary

- Most basic KV APIs remain compatible.
- The key practical changes are:
  - safer shard-count constructor behavior in `2.0.0`,
  - optional rebalance APIs in `2.1+`,
  - optional snapshot modes in `2.1+`.

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

### 4.3 Snapshot Mode Selection (`2.1+`)

2.1+ makes snapshot strategy explicit:
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

## 5. Feature Flag Reminder

If migrating from early 1.x, re-check `Cargo.toml` feature selection:

```toml
[dependencies]
starshard = { version = "2.2.0", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
```

## 6. Migration Checklist

1. Upgrade dependency to 2.x (recommended: `2.2.x`).
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
