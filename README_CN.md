# Starshard

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

[English](README.md) | 简体中文

<b>Starshard</b>：高性能、延迟初始化分片的并发 HashMap

<code>同步 + 异步 + 可选 Rayon 并行 + 可选 Serde 序列化 + 生命周期管理 + 高级特性</code>

---

## 状态

生产就绪 (v2.1+)。API 稳定性优先。

## 为什么需要它

常见方案与痛点：

- 全局 `RwLock<HashMap<..>>`：中高写入并发时锁冲突放大。
- 复杂无锁结构：实现/调试/内存开销较高。
- 固定分片 + 延迟创建：在低接触分片场景节省内存，控制复杂度。

Starshard 目标：

1. 降低写冲突（分片 + 精短锁区间）。
2. 延迟分片分配（避免冷启动全量内存）。
3. 原子长度缓存（`len()` O(1)）。
4. 可并行快照迭代（`rayon`）。
5. 同步 / 异步接口语义一致。
6. 架构可拓展（生命周期管理、高级并发）。

---

## 特性 (features)

| Feature     | 说明                                       | 备注          |
|-------------|------------------------------------------|-------------|
| `async`     | 提供 `AsyncShardedHashMap`（Tokio `RwLock`） | 独立启用        |
| `rayon`     | 大体量迭代时分片快照并行扁平化                          | 内部加速        |
| `serde`     | 同步版序列化/反序列化；异步版提供快照封装                    | 不持久化 hasher |
| `lifecycle` | 生命周期工具与原语（`per_shard_load`、`memory_stats`、`drain`、指标/淘汰配置类型） | 整合 v0.9+ 特性 |
| `advanced`  | 事务、CAS、复制、诊断                             | 整合 v1.0 特性  |
| （空）         | 仅同步核心 + 批量操作                             | 依赖面最小       |

`docs.rs` 展示全部：

```toml
[package.metadata.docs.rs]
all-features = true
```

`v2.1.0` 当前内部结构：

- `src/core/sync_impl.rs`
- `src/core/async_impl.rs`
- `src/core/types.rs`
- `src/core/helpers.rs`
- `src/serde/sync_serde.rs`
- `src/serde/async_snapshot.rs`

---

## 安装

```toml
[dependencies]
starshard = { version = "2.1.0", features = ["async", "rayon", "serde", "lifecycle", "advanced"] }
# 最小：
# starshard = "2.1.0"
```

开发/测试:

```toml
[dev-dependencies]
serde_json = "1"
```

---

## 快速上手（同步）

```rust
use starshard::ShardedHashMap;
use rustc_hash::FxBuildHasher;

let map: ShardedHashMap<String, i32, FxBuildHasher> = ShardedHashMap::new(64);
map.insert("a".into(), 1);
assert_eq!(map.get(&"a".into()), Some(1));
assert_eq!(map.len(), 1);
```

### 自定义 Hasher（对抗恶意键）

```rust
use starshard::ShardedHashMap;
use std::collections::hash_map::RandomState;

let secure = ShardedHashMap::<String,u64,RandomState>
::with_shards_and_hasher(128, RandomState::default ());
secure.insert("k".into(), 7);
```

---

## 异步示例

```rust
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let m: AsyncShardedHashMap<String, u32> = AsyncShardedHashMap::new(64);
    m.insert("x".into(), 42).await;
    assert_eq!(m.get(&"x".into()).await, Some(42));
}
```

## 自适应重平衡（`v2.1+`）

同步：

```rust
use starshard::{RebalanceOptions, ShardedHashMap};

let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.rebalance_to(32, RebalanceOptions::default()).unwrap();
```

在线渐进迁移（同步）：

```rust
let m: ShardedHashMap<String, i32> = ShardedHashMap::new(8);
m.start_rebalance_online(32).unwrap();
while m.rebalance_status().state == "migrating" {
    m.advance_rebalance(2);
}
```

---

## 并行迭代 (`rayon`)

```rust
#[cfg(feature = "rayon")]
{
use starshard::ShardedHashMap;
let m: ShardedHashMap<String,u32> = ShardedHashMap::new(32);
for i in 0..50_000 {
m.insert(format ! ("k{i}"), i);
}
let count = m.iter().count();
assert_eq!(count, 50_000);
}
```

---

## Serde 语义

同步：

- 结构：`{ shard_count: usize, entries: [[K,V], ...] }`
- 不持久化 hasher 状态；反序列化使用 `S::default()`
- 约束：`K: Eq + Hash + Clone + Serialize + Deserialize`，`V: Clone + Serialize + Deserialize`，
  `S: BuildHasher + Default + Clone`

异步：

```rust
#[cfg(all(feature = "async", feature = "serde"))]
{
let snap = async_map.async_snapshot_serializable().await;
let json = serde_json::to_string( & snap).unwrap();
}
```

