# Benchmark results

Each run of `scripts/run-benchmarks.sh` writes a folder here
(`<machine>-<date>/`, with a `RESULTS.md` written by whoever ran it, following
`docs/BENCHMARKING.md`). Each run of `scripts/bench-tails.sh` alone writes one
`tails-<machine>-<date>.txt` file.

## The development Mac's reference runs

MacBook Pro 15,2: Intel Core i7-8569U (4 cores / 8 threads, 2.8 GHz), 16 GB, macOS 15.6,
on AC power, with other applications running. Each file is the tail harness only
(`scripts/bench-tails.sh`), from the code as it was at that step:

| File | Code as of | Load average (start → end) | Notes |
|---|---|---|---|
| `tails-MacBookPro15,2_-20261126-122250.txt` | Step 23 (the harness itself) | 2.4 → 7.0 | **The cleanest of the three.** The baseline the step notes compare with |
| `tails-MacBookPro15,2_-20261126-130544.txt` | Step 24 (our TLS record layer) | 9.0 → 3.5 | Heavily loaded: OS-involving rows (wire, read) are unreliable; CPU rows usable |
| `tails-MacBookPro15,2_-20261126-140506.txt` | Step 25 (SIMD level parser) | 10.8 → 5.7 | Heavily loaded: same caveat |

Later changes (step 26's price-indexed book, step 28's harness additions: section 7
"jitter" and the "SIMD level parser" header line) aren't in these files. Their effect was
measured with interleaved A/B runs, recorded in `docs/steps/26-tick-book.md` and
`docs/steps/28-linux.md`.

**Comparing another machine with these files:**
- **Our own code's rows** (pipeline warm/cold, message anatomy, the criterion benchmarks
  in the step notes and `docs/LATENCY.md`) compare across machines. Allow for the code
  having improved since each file (steps 24–26).
- **OS-involving rows** (wire, `recv`, jitter) describe each machine's OS and its load at
  the time, more than the code.
- **On Apple Silicon** the parser is scalar (the SIMD kernel is x86-only), and the timer
  resolves 41.7 ns; see `docs/BENCHMARKING.md` §7.
