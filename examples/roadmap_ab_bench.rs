//! Local A/B diagnostics, not a production latency or scalability claim.
//!
//! Build: `cargo build --release --example roadmap_ab_bench --features async`.
//! Run: `roadmap_ab_bench snapshot-write sync 4096 1000 3 cow`.
//! Arguments: workload, sync|async, entries, iterations, samples, cow|cached|clone.
//! Workloads: read, snapshot-hit, snapshot-write, snapshot-scan, shared-write,
//! migration, background (candidate only). Async uses a current-thread runtime constructed before measurement.
//!
//! To build the same driver against baseline 16deb5b, copy this example to that
//! checkout and use `cargo rustc --release --example roadmap_ab_bench`
//! `--features async -- --cfg roadmap_baseline`. The adapter labels the
//! baseline's Arc<Vec> snapshot and whole-source-shard migration explicitly;
//! these are equivalent end results, not equivalent per-step work budgets.
//! Run each workload A1/B1/B2/A2, retain every JSONL row, and examine baseline
//! drift before interpreting differences. Audit other workloads separately.
//!
//! Snapshot/read throughput runs at least 100 ms; latency uses the configured
//! iteration count. Throughput and latency use separate passes; no allocation/clone counters are
//! enabled in those passes (the allocator flag check remains). Latency percentiles are nearest-rank observations
//! of the named operation/cycle, including clock overhead, not service p99.
//! Counts use a third pass with a 256-byte inline value, avoiding driver payload
//! allocations. Allocation bytes are requested bytes (including reallocations),
//! not RSS or peak/live memory. Final map destruction and setup are excluded.

use serde_json::json;
use starshard::{ShardedHashMap, SnapshotMode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static KEY_CLONES: AtomicU64 = AtomicU64::new(0);
static VALUE_CLONES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

fn record_allocation(pointer: *mut u8, bytes: usize) {
    if !pointer.is_null() && COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

// SAFETY: every operation delegates to System with exactly the caller's pointer,
// layout and size. Counters perform no allocation and do not alter the result.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller provides a valid allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        record_allocation(pointer, layout.size());
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller provides a valid allocation layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        record_allocation(pointer, layout.size());
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: allocations are delegated to System, preserving its contract.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller supplies the original layout and valid new size.
        let result = unsafe { System.realloc(pointer, layout, size) };
        record_allocation(result, size);
        result
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Eq, PartialEq, Hash)]
struct Key<const COUNT: bool>(usize);

impl<const COUNT: bool> Clone for Key<COUNT> {
    fn clone(&self) -> Self {
        if COUNT {
            KEY_CLONES.fetch_add(1, Ordering::Relaxed);
        }
        Self(self.0)
    }
}

struct Value<const COUNT: bool>([u8; 256]);

impl<const COUNT: bool> Value<COUNT> {
    fn new(value: usize) -> Self {
        Self([value as u8; 256])
    }
}

impl<const COUNT: bool> Clone for Value<COUNT> {
    fn clone(&self) -> Self {
        if COUNT {
            VALUE_CLONES.fetch_add(1, Ordering::Relaxed);
        }
        Self(self.0)
    }
}

type Map<const COUNT: bool> = ShardedHashMap<Key<COUNT>, Value<COUNT>>;
#[cfg(feature = "async")]
type AsyncMap<const COUNT: bool> = starshard::AsyncShardedHashMap<Key<COUNT>, Value<COUNT>>;

// Scope the custom baseline cfg to this adapter; no library feature or Cargo
// dependency changes are needed to compile the identical source against A/B.
#[allow(unexpected_cfgs)]
mod adapter {
    use super::*;

    pub enum Held<const COUNT: bool> {
        Legacy(Arc<Vec<(Key<COUNT>, Value<COUNT>)>>),
        #[cfg(not(roadmap_baseline))]
        View(starshard::SnapshotView<Key<COUNT>, Value<COUNT>>),
    }

