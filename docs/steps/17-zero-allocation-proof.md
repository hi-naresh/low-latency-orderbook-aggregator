# Step 17: Zero-allocation proof

Every step so far has said "no allocation on the hot path". This step **proves** it with
a test that fails if anyone ever adds one, and **finds the one allocation that remains**
on the real network path: it's inside rustls, at a known line, and costs about 60–95 ns
per message.

## What changed

| File | What |
|---|---|
| `crates/ob-feed/tests/zero_alloc.rs` | **New.** A counting global allocator and 6 tests covering every hot path, the unhappy paths, and the whole feed thread over real TLS. |

No production code changed: the tests found nothing to fix in our code.

## 1. Why allocation matters on a hot path

- **Time.** An allocation calls into the allocator: tens of ns on a good day, sometimes
  a lock or a system call for fresh memory (milliseconds when the OS has to find pages).
  Measured on this Mac: allocating, copying and freeing a 300–1200-byte buffer costs
  **60–95 ns**. Copying into a reused buffer costs **7–13 ns**.
- **Unpredictable latency.** The cost depends on heap state (fragmentation, other
  threads), which shows up in the tail.
- **Cache misses.** Fresh memory is scattered across the heap; reused buffers stay hot.
- **In Java,** every allocation is also future garbage-collector work. That's why
  low-latency Java goes "garbage-free" (section 6).

## 2. How the proof works

```rust
thread_local! {
    // `const` initialisation: no lazy set-up, so reading it inside the allocator can't
    // itself allocate (which would recurse forever).
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged …
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }
    // alloc_zeroed and realloc are counted too (growing a Vec is an allocation);
    // dealloc is not.
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;
```

The design decisions:

- **The counter is per thread.** Tests run in parallel, and a global counter would pick
  up other tests' allocations, making "0" flaky or meaningless. A test can therefore
  assert on *its* thread only. The network test counts the feed thread and ignores the
  server thread in the same process.
- **The thread-local is `const`-initialised.** A lazily initialised thread-local could
  allocate on first access, from inside the allocator, and recurse. `try_with` handles
  thread teardown, when the slot may already be gone.
- **Warm-up first, then count.** The first message may size buffers. The claim is
  "nothing per message in steady state", which is what matters for latency.
- **`unsafe` lives only in the test.** Implementing `GlobalAlloc` requires it, and it
  carries a `SAFETY:` comment (the workspace lint enforces this). The library crates
  still forbid `unsafe`.
- **Each integration test file is its own binary,** so this global allocator affects
  only this file's tests.

### Controls: proving the counter isn't broken

A counter that always returned 0 would make every test pass. So
`the_counter_counts` checks:
- `Vec::with_capacity(10)` counts exactly 1;
- growing a `Vec` past its capacity counts (a realloc);
- `clear()` counts 0;
- **another thread's allocations aren't counted.** Spawning a thread that allocates
  1 MiB a hundred times costs this thread exactly what spawning an idle thread does.

  My first version asserted "spawning costs at most 3 allocations". That was a guess,
  and it failed under parallel load, where a spawn cost 5. The comparison with an idle
  spawn is exact and doesn't depend on the platform's thread library.

## 3. Results

| Path | Messages or calls | Allocations |
|---|---|---|
| Binance parse → apply → publish | 59 recorded messages | **0** |
| Bybit parse → apply → publish | 59 recorded messages | **0** |
| Both venues with the latency probe on (rdtsc, histograms, wall clock) | 59 + 59 | **0** |
| Error paths: gap → resync, garbage → resync, fresh snapshot, stale → reconnect → backoff → connect | one scenario, more than 8 commands | **0** |
| Aggregator: refresh, best bid and ask, two volume queries, buy and sell execution, arbitrage scan | 1,000 rounds | **0** |
| Binary log push | 10,000 records | **0** |
| Reading a recording | 120 frames | **0** |
| **The whole feed thread over real TLS** (socket, TLS, WebSocket, parse, apply, publish, idle polls) | 55 messages | **exactly 1 per message** |

The error paths matter as much as the happy path. A feed that only allocates while
recovering from a gap is allocating exactly when the market is busiest.

## 4. The one allocation left: inside rustls

The whole-thread test showed exactly one allocation per message. To find it rather than
guess, I made the allocator temporarily capture a backtrace at the first allocation
during a message. A guard flag stops the backtrace's own allocations from recursing. It
points into rustls 0.23.46:

```text
rustls/src/msgs/base.rs:43          Self::Borrowed(bytes) => bytes.to_vec(),
rustls/src/common_state.rs:557      self.received_plaintext.append(bytes.into_vec());
rustls/src/client/tls13.rs:1610     MessagePayload::ApplicationData(payload) => …take_received_plaintext(payload)
rustls/src/conn.rs:941              process_new_packets
```

