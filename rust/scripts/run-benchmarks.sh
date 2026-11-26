#!/usr/bin/env sh
# Every benchmark in the project, in one command, into one folder.
#
#   scripts/run-benchmarks.sh             # everything (~25–40 min; needs internet for part 4)
#   scripts/run-benchmarks.sh --no-live   # skip the live exchange runs (no internet needed)
#   scripts/run-benchmarks.sh --quick     # smoke test of the whole pipeline (~5 min): NOT results
#
# Output: bench-results/<machine>-<date>/
#   environment.txt         machine, OS, compiler, commit, load average
#   tests.txt               the full test suite (a failing suite stops the run)
#   tails.txt               tail latencies: every stage, p50 … p99.99, max; jitter
#   criterion.txt           every criterion benchmark, full output
#   criterion-summary.txt   one line per benchmark: name and time [low median high]
#   live.txt                60 s live against Binance + Bybit, stage histograms
#   live-full-depth.txt     the same with Binance full depth (diff stream + REST snapshot)
#
# Then write RESULTS.md in that folder as described in docs/BENCHMARKING.md.
set -eu
cd "$(dirname "$0")/.."
. scripts/machine.sh

live=1
quick=0
for a in "$@"; do
    case "$a" in
        --no-live) live=0 ;;
        --quick) quick=1 ;;
        *) echo "usage: scripts/run-benchmarks.sh [--no-live] [--quick]" >&2; exit 2 ;;
    esac
done

dir="bench-results/$(machine_name)-$(date +%Y%m%d-%H%M%S)"
[ "$quick" = 1 ] && dir="$dir-QUICK"
mkdir -p "$dir"
log() { echo "== $(date +%H:%M:%S) $*" >&2; }

log "results: $dir"
{
    describe
    echo "load avg:   $(loadavg) (at start)"
    echo "mode:       $([ "$quick" = 1 ] && echo 'QUICK (smoke test, not results)' || echo full)$([ "$live" = 0 ] && echo ', no live runs' || true)"
} > "$dir/environment.txt"
cat "$dir/environment.txt" >&2

log "1/4 building and running the test suite (release)"
if ! cargo test --workspace --release > "$dir/tests.txt" 2>&1; then
    log "TESTS FAILED: see $dir/tests.txt. Stopping: benchmarks of failing code mean nothing."
    exit 1
fi
grep -E "^test result" "$dir/tests.txt" \
    | awk '{p+=$4; f+=$6; i+=$8} END {printf "tests: %d passed, %d failed, %d ignored\n", p, f, i}' \
    | tee -a "$dir/environment.txt" >&2

log "2/4 tail-latency harness (scripts/bench-tails.sh)"
if [ "$quick" = 1 ]; then
    BENCH_OUT="$dir/tails.txt" scripts/bench-tails.sh --quick > /dev/null
else
    BENCH_OUT="$dir/tails.txt" scripts/bench-tails.sh > /dev/null
fi

log "3/4 criterion benchmarks (every crate)"
{
    cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json, sys
for package in json.load(sys.stdin)["packages"]:
    for target in package["targets"]:
        if "bench" in target["kind"]:
            print(package["name"], target["name"])'
} | while read -r package target; do
    log "   $package/$target"
    if [ "$quick" = 1 ]; then
        cargo bench -p "$package" --bench "$target" -- \
            --noplot --warm-up-time 0.1 --measurement-time 0.2
    else
        cargo bench -p "$package" --bench "$target" -- --noplot
    fi
done > "$dir/criterion.txt" 2>&1
# One line per benchmark: its name and time interval. Criterion prints a short name on
# the same line as "time:", a long one on the line before it.
awk '
    /^Benchmarking /       { next }
    /^[^ ].*time:/         { n = $0; sub(/ +time:.*/, "", n); t = $0; sub(/.*time: */, "", t)
                             printf "%-70s %s\n", n, t; next }
    /^[^ ]/                { name = $0 }
    /^ +time:/             { t = $0; sub(/^ *time: */, "", t); printf "%-70s %s\n", name, t }
' "$dir/criterion.txt" > "$dir/criterion-summary.txt"
log "   $(wc -l < "$dir/criterion-summary.txt" | tr -d ' ') benchmarks"

if [ "$live" = 1 ]; then
    secs=60
    [ "$quick" = 1 ] && secs=10
    cargo build --release -p ob-app >&2
    log "4/4 live: ${secs} s against Binance + Bybit (needs internet)"
    target/release/ob live --secs "$secs" --latency --rt > "$dir/live.txt" 2>&1 \
        || log "   live run failed (network?): see $dir/live.txt"
    log "4/4 live: ${secs} s with Binance full depth"
    target/release/ob live --secs "$secs" --latency --rt --binance-full-depth \
        > "$dir/live-full-depth.txt" 2>&1 \
        || log "   live full-depth run failed (network?): see $dir/live-full-depth.txt"
else
    log "4/4 live runs skipped (--no-live)"
fi

echo "load avg:   $(loadavg) (at end)" >> "$dir/environment.txt"
log "done: $dir"
log "next: write $dir/RESULTS.md as described in docs/BENCHMARKING.md"
