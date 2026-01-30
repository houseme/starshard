# Changelog

All notable changes to this project will be documented in this file.

## [0.8.0] - 2026-01-30

### Added

#### Core Features (v0.8.0)

- **Entry API**: Rust-idiomatic `entry()` method for efficient in-place updates
    - `Entry::Occupied` and `Entry::Vacant` enum variants
    - `or_insert()`, `or_insert_with()`, `and_modify()` chainable methods
    - Zero extra lock acquisition for conditional updates
- **Batch Operations**: Amortized lock acquisition for multiple keys
    - `batch_insert()`: Insert multiple key-value pairs
    - `batch_remove()`: Remove multiple keys
    - `batch_get()`: Fetch multiple keys in one call
- **Conditional Operations**:
    - `compute_if_present()`: Update value only if key exists; remove if closure returns None
    - `compute_if_absent()`: Insert only if key is absent; return final value
- **Collection Views**:
    - `keys()`: Iterate over all keys
    - `values()`: Iterate over all values
    - `entries()`: Alias for `iter()` (consistency with std)
- **Shard Introspection**:
    - `shard_stats()`: Returns `ShardStats` with detailed shard distribution
    - `shard_utilization()`: Percentage of initialized shards
- **Async Equivalents**: All v0.8.0 methods available on `AsyncShardedHashMap`

#### Infrastructure

- New feature flags: `entry`, `batch`, `ttl`, `metrics`, `advanced-iter`, `transactions`, `cas`, `replication`,
  `diagnostics`
- Comprehensive test suite (50+ new unit tests)
- Example programs: `v080_entry_api`, `v080_async_batch`
- ROADMAP.md: Multi-version development plan (v0.8, v0.9, v1.0)
- IMPLEMENTATION_SUMMARY.md: Design decisions, architecture, performance characteristics

## [0.7.0] - 2026-01-30

- add cross-platform test
- chore(deps): bump tokio from 1.48.0 to 1.49.0 and serde_json from 1.0.145 to 1.0.149

## [0.6.0] - 2025-11-23

- set `starshard` version 0.6.0
- upgrade `actions/checkout` from `v5` to `v6`
- upgrade crates version
- chore(deps): bump github/codeql-action from 3 to 4 (#1)

## [0.5.0] - 2025-09-29

- Replace `fxhash` with `rustc-hash` for improved hashing performance and compatibility

## [0.4.0] - 2025-09-26

- improve for README
- feat(serde): finalize serde support, add async snapshot serialization, doc updates

## [0.3.0] - 2025-09-26

- improve for v0.3.0

## [0.2.0] - 2025-09-25

- rename async func
- fmt MIT License
- chore(init): bootstrap starshard crate (sync/async sharded HashMap)
