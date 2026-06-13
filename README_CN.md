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

已达到生产可用（`v2.2.1`）。

`v2.2.1` 已交付的 Roadmap 主能力：
- 自适应分片扩容与重平衡（停顿式 + 在线渐进）。
- 快照模式（`Clone` / `Cached` / `Cow`）及基于 epoch 的缓存失效机制。

## 安装

```toml
[dependencies]
starshard = { version = "2.2.1", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# 最小依赖：
# starshard = "2.2.1"
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
| `rayon` | 快照迭代并行扁平化 | 大规模扫描/导出 |
| `serde` | 同步版序列化/反序列化 + 异步快照序列化辅助 | 持久化与数据导出 |
| `lifecycle` | `per_shard_load`、`memory_stats`、`drain` 等 | 运维观测与维护 |
| `advanced` | 事务/CAS/复制/诊断 API | 高级并发控制与控制面 |

## 快速上手（同步）

```rust
use starshard::ShardedHashMap;

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
m.insert("k1".into(), 10);
assert_eq!(m.get(&"k1".into()), Some(10));
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
    assert_eq!(m.get(&"k1".into()).await, Some(10));
}
```

## 常用操作速查

| 目标 | 同步 API | 异步 API |
|---|---|---|
| 插入/更新 | `insert(k, v)` | `insert(k, v).await` |
| 读取 | `get(&k)` | `get(&k).await` |
| 删除 | `remove(&k)` | `remove(&k).await` |
| 批量插入 | `batch_insert(items)` | `batch_insert(items).await` |
| 批量读取 | `batch_get(&keys)` | `batch_get(&keys).await` |
| 条件更新 | `compute_if_present(&k, f)` | `compute_if_present(&k, f).await` |
| 条件插入 | `compute_if_absent(k, f)` | `compute_if_absent(k, f).await` |
| 指标/内省 | `shard_stats()` / `memory_stats()` | `shard_stats().await` / `memory_stats().await` |

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

## 快照模式（`v2.2.1`）

`SnapshotMode` 支持按负载选择：

- `Clone`：每次重建快照（默认，写路径开销最低）。
- `Cached`：无写入时复用快照缓存。
- `Cow`：分片级 COW 快照视图，适合高频快照读取。

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
| 低写入 + 高频快照读取 | `Cow` 或 `Cached` |

## 一致性模型

- 分片内操作是线性化可见的。
- 全局迭代/快照是“分片级快照拼接”，不是全局串行化事务。
- 在线迁移期间，通过 active-first + previous-fallback 保证 key 可达性。

## 性能建议

- 相比单全局 `RwLock<HashMap<..>>`，分片模型在混合负载下通常能降低竞争。
- 延迟分片初始化可让内存更接近“按访问付费”。
- 大规模扫描场景建议启用 `rayon`。
- 快照密集型服务建议按真实键分布对比 `Cached` 与 `Cow`。

## Serde 语义

- 同步 map 支持直接 `Serialize` / `Deserialize`。
- 不持久化 hasher 内部状态；反序列化时使用 `S::default()`。
- 异步 map 使用 `async_snapshot_serializable().await` 进行序列化。

## 示例与基准

示例：
- `examples/v210_rebalance.rs`
- `examples/snapshot_mode_clone_demo.rs`
- `examples/snapshot_mode_cached_demo.rs`
- `examples/snapshot_mode_cow_demo.rs`
- `examples/mixed_workload_snapshot_tradeoff_demo.rs`

基准入口：
- `benches/bench_main.rs`

## 验证建议

发布前或升级后建议执行：

```bash
cargo fmt --all
cargo test --all-features
cargo check --all-features
```

## 当前边界

- 不是 lock-free；热点分片写压力仍可能串行化。
- 快照输出仍需物化为 `Vec<(K, V)>`。
- `RebalanceOptions` 的 `background`、`batch_size`、`max_pause_ns` 在 `v2.2.1` 中为前向兼容预留参数。

## License

双许可证：
- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)

你可以任选其一。

## 免责声明

- README 中的基准和性能描述仅作参考，不构成性能承诺。
- 生产使用前请务必基于真实负载完成延迟、吞吐与内存验证。
