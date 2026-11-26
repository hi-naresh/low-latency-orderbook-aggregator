# Step 21: OS tuning (Linux low-latency features, and what macOS allows)

Steps 12–20 tuned *our* code. What's left on this machine is the operating system:
- the scheduler that wakes the feed thread;
- the kernel path data takes from the network card to our `read`;
- page faults, interrupts and CPU power states.

This step adds the standard low-latency controls (thread pinning, real-time priority,
memory locking, kernel busy polling, quick ACKs, kernel receive timestamps) behind
opt-in flags. It also documents host tuning and colocation in `docs/TUNING.md`.

**The constraint:** this is a Mac, and most of these controls are Linux features. So:
- every control **reports what the OS actually did** (applied, hint only, or
  unsupported), never failing silently;
- macOS behaviour is **tested and measured here**;
- Linux code is **compile- and lint-checked here** (with the Linux target installed, with
  your approval), and **run by CI's Ubuntu jobs**.

## What changed

| File | What |
|---|---|
| `crates/ob-os/` | **New crate:** the one place that calls the OS directly. 8 functions, each `unsafe` block with a `SAFETY:` comment, a safe API returning [`Support`]. 4 tests. |
| `crates/ob-net/src/conn.rs` | `SocketTuning` in `ConnConfig` (busy poll, quick ACK, kernel timestamps); `recvmsg` reads when stamping; `last_kernel_wait_ns`; refused options reported, never fatal. |
| `crates/ob-feed/src/{probe,driver}.rs` | A "kernel wait (Linux)" row in the latency report. |
| `crates/ob-app/src/main.rs` | `--pin A,B --rt --lock-memory --busy-poll US --quickack --kernel-timestamps`, with a start-up report. |
| `crates/ob-net/tests/latency.rs` | `qos_vs_default_wakeup_latency`, interleaved. |
| `docs/TUNING.md`, `scripts/tune-linux.sh` | **New.** Host tuning: what each setting removes; a script that applies them or prints them (`--dry-run`). |
| `.github/workflows/ci.yml` | Clippy now runs on Ubuntu **and** macOS (`ob-os` has code for each). |

## 1. `ob-os`: one small, audited place for OS calls

| Function | Linux | macOS |
|---|---|---|
| `pin_current_thread(cpu)` | `sched_setaffinity`: **hard pinning** | affinity tag: **hint** (Intel only) |
| `set_realtime_priority(p)` | `SCHED_FIFO` (needs `CAP_SYS_NICE`) | QoS class user-interactive: **hint** |
| `lock_memory()` | `mlockall(MCL_CURRENT \| MCL_FUTURE)` | unsupported |
| `set_busy_poll(sock, µs)` | `SO_BUSY_POLL` | unsupported |
| `set_quickack(sock)` | `TCP_QUICKACK` | unsupported |
| `enable_rx_timestamps` + `recv_timestamped` | `SO_TIMESTAMPNS` + `recvmsg` control messages | **unsupported for TCP** (see below) |

Design points:
- **[`Support`] = `Applied | Hint(..) | Unsupported(..)`.** `ob live` prints exactly what
  it got, so nobody believes a laptop run had real-time scheduling when it didn't.
- **The other crates stay `unsafe`-free.** `ob-net` keeps `#![forbid(unsafe_code)]`: it
  calls `ob_os::recv_timestamped` through a tiny `Read` adapter, so rustls's `read_tls`
  works unchanged.
- **Zero cost when off.** Without `--kernel-timestamps` the read path is the same
  `read_tls(&mut sock)` as before; the choice is one predictable branch per read.

### What the tests found

1. **macOS accepts `SO_TIMESTAMP` on a TCP socket but never delivers a timestamp.** The
   receive test got data with no control message. BSD kernels timestamp datagrams only,
   so on macOS the call now reports `Unsupported` instead of pretending.
2. **On macOS, QoS after the affinity tag fails with EPERM.** `ob live --pin 1,2 --rt`
   got "Operation not permitted" for the QoS request, though the unit test (a different
   order) had worked. A dedicated test shows the rule: once `thread_policy_set` (the
   affinity tag) has touched a thread, it is no longer QoS-managed. `ob live` now sets
   priority first, and the function documents it.
