# Starshard 2.x 与 1.x 使用差异说明

## 1. 适用范围

- 1.x 系列：`1.0.0` ~ `1.2.x`
- 2.x 系列：`2.0.0` ~ `2.2.x`

本文聚焦**使用层面**差异与迁移建议，不展开内部重构细节。

## 2. 一句话总结

- 基础 KV API 大多保持兼容。
- 主要变化是：
  - `2.0.0` 的分片安全构造策略升级，
  - `2.1+` 新增可选重平衡 API，
  - `2.1+` 新增可选快照模式。

## 3. 核心差异总览

| 维度 | 1.x | 2.x |
|---|---|---|
| 分片数量安全 | 超大 `shard_count` 可能带来内存风险 | 默认 `MAX_SHARDS` 保护，并提供严格构造器 |
| 构造器策略 | 以直接构造为主 | 兼容构造 + capped 构造 + strict 构造 |
| 动态重平衡 | 不支持 | `rebalance_to`、`start_rebalance_online`、`advance_rebalance` |
| 快照策略 | 传统路径 | `SnapshotMode::{Clone, Cached, Cow}` |
| 升级改造量 | - | 默认使用场景改造量低，可渐进启用新能力 |

## 4. 使用差异详解

### 4.1 构造器与分片安全（`2.0.0`）

2.x 的实质变化：

- 兼容构造器继续可用，但对超大请求会进行保护性钳制。
- 新增 capped 构造器：
  - `with_shards_and_hasher_capped(...)`
- 新增 strict 构造器（超限返回 `ShardCountError`）：
  - `try_with_shards_and_hasher(...)`
  - `try_with_shards_and_hasher_capped(...)`

建议在“外部输入/用户配置”场景使用 strict：

```rust
use starshard::ShardedHashMap;
use rustc_hash::FxBuildHasher;

let map = ShardedHashMap::<String, i32>::try_with_shards_and_hasher_capped(
    4096,
    FxBuildHasher,
    8192,
)?;
```

迁移提示：
- 如果 1.x 时依赖“超大 shard_count 一定原样生效”，升级后务必复核该路径。

### 4.2 动态重平衡 API（`2.1+`）

1.x 不支持动态重平衡。

2.1+ 新增：
- 停顿式：
  - `rebalance_to(new_shard_count, options)`
- 在线渐进式：
  - `start_rebalance_online(new_shard_count)`
  - `advance_rebalance(batch_shards)`
  - `rebalance_status()`

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.rebalance_to(32, RebalanceOptions::default())?;
```

### 4.3 快照模式选型（`2.1+`）

2.1+ 将快照策略显式化：
- `SnapshotMode::Clone`（默认）
- `SnapshotMode::Cached`
- `SnapshotMode::Cow`

并新增模式构造器：
- `with_snapshot_mode(...)`
- `with_shards_and_hasher_and_snapshot_mode(...)`
- `with_shards_and_hasher_capped_and_snapshot_mode(...)`

```rust
use starshard::{ShardedHashMap, SnapshotMode};

let map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cached);
```

### 4.4 性能相关可见变化

- `batch_insert` / `batch_remove` / `batch_get` 在 2.0 做了分组路径优化。
- async `compute_if_present` 路径在 2.0 做了行为对齐，减少不必要 remove+reinsert。

这些变化通常对调用方透明。

## 5. Feature Flag 提醒

从早期 1.x 升级时，建议重新确认 `Cargo.toml` 的 feature 组合：

```toml
[dependencies]
starshard = { version = "2.2.0", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
```

## 6. 迁移清单

1. 升级依赖到 2.x（建议 `2.2.x`）。
2. 检查构造入口：
   - 外部输入参数 -> 用 `try_with_*`
   - 内部固定参数 -> 继续兼容构造器即可
3. 仅在需要运行时分片迁移时接入 rebalance API。
4. 快照高频场景评估 `SnapshotMode::Cached` / `Cow`。
5. 执行验证：
   - `cargo test --all-features`
   - 结合业务负载做延迟/吞吐/内存压测

## 7. 兼容性结论

- 对仅使用基础 KV 能力的 1.x 用户，升级到 2.x 通常低风险。
- 对依赖极大分片数或复杂快照/迁移行为的系统，建议做显式迁移验证。
