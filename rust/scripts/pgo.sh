#!/usr/bin/env sh
# Profile-guided optimisation (step 20) for the `ob` binary.
#
# 1. Build `ob` instrumented (it counts how often each branch and function runs).
# 2. Train it: a live session (covers the network path: TLS, WebSocket, the feed loop)
#    plus replays of any recordings given (the parse → apply → publish path, at volume).
# 3. Merge the counts and rebuild `ob` with them: LLVM lays out hot code together,
#    inlines along hot paths and moves cold code (errors, resyncs) out of the way.
#
# Train and use the profile on the SAME binary: with fat LTO, function identity is
# per-binary, and a profile from another binary (step 20 tried the benchmark binary)
# only partly matches and made code slower. Code the training never runs is treated as
# cold, which is why the live session is part of training.
#
# Needs: rustup component add llvm-tools   (for llvm-profdata)
# Usage (from rust/): scripts/pgo.sh [live_seconds] [recording.obrec ...]
# Output: target/pgo/release/ob
set -eu
cd "$(dirname "$0")/.."
secs="${1:-30}"
[ $# -gt 0 ] && shift
profdata="$(find "$(rustc --print sysroot)" -name llvm-profdata -type f | head -1)"
[ -n "$profdata" ] || { echo "llvm-profdata not found: rustup component add llvm-tools" >&2; exit 1; }
work="target/pgo"
# An environment RUSTFLAGS replaces .cargo/config.toml's: keep its Linux CPU level, or the
# PGO build would silently drop the SIMD level parser (step 25; found in step 28).
cpu=""
[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] && cpu="-C target-cpu=x86-64-v2"
rm -rf "$work/raw" && mkdir -p "$work/raw"

echo "== 1. instrumented build"
RUSTFLAGS="-Cprofile-generate=$PWD/$work/raw $cpu" CARGO_TARGET_DIR="$work/gen" \
    cargo build --release -p ob-app

echo "== 2. training: live for ${secs}s, then replays"
"$work/gen/release/ob" live --secs "$secs" >/dev/null 2>&1
"$work/gen/release/ob" live --secs "$secs" --binance-full-depth >/dev/null 2>&1
for rec in "$@"; do
    "$work/gen/release/ob" replay "$rec" >/dev/null
done

echo "== 3. merge and optimised build"
"$profdata" merge -o "$work/merged.profdata" "$work/raw"
RUSTFLAGS="-Cprofile-use=$PWD/$work/merged.profdata $cpu" CARGO_TARGET_DIR="$work" \
    cargo build --release -p ob-app
echo "PGO binary: $work/release/ob"
