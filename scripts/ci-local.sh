#!/usr/bin/env bash
set -euo pipefail

cargo check --all-targets --all-features --locked
RUST_TEST_THREADS=1 cargo test --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
