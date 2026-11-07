//! Serial local capacity-policy diagnostics; no background workers are started.
//!
//! Build the same source twice, keeping all flags except lifecycle identical:
//! `cargo build --release --example eviction_bench --no-default-features`
//! `cargo build --release --example eviction_bench --no-default-features --features lifecycle`
//! Preserve both binaries and run disabled/disabled A1/B1/B2/A2 before comparing
//! disabled to configured policies. A 20% overhead target needs measurements;
//! this driver neither assumes that target nor claims an isolated host.
//!
//! Usage: `eviction_bench MODE WORKLOAD ENTRIES DURATION_MS SAMPLES`.
//! MODE: disabled, or lru|lfu|fifo followed by -track|-global|-shard|-weighted.
//! WORKLOAD: read (existing-key schedule), replace (same keys), or churn
//! (cyclic 4x-capacity key space; no unbounded growth with policy disabled).
//! Use -track read for hit-for-hit policy overhead. Threshold workloads can
//! differ in resident keys/hit rates; compare their reported work and metrics,
//! not just the operation rate (per-shard preload may already evict entries).
//! Defaults: disabled read 4096 200 3. Entries must be a positive multiple of 32.
//!
//! All construction, precomputed keys and warmup are outside measurement. Timed
//! values have compile-time-disabled clone counters; a separate 16-operation
//! probe counts library key/value clones and reports lifecycle metrics. There
//! is no allocator instrumentation or latency claim. Weighted cost is the
//! application estimator (64/128 units), not heap/RSS/retained-snapshot memory.

use serde_json::json;
use starshard::ShardedHashMap;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SHARDS: usize = 32;
static KEY_CLONES: AtomicU64 = AtomicU64::new(0);
static VALUE_CLONES: AtomicU64 = AtomicU64::new(0);

#[derive(Hash, Eq, PartialEq)]
struct Key<const COUNT: bool>(u64);
impl<const COUNT: bool> Clone for Key<COUNT> {
    fn clone(&self) -> Self {
        if COUNT {
            KEY_CLONES.fetch_add(1, Ordering::Relaxed);
        }
        Self(self.0)
    }
}

struct Value<const COUNT: bool> {
    weight: usize,
    bytes: [u8; 64],
}
impl<const COUNT: bool> Clone for Value<COUNT> {
    fn clone(&self) -> Self {
        if COUNT {
            VALUE_CLONES.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            weight: self.weight,
            bytes: self.bytes,
        }
    }
}
type Map<const COUNT: bool> = ShardedHashMap<Key<COUNT>, Value<COUNT>>;

struct Config {
    mode: String,
    policy: String,
    limit: String,
    workload: String,
    entries: usize,
    duration: Duration,
    samples: usize,
}
impl Config {
    fn parse() -> Self {
        let args: Vec<_> = std::env::args().collect();
        let mode = args.get(1).cloned().unwrap_or_else(|| "disabled".into());
        let (policy, limit) = if mode == "disabled" {
            ("disabled", "none")
        } else {
            mode.split_once('-').expect("expected POLICY-LIMIT mode")
        };
        assert!(
            mode == "disabled"
                || (matches!(policy, "lru" | "lfu" | "fifo")
                    && matches!(limit, "track" | "global" | "shard" | "weighted")),
            "unknown policy or limit"
        );
        assert!(
            mode == "disabled" || cfg!(feature = "lifecycle"),
            "configured modes require --features lifecycle"
        );
        let workload = args.get(2).cloned().unwrap_or_else(|| "read".into());
        assert!(matches!(workload.as_str(), "read" | "replace" | "churn"));
        let number = |index: usize, default| {
            args.get(index)
                .map(|arg| arg.parse::<usize>().expect("expected positive integer"))
                .unwrap_or(default)
        };
        let entries = number(3, 4096);
        let duration_ms = number(4, 200);
        let samples = number(5, 3);
        assert!(
            entries > 0 && entries.is_multiple_of(SHARDS) && entries.checked_mul(128).is_some()
        );
        assert!(duration_ms > 0 && samples > 0);
        Self {
            policy: policy.into(),
            limit: limit.into(),
            mode,
            workload,
            entries,
            duration: Duration::from_millis(duration_ms as u64),
            samples,
        }
    }
}

fn make_map<const COUNT: bool>(config: &Config) -> Map<COUNT> {
    if config.mode == "disabled" {
        return Map::new(SHARDS);
    }
    #[cfg(feature = "lifecycle")]
    {
        use starshard::{CapacityPolicy, EvictionConfig, EvictionOptions, SnapshotMode};
        let policy = match config.policy.as_str() {
            "lru" => CapacityPolicy::Lru,
            "lfu" => CapacityPolicy::Lfu,
            "fifo" => CapacityPolicy::Fifo,
            _ => unreachable!(),
        };
        let settings = EvictionConfig {
            max_entries: (config.limit == "global").then_some(config.entries),
            max_items_per_shard: (config.limit == "shard").then_some(config.entries / SHARDS),
            background_enabled: false,
            ..EvictionConfig::default()
        };
        let mut options = EvictionOptions::new(settings).with_policy(policy);
        if config.limit == "weighted" {
            options = options.with_memory_budget(
                config.entries * 64,
                |_: &Key<COUNT>, value: &Value<COUNT>| value.weight,
            );
        }
        Map::try_with_manual_eviction(
            SHARDS,
            rustc_hash::FxBuildHasher,
            SnapshotMode::Clone,
            options,
        )
        .expect("valid manual eviction configuration")
    }
    #[cfg(not(feature = "lifecycle"))]
    unreachable!("configuration checked before setup")
}

