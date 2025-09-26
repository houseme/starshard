# Starshard

English | [简体中文](README_CN.md)

[![Build](https://github.com/houseme/starshard/workflows/Build/badge.svg)](https://github.com/houseme/starshard/actions?query=workflow%3ABuild)
[![crates.io](https://img.shields.io/crates/v/starshard.svg)](https://crates.io/crates/starshard)
[![docs.rs](https://docs.rs/starshard/badge.svg)](https://docs.rs/starshard/)
[![License](https://img.shields.io/crates/l/starshard)](./LICENSE-APACHE)
[![Downloads](https://img.shields.io/crates/d/starshard)](https://crates.io/crates/starshard)

A high\-performance sharded concurrent HashMap for Rust, built on `hashbrown` and `RwLock`/`tokio::sync::RwLock`.

## Features

- Lazy shard initialization
- O\(1\) atomic `len`
- Optional parallel iteration with `rayon`
- Async API with Tokio \(feature `async`\)
- Pluggable hashers \(default: `fxhash`\)

## Install

Add to `Cargo.toml`:

```toml
[dependencies]
starshard = { version = "0.1.0", features = ["rayon", "async"] }
fxhash = "0.2.1"
hashbrown = { version = "0.16.0", features = ["serde", "rayon"] }
rayon = "1.11.0"
tokio = { version = "1.47.1", features = ["sync", "rt-multi-thread"] }
```

## Quick Start

```rust
use starshard::ShardedHashMap;

let map: ShardedHashMap<String, i32, fxhash::FxBuildHasher> = ShardedHashMap::new(64);
map.insert("k".into(), 1);
assert_eq!(map.get(&"k".into()), Some(1));
```

## License

Dual\-licensed under MIT or Apache\-2\.0.

- See [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).
- © 2025 [houseme](https://github.com/houseme).

