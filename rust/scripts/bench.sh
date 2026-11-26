#!/usr/bin/env sh
# Run benchmarks with instructions specific to this CPU (e.g. AVX2, BMI2 on x86-64).
# Kept out of the default build so normal binaries still run on any x86-64 / arm64 machine.
# Usage: scripts/bench.sh [cargo bench args...]
set -eu
cd "$(dirname "$0")/.."
RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native" exec cargo bench "$@"
