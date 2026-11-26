# Step 16: Record/replay and per-stage latency histograms

Until now, latency numbers came from micro-benchmarks: one function, warm caches, a
synthetic loop. This step measures **the real pipeline, per message, in production**,
and makes live data **repeatable**:

- `ob live --latency` timestamps each message's stages with the CPU's time-stamp counter
  and prints HDR histograms (p50 to max) at exit.
- `ob live --record FILE` saves every message with its receive times.
- `ob replay FILE` runs a recording through the same feed code as fast as possible,
  deterministically.

## What changed

| File | What |
|---|---|
| `crates/ob-feed/src/probe.rs` | **New.** The `Probe` trait; `NoProbe` (free) and `StageProbe` (rdtsc + HDR histograms). |
| `crates/ob-feed/src/feed.rs` | `Feed<V, P: Probe = NoProbe>`: stamps around parse+apply and publish. `Feed::new` is unchanged. |
| `crates/ob-feed/src/driver.rs` | Stamps around the `poll()` that returns a message; `run_with_tap` lets a recorder see every message. |
| `crates/ob-feed/src/record.rs` | **New.** The recording format (`RecordWriter`, `RecordReader`) and `ReplayFeed`. |
| `crates/ob-feed/tests/record.rs` | Format round trip, damage detection, replay = direct application, determinism. |
| `crates/ob-feed/benches/probe.rs` | What measuring costs per message. |
| `crates/ob-app/src/main.rs` | `live --latency`, `live --record FILE`, `replay FILE`. |
| `crates/ob-feed/Cargo.toml` | New dependencies: `quanta` (TSC clock), `hdrhistogram`. |

## 1. The stages

```text
 poll() returns a message         venue.on_text()            publish()
├── read: socket + TLS + WS ──┤├── handle: parse + apply ──┤├── publish: seqlock ──┤
├──────────────────────────────────── total ───────────────────────────────────────┤
                                         + exchange → us: venue timestamp vs our wall clock
```

- **read:** from just before the `poll()` call that delivers a message until it returns.
  That covers the `read` system call, TLS decryption and WebSocket decoding.
- **handle:** parsing the JSON and applying it to the book.
- **publish:** the seqlock write that makes it visible to readers.
- **exchange → us:** our wall clock minus the venue's event timestamp. This includes the
  network **and the skew between their clock and ours**, so it's an upper bound on
  transport delay, not a measurement of it. Negative values (their clock ahead of ours)
  are counted separately instead of being hidden.

## 2. Design decisions

### Zero cost when off: the probe is a type parameter

```rust
pub struct Feed<V: Venue, P: Probe = NoProbe> { … }

impl Probe for NoProbe {
    #[inline(always)] fn stamp(&self) -> u64 { 0 }
    #[inline(always)] fn applied(&mut self, _: u64, _: u64, _: u64, _: u64) {}
    …
}
```

- With `NoProbe`, every `stamp()` is the constant 0 and every record call is empty.
  After monomorphisation and inlining nothing is left: no branch, no flag check.
- `ob live` picks the type once at start-up (`if opts.latency { spawn_feed(…,
  StageProbe…) } else { spawn_feed(…, NoProbe) }`), so both versions are compiled and the
  choice costs nothing per message.
- The alternative, a runtime `if self.measuring { … }` on every message, costs a branch
  and keeps the timing code in the hot loop's instruction cache.
- `Feed::new` still builds `Feed<V, NoProbe>`, so no existing caller changed: all 14
  state-machine tests and 8 mock-exchange tests compile and pass untouched.

### The clock: rdtsc, through `quanta`

Measured on this machine (20 M calls each):

| Clock | Cost per read |
|---|---|
| `std::time::Instant::now()` | 39.4 ns |
| `SystemTime::now()` | 37.5 ns |
| `quanta::Clock::raw()` (= `rdtsc`) | **6.2 ns** |
| `core::arch::x86_64::_rdtsc()` | 6.25 ns, but requires `unsafe` (error E0133) |

- Three stamps per message cost 19 ns with rdtsc and 118 ns with `Instant`. Timing a
  1.5 µs operation with a 118 ns clock distorts it.
