#!/usr/bin/env bash
# Auto-fix format and clippy suggestions
# Run: ./scripts/fix-all.sh

set -euo pipefail

echo "🔧 Formatting code..."
cargo fmt --all

echo "🔧 Running clippy fixes..."
cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged -- -D warnings 2>/dev/null || true

echo "✅ Fixes applied. Run check-all.sh to verify."