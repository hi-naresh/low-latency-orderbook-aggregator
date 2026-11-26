# Linux: what this design needs on its real target, and what it would gain

**The project is designed for Linux** (x86-64, one isolated core per venue feed). It was
built and measured on the only machine available, a MacBook (Intel i7-8569U, macOS).
This document lists:
- what is **already in the code** for Linux;
- everything that still has to be **added or changed** for Linux;
- for each item, the **impact to expect**, the measurement from the Mac it rests on,
  and how to **verify** it on the first day a Linux machine is available.

Every "expected" figure below is an **estimate** until measured. Each one names its
evidence.

**Policy followed throughout:** the code is written for Linux. Where a Linux facility
doesn't exist on macOS, the code asks for it anyway and **reports what the OS did**
(applied, a weaker hint, or unsupported). It doesn't build a macOS imitation of it (§5).

## 1. The starting point: what dominates on the Mac, and why Linux changes it

Per market-data message, from the Mac's tail harness (steps 23–28, p50 / p99):

| Stage | Mac, measured | What sets it on the Mac | On a tuned Linux host |
|---|---|---|---|
| `recv` system call returning data | **3.0–3.2 µs / 17–20 µs** (loopback) | macOS kernel path; part of it probably loopback-specific | Busy polling or `io_uring`: §3.2 |
| Each idle spin (`recv` with nothing waiting) | 0.58 µs / 0.9–1.3 µs | A system call per poll | `io_uring` or bypass: a memory read, no system call |
| TLS decryption (our record layer, step 24) | 0.17–0.18 µs / 0.28–0.50 µs | AES-GCM itself (floor 0.13 µs) | Same; or the kernel (kTLS) / NIC does it: §3.3 |
| WebSocket decode | 0.05 µs | – | Same |
| Parse + apply + publish, warm | 0.36 µs (Binance) / 0.56 µs (Bybit) at p50 | Our code | Same code; less disturbed |
| The same, Binance full depth's largest message (25 KB, 1,177 levels) | ~37 µs (step 26; was ~60 µs) | Our code: ~75% parsing at ~1.1 ns per byte | Same: CPU work, no OS involvement |
| The same when messages are sparse | 1.5–2.5× the warm p50; 3–4× at p99 | Caches cooled by other programs, interrupts, the kernel's own work | An isolated core keeps our working set warm: §3.1 |
| **p99.9** of a warm loop | 6–14 µs, varying run to run | Interrupts hit ~1 message in 1,000 | `nohz_full` and interrupt affinity remove them: §3.1 |
| **A spinning thread's lost CPU** (`tails --only jitter`, step 28) | **~4,000 gaps over 1 µs per second, ~2,600–2,800 over 10 µs; 5.5–7.8% of CPU time lost; worst 48 ms** (under load) | Interrupts, timer ticks, other threads on the same core | Near zero on an isolated core: the day-one test (§6) |

**The pattern:** the Mac's p99 and p99.9 are set by the **operating system** (system
calls, scheduling, interrupts), not by our code. Those are exactly what Linux lets a
latency-critical process take control of.

## 2. Already in the code, ready for Linux

**What has been checked for Linux, precisely:**
- **Linted here for `x86_64-unknown-linux-gnu`** (clippy, no warnings): `ob-core`, `ob-parse`,
  `ob-simd`, `ob-sync`, `ob-agg`, `ob-os`.
- **Built and tested only by CI's Ubuntu job:** `ob-net`, `ob-feed`, `ob-app`, `ob-bench`.
  They depend on `ring` (TLS crypto), which needs a Linux C compiler to build, and this
  Mac has none.
- CI's Linux jobs now build with `target-cpu=x86-64-v2` (§3.5), so the SIMD parser is
  tested there. CI's macOS runners are Apple Silicon, so they test the scalar fallback.

**None of it has run on a Linux host under load.**

