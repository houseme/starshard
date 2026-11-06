# Starshard

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

[English](README.md) | 简体中文

Starshard 是一个高性能、延迟初始化分片的并发 `HashMap`。

它面向真实生产场景，重点解决：
- 单全局锁在混合读写下的竞争问题，
- 同步/异步代码路径的一致能力，
- 扩容重平衡与快照策略的可控取舍。

## 当前状态

当前 crate 版本为 `2.4.1`。发布变更记录在 [CHANGELOG.md](CHANGELOG.md#241---2026-11-07)。

`2.4.1` 补齐迁移、修改操作和复制 fanout 的 panic 恢复边界，并修正迁移统计。详见 [2.4.0 补丁升级说明](MIGRATION-1X-TO-2X_CN.md#46-从-240-升级到-241)。

截至 `v2.4.0` 已交付的 Roadmap 主能力：
- 自适应分片扩容与重平衡（停顿式 + 在线渐进）。
- 快照模式（`Clone` / `Cached` / `Cow`）及基于 epoch 的缓存失效机制。
- Patch 级依赖治理：`async` feature 不再强制启用 Tokio 多线程运行时。
- 同步/异步 map 都支持 Entry 风格和 get-or-create 原子初始化 API。
- 迁移期间的条件操作、CAS、事务保持一致性，快照覆盖新旧两代分片。
- 共享快照句柄与借用键查询，快照重建允许普通读取继续。
- 按分片汇总批量元数据，以及计入 primary、受超时约束并保持顺序的复制。

从 2.3.x 升级时，快照版本改为标识已提交数据，无效复制拓扑会被拒绝。使用高级 API 前请阅读 [2.4 升级说明](MIGRATION-1X-TO-2X_CN.md#45-从-23x-升级到-240)。本轮优化有操作次数与分配次数证据，具体负载的吞吐和 p99 仍需实测。

## 安装

```toml
[dependencies]
starshard = { version = "2.4.1", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# 最小依赖：
# starshard = "2.4.1"
```

## 5 分钟上手路径

1. 先用默认同步 map：`ShardedHashMap::new(64)`。
2. 如果在 Tokio 运行时内，切换到 `AsyncShardedHashMap`。
3. 如果快照调用频繁，先尝试 `SnapshotMode::Cached`，再评估 `SnapshotMode::Cow`。
4. 如果分片数量来自用户输入或外部配置，优先用严格构造器（`try_with_*`）。

迁移说明：
- [1.x 到 2.x 使用差异](MIGRATION-1X-TO-2X_CN.md)

## 特性开关

| Feature | 能力 | 典型场景 |
|---|---|---|
| `async` | `AsyncShardedHashMap`（Tokio `RwLock`） | 异步服务与任务系统 |
| `rayon` | 大型同步快照；启用 `lifecycle` 时支持 `IterBuilder` 并行过滤 | 大规模扫描和较重的过滤计算 |
| `serde` | 同步版序列化/反序列化 + 异步快照序列化辅助 | 持久化与数据导出 |
| `lifecycle` | `per_shard_load`、`memory_stats`、`drain` 等 | 运维观测与维护 |
| `advanced` | 事务/CAS/复制/诊断 API | 高级并发控制与控制面 |

## 快速上手（同步）

```rust
use starshard::ShardedHashMap;

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
m.insert("k1".into(), 10);
assert_eq!(m.get_borrowed("k1"), Some(10));
assert_eq!(m.len(), 1);
```

## 快速上手（异步）

```rust
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;

    let m: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(64);
    m.insert("k1".into(), 10).await;
    assert_eq!(m.get_borrowed("k1").await, Some(10));
}
```

## 常用操作速查

| 目标 | 同步 API | 异步 API |
|---|---|---|
| 插入/更新 | `insert(k, v)` | `insert(k, v).await` |
| 读取 | `get(&k)` | `get(&k).await` |
| 借用键读取 | `get_borrowed(q)` | `get_borrowed(q).await` |
| 删除 | `remove(&k)` | `remove(&k).await` |
| entry 插入/更新 | `entry(k).or_insert_with(f)` | `entry(k).await.or_insert_with(f).await` |
| 获取或插入 | `get_or_insert_with(k, f)` | `get_or_insert_with(k, f).await` |
| 批量插入 | `batch_insert(items)` | `batch_insert(items).await` |
| 批量读取 | `batch_get(&keys)` | `batch_get(&keys).await` |
| 条件更新 | `compute_if_present(&k, f)` | `compute_if_present(&k, f).await` |
| 条件插入 | `compute_if_absent(k, f)` | `compute_if_absent(k, f).await` |
| 指标/内省 | `shard_stats()` / `memory_stats()` | `shard_stats().await` / `memory_stats().await` |

使用 `get_borrowed`、`contains_borrowed`、`remove_borrowed` 可传入借用键，例如用 `&str` 查询 `String` 键。原有 `get`、`contains`、`remove` 保留 `&K` 签名；读取仍返回克隆后的值。

Entry 风格插入/更新：

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, usize> = ShardedHashMap::new(16);
let value = map
    .entry("read-version".to_string())
    .and_modify(|count| *count += 1)
    .or_insert_with(|| 1);
assert_eq!(value, 1);
```

原子 get-or-create 辅助方法：

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, Vec<u64>> = ShardedHashMap::new(16);
let lane = map.get_or_insert_with("read-version".to_string(), Vec::new);
assert!(lane.is_empty());
```

## 构造器选型

按“安全边界 + 控制度”选择：

- 兼容型（超限自动钳制）：
  - `with_shards_and_hasher(...)`
  - `with_shards_and_hasher_capped(...)`
- 严格型（超限返回 `ShardCountError`）：
  - `try_with_shards_and_hasher(...)`
  - `try_with_shards_and_hasher_capped(...)`
- 带快照模式：
  - `with_snapshot_mode(...)`
  - `with_shards_and_hasher_and_snapshot_mode(...)`
  - `with_shards_and_hasher_capped_and_snapshot_mode(...)`

## 自适应重平衡（`v2.2.1`）

### 停顿式重平衡

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
let report = m.rebalance_to(32, RebalanceOptions::default()).unwrap();
assert_eq!(report.from_shards, 8);
assert_eq!(report.to_shards, 32);
```

### 在线渐进迁移

```rust
use starshard::ShardedHashMap;

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.start_rebalance_online(32).unwrap();

while m.rebalance_status().state == "migrating" {
    m.advance_rebalance(2);
}

assert_eq!(m.rebalance_status().state, "idle");
```

语义说明：
- 写入立即路由到新 active 分片。
- 迁移期间读取走 active 优先，miss 后回退 previous。
- 所有源分片迁移完成后，状态回到 `idle`。
- 每次推进会暂停 map 操作，最多搬迁指定数量的源分片；单个源分片较大时仍可能出现长暂停。该数量不代表时间预算或条目数预算。

### 有预算的批次与后台任务

`advance_rebalance_with_options(&options)` 按 `batch_size` 限制每批搬迁条目数，并在安全提交边界检查 `max_pause_ns`。单次 Hash/Clone、分片复制和锁等待可能超出该预算，`rebalance_telemetry()` 会报告实测耗时与超时次数。

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let map = ShardedHashMap::<u64, u64>::new(8);
map.batch_insert((0..1_000).map(|key| (key, key)));
let worker = map.spawn_rebalance(32, RebalanceOptions {
    background: true,
    batch_size: 64,
    max_pause_ns: 100_000,
}).unwrap();
assert!(worker.join().unwrap().completed);
```

`pause_rebalance()` 在批次边界暂停，`resume_rebalance()` 恢复。后台句柄支持 `cancel()` 和 `join()`，丢弃句柄会请求停止。取消后已经搬迁的数据保持可达，可通过手动推进或新 worker 继续完成。异步对应方法使用 `.await`，异步句柄丢弃还会中止待处理的 await。

`rebalance_to()` 在迁移完成后返回。`background = true` 时采用协作式条目批次，`spawn_rebalance()` 用于返回可监督的后台句柄。worker 根据预算超时降低后续批次大小。负载阈值驱动的自动扩缩容仍列在 G 线计划中。

## 快照模式（`v2.2.1`）

`SnapshotMode` 支持按负载选择：

- `Clone`：每次请求都重建快照（默认模式）。
- `Cached`：复用带版本的全 map 共享快照，写入后失效。
- `Cow` 以共享句柄持有不可变分片。`snapshot_view()` 捕获分片句柄时零键值复制。视图仍持有某个分片时，对该分片的写入会触发分片级复制。

`shared_snapshot()` 的有效缓存命中只需短暂校验数据版本。缓存缺失时，构建锁合并重建请求，并通过有序分片读锁捕获一致视图。Cow 在释放 map 锁后复制输出条目，Clone/Cached 在分片读锁保护下复制。异步缓存读取保留 Tokio 协作式调度预算。

`snapshot_view()` 返回支持借用迭代的 `SnapshotView`，同时捕获数据版本和 active 分片数。Cow 视图持有分片分配，保留多个视图并继续写入会增加内存占用。释放视图后，旧分片可随最后一个引用释放而回收。Clone/Cached 下调用该接口会复制已初始化分片。

`iter()` 输出拥有所有权的条目。`shared_snapshot()` 将条目物化为 `Arc<Vec<(K,V)>>`，并在 Cached/Cow 模式缓存。advanced 下的 `cow_snapshot()` 包装该物化结果及数据版本。这些接口创建新物化快照时会复制条目。需要低拷贝捕获和借用遍历时，使用 Cow 模式的 `snapshot_view()`。

```rust
use starshard::{ShardedHashMap, SnapshotMode};

let clone_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Clone);
let cached_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cached);
let cow_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cow);
```

### 模式选择建议

| 负载画像 | 推荐模式 |
|---|---|
| 高写入 + 低快照频率 | `Clone` |
| 中写入 + 中快照频率 | `Cached` |
| 高频借用快照或序列化 | `Cow` 配合 `snapshot_view()` |
| 多次读取物化快照且中间写入较少 | `Cached` 配合 `shared_snapshot()` |

## 一致性模型

- 分片内操作是线性化可见的。
- 快照重建保留目录读锁并按顺序获取分片读锁，在全部锁取得后形成覆盖 active 与 previous 分片的稳定视图。普通读取可继续，但遵守锁公平性；排队的写入或冷分片初始化仍可能延迟后续读取。有效的不可变缓存无需访问路由，数据与版本成对捕获。
- 在线迁移期间，通过 active-first + previous-fallback 保证 key 可达性。
- 事务按固定顺序获取涉及分片的写锁，采用悲观锁；读写集合不构成 MVCC 冲突检测。持锁期间的回调不得重入同一个 map。
- 事务中的用户代码发生 panic 时，已经执行的操作不会回滚；长度和快照缓存元数据会与实际存储内容保持一致。

## 性能建议

- 根据实际线程数与键分布运行竞争基准，再选择分片数量。
- 延迟分片初始化可让内存更接近“按访问付费”。
- 启用 Rayon 后，`IterBuilder::parallel(true)` 对至少 1024 项的输入并行过滤；该初始阈值尚未经过针对不同负载的调优。先过滤再截取结果，结果顺序与输入一致。过滤调用可以并发执行，`for_each` 回调仍按结果顺序串行执行。小输入或未启用 `rayon` 时顺序过滤。
- 原子 get-or-create 路径优先使用 `get_or_insert_with` 或 `entry(...).or_insert_with(...)`，避免外部 check-then-insert 锁。
- 快照密集型服务建议按真实键分布对比 `Cached` 与 `Cow`。
- 非迁移状态的批量操作按实际修改分片汇总长度和版本更新，panic 展开时也会结算。快照版本是变更标记，不是逐操作计数器。

## Serde 语义

- 同步 map 支持直接 `Serialize` / `Deserialize`。
- 不持久化 hasher 内部状态；反序列化时使用 `S::default()`。
- 异步 map 使用 `async_snapshot_serializable().await` 进行序列化。同步序列化和异步包装器从已捕获视图借用条目并逐条输出。Cow 捕获及序列化避免键值 Clone，现有对象/数组格式继续兼容。

## 示例与基准

示例：
- `examples/v210_rebalance.rs`
- `examples/snapshot_mode_clone_demo.rs`
- `examples/snapshot_mode_cached_demo.rs`
- `examples/snapshot_mode_cow_demo.rs`
- `examples/mixed_workload_snapshot_tradeoff_demo.rs`

基准入口：
- `benches/bench_main.rs`

基准提前生成键并复用并发工作线程。计时批次包含屏障同步，每个线程执行 1000 次操作以摊薄同步成本。按维度抽样覆盖 1/2/4/8/16 线程、90% 热点键、16 字节/4 KiB 值、10%/50% 写入以及全部快照模式，避免执行完整笛卡尔积。拥有所有权的迭代与共享快照句柄分别测量。

```bash
cargo bench --bench bench_main -- concurrent_mixed
cargo bench --bench bench_main -- snapshot_modes
cargo bench --bench bench_main -- shared_snapshot
cargo bench --bench bench_main -- concurrent_read_paths
cargo bench --bench bench_main -- concurrent_cached_snapshot
cargo bench --all-features --bench bench_main -- snapshot_rebuild
cargo bench --all-features --bench bench_main -- batch_mutations
```

这些 Criterion 基准报告批次耗时和吞吐，不测量单次操作 p99 延迟、分配次数、内存峰值或迁移暂停上界；上述指标需要单独的负载观测。

## 验证建议

发布前或升级后建议执行：

```bash
cargo fmt --all
cargo test --all-features
cargo check --all-features
```

## 当前边界

- 不是 lock-free；热点分片写压力仍可能串行化。
- 捕获快照需要路由及有序分片读锁。Cow 视图在释放 map 锁后遍历，并持有旧分片分配。写入这些分片会产生复制成本，包含用户自定义 Clone 的工作量。物化快照还会分配全 map 的条目向量。
- `EvictionConfig`、淘汰策略和 `AtomicMetrics` 是独立类型。map 不会自动运行 TTL/LRU/LFU 调度器，也不会自动累加这些操作计数；`memory_stats()` 与 `per_shard_load()` 用于查询 map 状态。
- 尚未实现锁耗时采集；`lock_profiles()` 不提供实测的锁竞争数据。
- 迁移时间预算在安全条目边界检查。用户 Hash/Clone/Drop 和锁等待可能造成超时，请结合 `budget_overruns` 与 `max_pause_ns` 实测值选择批次大小。后台 worker 由调用者指定目标分片数。

## License

双许可证：
- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)

你可以任选其一。

## 免责声明

- README 中的基准和性能描述仅作参考，不构成性能承诺。
- 生产使用前请务必基于真实负载完成延迟、吞吐与内存验证。