    impl<const COUNT: bool> Held<COUNT> {
        pub fn len(&self) -> usize {
            match self {
                Self::Legacy(items) => items.len(),
                #[cfg(not(roadmap_baseline))]
                Self::View(view) => view.len(),
            }
        }

        pub fn checksum(&self) -> u64 {
            match self {
                Self::Legacy(items) => items.iter().fold(0_u64, |sum, (key, value)| {
                    sum.wrapping_add(black_box(key.0 as u64 + value.0[0] as u64))
                }),
                #[cfg(not(roadmap_baseline))]
                Self::View(view) => view.iter().fold(0_u64, |sum, (key, value)| {
                    sum.wrapping_add(black_box(key.0 as u64 + value.0[0] as u64))
                }),
            }
        }
    }

    pub fn hold<const COUNT: bool>(map: &Map<COUNT>, shared: bool) -> Held<COUNT> {
        #[cfg(roadmap_baseline)]
        {
            let _ = shared;
            Held::Legacy(map.shared_snapshot())
        }
        #[cfg(not(roadmap_baseline))]
        if shared {
            Held::Legacy(map.shared_snapshot())
        } else {
            Held::View(map.snapshot_view())
        }
    }

    #[cfg(feature = "async")]
    pub async fn async_hold<const COUNT: bool>(map: &AsyncMap<COUNT>, shared: bool) -> Held<COUNT> {
        #[cfg(roadmap_baseline)]
        {
            let _ = shared;
            Held::Legacy(map.shared_snapshot().await)
        }
        #[cfg(not(roadmap_baseline))]
        if shared {
            Held::Legacy(map.shared_snapshot().await)
        } else {
            Held::View(map.snapshot_view().await)
        }
    }

    pub fn advance<const COUNT: bool>(map: &Map<COUNT>) {
        #[cfg(roadmap_baseline)]
        black_box(map.advance_rebalance(1));
        #[cfg(not(roadmap_baseline))]
        black_box(
            map.advance_rebalance_with_options(&starshard::RebalanceOptions {
                background: false,
                batch_size: 128,
                max_pause_ns: 50_000,
            }),
        );
    }

    #[cfg(feature = "async")]
    pub async fn async_advance<const COUNT: bool>(map: &AsyncMap<COUNT>) {
        #[cfg(roadmap_baseline)]
        black_box(map.advance_rebalance(1).await);
        #[cfg(not(roadmap_baseline))]
        black_box(
            map.advance_rebalance_with_options(&starshard::RebalanceOptions {
                background: false,
                batch_size: 128,
                max_pause_ns: 50_000,
            })
            .await,
        );
    }

    pub fn background(config: &Config, sample: usize) {
        #[cfg(roadmap_baseline)]
        println!(
            "{}",
            json!({"sample": sample, "workload": "background", "status": "unsupported_by_baseline"})
        );
        #[cfg(roadmap_baseline)]
        let _ = config;
        #[cfg(not(roadmap_baseline))]
        background_report(
            config,
            sample,
            background_sync_pass::<false>(config, false),
            background_sync_pass::<false>(config, true),
            background_sync_pass::<true>(config, false),
        );
    }

    #[cfg(feature = "async")]
    pub async fn async_background(config: &Config, sample: usize) {
        #[cfg(roadmap_baseline)]
        println!(
            "{}",
            json!({"sample": sample, "workload": "background", "status": "unsupported_by_baseline"})
        );
        #[cfg(roadmap_baseline)]
        let _ = config;
        #[cfg(not(roadmap_baseline))]
        background_report(
            config,
            sample,
            background_async_pass::<false>(config, false).await,
            background_async_pass::<false>(config, true).await,
            background_async_pass::<true>(config, false).await,
        );
    }

    #[cfg(not(roadmap_baseline))]
    struct BackgroundPass {
        read_ns: u128,
        migration_ns: u128,
        reads: usize,
        latencies: Vec<u128>,
        counts: [u64; 5],
    }

