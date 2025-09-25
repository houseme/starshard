# Starshard

简体中文 | [English](README.md)

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

一个高性能的分片并发 HashMap，基于 `hashbrown` 与 `RwLock`／`tokio::sync::RwLock` 实现。

## 特性

- 分片懒初始化，降低内存占用
- 原子 O\(1\) `len`
- 可选 `rayon` 并行遍历
- 启用 `async` 特性提供 Tokio 异步 API
- 可插拔哈希器（默认 `fxhash`）

## 安装

在 `Cargo.toml` 中添加：

```toml
[dependencies]
starshard = { version = "0.1.0", features = ["rayon", "async"] }
```

## 快速上手

```rust
use starshard::ShardedHashMap;
let map: ShardedHashMap<String, i32> = ShardedHashMap::new(64);
map.insert("k".into(), 1);
assert_eq!(map.get(&"k".into()), Some(1));
```

## 许可证

本项目采用双许可证：MIT 或 Apache\-2\.0。

- 参见 `LICENSE-MIT` 与 `LICENSE-APACHE`。
- © 2025 [houseme](https://github.com/houseme)。