| Feature | How it's switched on | Linux mechanism | Expected impact |
|---|---|---|---|
| Pin each feed thread to a core | `ob live --pin 2,3` | `sched_setaffinity` (hard) | No migrations: caches and TLB stay with the thread |
| Real-time priority | `--rt` | `SCHED_FIFO` (needs `CAP_SYS_NICE`) | Never queued behind normal threads. **Only on an isolated core**: a spinning FIFO thread never yields |
| No page faults | `--lock-memory` | `mlockall(MCL_CURRENT \| MCL_FUTURE)` | No page fault on the hot path, ever |
| Kernel busy polling | `--busy-poll 50` | `SO_BUSY_POLL` | `recv` polls the NIC queue instead of waiting for an interrupt (§3.2) |
| Quick ACKs | `--quickack` | `TCP_QUICKACK`, re-armed after each read | The venue's sender never waits on our delayed ACKs (up to 40 ms) |
| Kernel receive timestamps | `--kernel-timestamps` | `SO_TIMESTAMPNS` via `recvmsg` | A "kernel wait" row: NIC → our read, the part user space can't see |
| SIMD level parser (step 25) | Always (build setting) | `rust/.cargo/config.toml`: `target-cpu=x86-64-v2` (SSSE3, SSE4.1); also passed by CI's Linux jobs and `scripts/pgo.sh` (§3.5) | Parsing −11% to −37%, as measured on the Mac (pure CPU work, no OS involvement) |
| Price-indexed full-depth book (step 26) | Always | – | Applying full-depth diffs −58%; the largest message 60 → 37 µs on the Mac (CPU work) |
| Our TLS record layer (step 24) | Always | – | TLS 0.46 → 0.18 µs per record; p99.9 15–18 µs → < 0.6 µs; zero allocations. It's also the hand-over kernel TLS needs (§3.3) |
| Host settings | `sudo scripts/tune-linux.sh [--dry-run] 2,3` | Prints or applies §3.1's settings | – |
| The tail harness, pinned | `BENCH_CPU=2 scripts/bench-tails.sh` | `taskset -c 2`; machine description from `/proc`, `/sys`, `/etc/os-release` (isolated CPUs, kernel command line and governor recorded) | Comparable runs; the file says how the host was tuned. **Not yet run on Linux:** syntax-checked under `dash` |
| Jitter test | `tails --only jitter` (or as part of the full run) | A thread spinning on the clock | Shows whether a core is really isolated (§3.1) |

## 3. To add or change for Linux

Ordered by expected impact on p99 and p99.9.

### 3.1 The host: give the feed threads cores of their own

| Change | What it removes | How | Expected impact |
|---|---|---|---|
| **CPU isolation** | Other tasks, the scheduler tick, RCU callbacks on the feed cores | Boot: `isolcpus=managed_irq,domain,2,3 nohz_full=2,3 rcu_nocbs=2,3` | **p99.9 stops being set by interrupts.** The Mac's 6–14 µs warm-loop p99.9 (step 23, finding 4) should fall towards the warm p99 (~0.4–5.5 µs depending on venue) |
| **Interrupts elsewhere** | Device interrupt handlers on the feed cores | `/proc/irq/*/smp_affinity`: NIC queues to a housekeeping core **on the same NUMA node** as the NIC | Part of the above |
| **Hyperthread sibling idle** | The other logical CPU sharing the core's caches and execution units | Isolate both siblings, or disable SMT for those cores | Removes a source of variance in every stage |
| **Fixed frequency, shallow C-states** | Clock ramp-up and deep-sleep exit (tens of µs) after idle | `performance` governor; `tuned-adm profile latency-performance`, or `/dev/cpu_dma_latency` = 0 | Removes most of the "sparse messages are slower" effect (Mac: 1.5–2.5× at p50) |
| **Cache allocation (Intel CAT)** | Other cores evicting our data from the shared L3 | `resctrl`: reserve L3 ways for the feed cores | Smaller, steadier cold-start cost per message |
| **Huge pages, THP `madvise`** | TLB misses on the book and buffers; compaction stalls | Explicit huge pages for the receive buffers and books; THP `madvise` | Small; mostly fewer outliers |
| **Memory on the NIC's NUMA node** | Remote-memory accesses (~1.5–2× the latency) | `numactl --cpunodebind --membind`, or start the process on that node | Matters on 2-socket servers; none on 1 socket |
| **`SO_INCOMING_CPU`** | Packets processed on one CPU and read on another | Check the socket's receive CPU matches the NIC queue's IRQ core | Keeps the kernel half of the receive path on the right core |

**Verify:** `taskset -c 2 target/release/tails --only jitter` on each feed core. The Mac
(not isolated) loses the CPU ~4,000 times a second, ~2,600–2,800 of them for over 10 µs,
up to 48 ms. A well-isolated core should show a handful of gaps over 1 µs per second and
a maximum of a few µs. Then rerun the full harness: the warm p99.9 should no longer flip
between two values.

