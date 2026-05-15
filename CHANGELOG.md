# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [2.0.0] - 2026-05-15

### Changed

- **Version bump**:
    - Crate version updated from `1.2.0` to `2.0.0`.

- **Internal architecture refactor (API unchanged)**:
    - Split core sync implementation out of `src/lib.rs` into `src/sync_impl.rs`.
    - Split core async implementation out of `src/lib.rs` into `src/async_impl.rs`.
    - Extracted serde implementation into `src/serde_impl.rs`.
    - Kept public API surface and feature behavior stable while reducing `lib.rs` complexity.

- **Batch-path performance optimization**:
    - Reworked sync/async `batch_insert`, `batch_remove`, and `batch_get` grouping from per-call
      `HashMap<usize, Vec<_>>` to indexed shard buckets.
    - Reduced grouping overhead and extra hashing work on hot paths while preserving semantics.

### Added

- **Refactor guardrails and verification scripts**:
    - Added `scripts/export_api_surface.sh` for API-surface snapshot/export.
    - Added `scripts/verify_feature_matrix.sh` for repeatable feature-matrix + doctest validation.

- **CI hardening**:
    - Added dedicated feature matrix job in GitHub Actions.
    - Added dedicated doctest job in GitHub Actions.

### Documentation

- Updated `README.md` and `README_CN.md` for `2.0.0` version references.
- Synced feature matrix and performance notes with current `v2.0` implementation status.

## [1.2.0] - 2026-05-15

### Added

- **Lifecycle API completion**:
    - Added `memory_stats()` for `ShardedHashMap` and `AsyncShardedHashMap` under `lifecycle`.
    - Added `drain()` for `ShardedHashMap` and `AsyncShardedHashMap` under `lifecycle`.
    - Added async `per_shard_load()` for `AsyncShardedHashMap` under `lifecycle`.
- **Tests**:
    - Added regression tests for shard empty-count accounting.
    - Added lifecycle tests for memory stats and drain behavior (sync + async).

### Fixed

- **Shard statistics correctness**:
    - Fixed `ShardStats.empty` calculation in both sync and async `shard_stats()` implementations.

### Documentation

- Updated `README.md` lifecycle section to match current implementation and added examples for:
    - `per_shard_load()` (sync + async)
    - `memory_stats()` (sync + async)
    - `drain()` (sync + async)

## [1.1.0] - 2026-02-03

### Added

- **Shard Introspection**:
    - `per_shard_load()`: Returns detailed load statistics (`PerShardLoad`) for each initialized shard, including entry
      count and capacity. Available for both `ShardedHashMap` and `AsyncShardedHashMap` when the `lifecycle` feature is
      enabled.

- **Logging**:
    - Added `#[tracing::instrument]` to public APIs and core helpers across sync/async maps and lifecycle/advanced
      modules.

### Changed

- **Robust Error Handling**:
    - Replaced `unwrap()` in shard initialization and transaction shard lookup with logged fallbacks; transactions abort
      safely on missing shard indices.
- **Refactoring**:
    - Introduced shared std `RwLock` guard helpers to reduce repetitive poisoned-lock handling.

## [1.0.1] - 2026-02-02

### Changed

- **Feature Consolidation**:
    - Merged `ttl`, `metrics`, and `advanced-iter` features into a single `lifecycle` feature.
    - Merged `transactions`, `cas`, `cow-snapshot`, `replication`, and `diagnostics` features into a single `advanced`
      feature.
    - Promoted `batch` operations to core functionality (removed `batch` feature flag).
    - Updated `full` feature to include `async`, `rayon`, `serde`, `lifecycle`, and `advanced`.

### Optimized

- **Batch Operations**:
    - Optimized `batch_insert` and `batch_remove` to acquire the shard lock only once per shard group, significantly
      reducing lock contention.

## [1.0.0] - 2026-01-31

### Added

#### Advanced Concurrency & Distributed Features

- **Atomic Transactions (MVCC-based)**:
    - `Transaction<K, V>`: Multi-key atomic operations with read/write/remove support
    - `execute_transaction()`: Sync and async transaction execution with deadlock prevention
    - `TransactionResult<T>`: Result enum (Committed, Aborted, Conflict)
    - Shard-level locking with sorted indices to prevent deadlocks

- **Compare-And-Swap (CAS) Operations**:
    - `compare_and_swap(key, expected, new)`: Atomic value replacement
    - `compare_and_remove(key, expected)`: Conditional removal
    - `CasResult<V>`: Success/Failure result with current value
    - Full support for both sync and async variants

- **Copy-On-Write Snapshots**:
    - `cow_snapshot()`: Zero-allocation snapshot for read-heavy workloads
    - `CowSnapshot<K, V>`: Immutable snapshot with version tracking
    - Minimal lock contention during snapshot creation
    - Iterator support with snapshot isolation guarantees

- **Snapshot Isolation & Versioning**:
    - `versioned_snapshot()`: Create timestamped snapshots for time-travel queries
    - `snapshot_at_version(version)`: Retrieve snapshot at specific version
    - `IsolatedSnapshot<K, V>`: Version-tagged snapshot with age tracking
    - Monotonic version counter for consistency

- **Lock Profiling & Diagnostics**:
    - `lock_profiles()`: Per-shard lock contention statistics
    - `enable_profiling(bool)`: Toggle profiling with minimal overhead
    - `LockProfile`: Metrics including reads, writes, contention count, wait times
    - Runtime-configurable profiling for production diagnostics

