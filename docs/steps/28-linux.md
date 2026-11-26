# Step 28: The Linux document, and making the project Linux-first

The project is designed for Linux: one isolated core per venue feed. It was built and
measured on a Mac. This step finished `docs/LINUX.md`, the document that says:
- what's in the code for Linux;
- what must be added or changed there;
- the impact to expect from each, against the Mac's measurements;
- how to verify it on day one.

Reviewing the project against "Linux first" also found two real problems, both fixed.

(Step 27, "publish only what changed", was dropped after step 26: publishing is ~0.4 µs
of the largest message's ~37 µs.)

## What changed

| File | What |
|---|---|
| `docs/LINUX.md` | Updated for steps 25–26 and this step; precise about what was checked for Linux and what wasn't; new day-one tools. |
| `crates/ob-bench/src/jitter.rs` | **New scenario** `tails --only jitter`: how often, and for how long, the OS takes the CPU from a spinning thread. 1 test. |
| `crates/ob-bench/src/bin/tails.rs` | Runs the jitter scenario; the header now says whether the SIMD level parser is on. |
| `crates/ob-parse/src/lib.rs` | `SIMD_LEVELS`: whether this build parses levels with the SIMD kernel. |
| `scripts/bench-tails.sh` | **Runs on Linux too:** machine description from `/proc`, `/sys`, `/etc/os-release` (isolated CPUs, kernel command line, CPU governor recorded); `BENCH_CPU=N` pins the run with `taskset`. POSIX `sh` (checked with `dash -n`). |
| `.github/workflows/ci.yml` | **Fix:** Linux jobs build with `target-cpu=x86-64-v2`, so CI tests the SIMD parser. |
| `scripts/pgo.sh` | **Fix:** keeps `target-cpu=x86-64-v2` on Linux x86-64. |
| `docs/TAIL-LATENCY-PLAN.md`, `docs/steps/26-tick-book.md` | Step 27 dropped, step 28 done. |

## 1. Two problems found by reviewing "Linux first"

**CI never tested the SIMD parser on Linux.** CI sets `RUSTFLAGS: -D warnings` for every
job, and Cargo uses an environment `RUSTFLAGS` *instead of* `.cargo/config.toml`'s
`rustflags`. So the `target-cpu=x86-64-v2` that step 25 added for Linux was ignored on CI,
and the kernel was compiled out. The tests still passed, because the scalar path gives
identical results, so nothing showed it.
- **Fix:** the Linux clippy, test and bench jobs set `-D warnings -C target-cpu=x86-64-v2`.
- **Bonus:** GitHub's `macos-latest` runners are Apple Silicon (no SSE), so CI now tests
  **both** paths: the kernel on Linux, the scalar fallback on macOS.
- A first version referred to a workflow-level variable from a job's `env:`, which
  GitHub Actions doesn't allow. The flags are now written out literally.

**`scripts/pgo.sh` had the same trap:** its `RUSTFLAGS` for profile-guided builds would
also have dropped the CPU level on Linux. It now adds it on Linux x86-64. `scripts/bench.sh`
was already fine (`target-cpu=native` includes SSSE3/SSE4.1).

**Why this matters:** a build that silently loses an optimisation is the worst kind of
regression. Correctness tests can't see it, and only a benchmark would. The harness header
now prints `SIMD level parser: on` or `OFF`, so every result file says which parser ran.

## 2. A jitter test, built into the harness

The Linux document's main claim is that **core isolation removes the interrupts that set
p99.9** (step 23, finding 4). Day one needs a way to check that, so the harness now has
one. A thread spins reading the clock, and every gap between consecutive reads longer
than the read itself is time the OS took the CPU away.

On this Mac (two runs, the machine moderately loaded):

| | Run 1 (10 s) | Run 2 (2 s) |
|---|---|---|
| Gaps over 1 µs, per second | 4,170 | 3,949 |
| Gaps over 10 µs, per second | 2,848 | 2,643 |
| Gaps over 100 µs, per second | 24.9 | 3.0 |
| Worst gap | 48.4 ms | – |
| CPU time lost | 7.8% | 5.5% |

At ~0.4 µs per message, a few thousand interruptions a second hit roughly one message in
a thousand: that is the p99.9 cliff step 23 saw. **On an isolated Linux core this should
be close to zero**, and this test is what shows it.

## 3. The bench script on Linux

`scripts/bench-tails.sh` was written for macOS (`sysctl`, `sw_vers`, `pmset`). It now
detects the OS:

| | Linux | macOS |
|---|---|---|
| Model, CPU, memory | `/sys/class/dmi/id/product_name`, `/proc/cpuinfo`, `/proc/meminfo` | `sysctl` |
| Caches (sizes the cold-mode flush) | `/sys/devices/system/cpu/cpu0/cache/index*/size` (`32K`, `16M` parsed) | `sysctl hw.l2/l3cachesize` |
| Also recorded | isolated CPUs, kernel command line, CPU governor, pinned core | power source |
| Pinning | `BENCH_CPU=2` → `taskset -c 2` | – |

**Verified:** the macOS path end to end; the Linux path's size parsing; the whole script
with `sh -n` and `dash -n` (Debian and Ubuntu's `/bin/sh`). **Not verified:** the Linux
path end to end, which needs a Linux machine. It's the first item of the day-one
checklist.

## 4. What docs/LINUX.md now says

1. **Where the time goes on the Mac**, stage by stage, and why each stage changes on
   Linux. The p99/p99.9 of every stage involving the OS (system calls, scheduling,
   interrupts) is a property of the laptop. Our code's costs carry over.
2. **What's already in the code**, and **precisely what was checked**:
   - linted for Linux here: `ob-core`, `ob-parse`, `ob-simd`, `ob-sync`, `ob-agg`, `ob-os`;
   - built and tested only by CI: the TLS crates, since `ring` needs a Linux C compiler.
3. **What to add or change, each with its expected impact:**
   - core isolation, interrupt affinity, frequency and C-states, cache allocation,
     NUMA, huge pages;
   - busy polling, then `io_uring`, then kernel bypass;
   - kernel TLS and NIC offload;
   - measurement and build.
4. **What the result should look like:** p99.9 for the default streams from 63–268 µs
   on the Mac to an estimated under 10 µs with isolation and busy polling, and ~4–8 µs
   with `io_uring`. Every Linux figure is a labelled estimate.
5. **How to read the Mac's numbers** given all that.
6. **A day-one checklist** using the project's own tools.

## How to verify it yourself

```bash
cd rust
target/release/tails --only jitter                 # how often the OS takes the CPU
scripts/bench-tails.sh --quick                     # header shows "SIMD level parser: on"
sh -n scripts/bench-tails.sh && dash -n scripts/bench-tails.sh
RUSTFLAGS="-D warnings" cargo build --release -p ob-bench --target x86_64-unknown-linux-gnu  # the trap
```

The last command can't finish on this Mac (no Linux C compiler for `ring`); it shows the
trap on Linux. The quickest demonstration here is
`RUSTFLAGS="-C target-cpu=x86-64" cargo build --release -p ob-bench`, then
`target/release/tails --only jitter --quick`: the header says `SIMD level parser: OFF`.

## Java counterpart

The same class of problem exists on the JVM: JIT intrinsics and vectorisation depend on
the CPU flags the JVM detects, and options like `-XX:UseAVX=0`, or a container masking
CPU features, silently remove them. The equivalent safeguard is logging the detected
features at start-up (`-XX:+PrintFlagsFinal`, or the Vector API's
`VectorShape.preferredShape()`) and recording them with every benchmark.

## Likely interview questions

1. **What would you do first with a Linux box?**
   - Isolate two cores, then run the jitter test until it's clean.
   - Run the harness pinned there, then add busy polling.
   - Then `io_uring`, measuring each step against the Mac's file, row by row.
2. **How do you know the Linux numbers in your document aren't wishful?** They're labelled
   estimates. Each rests on a Mac measurement of the component it replaces, such as
   `recv` at 3 µs or interrupts at ~4,000 per second, and each comes with the command
   that will replace it with a measurement.
3. **What's a silent performance regression, and how do you guard against one?** A
   change that keeps results correct but loses speed, like `RUSTFLAGS` dropping the CPU
   level here. Tests can't see it. Guard against it by recording the configuration with
   every benchmark result, and by testing both paths explicitly (CI now does: SIMD on
   Linux, scalar on macOS).
