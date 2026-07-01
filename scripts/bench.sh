#!/usr/bin/env bash
# Build everything (release), run all benchmarks, and regenerate the README tables.
# All the real work lives in the `xtask` crate; this is just a friendly entry point.
#
# Usage:
#   ./scripts/bench.sh                # build, run, regenerate
#   ./scripts/bench.sh --readme-only  # regenerate tables from existing results/*.json
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --quiet -p xtask -- "$@"
