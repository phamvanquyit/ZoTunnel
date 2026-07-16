#!/usr/bin/env bash
set -euo pipefail

# Pre-release checks — run before tagging a release.
# Usage: ./scripts/pre-release.sh

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "▸ cargo fmt"
cargo fmt --all -- --check

echo "▸ cargo clippy"
cargo clippy --workspace -- -D warnings

echo "▸ cargo test"
cargo test --workspace

echo "✅ Pre-release checks passed"