    #[cfg(not(roadmap_baseline))]
    fn background_sync_pass<const COUNT: bool>(config: &Config, latency: bool) -> BackgroundPass {
        let map = populate::<COUNT>(config);
        let mut latencies = Vec::with_capacity(config.iterations);
        if COUNT {
            begin_counts();
        }
        let started = Instant::now();
        // The public spawn API's internal worker creation is part of lifecycle
        // time. Foreground timing starts only after that API has returned.
        let handle = map
            .spawn_rebalance(
                64,
                starshard::RebalanceOptions {
                    background: true,
                    batch_size: 128,
                    max_pause_ns: 50_000,
                },
            )
            .unwrap();
        let read_start = Instant::now();
        let mut reads = 0;
        if !COUNT {
            while map.rebalance_status().state == "migrating" {
                let before = (latency && latencies.len() < config.iterations).then(Instant::now);
                black_box(map.get(&Key(reads % config.entries)));
                if let Some(before) = before {
                    latencies.push(before.elapsed().as_nanos());
                }
                reads += 1;
                if read_start.elapsed().as_secs() >= 10 {
                    handle.cancel();
                    panic!("background measurement exceeded 10 seconds");
                }
            }
        }
        let read_ns = read_start.elapsed().as_nanos();
        assert!(handle.join().unwrap().completed);
        let migration_ns = started.elapsed().as_nanos();
        let counts = if COUNT { finish_counts() } else { [0; 5] };
        assert_eq!(map.len(), config.entries);
        for index in 0..config.entries {
            assert!(map.get(&Key(index)).is_some());
        }
        BackgroundPass {
            read_ns,
            migration_ns,
            reads,
            latencies,
            counts,
        }
    }

    #[cfg(all(not(roadmap_baseline), feature = "async"))]
    async fn background_async_pass<const COUNT: bool>(
        config: &Config,
        latency: bool,
    ) -> BackgroundPass {
        let map = async_populate::<COUNT>(config).await;
        let mut latencies = Vec::with_capacity(config.iterations);
        if COUNT {
            begin_counts();
        }
        let started = Instant::now();
        let handle = map
            .spawn_rebalance(
                64,
                starshard::RebalanceOptions {
                    background: true,
                    batch_size: 128,
                    max_pause_ns: 50_000,
                },
            )
            .await
            .unwrap();
        let read_start = Instant::now();
        let mut reads = 0;
        if !COUNT {
            while map.rebalance_status().state == "migrating" {
                let before = (latency && latencies.len() < config.iterations).then(Instant::now);
                black_box(map.get(&Key(reads % config.entries)).await);
                if let Some(before) = before {
                    latencies.push(before.elapsed().as_nanos());
                }
                reads += 1;
                if read_start.elapsed().as_secs() >= 10 {
                    handle.cancel();
                    panic!("background measurement exceeded 10 seconds");
                }
            }
        }
        let read_ns = read_start.elapsed().as_nanos();
        assert!(handle.join().await.unwrap().completed);
        let migration_ns = started.elapsed().as_nanos();
        let counts = if COUNT { finish_counts() } else { [0; 5] };
        assert_eq!(map.len().await, config.entries);
        for index in 0..config.entries {
            assert!(map.get(&Key(index)).await.is_some());
        }
        BackgroundPass {
            read_ns,
            migration_ns,
            reads,
            latencies,
            counts,
        }
    }