3. **The deprecated `mach_thread_self()` was replaced by
   `pthread_mach_thread_np(pthread_self())`.** Beyond the deprecation, the old call
   creates a new port reference on every call, which leaks unless explicitly freed; the
   new one borrows.
4. **The Linux lint pass caught three things the macOS build couldn't:**
   - a `SAFETY:` comment in the wrong place, on Linux-only code;
   - two conversions that are no-ops on Linux but needed on macOS (`msg_controllen` is
     `usize` on one and `socklen_t` on the other);
   - a helper unused on macOS.

   That's why the Linux target was worth installing. Crates whose build scripts compile
   C (`ring`) can't be cross-checked without a Linux C toolchain; none of the
   Linux-specific code lives in them.

### A robustness fix found while writing it

On Linux, *raising* `SO_BUSY_POLL` above `net.core.busy_read` without `CAP_NET_ADMIN`
fails with EPERM. My first version propagated that error from `connect` with `?`, so an
unprivileged `--busy-poll` would have made every connection fail and the feed reconnect
forever. Refused options are now **reported, never fatal**: the connection simply works
without them.

## 2. Measured on macOS: the QoS class cuts the tail

Step 12's local latency harness (a TLS WebSocket server in the same process; the client
measures `decoded − sent`) with 150 messages 20 ms apart, the sparse case step 12 found
slow. Default versus the user-interactive QoS class, **interleaved over 2 rounds**
(step 20's lesson):

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| spin, default (r1 / r2) | 77.2 / 75.2 µs | 108.7 / 102.2 | 213.1 / 163.4 | 882.8 / 166.6 |
| spin, **QoS** (r1 / r2) | 69.9 / 74.5 µs | 96.3 / 98.3 | **129.7 / 126.2** | **148.2 / 134.3** |
| block, default (r1 / r2) | 95.6 / 95.5 µs | 130.0 / 125.8 | 161.7 / 537.3 | 759.9 / 1,512.7 |
| block, **QoS** (r1 / r2) | 96.7 / 91.9 µs | 123.4 / 121.6 | **152.9 / 142.0** | **180.5 / 176.6** |

- **The median barely moves.** Wake-up from a sparse stream is still ~70–95 µs, set by
  the kernel and power states, which macOS doesn't let a user process control.
- **The tail shrinks consistently:** spin p99 −20 to −40%, block maximum 4–8× lower. The
  scheduler stops parking the thread on a slow core or behind other work. For trading
  the tail is what matters, so `--rt` is worth using even on a Mac.
- What Linux adds (isolated cores, `SCHED_FIFO`, fixed frequency, busy polling) attacks
  the median itself. That's documented in `docs/TUNING.md` and can't be measured here.

## 3. Kernel receive timestamps (Linux)

`ob live --latency --kernel-timestamps` reads with `recvmsg` and collects the kernel's
receive timestamp (`SO_TIMESTAMPNS`). The connection records
`now − kernel timestamp` per read, and the latency report gains a row:

```text
kernel wait (Linux)    …   ← data in the socket buffer → our recvmsg returned
read (socket+TLS+WS)   …
```

That splits step 16's dominant "read" stage into the kernel's part and ours. It needs no
clock synchronisation: both timestamps come from the same machine's clock. It's Linux-only
because of finding 1. Its test (`kernel_receive_timestamps_arrive_with_the_data`) checks
the timestamp falls between send and read, and runs in CI's Ubuntu job.

## 4. `docs/TUNING.md` and `scripts/tune-linux.sh`

- **Location first.** Measured from here: a 234 ms round trip to Binance against ~50 µs
  in-process. Colocating in each venue's AWS region is worth 1,000× any setting.
- **The host:** CPU isolation (`isolcpus`, `nohz_full`, `rcu_nocbs`), pinning,
  `SCHED_FIFO` (only on isolated cores), `mlockall`, the `performance` governor and
  shallow C-states, NIC interrupt affinity, kernel busy polling, quick ACKs, THP on
  `madvise`. Each row says what it removes.
- **The script** prints every change it makes, offers `--dry-run`, refuses to run on
  anything but Linux (checked here: "Linux only"), and prints the boot-time settings
  rather than applying them.
- **Beyond:** kernel TLS (removes step 17's rustls allocation and both copies), io_uring,
  kernel bypass (Onload/DPDK), PTP plus hardware timestamps, SIMD JSON (step 20).

## 5. What this step deliberately doesn't do

- **io_uring and kernel TLS.** Each is a different I/O path that can only be tested on
  Linux. They're documented with what they'd remove and how.
- **Run anything as root, or change this Mac's settings.** The script is Linux-only and
  opt-in.
- **Claim Linux numbers.** None were measured; the note says what each setting removes,
  not by how much.

## 6. Java counterpart

- **Affinity:** Java has no API; OpenHFT's Java-Thread-Affinity library (JNI) or
  `taskset` / `numactl` around the JVM.
- **Priority:** `Thread.setPriority` maps to almost nothing on Linux; `chrt -f` on the
  JVM, or JNI.
- **Memory:** `-XX:+AlwaysPreTouch` and `-XX:+UseLargePages` (pre-faults the heap), plus
  `mlockall` via JNI. The GC can still move objects, which is why low-latency Java also
  avoids allocation (step 17).
- **Socket options:** `SO_BUSY_POLL` and `TCP_QUICKACK` aren't in `java.net`; Netty's
  native epoll transport exposes them (`EpollChannelOption`). There are no kernel
  timestamps in the JDK.
- **Kernel bypass:** Solarflare Onload works under an unmodified JVM (it intercepts the
  socket calls), one of the few places Java gets the same benefit for free.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-os -- --nocapture                    # what this OS supports, and the QoS-order rule
cargo test -p ob-net --release --test latency qos -- --ignored --nocapture   # the table above (~30 s)
cargo run --release -p ob-app -- live --secs 5 --pin 1,2 --rt --lock-memory --busy-poll 50 --quickack --kernel-timestamps
cargo clippy -p ob-os --all-targets --target x86_64-unknown-linux-gnu -- -D warnings   # Linux lint, here
scripts/tune-linux.sh --dry-run 2,3                   # on Linux: prints every change
```

Things to try (the first two are what happened during this step; the third I haven't run):
- Swap the order back in `spawn_feed` (pin before priority) and run with `--pin 1,2 --rt`
  on a Mac: "real-time priority: Err(… Operation not permitted)".
- In `ob-os`, make the macOS `enable_rx_timestamps` set `SO_TIMESTAMP` and return
  `Applied` (the first version). `kernel_receive_timestamps_arrive_with_the_data` fails:
  no timestamp arrives.
- Run the QoS latency test while a parallel build loads the machine, and see whether the
  default variant's tail grows more than the QoS variant's (as the scheduler-priority
  explanation predicts).

## Likely interview questions

1. **How would you deploy this for the lowest latency?**
   Colocate in each venue's AWS region first (234 ms → ~1 ms). Then isolated cores
   (`isolcpus`, `nohz_full`, `rcu_nocbs`), each feed thread pinned and `SCHED_FIFO`,
   memory locked, fixed CPU frequency with shallow C-states, NIC interrupts elsewhere,
   kernel busy polling. Measure with kernel receive timestamps, which split "read" into
   kernel time and ours.
2. **Why is `SCHED_FIFO` on a spinning thread dangerous?**
   A FIFO thread runs until it blocks or yields, and a spinning one never does. On a
   shared core it can starve everything else, including kernel threads (RCU stalls).
   Only use it on an isolated core.
3. **What did you measure on macOS, given it has none of this?**
   The user-interactive QoS class: no median change, but p99 −20 to −40% and maximum
   wake-up latency 4–8× lower, interleaved over two rounds. Plus two behaviours found by
   tests: no TCP receive timestamps on macOS, and QoS must be set before the affinity tag.
4. **How do you keep OS-specific `unsafe` code honest?**
   One small crate, a safe API, a `SAFETY:` comment on every block (lint-enforced), and a
   result type that says what the OS really did. Lint it for every target it compiles
   for: the Linux cross-lint found three issues the macOS build couldn't.
5. **What are kernel receive timestamps for?**
   They separate time spent in the kernel (interrupt, protocol processing, socket queue,
   wake-up) from time in our code, without clock synchronisation, since it's the same
   machine's clock. That tells you whether to fix the host (busy polling, isolation,
   bypass) or the code.
6. **What would you do after all this?**
   Kernel TLS (removes the decrypt copy and rustls's allocation), io_uring or kernel
   bypass for the receive path, hardware timestamps with PTP, and SIMD parsing. These are
   in `docs/TUNING.md`, with what each removes.