恢复：新建空异步 map 后逐条插入。

---

## 一致性模型

- 分片内操作线性化。
- 全局迭代：按“获取该分片锁”时刻的快照；不保证全局原子视图。
- `len()` 为结构性更新后的原子值；进行中的未完成操作不可见。
- 迭代时并发新增/删除，可能部分缺失或包含旧值（跨分片）。

---

## 性能提示（示意）

| 场景               | 说明                    |
|------------------|-----------------------|
| 读多写少 vs 全局单锁     | 写冲突显著下降               |
| 大量快照迭代 + `rayon` | 3~4 倍扁平化速度 (100k+ 元素) |
| 稀疏访问             | 仅访问分片分配内存             |
| 批量插入/移除          | 每个分片组仅一次锁获取，`v2.0.0` 使用稀疏分桶降低额外内存开销 |
| 在线重平衡             | 支持按分片渐进迁移，读路径 active 优先 |

请基准测试你的真实负载（键分布、核心数、缓存行为）。

---

## 并发 / 安全

- 不做多分片级联锁，避免死锁。
- 快照迭代引入短读锁 + 克隆，减少长持锁。
- 高热点单分片写压力仍会形成竞争瓶颈。
- 非 lock-free；但结构更简单、可预测。

---

## 局限

- 已支持停顿式与在线渐进分片重平衡（`v2.1`）。
- 迭代需分配临时 `Vec`。
- Hasher 状态不序列化。
- 大量写倾斜仍可能产生热点锁。

---

## 快照模式（`v2.1.0`）

`SnapshotMode` 支持按负载选择快照策略：

- `Clone`：每次快照都重建（默认，写入开销最低）。
- `Cached`：无写入时复用快照缓存。
- `Cow`：分片级 COW 快照视图，降低高频快照重建成本。

```rust
use starshard::{ShardedHashMap, SnapshotMode};

let clone_map: ShardedHashMap<String, i32> = ShardedHashMap::new(64); // 默认 Clone
let cached_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cached);
let cow_map: ShardedHashMap<String, i32> =
    ShardedHashMap::with_snapshot_mode(64, SnapshotMode::Cow);
```

选型建议：

| 负载特征             | 推荐模式         |
|----------------------|------------------|
| 高写低快照           | `Clone`          |
| 中写中快照           | `Cached`         |
| 低写高频快照读取     | `Cow` / `Cached` |

灰度与回滚建议：

1. 先以 `Clone` 作为生产基线。
2. 单实例灰度到 `Cached`，观察快照延迟和分配指标。
3. 快照占比高且写入比例低时，再灰度 `Cow`。
4. 回滚时重建为 `SnapshotMode::Clone` 即可。

---

## 核心特性 (v0.8+)

### 条件操作 (基于方法，高效)

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);

// 仅当键存在时更新；单分片锁
map.compute_if_present( & "counter".into(), | v| Some(v + 1));

// 仅当键不存在时插入；单分片锁
let val = map.compute_if_absent("new_key".into(), | | 42);

// 条件删除
map.compute_if_present( & "key".into(), | _v| None);
```

### 批量操作 (已优化)

```rust
// 批量插入 (分摊分片锁获取成本 - 每个分片组仅一次锁)
let entries = vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)];
let inserted = map.batch_insert(entries);

// 批量移除
let removed = map.batch_remove(vec!["a".into(), "b".into()]);

// 批量获取
let keys = vec!["a", "b", "c"];
let results = map.batch_get( & keys);
```

### 集合视图与过滤

```rust
// 迭代所有键
let all_keys: Vec<_ > = map.keys().collect();

// 迭代所有值
let all_values: Vec<_ > = map.values().collect();

// 标准迭代
for (k, v) in map.iter() {
println ! ("{}: {}", k, v);
}

// 保留过滤
map.retain( | k, v| v > & 10);  // 仅保留 v > 10 的条目
```

### 分片内省

```rust
// 获取分片分布统计
let stats = map.shard_stats();
println!("Total slots: {}", stats.total);
println!("Initialized shards: {}", stats.initialized);
println!("Avg load: {:.2}", stats.avg_load);