    #[cfg(not(roadmap_baseline))]
    fn background_report(
        config: &Config,
        sample: usize,
        throughput: BackgroundPass,
        mut latency: BackgroundPass,
        counts: BackgroundPass,
    ) {
        latency.latencies.sort_unstable();
        let quantile = |percent: usize| {
            latency
                .latencies
                .get(
                    (latency.latencies.len() * percent)
                        .div_ceil(100)
                        .saturating_sub(1),
                )
                .copied()
        };
        println!(
            "{}",
            json!({
                "sample": sample, "workload": "background", "candidate_only": true,
                "runtime": if config.asynchronous { "async_current_thread" } else { "sync_foreground_and_background_thread" },
                "entries": config.entries, "snapshot_mode": config.mode_name,
                "throughput_operations": throughput.reads, "throughput_elapsed_ns": throughput.read_ns,
                "throughput_per_second": (throughput.reads > 0 && throughput.read_ns > 0).then(|| throughput.reads as f64 * 1e9 / throughput.read_ns as f64),
                "operation_unit": "foreground_get_while_worker_migrates", "latency_unit": "foreground_get",
                "latency_sampling": "first_N_reads_of_separate_migration_run_including_clock_overhead",
            "foreground_driver_overhead": "status_polling_and_cooperative_deadline_checks",
                "latency_samples": latency.latencies.len(), "has_foreground_latency_evidence": !latency.latencies.is_empty(), "has_foreground_throughput_evidence": throughput.reads > 0, "p50_ns": quantile(50), "p95_ns": quantile(95), "p99_ns": quantile(99),
                "migration_elapsed_ns": throughput.migration_ns, "migration_timing_includes": "public_spawn_and_join_worker_lifecycle",
                "counter_scope": "separate_migration_without_foreground_reads", "allocation_calls": counts.counts[0],
                "allocated_requested_bytes": counts.counts[1], "deallocation_calls": counts.counts[2], "key_clones": counts.counts[3], "value_clones": counts.counts[4],
                "scope": "local_diagnostic_no_resource_isolation_or_production_latency_claim"
            })
        );
    }

    pub const SNAPSHOT_API: &str = if cfg!(roadmap_baseline) {
        "baseline_shared_arc_vec"
    } else {
        "candidate_snapshot_view"
    };
    pub const MIGRATION_BUDGET: &str = if cfg!(roadmap_baseline) {
        "one_complete_source_shard_per_call"
    } else {
        "128_entries_or_50000ns_cooperative_per_call"
    };
}

struct Config {
    workload: String,
    asynchronous: bool,
    entries: usize,
    iterations: usize,
    samples: usize,
    mode: SnapshotMode,
    mode_name: String,
}

impl Config {
    fn parse() -> Self {
        let args: Vec<_> = std::env::args().collect();
        let workload = args.get(1).map(String::as_str).unwrap_or("snapshot-write");
        assert!(
            matches!(
                workload,
                "read"
                    | "snapshot-hit"
                    | "snapshot-write"
                    | "snapshot-scan"
                    | "shared-write"
                    | "migration"
                    | "background"
            ),
            "unknown workload"
        );
        let number = |index: usize, default| {
            args.get(index)
                .map(|arg| arg.parse().expect("expected positive integer"))
                .unwrap_or(default)
        };
        let mode_name = args.get(6).cloned().unwrap_or_else(|| "cow".into());
        let mode = match mode_name.as_str() {
            "cow" => SnapshotMode::Cow,
            "cached" => SnapshotMode::Cached,
            "clone" => SnapshotMode::Clone,
            _ => panic!("mode must be cow, cached or clone"),
        };
        assert!(
            args.get(2)
                .is_none_or(|arg| arg == "sync" || arg == "async"),
            "runtime must be sync or async"
        );
        let result = Self {
            workload: workload.into(),
            asynchronous: args.get(2).is_some_and(|arg| arg == "async"),
            entries: number(3, 4096),
            iterations: number(4, 1000),
            samples: number(5, 3),
            mode,
            mode_name,
        };
        assert!(result.entries > 0 && result.iterations > 0 && result.samples > 0);
        result
    }

    fn shared(&self) -> bool {
        self.workload == "shared-write"
    }
}

fn populate<const COUNT: bool>(config: &Config) -> Map<COUNT> {
    let map = Map::with_snapshot_mode(32, config.mode);
    for index in 0..config.entries {
        map.insert(Key(index), Value::new(index));
    }
    map
}

