#!/usr/bin/env bash
# Full verification pipeline for Rust workspace
# Run: ./scripts/check-all.sh

set -euo pipefail

echo "🔍 Checking format..."
cargo fmt --all --check

echo "🔍 Running clippy..."
cargo clippy --workspace --all-targets

echo "🔍 Building..."
cargo build --workspace

echo "🔍 Running tests..."
cargo test --workspace

echo "✅ All checks passed!"