fn prepare<const COUNT: bool>(config: &Config, keys: &[u64]) -> Map<COUNT> {
    let map = make_map(config);
    for &key in &keys[..config.entries] {
        map.insert(
            Key(key),
            Value {
                weight: 64,
                bytes: [key as u8; 64],
            },
        );
    }
    for index in 0..128 {
        operation(&map, config, keys, index);
    }
    map
}

fn operation<const COUNT: bool>(map: &Map<COUNT>, config: &Config, keys: &[u64], index: usize) {
    let key = if config.workload == "churn" {
        keys[(index + config.entries) % keys.len()]
    } else {
        keys[index % config.entries]
    };
    if config.workload == "read" {
        black_box(map.get(&Key(key)));
    } else {
        // Replacements alternate estimator weights as well as payloads, so a
        // weighted replacement may trigger maintenance without map growth.
        let weight = if index.is_multiple_of(2) { 64 } else { 128 };
        black_box(map.insert(
            Key(key),
            Value {
                weight,
                bytes: [index as u8; 64],
            },
        ));
    }
}

fn reset_metrics<const COUNT: bool>(map: &Map<COUNT>) {
    #[cfg(feature = "lifecycle")]
    map.reset_metrics();
    #[cfg(not(feature = "lifecycle"))]
    let _ = map;
}

fn metrics<const COUNT: bool>(map: &Map<COUNT>) -> serde_json::Value {
    #[cfg(feature = "lifecycle")]
    {
        let m = map.metrics_snapshot();
        let r = map.eviction_runtime_stats();
        json!({"gets": m.total_gets, "hits": m.hits, "misses": m.misses,
            "inserts": m.total_inserts, "removes": m.total_removes, "evictions": m.evictions,
            "enabled": r.enabled, "pending": r.pending, "dirty": r.dirty,
            "tracked_entries": r.tracked_entries, "estimated_weight": r.estimated_weight,
            "weight_overflow": r.weight_overflow, "reconciliations": r.reconciliations})
    }
    #[cfg(not(feature = "lifecycle"))]
    {
        let _ = map;
        serde_json::Value::Null
    }
}

fn verify<const COUNT: bool>(map: &Map<COUNT>, config: &Config) {
    // Untimed postconditions prevent a fast but unenforced policy from being
    // mistaken for a performance win. Iteration is not a policy access.
    match config.limit.as_str() {
        "global" => assert!(map.len() <= config.entries),
        "shard" => {
            use std::hash::BuildHasher;
            let mut loads = [0_usize; SHARDS];
            for (key, _) in map.iter() {
                loads[(rustc_hash::FxBuildHasher.hash_one(&key) % SHARDS as u64) as usize] += 1;
            }
            assert!(loads.iter().all(|&load| load <= config.entries / SHARDS));
        }
        "weighted" => {
            let weight: usize = map.iter().map(|(_, value)| value.weight).sum();
            assert!(weight <= config.entries * 64);
        }
        _ => {}
    }
}

fn main() {
    let config = Config::parse();
    // A fixed 4x working set bounds disabled/tracking-only maps too.
    let keys: Vec<_> = (0..config.entries * 4).map(|key| key as u64).collect();
    for sample in 0..config.samples {
        let map = prepare::<false>(&config, &keys);
        reset_metrics(&map);
        let start = Instant::now();
        let mut operations = 0;
        loop {
            for offset in 0..128 {
                // Continue the warmup schedule so churn starts with fresh keys.
                operation(&map, &config, &keys, 128 + operations + offset);
            }
            operations += 128;
            if start.elapsed() >= config.duration {
                break;
            }
        }
        let elapsed = start.elapsed();
        let timed_metrics = metrics(&map);
        let final_len = map.len();
        verify(&map, &config);
        drop(map);

        let probe = prepare::<true>(&config, &keys);
        reset_metrics(&probe);
        KEY_CLONES.store(0, Ordering::Relaxed);
        VALUE_CLONES.store(0, Ordering::Relaxed);
        for index in 128..144 {
            operation(&probe, &config, &keys, index);
        }
        let key_clones = KEY_CLONES.load(Ordering::Relaxed);
        let value_clones = VALUE_CLONES.load(Ordering::Relaxed);
        let probe_metrics = metrics(&probe);
        verify(&probe, &config);
        println!(
            "{}",
            json!({
                "sample": sample, "lifecycle_compiled": cfg!(feature = "lifecycle"),
                "mode": config.mode, "policy": config.policy, "limit": config.limit,
                "workload": config.workload, "runtime": "sync_single_thread", "background": false,
                "initial_entries": config.entries, "shards": SHARDS, "key_space": keys.len(),
                "value_size_bytes": std::mem::size_of::<Value<false>>(),
                "operations": operations, "elapsed_ns": elapsed.as_nanos(),
                "operations_per_second": operations as f64 / elapsed.as_secs_f64(),
                "final_len": final_len, "timed_metrics": timed_metrics,
                "probe_operations": 16, "probe_key_clones": key_clones, "probe_value_clones": value_clones,
                "probe_metrics": probe_metrics, "count_scope": "separate_16_operations_after_128_warmup",
                "weight_scope": "application_estimator_units_not_heap_RSS_or_retained_snapshots",
                "scope": "local_serial_diagnostics_host_isolation_not_established_no_threshold_claim"
            })
        );
    }
}
