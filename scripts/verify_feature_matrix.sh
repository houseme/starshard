#!/usr/bin/env bash
set -euo pipefail

cargo test --lib --tests --no-default-features
cargo test --lib --tests --features lifecycle
cargo test --lib --tests --features advanced
cargo test --lib --tests --features "async advanced"
cargo test --lib --tests --features "async serde"
cargo test --lib --tests --features full
