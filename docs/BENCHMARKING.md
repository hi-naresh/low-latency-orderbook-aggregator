# Running the benchmarks on another machine

This page is written for **a person or an AI agent** running the project's benchmarks on
a machine other than the development Mac, in particular an **Apple Silicon Mac (M2, M3,
M4)**. Follow it in order. Running everything takes **about 30–45 minutes**, most of it
unattended.

**If you are an AI agent:** follow §1–§6 exactly.
- **Don't change the source code, the scripts or the build settings.**
- **If a step fails, stop and report the failure** (the command, and the last 50 lines of
  its output) rather than working around it.
- **Never edit or round a number** when copying it into `RESULTS.md`; copy it as printed.

## 1. Prerequisites

| What | How to check | How to install |
|---|---|---|
| macOS on Apple Silicon (or Intel), or Linux x86-64 | `uname -sm` → `Darwin arm64` | – |
| Xcode Command Line Tools (a C compiler for the TLS library) | `xcode-select -p` prints a path | `xcode-select --install` |
| rustup (Rust's installer) | `rustup --version` | <https://rustup.rs> (one command) |
| git | `git --version` | comes with the Command Line Tools |
| Internet access | – | Needed to download dependencies, and for the live runs in §3 part 4 |

**You don't install a Rust version by hand.** `rust/rust-toolchain.toml` pins the exact
compiler (1.100.0), and the first `cargo` command downloads it automatically.

```bash
git clone <the repository URL>
cd <repository>/rust
cargo build --release --workspace      # first build: downloads the toolchain and crates (~5 min)
```

## 2. Prepare the machine

Results depend heavily on what else the machine is doing. Step 20 measured machine load
moving results by up to 50%. Before running:

1. **Plug in the power** (on battery, macOS lowers CPU performance). In System Settings
   → Battery, turn **Low Power Mode off**.
2. **Quit every other application,** including browsers, IDEs, Docker and backups. Keep
   the lid open, so the machine stays cool.
3. **Don't use the machine during the run.** Even a browser tab changes the numbers.
4. Wait until the load average settles below ~2 (`sysctl -n vm.loadavg`: the first
   number). The run records it at the start and end anyway.

## 3. Run everything

```bash
cd rust
scripts/run-benchmarks.sh            # everything, including 2 × 60 s live against the exchanges
# or, without internet access:
scripts/run-benchmarks.sh --no-live
```

It prints progress (`== 1/4 …` to `== 4/4 …`) and writes everything into one folder,
`bench-results/<machine>-<date>/`:

| File | Contents | Takes |
|---|---|---|
| `environment.txt` | Machine, OS, compiler, commit, load average; the test totals | – |
| `tests.txt` | The full test suite, release build. **If any test fails, the run stops here.** | ~3 min |
| `tails.txt` | The tail-latency harness: every stage's p50 / p90 / p99 / p99.9 / p99.99 / max (sections 1–7 below) | ~5 min |
| `criterion.txt`, `criterion-summary.txt` | Every criterion micro-benchmark (~100), and one line each: name and `[low median high]` | ~15–25 min |
| `live.txt`, `live-full-depth.txt` | 60 s live against Binance and Bybit, with per-stage histograms (default streams, then Binance full depth) | 2 × 1 min |

`scripts/run-benchmarks.sh --quick` exists to check the pipeline works end to end in
about 5 minutes. Its numbers are **not results** (the folder name ends in `-QUICK`).

**What `tails.txt` contains** (the harness is described in `docs/steps/23-tail-harness.md`):

| Section | Measures |
|---|---|
| 1. Pipeline, warm | Parse → apply → publish per message, back to back, in process |
| 2. Pipeline, cold | The same, with the CPU caches flushed before each message |
| 3. Wire, busy / sparse | A local TLS server → kernel → read + TLS + WebSocket → parse → apply → publish, open loop at 20,000 and 1,000 messages/s |
| 5. Read stage, taken apart | `recv`, rustls, our TLS record layer, WebSocket decode, AES alone |
| 6. Message anatomy | The slowest recorded messages: parse vs apply + publish |
| 7. Jitter | How often the OS takes the CPU away from a spinning thread |
| 4. Reader | Aggregator queries on another thread, with the writer idle or busy |

## 4. Check the run is valid

Before writing anything up, check each of these. Report any that fail.

| Check | Where | Expected |
|---|---|---|
| All tests passed | `environment.txt`, line `tests:` | `0 failed` |
| Every feed scenario stayed on the normal path | `tails.txt`: search for `INVALID RUN` | No matches |
| Which parser ran | `tails.txt` header: `SIMD level parser:` | **Apple Silicon: `OFF`** (expected: the SIMD kernel is x86-only; see §7). Intel Mac or Linux x86-64: `on` |
| The machine was quiet | `environment.txt`: load average at start and end | Below ~2 at the start; say so if it wasn't |
| Live runs stayed healthy | `live.txt`, `live-full-depth.txt`: the per-venue counter lines | `resyncs 0`, `rejected 0`, `reconnects 0` (a reconnect on a flaky network is worth reporting, not hiding) |
| Criterion ran every suite | `criterion-summary.txt` | ~100 lines |

## 5. Write `RESULTS.md`

Create `RESULTS.md` in the results folder with exactly these sections. Copy numbers as
printed. Where the guide says p50 / p99 / p99.9, take the columns with those names from
the row named.

```markdown
# Benchmark results: <machine model, e.g. MacBook Pro M3 Pro>

## Machine
<environment.txt, verbatim>

## Validity
<each §4 check: passed / failed, with the value>

## A. Our code per message (tails.txt §1 and §2, row "read start → published")
| Stream | Warm p50 / p99 / p99.9 | Cold p50 / p99 / p99.9 |
| binance depth20+bookTicker | | |
| bybit orderbook.50+1 | | |
| binance full depth | | |

## B. The whole receive path (tails.txt §3)
| | read p50 / p99 / p99.9 | read start → published p50 / p99 / p99.9 | sent → published p50 / p99 / p99.9 |
| Binance, busy | | | |
| Bybit, busy | | | |
| Binance, sparse | | | |
| Bybit, sparse | | | |

## C. The read stage, taken apart (tails.txt §5), p50 / p99 / p99.9
| | Binance | Bybit |
| recv, data waiting | | |
| recv, nothing yet | | |
| rustls: all three | | |
| ours: decrypt in place | | |
| AES-128-GCM alone | | |

## D. Slowest messages (tails.txt §6): copy the two tables verbatim

## E. Reader queries (tails.txt §4), p50 / p99 / p99.9: writer idle | writer busy
<one row per query>

## F. Jitter (tails.txt §7): the gap row and the "interruptions per second" line, verbatim

## G. Criterion highlights (criterion-summary.txt): copy the lines starting with
- `pipeline/` (parse → apply → publish per recorded batch, every stream)
- `full_depth_apply/` (the full-depth book: sorted arrays vs price-indexed)
- `binance_depth20/` (our parser vs `serde_typed` and `serde_value`)
- `read_top/`, `read_full_20x20/`, `contended/` (seqlock vs mutex)
- `execution/` and `aggregator/` (the cross-venue queries)

## H. Live (live.txt, live-full-depth.txt)
<per venue: the stage table, and the counter line, verbatim>

## Notes
<anything unusual: warnings, retries, load, thermal throttling, network trouble>
```

Then add **one short comparison paragraph** against the development Mac's results
(described in `rust/bench-results/README.md`). Compare only like with like:
- **CPU-bound rows** (sections A, D, G) compare directly between machines.
- **OS-involving rows** (B, C's `recv` rows, F) mostly reflect each machine's OS and load.
- **Parsing:** this machine runs the scalar parser (§7).

## 6. Send the results back

Inside the repository, if you can push to it:

```bash
cd <repository>
git checkout -b bench/<results-folder-name>
git add rust/bench-results/<results-folder-name>
git commit -m "Benchmark results: <machine model>"
git push -u origin bench/<results-folder-name>
```

If you can't push: zip the results folder (`zip -r results.zip
rust/bench-results/<results-folder-name>`) and send it to the repository's owner.

## 7. Apple Silicon: what's different, and how to read it

| | Effect on the numbers |
|---|---|
| **The timer ticks every 41.7 ns** (the ARM system counter at 24 MHz; Intel's `rdtsc` resolves ~0.3 ns) | Per-sample timings come in 41.7 ns steps. Stages under ~100 ns (publish, WebSocket decode, reader queries, the "timer floor" row) show as 0, 42, 83… ns. **Use criterion's numbers for those** (§G): criterion times many operations together. |
| **No SIMD level parser** (`SIMD level parser: OFF`) | The kernel uses x86 SSSE3/SSE4.1. On ARM the scalar parser runs: correct, but 11–37% slower to parse level-heavy messages than it would be with a NEON kernel (not written: `docs/LINUX.md` §5). |
| **Performance and efficiency cores** | The harness requests the "user-interactive" QoS class (`--rt`), which steers the measured threads to performance cores. macOS can still move them. |
| **macOS can't isolate a core** | As on the development Mac, p99.9 of anything involving the OS is set by interruptions (§F shows how many). `docs/LINUX.md` explains what a tuned Linux host changes. |
| **Faster cores, bigger caches** | Expect our code's warm numbers (A, D, G) to be substantially lower than the Intel Mac's. That's the main point of running here. |
