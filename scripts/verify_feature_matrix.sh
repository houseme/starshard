#!/usr/bin/env bash
set -euo pipefail

cargo test --locked --lib --tests --no-default-features
cargo test --locked --lib --tests --no-default-features --features lifecycle
cargo test --locked --lib --tests --no-default-features --features advanced
cargo test --locked --lib --tests --no-default-features --features async
cargo test --locked --lib --tests --no-default-features --features "async advanced"
cargo test --locked --lib --tests --no-default-features --features "async serde"
cargo test --locked --lib --tests --features full