- **Why `quanta` rather than the raw intrinsic:** the intrinsic needs `unsafe`, which
  `ob-feed` forbids. `quanta` also checks that the CPU has an *invariant* TSC (one that
  ticks at a constant rate regardless of frequency scaling and sleep states) and falls
  back to the OS clock if not. It calibrates ticks to nanoseconds; I checked that a
  100 ms sleep measured 103.9 ms.
- **What `rdtsc` doesn't do:** it isn't a serialising instruction, so the CPU may
  execute it a few dozen cycles early or late relative to surrounding code. That is
  irrelevant for stages of hundreds of ns or more. For single-digit-ns precision you'd
  use `rdtscp` plus `lfence`.

### HDR histograms, not averages or sorted vectors

- An average hides exactly what matters in trading (the tail), and storing every sample
  to sort later allocates without bound.
- HdrHistogram records in constant time with **no allocation after creation**, and keeps
  3 significant digits from 1 ns to 100 s. Its memory is allocated once in
  `StageProbe::new`. Step 17 will prove there are no allocations per message.

### Recording: after the message, not before

`run_with_tap` calls the tap **after** `on_text` has applied and published the message.
Recording therefore never delays the message being recorded; it costs time only before
the next one. The two feeds share the file through a `Mutex<BufWriter>` with a 1 MiB
buffer. That's acceptable for a capture tool that is off by default, and it never touches
the default path.

### File format

```text
header:  b"OBREC" 0 0 1                                        (version 1)
record:  wall_ns u64 | mono_ns u64 | venue u8 | kind u8 | 0 0 | len u32 | bytes
```

- **Two timestamps.** The wall time gives the exchange → us delay on replay. The monotonic
  time gives correct intervals and the feed's `now`.
- **`kind` 1 marks a (re)connection,** so a replay reproduces resubscriptions and resyncs
  exactly as they happened.
- **Damage is detected, not misread:**
  - a wrong magic number, or another format version, is refused;
  - a cut header and a cut payload both give `UnexpectedEof`;
  - a length over 16 MiB is refused *before* allocating, so a corrupt length can't
    request gigabytes.
- **`read_frame()` reuses one buffer.** It can't be an `Iterator` because each frame
  borrows that buffer, which the next call overwrites (a "lending iterator").

## 3. Measured

### Live: 60 s, both venues (`ob live --secs 60 --latency --record …`)

```text
binance                count     p50     p90      p99    p99.9      max   (ns)
read (socket+TLS+WS)     576   44479   71871   117951   263679   263679
handle (parse+apply)     575    3425    5851    24463    63391    63391
publish (seqlock)        575     214     329      558    15839    15839
total                    575   49311   78527   135423   270335   270335
exchange → us            575   44991   46367    50367    64479    64479   (µs, incl. clock skew)

bybit
read (socket+TLS+WS)    2490   48543   71935   116543  1267711  5492735
handle (parse+apply)    2487    2927    5471    21951    31695    53983
publish (seqlock)       2487     323     442      662    18911    19247
total                   2487   52479   79039   124415  1297407  5496831
exchange → us           2487   42303   42687    46335   334335   354303   (µs, incl. clock skew)
```

### Replay of the same 60 s (`ob replay …`, run twice)

```text
replayed 3068 records (59.2 s of market data) in 5.2 ms: 594783 records/s, 11484x real time
binance  handle p50 1517  p99 3365   publish p50 46   (same 575 applied as live)
bybit    handle p50  575  p99 5819   publish p50 86   (same 2487 applied as live)
final books identical across both replays
```

### What the numbers say

1. **Reading the socket is about 90% of our in-process time.** It takes ~45 µs of the
   ~50 µs total. Parsing and applying take 3 µs; publishing takes 0.2–0.3 µs.
   - That 45 µs was surprising for one non-blocking `read` and decryption, so I measured
     the successful `poll()` alone in step 12's local harness. It takes **4.5 µs when
     messages arrive every 200 µs, and 19.6 µs when they arrive every 20 ms**.
   - It's the same code: between sparse messages the caches, branch predictors, TLS key
     schedule and kernel socket paths go cold, so the first touch is slow. Live messages
     are sparse (20–100 ms apart) and larger.
   - This accounts for much of step 12's open question (7× worse latency when traffic
     is sparse).
   - Likely contributors I haven't proven: macOS's network stack doing ACK and
     window-update work inside the `read` call, and CPU power states. Step 21 targets
     exactly this: pinning, busy-polling inside the kernel, and kernel TLS on Linux.