**Not needed on a tuned host:** keeping the hot path warm with dummy work while idle.
On the Mac, the cooling between messages comes from other programs, interrupts, the
kernel's work in each idle `recv`, and deep sleep states. Isolation, `io_uring` and
shallow C-states remove all four, and our working set (code plus 16–64 KB of book) fits
in the core's private L1/L2. It would be worth revisiting only on a partly tuned host,
or for rarely run code paths (resync; in a trading system, the order path).

### 3.2 Getting data off the network: no system call per read

Today the feed thread spins on a non-blocking `read` (on the Mac: 3.0–3.2 µs when data is
there, 0.58 µs when not). Linux offers three steps, each a larger change:

| Option | What changes in the code | Expected impact |
|---|---|---|
| **Kernel busy polling** (`SO_BUSY_POLL`, already flagged) | Nothing (§2) | `recv` spins on the NIC queue: no interrupt and no wake-up on arrival. Typically ~1–2 µs per receive on modern NICs (estimate); verify with `--kernel-timestamps` |
| **`io_uring`** (multishot receive, registered buffers, `SQPOLL` or busy-polled completions) | `ob-net`: a new receive loop that reads completions from shared memory; ciphertext lands directly in our receive buffer | **No system call per read or per idle spin.** The spin loop becomes a memory read, which keeps our caches warm too. Removes most of the 3 µs and the 0.58 µs idle cost (estimate: receive cost ~0.5–1 µs) |
| **Kernel bypass** (AMD Solarflare Onload / ef_vi, or DPDK / AF_XDP with a user-space TCP stack) | `ob-net`'s transport replaced; with Onload, unchanged socket code (it intercepts the calls) | Wire to user space in ~1–2 µs total on a supported NIC (estimate); the kernel network stack disappears from the path |

The record layer (step 24) and WebSocket decoder already work on a caller-owned buffer,
so none of these changes touch TLS, parsing or the book.

**Recommended order:** busy polling first (a flag), then `io_uring`. Kernel bypass only
if measurements on the target host still show the kernel path dominating.

### 3.3 TLS in the kernel (kTLS), and on the NIC

| Change | What changes in the code | Expected impact |
|---|---|---|
| **kTLS receive** (`setsockopt(SOL_TLS, TLS_RX)`) | After the handshake, give the step 24 secrets to the kernel instead of our record layer; key updates via rustls's `KernelConnection::update_rx_secret` (already used) | On the CPU, roughly **neutral**: the kernel runs the same AES-GCM we do (0.13 µs floor, our layer 0.18 µs). Its value is enabling the next row, and fewer copies with bypass stacks |
| **NIC TLS offload** (e.g. NVIDIA ConnectX `mlx5` kTLS offload) | None beyond kTLS | Decryption on the NIC: the ~0.18 µs per record leaves the CPU entirely |

Our record layer stays as the fallback (TLS 1.2/1.3, all suites, tested against rustls).

### 3.4 Measurement on Linux

| Change | Why | Impact |
|---|---|---|
| **`SO_TIMESTAMPING` with hardware timestamps** (NIC + PTP) | True wire → book latency; today's start point is when `recv` returns | Measurement, not speed: shows the kernel's share |
| **Run the harness pinned to an isolated core** | `BENCH_CPU=2 scripts/bench-tails.sh` (step 28) | Comparable runs; load no longer moves results (on the Mac, load averages of 9–14 did) |
| **Wire scenario from a second machine** | The Mac's loopback probably inflates `recv` | Realistic NIC → book numbers |

### 3.5 The build

**A trap found in step 28: `RUSTFLAGS` replaces `.cargo/config.toml`.** Cargo uses an
environment `RUSTFLAGS` *instead of* the config file's `rustflags`. Any build that sets it
must repeat `-C target-cpu=x86-64-v2`, or the SIMD level parser is silently compiled out
(results stay correct, just ~11–37% slower to parse). CI's Linux jobs and `scripts/pgo.sh`
now do. `scripts/bench.sh` passes `target-cpu=native`, which includes it. A production
build pipeline must do the same.

| Change | Expected impact |
|---|---|
| `target-cpu` for the actual server (`x86-64-v3`, or `native` when build and run hosts match) | Small: step 20 found `native` within noise on the Mac; the SIMD kernel only needs v2 |
| **PGO trained on the Linux binary** (`scripts/pgo.sh`) | −10% to −16% per message on the Mac (step 20); the profile must come from the same binary |