What happens per TLS record:
1. rustls decrypts **in place** in its receive buffer (good);
2. it then **copies the plaintext into a new `Vec`** and queues it (the allocation);
3. our `reader().read()` copies it again into our buffer.

**Does rustls's other ("unbuffered") API avoid it?** Step 12 said no, and I re-checked
against the source with the exact line in hand: `ReadTraffic::next_record()` pops from
the same `received_plaintext` queue of copied `Vec`s. So both APIs allocate once per
record in this version.

**The options, and the decision:**

| Option | Effect | Decision |
|---|---|---|
| Keep rustls; document | about 60–95 ns per message, about 0.15% of the ~45 µs socket-read stage (step 16) | **Now** |
| Kernel TLS (Linux `kTLS`): the kernel decrypts, and `read()` returns plaintext into our buffer | removes the allocation *and* both copies | **Step 21** (Linux feature) |
| A faster allocator (e.g. mimalloc) | makes the allocation cheaper, doesn't remove it | Possible in step 20, if measured |
| OpenSSL via FFI | `SSL_read` writes into our buffer | Rejected: `unsafe` FFI and a C dependency for ~60 ns |
| Patch rustls to hand out borrowed plaintext | the proper fix, upstream | Out of scope; noted |

**The test pins the current behaviour exactly** (`== 1` per message, not "≤ 1"). If a
rustls upgrade removes the allocation, the test fails, and we update it and celebrate.
If our code adds one, the count becomes 2 and the test fails.

## 5. Mutation check

- I added `black_box(text.to_vec())` to `Feed::on_text` (one allocation per message).
  Three tests fail: parse/apply/publish, error paths, and the TLS whole-thread test. The
  aggregator, log and replay tests correctly pass, because they don't go through
  `on_text`.
- The backtrace diagnostic above was a temporary edit and is not in the repository.

## 6. Java counterpart

Java can't stop the garbage collector, so low-latency Java aims for "garbage-free" or
"zero-GC" steady state, and proves it with tools:

- **JMH with `-prof gc`** reports `gc.alloc.rate.norm`, bytes allocated per operation.
  The usual assertion is `0 B/op`. That's the direct equivalent of our test.
- **`-XX:+UseEpsilonGC`** is a no-op collector: the JVM crashes when the heap fills.
  Running a soak test with it proves the path doesn't allocate.
- **Allocation profilers** (async-profiler `-e alloc`, JFR `ObjectAllocationSample`) show
  where allocations come from, like our backtrace trick.
- The skeleton allocates on every message, many times over:
  - `String` for the message;
  - JSON tree nodes;
  - a `BigDecimal` per price and quantity;
  - `TreeMap` entries;
  - SLF4J's argument arrays.

```java
@Benchmark
public void onMessage(Blackhole bh) { feed.onText(nextMessage()); }
// run: java -jar benchmarks.jar -prof gc
//   onMessage:·gc.alloc.rate.norm   0.000 B/op   ← the garbage-free target
```

## How to verify it yourself

```bash
cd rust
cargo test -p ob-feed --release --test zero_alloc -- --nocapture   # prints every count
```

Things to try (I ran all three):
- In `crates/ob-feed/src/feed.rs`, add `std::hint::black_box(text.to_vec());` at the
  top of `on_text`. Three tests fail with counts of 58–59.
- In the TLS test, change `assert!(per_msg.iter().all(|&n| n == 1))` to `n == 0`. It
  fails and prints the per-message counts: all 1s, rustls's `to_vec`.
- Make `bump()` do nothing (disable counting). The four "allocates nothing" tests
  **still pass**: that's the vacuous-proof trap. Only `the_counter_counts` fails, along
  with the TLS test, because it pins exactly 1. This is why the control test exists.

## Likely interview questions

1. **How do you prove your hot path doesn't allocate?**
   A counting global allocator with a per-thread counter. Tests warm up, then assert
   the count per message is exactly 0, including on error and resync paths, and with
   latency measurement on. Control tests prove the counter works. In CI, any future
   allocation fails the build.
2. **Why a thread-local counter rather than a global atomic?**
   Tests run in parallel; a global counter would count other tests' allocations. And
   the network test needs to count just the feed thread, not the server thread in the
   same process.
3. **Your hot path allocates once per message over TLS. Why, and does it matter?**
   It's rustls copying each decrypted record into a new `Vec`; I found the exact line
   with an allocation backtrace. It costs ~60–95 ns, about 0.15% of the socket-read
   stage. The real fix is kernel TLS, which removes the allocation and both copies. A
   test pins the count at exactly 1, so it can't silently grow.
4. **Why does it matter that the error paths don't allocate?**
   Gaps and resyncs happen when the market is busiest, which is exactly when latency
   matters most. Recovery shouldn't be the moment the heap gets involved.
5. **What's the Java equivalent?**
   JMH's GC profiler (`gc.alloc.rate.norm == 0 B/op`), Epsilon GC soak tests, and
   allocation profilers. The goal is the same: a garbage-free steady state.