2. **Cold vs warm is 2–5× for our own code.**
   - Handle: 3.4 µs live vs 1.5 µs replayed for Binance, and 2.9 vs 0.58 µs for Bybit.
   - Publish: 214–323 ns live vs 46–86 ns replayed. Live, the reader threads have pulled
     the seqlock's cache lines to their own cores, so each publish must take them back.
   - Micro-benchmarks show the warm case; **production runs cold.** Keeping the hot path
     warm between messages (cache warming, spinning on the data) is a known HFT
     technique; it's noted as a step 20 candidate.
3. **The network dwarfs all of it.** The exchange → us delay is ~42–45 ms (including
   clock skew), 1,000× our 50 µs. That's the colocation argument (step 22 / LATENCY.md),
   now measured.
4. **Tails:** Bybit's read p99.9 of 1.3 ms and max of 5.5 ms are the OS descheduling the
   thread on a laptop with no CPU isolation. The handle stage's tail (24–63 µs) is the
   same effect plus page faults on first touch.
5. **Replay is faithful and deterministic.** It applies the same number of messages as
   live, two replays give identical final books, and it processes 60 s of market data in
   5.2 ms.

### What measuring costs (criterion, 59 Binance messages per iteration)

| Feed built with | Per message | Overhead |
|---|---|---|
| `NoProbe` | 1.613 µs | none |
| `StageProbe`, replay wall clock | 1.690 µs | +77 ns (3 TSC stamps, 4 histogram records) |
| `StageProbe`, system wall clock (live) | 1.724 µs | +111 ns (+34 ns `SystemTime::now`) |

About 7% of the handle stage. That's why `--latency` is opt-in, and why the off state is
a type rather than a flag.

## 4. Tests

| Test | Proves |
|---|---|
| `the_format_round_trips` | 122 records (2 connects + 120 messages) read back with the exact bytes, times, venues and kinds. |
| `replay_rebuilds_exactly_the_directly_built_books_every_time` | Replayed books equal books built by feeding the lines directly; two replays give identical books, metadata and counters; every applied message was timed. |
| `damage_is_detected_not_misread` | Wrong magic, a cut payload, a cut header and an absurd length all give errors, never a silently short or wrong read. |
| `probe::tests` (2) | Stages and the total are recorded; a 5 ms exchange delay measures as 5 ms; an exchange clock ahead of ours is counted, not recorded. |

**Mutation testing found a gap.** Making a cut *header* look like a clean end of file
passed all tests: my truncation test cut the last record's *payload*, which takes a
different code path. I added the header case, and the mutation now fails the test.

A wrong expectation in my own test: the synthetic wall clock started 17 s before the
Bybit fixture's exchange timestamps, so every delay was negative and was correctly
counted instead of recorded. The code was right, so I fixed the test data.

## 5. What this step deliberately doesn't do

- **Kernel receive timestamps** (`SO_TIMESTAMPING`), which would split "read" into
  "wire → socket buffer" and "socket buffer → us". That's Linux only, in step 21.
- **Replay at the original pace.** Replay runs as fast as possible, which suits
  measuring and regression testing. Paced replay would add sleeps between records.
- **A committed recording.** One minute is 1.5 MB. Tests build recordings from the
  existing fixtures; make your own with `--record`.
- **Aggregator query timing.** Queries run on the reader side; step 10's benchmarks cover
  them.

## 6. Java counterpart

The skeleton measures nothing. A Java version would use:

- **`System.nanoTime()`** (~20–40 ns, similar to `Instant`). Java has no supported rdtsc;
  libraries reach it through JNI, or more recently the Foreign Function & Memory API,
  each with call overhead.
- **HdrHistogram itself:** it was written in Java first (by Gil Tene, Azul) and is the
  standard there. Our crate is a port of it.
- **Chronicle Queue** for record/replay: memory-mapped binary files, the same idea as
  ours. LMAX-style event sourcing is the same idea at the architecture level: replay the
  input log to rebuild state deterministically.
- **JMH** for the "what does measuring cost" question. It also exposes JIT effects: a
  Java replay's first seconds would be slower (interpreted, then compiled), which is
  exactly the cold-vs-warm effect above, made worse.

```java
long t0 = System.nanoTime();
handler.onMessage(msg);
histogram.recordValue(System.nanoTime() - t0);   // org.HdrHistogram.Histogram
```