#[cfg(feature = "async")]
async fn async_populate<const COUNT: bool>(config: &Config) -> AsyncMap<COUNT> {
    let map = AsyncMap::with_snapshot_mode(32, config.mode);
    for index in 0..config.entries {
        map.insert(Key(index), Value::new(index)).await;
    }
    map
}

fn operation<const COUNT: bool>(
    map: &Map<COUNT>,
    config: &Config,
    index: usize,
    scan: Option<&adapter::Held<COUNT>>,
) {
    match config.workload.as_str() {
        "read" => {
            black_box(map.get(&Key(index % config.entries)));
        }
        "snapshot-hit" => {
            black_box(adapter::hold(map, false).len());
        }
        "snapshot-scan" => {
            black_box(scan.expect("prepared snapshot").checksum());
        }
        "snapshot-write" | "shared-write" => {
            let held = adapter::hold(map, config.shared());
            black_box(map.insert(Key(index % config.entries), Value::new(index)));
            let updated = adapter::hold(map, config.shared());
            black_box((held.len(), updated.len()));
        }
        _ => unreachable!("migration has its own driver"),
    }
}

#[cfg(feature = "async")]
async fn async_operation<const COUNT: bool>(
    map: &AsyncMap<COUNT>,
    config: &Config,
    index: usize,
    scan: Option<&adapter::Held<COUNT>>,
) {
    match config.workload.as_str() {
        "read" => {
            black_box(map.get(&Key(index % config.entries)).await);
        }
        "snapshot-hit" => {
            black_box(adapter::async_hold(map, false).await.len());
        }
        "snapshot-scan" => {
            black_box(scan.expect("prepared snapshot").checksum());
        }
        "snapshot-write" | "shared-write" => {
            let held = adapter::async_hold(map, config.shared()).await;
            black_box(
                map.insert(Key(index % config.entries), Value::new(index))
                    .await,
            );
            let updated = adapter::async_hold(map, config.shared()).await;
            black_box((held.len(), updated.len()));
        }
        _ => unreachable!("migration has its own driver"),
    }
}