- **Distributed Replication Framework** (async only):
    - `Replica<K, V>` trait: Interface for custom replica implementations
    - `with_replication()`: Configure map with replica set and quorum
    - `insert_replicated()` / `remove_replicated()`: Quorum-based operations
    - `ReplicationOp<K, V>`: Serializable operation types (Insert, Remove, Clear)
    - `QuorumConfig`: Majority and strict quorum configurations
    - Parallel async replication with configurable timeouts
    - `ReplicaError`: Comprehensive error handling for replication failures

### Implementation Details

- **Type Safety**: All generic types use `'static` bounds for async compatibility
- **Zero External Dependencies**: No `dashmap` or `parking_lot` - uses only `hashbrown` and std library
- **Type Aliases**: `ReplicaList<K, V>` to reduce type complexity (clippy-clean)
- **Version Tracking**: `Arc<AtomicUsize>` for lock-free version increments
- **Feature Flags**: All functionality behind `advanced` feature flag

### Testing

- **40+ Test Cases**: Comprehensive coverage for all features
    - Transaction tests (5): basic commit, multi-shard, read ops, empty, async
    - CAS tests (11): success/failure scenarios, multi-threaded, async variants
    - CoW snapshot tests (6): creation, isolation, iteration, race conditions
    - Versioned snapshot tests (8): ordering, version queries, time-travel
    - Lock profiling tests (3): enable/disable, structure validation
    - Replication tests (4): quorum configs, insert/remove operations
    - Integration tests (3): cross-feature interactions

### Fixed

- Type complexity warning (clippy): Added `ReplicaList<K, V>` type alias
- Lifetime errors: Added `'static` bounds to K and V types for async compatibility
- Test race conditions: Used `Barrier` for proper synchronization
- Quorum validation: Corrected replica count matching in tests

### Documentation

- 14 detailed documentation files covering implementation and fixes
- API documentation with examples for all public methods
- Comprehensive test suite serving as usage examples
- Full async/await pattern documentation

## [0.9.0] - 2026-01-31

### Added

#### Production Features (v0.9.0)

- **TTL & Eviction System**:
    - `EvictionPolicy` enum: LRU, LFU, TimeToLive, Custom predicates
    - `EvictionConfig`: Configurable policy, max_entries, check intervals
    - Background eviction task with atomic control
    - Per-shard eviction statistics tracking
- **Metrics Collection**:
    - `MetricsStats`: Comprehensive operation counters
    - `AtomicMetrics`: Lock-free metrics aggregation
    - `MemoryStats`: Memory utilization tracking
    - Hit rate calculation and snapshot functionality
    - `reset_metrics()`: Clear accumulated statistics
- **Advanced Iteration Builders**:
    - `IterBuilder<K, V>`: Fluent API for complex iteration patterns
    - Filter, limit, and parallel control
    - `collect()`, `for_each()` execution methods
- **Drain Operation**:
    - `DrainIterator<K, V>`: Efficient bulk removal
    - Lock-per-shard acquisition
    - ExactSizeIterator implementation
    - Zero intermediate allocation
- **Extended Introspection**:
    - `memory_stats()`: Shard allocation and load factor
    - `per_shard_load()`: Per-shard entry counts
    - Detailed shard distribution metrics

#### Infrastructure (v0.9.0)

- New feature flags: `ttl`, `metrics`, `advanced-iter`
- Type aliases for reduced complexity
- Comprehensive test suite (30+ tests)
- Performance benchmarking infrastructure
- Documentation examples for each feature

### Configuration Changes

**Cargo.toml**:

- Added optional dependencies: `parking_lot`
- New feature flags for modular enablement
- Updated `full` feature set

#### v0.8.0 & v1.0.0 Modules (Specification)

- `eviction.rs`: TTL, eviction policies, metrics, advanced iteration framework
- `advanced.rs`: Transactions, CAS, CoW snapshots, replication, diagnostics framework

## [0.8.0] - 2026-01-30

### Added

#### Core Features (v0.8.0)

- **Conditional Update Operations**: Efficient in-place updates with single shard lock
    - `compute_if_present()`: Update value only if key exists; remove if closure returns None
    - `compute_if_absent()`: Insert only if key is absent; return final value
    - Single lock acquisition per operation (no extra contention)
    - Ideal for check-then-update patterns common in concurrent scenarios
- **Batch Operations**: Amortized lock acquisition for multiple keys
    - `batch_insert()`: Insert multiple key-value pairs in grouped shard batches
    - `batch_remove()`: Remove multiple keys with efficient per-shard grouping
    - `batch_get()`: Fetch multiple keys in one call, preserving order
- **Filtering & Retention**:
    - `retain()`: Remove entries where predicate returns false (per-shard locking)
- **Collection Views**:
    - `keys()`: Iterate over all keys (snapshot-based)
    - `values()`: Iterate over all values (snapshot-based)
    - `iter()`: Full key-value pair iteration with optional rayon parallelism
- **Shard Introspection**:
    - `shard_stats()`: Returns `ShardStats` with detailed shard distribution metrics
    - `shard_utilization()`: Percentage of initialized shards for capacity planning
    - Per-shard load tracking and distribution analysis
- **Async Equivalents**: All v0.8.0 methods available on `AsyncShardedHashMap`

#### Infrastructure

- New feature flags: `batch` (Entry API provided as methods, not as traditional enum)
- Conditional operations available on both `ShardedHashMap` and `AsyncShardedHashMap`
- Comprehensive test suite (50+ new unit tests for conditional operations, batch ops, filtering)
- Example programs: `v080_entry_api.rs` (conditional operations & views), `v080_async_batch.rs` (async variants)
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