## How to verify it yourself

```bash
cd rust
cargo run --release -p ob-app -- live --secs 60 --latency --record /tmp/btc.obrec
cargo run --release -p ob-app -- replay /tmp/btc.obrec          # run it twice: same final books
cargo test -p ob-feed --release --test record
cargo bench -p ob-feed --bench probe
```

Things to try (I ran all three):
- In `record.rs`'s `read_frame`, change `0 => return Err(io::ErrorKind::UnexpectedEof.into())`
  to `0 => return Ok(None)`. `damage_is_detected_not_misread` fails at the header-cut
  case, the one mutation testing showed was missing.
- Compare the `live --latency` histograms with `replay` on the same recording. Handle
  and publish are 2–5× slower live: the cold-cache effect.
- In `ob-net/tests/latency.rs`, time just the successful `conn.poll()` call (an
  `Instant` before and after). It takes ~4.5 µs at a message every 200 µs and ~20 µs at
  one every 20 ms.

## Likely interview questions

1. **Where does your latency go?**
   Measured, per message: the network ~43 ms (including clock skew); in-process ~50 µs,
   of which ~45 µs is the socket read + TLS + WebSocket, ~3 µs parse + apply, and
   ~0.3 µs publish. So: colocate first, then fix the receive path (kernel bypass or
   busy-poll, kernel TLS), and only then the parser.
2. **Why rdtsc instead of `clock_gettime` / `Instant`?**
   6 ns vs 39 ns per read here. With three stamps per message, the clock would otherwise
   distort a 1.5 µs stage by 8%. Caveats: you need an invariant TSC, and rdtsc isn't
   serialising (use `rdtscp` and `lfence` for very short intervals).
3. **Why histograms instead of averages?**
   Trading cares about the tail: p99, p99.9, max. HdrHistogram records in constant time
   without allocating and keeps 3 significant digits across 11 orders of magnitude.
4. **How do you make instrumentation free when it's off?**
   Make it a type parameter rather than a flag. `NoProbe`'s methods are empty and
   `#[inline(always)]`, so they compile to nothing. Measured: 1.613 µs vs 1.690 µs
   when on.
5. **Why do your micro-benchmarks disagree with production?**
   Benchmarks run warm. Live messages arrive 20–100 ms apart, by which time caches,
   predictors and kernel paths have gone cold. Measured: parse+apply is 2–5× slower live
   than in replay, and a socket read is 4× slower at sparse rates. Always measure the
   production path, not only the benchmark.
6. **What is replay for?**
   Comparing builds on identical input, regression-testing performance, reproducing
   production bugs byte for byte, and deterministic tests. Ours is exact: same messages
   applied as live, and identical final books on every replay.
7. **How can you trust an exchange → us latency number?**
   Only as an upper bound. It includes the skew between the venue's clock and ours. On a
   laptop with ordinary NTP that's milliseconds; in colocation you'd use PTP plus
   hardware timestamps. Negative values are counted, not dropped, because they're the
   evidence of skew.

---

## Correction (found in step 18): "exchange → us" was mostly clock skew

Step 18 repeated the live run about an hour later and got an exchange → us p50 of
**7.4 ms** for Binance and **1.0 ms** for Bybit, against ~43–45 ms here, from the same
place. Bybit also had 60 *negative* delays. Checking the clocks directly:

- `sntp time.apple.com`: this laptop's clock was **113 ms off**, more than the delays
  being measured. The clock is periodically corrected, so the skew drifts between runs.
- Round-trip times, which need no clock agreement (TCP connect, median of 5):
  **Binance 234–244 ms**, **Bybit 7–10 ms**. Bybit's WebSocket ends at a nearby edge
  server; its matching engine is further away behind it.
- They're consistent: 7.4 ms measured + 113 ms of skew ≈ 120 ms ≈ half of Binance's
  round trip.

So the statement above, "the exchange → us delay is ~42–45 ms … 1,000× our 50 µs",
measured skew, not the network. What stands is the conclusion: the network (a 234 ms
round trip to Binance from here) is thousands of times our in-process ~50 µs. The
honest measures of distance are a **round trip** (as here), or one-way delays with
**synchronised clocks** (PTP plus hardware timestamps in colocation). Comparing two
*exchange* timestamps with each other is also skew-free; step 18 does that.