// 获取利用率百分比
let util = map.shard_utilization();
println!("Utilization: {:.1}%", util);
```

---

## 生命周期特性 (v0.9+)

通过 `features = ["lifecycle"]` 启用。

- **分片内省**: `per_shard_load()`（同步 + 异步）
- **内存导向统计**: `memory_stats()`（同步 + 异步）
- **批量排空**: `drain()`（同步 + 异步）
- **生命周期原语**: `EvictionConfig`、`EvictionPolicy`、`AtomicMetrics`、`IterBuilder`

```rust
#[cfg(feature = "lifecycle")]
{
    use starshard::ShardedHashMap;
    let map: ShardedHashMap<String, i32> = ShardedHashMap::new(16);
    map.insert("a".into(), 1);
    map.insert("b".into(), 2);

    let _loads = map.per_shard_load();
    let _memory = map.memory_stats();
    let drained: Vec<_> = map.drain().collect();
    assert_eq!(drained.len(), 2);
}
```

```rust
#[cfg(all(feature = "async", feature = "lifecycle"))]
#[tokio::main]
async fn main() {
    use starshard::AsyncShardedHashMap;
    let map: AsyncShardedHashMap<String, i32> = AsyncShardedHashMap::new(16);
    map.insert("a".into(), 1).await;

    let _loads = map.per_shard_load().await;
    let _memory = map.memory_stats().await;
    let drained: Vec<_> = map.drain().await.collect();
    assert_eq!(drained.len(), 1);
}
```

说明：`lifecycle` 当前提供的是实用 API 与配置原语，尚未内置自动化 TTL 淘汰调度器。

---

## 高级特性 (v1.0+)

通过 `features = ["advanced"]` 启用。

- **MVCC 事务**: 带冲突检测的原子多键操作
- **比较并交换 (CAS)**: 无锁协调原语
- **写时复制快照**: 最小化争用的读取优化
- **分布式复制**: 基于 Quorum 的一致性框架
- **锁诊断**: 分片级争用分析

---

## 特性矩阵

| 特性                     | v0.7 | v0.8 | v0.9 | v1.0 | v2.0 | 状态              |
|------------------------|------|------|------|------|------|-----------------|
| 分片 HashMap (同步)      | ✅    | ✅    | ✅    | ✅    | ✅    | 稳定              |
| 异步 (Tokio)             | ✅    | ✅    | ✅    | ✅    | ✅    | 稳定              |
| 并行迭代 (rayon)           | ✅    | ✅    | ✅    | ✅    | ✅    | 稳定              |
| Serde (反)序列化           | ✅    | ✅    | ✅    | ✅    | ✅    | 稳定              |
| 条件操作                   | -    | ✅    | ✅    | ✅    | ✅    | 稳定              |
| 批量操作                   | -    | ✅    | ✅    | ✅    | ✅    | 稳定              |
| 生命周期工具                | -    | -    | ✅    | ✅    | ✅    | 稳定 (lifecycle) |
| 淘汰/指标原语               | -    | -    | ✅    | ✅    | ✅    | 稳定 (lifecycle) |
| 事务                     | -    | -    | -    | ✅    | ✅    | 稳定 (advanced)  |
| CAS 操作                 | -    | -    | -    | ✅    | ✅    | 稳定 (advanced)  |
| 复制                     | -    | -    | -    | ✅    | ✅    | 稳定 (advanced)  |

---

## 结构示意

```
Arc -> RwLock<Vec<Option<Arc<RwLock<HashMap<K,V,S>>>>>> + AtomicUsize(len)
```

---

## 示例索引

| 目标         | 代码                              |
|------------|---------------------------------|
| 基础插入/读取    | 快速上手                            |
| 异步操作       | 异步示例                            |
| 并行迭代       | 并行迭代                            |
| Serde 同步   | `serde_json::to_string(&map)`   |
| 异步快照序列化    | `async_snapshot_serializable()` |
| 自定义 hasher | `with_shards_and_hasher`        |
| 自定义分片上限    | `with_shards_and_hasher_capped` |
| 严格校验构造      | `try_with_shards_and_hasher`    |

---

## 分片数量安全策略

- 不抛错构造函数默认使用 `MAX_SHARDS` 上限，避免超大分配导致内存风险。
- 如需自定义上限，可使用 `with_shards_and_hasher_capped(shard_count, hasher, max_shards)`。
- 如需严格模式（不裁剪，超限直接报错），使用
  `try_with_shards_and_hasher(..)` 或 `try_with_shards_and_hasher_capped(..)`，
  返回 `ShardCountError`。

---

## 许可

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) 双许可。

---

## 贡献

欢迎 Issue / PR：

```bash
cargo clippy --all-features -- -D warnings
cargo test --all-features
```

提交前确保：

- 新特性具文档与测试
- 不破坏现有语义（或在 CHANGELOG 中注明）

---

## 全特性简例

```rust
use starshard::{ShardedHashMap, AsyncShardedHashMap};
#[cfg(feature = "async")]
#[tokio::main]
async fn main() {
    let sync_map: ShardedHashMap<u64, u64> = ShardedHashMap::new(32);
    sync_map.insert(1, 10);

    #[cfg(feature = "serde")]
    {
        let json = serde_json::to_string(&sync_map).unwrap();
        let _de: ShardedHashMap<u64, u64> = serde_json::from_str(&json).unwrap();
    }

    let async_map: AsyncShardedHashMap<u64, u64> = AsyncShardedHashMap::new(32);
    async_map.insert(2, 20).await;
}
```

---

## 免责声明

性能数据仅作参考；生产使用请结合自身压测验证。
