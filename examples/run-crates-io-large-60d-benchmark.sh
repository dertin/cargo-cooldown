#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WORKSPACE_DIR="${ROOT_DIR}/crates-io-large-benchmark-workspace" \
COOLDOWN_MINUTES="${COOLDOWN_MINUTES:-86400}" \
BENCH_OFFLINE="${BENCH_OFFLINE:-0}" \
BENCH_PREFETCH_COOLDOWN="${BENCH_PREFETCH_COOLDOWN:-0}" \
  "${ROOT_DIR}/run-crates-io-benchmark.sh"
