# Step 29: Ready to benchmark on another machine, and on GitHub

The final measurements will run on a faster machine, an Apple Silicon MacBook Pro
(M2/M3), cloned from GitHub. Possibly an AI agent will run them there. So this step makes
**running every benchmark one command**, documents it for someone with no context,
checks the project **builds for Apple Silicon** (it had only ever been built for
Intel), and puts the repository under git. No benchmarks were run on the development
Mac in this step.

## What changed

| File | What |
|---|---|
| `rust/scripts/run-benchmarks.sh` | **New:** one command. Tests (a failure stops the run), the tail harness, every criterion benchmark plus a one-line-per-benchmark summary, optionally 2 × 60 s live runs; all into `bench-results/<machine>-<date>/`. `--no-live`, `--quick`. |
| `rust/scripts/machine.sh` | **New:** the machine description, shared by both scripts. Linux; Intel Mac; Apple Silicon, with performance/efficiency core counts and per-cluster caches. Includes the git commit. |
| `rust/scripts/bench-tails.sh` | Uses `machine.sh`; `BENCH_OUT` sets the output path. |
| `docs/BENCHMARKING.md` | **New:** for a person or an AI agent: prerequisites, preparing the machine, the command, validity checks, the exact `RESULTS.md` format, sending results back, Apple Silicon caveats. |
| `AGENTS.md` | **New:** points an AI agent in the repository to `docs/BENCHMARKING.md`. |
| `rust/bench-results/README.md` | **New:** the three reference runs from the development Mac: code version, load, how to compare. |
| `crates/ob-os/src/lib.rs` | **Fix:** `qos_must_come_before_the_affinity_tag` would have failed on Apple Silicon (below). |
| `README.md`, `docs/LATENCY.md` | Updated for steps 23–29. |

## 1. Apple Silicon: compiled here, and one test that would have failed

The project had never been compiled for ARM. With your approval I installed Rust's
`aarch64-apple-darwin` target. **Every crate, test, benchmark and both binaries build and
lint cleanly for it** (`cargo build`/`clippy --workspace --all-targets --target
aarch64-apple-darwin`). This Intel Mac can't run ARM code, so ARM correctness rests on the
test suite running there first: `run-benchmarks.sh` stops if any test fails.

Reading the platform-specific code for ARM found one test that **would have failed**
there for a reason that isn't a bug:
- **The test:** `qos_must_come_before_the_affinity_tag` asserted that setting the QoS class
  fails after the affinity tag is set. True on Intel Macs.
- **Why it fails on Apple Silicon:** it refuses the affinity tag outright. The code
  already reports that as `Unsupported`, so the thread's policy never changes, and the QoS
  call succeeds.
- **The fix:** the test now checks the rule that applies on each chip, deciding by what
  the tag call reported.

## 2. What changes in the numbers on Apple Silicon

Three things the results must be read with, also in `docs/BENCHMARKING.md` §7:

| | Why |
|---|---|
| **Timer resolution 41.7 ns** | The harness's clock (`quanta`) reads the ARM system counter (`cntvct_el0`), which runs at 24 MHz on Apple Silicon; Intel's `rdtsc` resolves ~0.3 ns. Rows under ~100 ns come in 41.7 ns steps; criterion's batched numbers are the ones to use there. |
| **Scalar parser** | The SIMD level kernel (step 25) is x86 SSSE3/SSE4.1; `SIMD level parser: OFF` in the header is expected. |
| **Performance vs efficiency cores** | `--rt` requests the user-interactive QoS class, which steers the measured threads to the performance cores; macOS can still move them. |

## 3. One command, checked as far as possible without running it

You asked for no benchmarks on this machine, so `run-benchmarks.sh` wasn't run end to
end here (an attempt at a `--quick` smoke run was blocked, as it should have been). What
was checked:
- **Syntax:** every script with `sh -n` and `dash -n`.
- **The machine description** on this Mac.
- **The criterion summary extraction,** on sample output in both of criterion's layouts.
  The first version garbled the one-line layout (`name   time: [...]`) and was fixed.
- **The criterion options** (`--noplot`, `--warm-up-time`, `--measurement-time`),
  against a benchmark binary's `--help`.
- **Every component it calls** has run here in earlier steps: the test suite, the tail
  harness, `ob live`.

The first real end-to-end run happens on the target machine. `--quick` exists for a
five-minute dry run there first.

## 4. Git and GitHub

The repository is initialised at the project root and committed. Build output, IDE
files and the regenerable fuzz corpus are ignored (checked in step 23 with a throwaway
git directory). The three reference result files and the recorded fixtures are included.

The GitHub CLI isn't installed here, and authentication must stay with you: I can't take
passwords or tokens, even with your permission. So the push needs either an empty
repository's URL (the push then uses the git credentials already on this Mac), or the
GitHub CLI logged in by you.

## How to verify it yourself

```bash
cd rust
cargo build --release --workspace --all-targets --target aarch64-apple-darwin   # ARM build
sh -n scripts/run-benchmarks.sh && dash -n scripts/run-benchmarks.sh
sh -c '. scripts/machine.sh; describe'                                          # this machine
```

On the M2/M3: `scripts/run-benchmarks.sh --quick` (a dry run, ~5 min), then
`scripts/run-benchmarks.sh`.

## Likely interview questions

1. **How do you make benchmark results from different machines comparable?**
   - Record the machine, OS, compiler, commit and load average with every run.
   - Use one command, so every run measures the same things the same way.
   - Say which numbers compare across machines (our code's CPU-bound costs) and which
     describe the machine (anything involving the OS).
2. **What's different about timing on Apple Silicon?** The user-space counter ticks at
   24 MHz (41.7 ns), so single short events can't be resolved; batch them (criterion) or
   measure longer spans. On Intel, `rdtsc` is invariant and sub-nanosecond.
3. **Why let the run stop on a failing test?** A benchmark of incorrect code is
   meaningless, and on a new architecture the tests are the only evidence the code is
   correct there.
