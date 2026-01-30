# Changelog

All notable changes to this project will be documented in this file.

## [1.0.0] - 2026-02-01

### Added

#### Advanced Concurrency & Distributed Features (v1.0.0)

- **Atomic Transactions**: MVCC-based multi-key operations
    - `Transaction<K, V>`: Structure for coordinated read/write/remove operations
    - `TransactionResult<T>`: Enum for commit/abort/conflict results
    - Per-shard versioning with optimistic locking
- **Compare-And-Swap (CAS) Operations**:
    - `compare_and_swap()`: Atomic value comparison and swap
    - `compare_and_remove()`: Conditional removal based on value matching
    - Lock-free coordination for multi-key consistency
- **Copy-On-Write (CoW) Snapshots**:
    - `CowSnapshot<K, V>`: Minimal-locking snapshot structure
    - Optimized for read-heavy workloads
    - Zero-copy iteration on snapshot data
- **Distributed Replication Framework**:
    - `Replica<K, V>` trait for implementing custom replicas
    - `ReplicationOp<K, V>`: Serializable operation types (Insert, Remove, Clear)
    - `QuorumConfig`: Majority and strict quorum configurations
    - Async replication with timeout handling
- **Lock Profiling & Diagnostics**:
    - `LockProfile`: Per-shard contention tracking and statistics
    - `IsolatedSnapshot<K, V>`: Time-travel queries with version tracking
    - Shard-level performance metrics (reads, writes, wait times)

#### Infrastructure (v1.0.0)

- New feature flags: `transactions`, `cas`, `cow-snapshot`, `replication`, `diagnostics`
- `async-trait` dependency for async trait support
- `dashmap` dependency for CoW snapshot alternatives
- `parking_lot` dependency for fair RwLock alternatives
- Comprehensive test suite for advanced features
- Full documentation with async patterns and examples

### Configuration Changes

**Cargo.toml**:

- Version bumped to 1.0.0
- Added dependencies: `async-trait`, `dashmap`, `parking_lot`
- Updated feature flags with new modules
- `full` feature includes all functionality

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