fn begin_counts() {
    for counter in [
        &ALLOCATIONS,
        &ALLOCATED_BYTES,
        &DEALLOCATIONS,
        &KEY_CLONES,
        &VALUE_CLONES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    COUNT_ALLOCATIONS.store(true, Ordering::Relaxed);
}

fn finish_counts() -> [u64; 5] {
    COUNT_ALLOCATIONS.store(false, Ordering::Relaxed);
    [
        &ALLOCATIONS,
        &ALLOCATED_BYTES,
        &DEALLOCATIONS,
        &KEY_CLONES,
        &VALUE_CLONES,
    ]
    .map(|counter| counter.load(Ordering::Relaxed))
}

struct Measurements {
    elapsed_ns: u128,
    latencies: Vec<u128>,
    counts: [u64; 5],
    counter_operations: usize,
    completed_operations: usize,
}

fn sync_snapshot(config: &Config) -> Measurements {
    let mut elapsed_ns = 0;
    let mut completed_operations = 0;
    let mut latencies = Vec::with_capacity(config.iterations);
    for latency_pass in [false, true] {
        let map = populate::<false>(config);
        let scan = (config.workload == "snapshot-scan").then(|| adapter::hold(&map, false));
        for index in 0..16 {
            operation(&map, config, index, scan.as_ref());
        }
        let start = Instant::now();
        if latency_pass {
            for index in 0..config.iterations {
                let started = Instant::now();
                operation(&map, config, index, scan.as_ref());
                latencies.push(started.elapsed().as_nanos());
            }
        } else {
            loop {
                for offset in 0..config.iterations {
                    operation(&map, config, completed_operations + offset, scan.as_ref());
                }
                completed_operations += config.iterations;
                if start.elapsed().as_millis() >= 100 {
                    break;
                }
            }
            elapsed_ns = start.elapsed().as_nanos();
        }
        assert_eq!(map.len(), config.entries);
    }
    let map = populate::<true>(config);
    let scan = (config.workload == "snapshot-scan").then(|| adapter::hold(&map, false));
    for index in 0..16 {
        operation(&map, config, index, scan.as_ref());
    }
    begin_counts();
    for index in 0..16 {
        operation(&map, config, index, scan.as_ref());
    }
    let counts = finish_counts();
    Measurements {
        elapsed_ns,
        latencies,
        counts,
        counter_operations: 16,
        completed_operations,
    }
}

#[cfg(feature = "async")]
async fn async_snapshot(config: &Config) -> Measurements {
    let mut elapsed_ns = 0;
    let mut completed_operations = 0;
    let mut latencies = Vec::with_capacity(config.iterations);
    for latency_pass in [false, true] {
        let map = async_populate::<false>(config).await;
        let scan = if config.workload == "snapshot-scan" {
            Some(adapter::async_hold(&map, false).await)
        } else {
            None
        };
        for index in 0..16 {
            async_operation(&map, config, index, scan.as_ref()).await;
        }
        let start = Instant::now();
        if latency_pass {
            for index in 0..config.iterations {
                let started = Instant::now();
                async_operation(&map, config, index, scan.as_ref()).await;
                latencies.push(started.elapsed().as_nanos());
            }
        } else {
            loop {
                for offset in 0..config.iterations {
                    async_operation(&map, config, completed_operations + offset, scan.as_ref())
                        .await;
                }
                completed_operations += config.iterations;
                if start.elapsed().as_millis() >= 100 {
                    break;
                }
            }
            elapsed_ns = start.elapsed().as_nanos();
        }
        assert_eq!(map.len().await, config.entries);
    }
    let map = async_populate::<true>(config).await;
    let scan = if config.workload == "snapshot-scan" {
        Some(adapter::async_hold(&map, false).await)
    } else {
        None
    };
    for index in 0..16 {
        async_operation(&map, config, index, scan.as_ref()).await;
    }
    begin_counts();
    for index in 0..16 {
        async_operation(&map, config, index, scan.as_ref()).await;
    }
    let counts = finish_counts();
    Measurements {
        elapsed_ns,
        latencies,
        counts,
        counter_operations: 16,
        completed_operations,
    }
}

fn sync_migration(config: &Config) -> Measurements {
    let mut latencies = Vec::with_capacity(config.entries + 64);
    let mut elapsed_ns = 0;
    for latency_pass in [false, true] {
        let map = populate::<false>(config);
        let start = Instant::now();
        map.start_rebalance_online(64).unwrap();
        while map.rebalance_status().state == "migrating" {
            let step = latency_pass.then(Instant::now);
            adapter::advance(&map);
            if let Some(step) = step {
                latencies.push(step.elapsed().as_nanos());
            }
        }
        if !latency_pass {
            elapsed_ns = start.elapsed().as_nanos();
        }
        assert_eq!(map.len(), config.entries);
        for index in 0..config.entries {
            assert!(map.get(&Key(index)).is_some());
        }
    }
    let map = populate::<true>(config);
    begin_counts();
    map.start_rebalance_online(64).unwrap();
    let mut steps = 0;
    while map.rebalance_status().state == "migrating" {
        adapter::advance(&map);
        steps += 1;
    }
    let counts = finish_counts();
    Measurements {
        elapsed_ns,
        latencies,
        counts,
        counter_operations: steps,
        completed_operations: config.entries,
    }
}

#[cfg(feature = "async")]
async fn async_migration(config: &Config) -> Measurements {
    let mut latencies = Vec::with_capacity(config.entries + 64);
    let mut elapsed_ns = 0;
    for latency_pass in [false, true] {
        let map = async_populate::<false>(config).await;
        let start = Instant::now();
        map.start_rebalance_online(64).await.unwrap();
        while map.rebalance_status().state == "migrating" {
            let step = latency_pass.then(Instant::now);
            adapter::async_advance(&map).await;
            if let Some(step) = step {
                latencies.push(step.elapsed().as_nanos());
            }
        }
        if !latency_pass {
            elapsed_ns = start.elapsed().as_nanos();
        }
        assert_eq!(map.len().await, config.entries);
        for index in 0..config.entries {
            assert!(map.get(&Key(index)).await.is_some());
        }
    }
    let map = async_populate::<true>(config).await;
    begin_counts();
    map.start_rebalance_online(64).await.unwrap();
    let mut steps = 0;
    while map.rebalance_status().state == "migrating" {
        adapter::async_advance(&map).await;
        steps += 1;
    }
    let counts = finish_counts();
    Measurements {
        elapsed_ns,
        latencies,
        counts,
        counter_operations: steps,
        completed_operations: config.entries,
    }
}

fn report(config: &Config, sample: usize, mut data: Measurements) {
    data.latencies.sort_unstable();
    let quantile = |percent: usize| {
        data.latencies
            .get(
                (data.latencies.len() * percent)
                    .div_ceil(100)
                    .saturating_sub(1),
            )
            .copied()
    };
    println!(
        "{}",
        json!({
            "sample": sample, "workload": config.workload,
            "runtime": if config.asynchronous { "async_current_thread" } else { "sync_single_thread" },
            "snapshot_mode": config.mode_name, "snapshot_api": if config.shared() { "shared_arc_vec" } else { adapter::SNAPSHOT_API },
            "entries": config.entries, "initial_shards": 32, "value_bytes": 256,
            "throughput_operations": data.completed_operations, "throughput_elapsed_ns": data.elapsed_ns,
            "throughput_per_second": data.completed_operations as f64 * 1e9 / data.elapsed_ns as f64,
            "operation_unit": if config.workload == "migration" { "migrated_entry" } else { "named_workload_cycle" },
            "latency_unit": if config.workload == "migration" { "advance_api_call" } else { "named_workload_cycle" },
            "latency_samples": data.latencies.len(), "p50_ns": quantile(50), "p95_ns": quantile(95), "p99_ns": quantile(99),
            "migration_elapsed_ns": (config.workload == "migration").then_some(data.elapsed_ns),
            "migration_budget": adapter::MIGRATION_BUDGET,
            "counter_operations": data.counter_operations,
            "counter_operation_unit": if config.workload == "migration" { "advance_api_call" } else { "named_workload_cycle" },
            "counter_scope": if config.workload == "migration" { "full_migration_of_entries_steps_may_differ_from_timing_pass" } else { "16_separate_workload_cycles_after_warmup" },
            "allocation_calls": data.counts[0],
            "allocated_requested_bytes": data.counts[1], "deallocation_calls": data.counts[2],
            "key_clones": data.counts[3], "value_clones": data.counts[4],
            "scope": "local_diagnostic_no_resource_isolation_or_production_latency_claim"
        })
    );
}

fn main() {
    let config = Config::parse();
    if config.workload == "background" {
        if config.asynchronous {
            #[cfg(feature = "async")]
            {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                for sample in 0..config.samples {
                    runtime.block_on(adapter::async_background(&config, sample));
                }
            }
            #[cfg(not(feature = "async"))]
            panic!("build with --features async for async workloads");
        } else {
            for sample in 0..config.samples {
                adapter::background(&config, sample);
            }
        }
        return;
    }
    if config.asynchronous {
        #[cfg(feature = "async")]
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            for sample in 0..config.samples {
                let data = runtime.block_on(async {
                    if config.workload == "migration" {
                        async_migration(&config).await
                    } else {
                        async_snapshot(&config).await
                    }
                });
                report(&config, sample, data);
            }
        }
        #[cfg(not(feature = "async"))]
        panic!("build with --features async for async workloads");
    } else {
        for sample in 0..config.samples {
            let data = if config.workload == "migration" {
                sync_migration(&config)
            } else {
                sync_snapshot(&config)
            };
            report(&config, sample, data);
        }
    }
}