## 4. What the result should look like

Received bytes → published book, per message. Mac columns are measured (steps 23–25);
Linux columns are **estimates** from §3, to be replaced by measurements:

| | Mac, busy p50 / p99 / p99.9 | Linux, §3.1 + busy polling | Linux, + `io_uring` |
|---|---|---|---|
| Read (socket + TLS + WS) | 3.5–3.9 / 23–31 / 57–118 µs | ~1.5–2.5 / ~3–5 / ~5–8 µs | ~0.7–1.2 / ~1.5–3 / ~3–5 µs |
| Parse + apply + publish (Binance / Bybit) | 0.36–0.56 µs p50; p99 0.4–5.5 µs | similar p50; p99.9 without interrupt spikes | same |
| **Total** | 3.9–4.7 / 24–35 / 63–268 µs | **~2–3 / ~4–8 / < 10 µs** | **~1–2 / ~2–6 / ~4–8 µs** |

These rows are Binance depth20 + bookTicker and Bybit 50 + 1, the default streams.

The large full-depth messages are CPU work and behave the same on both platforms. The
price-indexed book (step 26) took the largest (25 KB, 1,177 levels) from ~60 to ~37 µs
on the Mac; three-quarters of what's left is parsing (~1.1 ns per byte). They stay above
10 µs on any OS: a 25 KB message carries 1,177 price changes, and only SIMD parsing
across whole arrays of levels (not one level at a time) would cut that substantially.

## 5. The Mac as a test bench: where the design meets macOS

Where a Linux facility has no macOS equivalent, the code asks for it and reports what it
got. There's no macOS-specific workaround beyond what's needed to compile and test:

| Linux design | On macOS | Code |
|---|---|---|
| `SCHED_FIFO` | QoS class user-interactive, reported as `Hint` | `ob-os::set_realtime_priority` |
| `sched_setaffinity` | Affinity tag, reported as `Hint` (refused on Apple Silicon); must follow the QoS call or macOS returns EPERM | `ob-os::pin_current_thread`, `ob-app` |
| `mlockall`, `SO_BUSY_POLL`, `TCP_QUICKACK` | Reported as `Unsupported` | `ob-os` |
| `SO_TIMESTAMPNS` on TCP | macOS accepts the option but never delivers a timestamp: reported as `Unsupported` | `ob-os::enable_rx_timestamps` |
| SSSE3/SSE4.1 kernel (x86-64-v2) | Enabled by default on Intel Macs. Apple Silicon has no SSE: the kernel switches itself off and the scalar parser runs (an ARM NEON version would also serve ARM Linux servers such as AWS Graviton; not written, since it can't be tested here) | `ob-simd` |

How to read the Mac's numbers, given that:
- The **p99 and p99.9 of every OS-involving stage** (read, sparse traffic) are
  properties of a shared, un-isolated laptop, not of the design. §4's Linux columns are
  what the design targets.
- **Our own code's numbers** (parse, apply, publish, TLS, the aggregator's queries) carry
  over, measured as warm p50/p99 and interleaved A/B comparisons.
- **The measurement method** (open loop, pooled percentiles, load recorded, interleaved
  A/B for every change) is what lets both of those be separated on a noisy machine.

## 6. Day-one checklist on a Linux machine

1. `sudo scripts/tune-linux.sh --dry-run 2,3`, apply, reboot (isolation is a boot
   parameter).
2. `taskset -c 2 target/release/tails --only jitter`, and the same on core 3: a maximum
   gap of a few µs, or find what's still running there (`/proc/interrupts`, `ps -eLo psr`).
3. `cargo test --workspace --release`: the Linux-only paths (`ob-os`) run for real, and the
   SIMD parser is exercised (the harness header in step 4 must say `SIMD level parser: on`).
4. `BENCH_CPU=2 scripts/bench-tails.sh`: compare with `bench-results/tails-MacBookPro15,2_*`
   row by row. The file records the kernel command line, isolated CPUs and governor.
5. `ob live --pin 2,3 --rt --lock-memory --busy-poll 50 --quickack --kernel-timestamps --latency`:
   the startup report must say `Applied` for every option, and the "kernel wait" row
   appears.
6. Then, in order, measuring each before and after: `io_uring` (§3.2), kTLS (§3.3),
   PGO (§3.5).
