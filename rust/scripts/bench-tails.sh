#!/usr/bin/env sh
# Tail-latency run (steps 23, 28): p50 / p99 / p99.9 / p99.99 / max for every stage, from
# encrypted bytes to the published book, the aggregator's queries, and OS jitter.
#
# Writes one self-describing file: the machine (model, CPU, cores, memory, caches, OS,
# compiler, commit), its load before and after, the exact command, and the results.
#
# Linux (the target) and macOS (Intel or Apple Silicon) are supported. On Linux, set
# BENCH_CPU to an isolated core (docs/LINUX.md §3.1) to run the harness pinned there:
#   BENCH_CPU=2 scripts/bench-tails.sh
#
# Usage (from rust/):  scripts/bench-tails.sh [extra options for `tails`]
#   e.g.  scripts/bench-tails.sh --quick         (a smoke test, ~25 s)
# Output: bench-results/tails-<machine>-<date>.txt, or the path in BENCH_OUT.
#
# To run everything (tests, this, criterion, live), use scripts/run-benchmarks.sh.
set -eu
cd "$(dirname "$0")/.."
. scripts/machine.sh

# The cache flush for "cold" mode must exceed the largest cache: 4x the largest one the
# OS reports, at least 32 MiB. (Apple chips also have a system-level cache that sysctl
# doesn't report; 4x the L2 covers its common sizes.)
cold_mb=$(( $(largest_cache) * 4 / 1048576 ))
[ "$cold_mb" -lt 32 ] && cold_mb=32

# On Linux, optionally pin the whole run to one (isolated) core.
pin=""
if [ "$os" = Linux ] && [ -n "${BENCH_CPU:-}" ]; then
    pin="taskset -c $BENCH_CPU"
fi

mkdir -p bench-results
out="${BENCH_OUT:-bench-results/tails-$(machine_name)-$(date +%Y%m%d-%H%M%S).txt}"

echo "building (release)..." >&2
cargo build --release -p ob-bench --bin tails >&2

{
    echo "== machine"
    describe
    echo "load avg:   $(loadavg) (before)"
    echo "command:    $pin target/release/tails --rt --cold-mb $cold_mb $*"
    echo
    # shellcheck disable=SC2086  # $pin is empty or "taskset -c N": split on purpose
    $pin target/release/tails --rt --cold-mb "$cold_mb" "$@"
    echo
    echo "load avg:   $(loadavg) (after)"
} | tee "$out"
echo "written: $out" >&2
