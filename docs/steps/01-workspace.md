# Step 1: Workspace, build profiles, lints, CI

Design reference: Phase 0 of the plan. No order book logic yet. This step sets up the
ground rules every later step is built and measured under.

## What changed

| File | Purpose |
|---|---|
| `rust/Cargo.toml` | Workspace root: crate list, shared metadata, **lints**, **build profiles** |
| `rust/rust-toolchain.toml` | Pins the compiler to rustc 1.100.0 |
| `rust/rustfmt.toml` | Formatting rules (edition 2024, 100 columns) |
| `rust/.gitignore` | Ignores `target/` |
| `rust/scripts/bench.sh` | Runs benchmarks with `-C target-cpu=native` |
| `rust/crates/ob-core/` | First crate: an empty `no_std` shell, filled in by steps 2–4 |
| `.github/workflows/ci.yml` | CI: fmt, clippy, tests (Linux + macOS, debug + release), docs |

The Java skeleton is untouched; the Rust solution lives beside it in `rust/`.

### 1. Workspace layout (`[workspace]`)
`members = ["crates/*"]`: every directory under `crates/` is a crate. Each crate has
exactly one job (core book, parsers, sync primitives, aggregator, network, feeds, app).
**Why:** the dependency graph enforces layering. `ob-core` can't use the network because
it doesn't depend on it. It also makes each piece testable and benchmarkable on its own.
`resolver = "3"` is the current dependency resolver for edition 2024.

### 2. Release profile: the settings that affect latency
```toml
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
debug = "line-tables-only"
```
- **`lto = "fat"`:** link-time optimisation over the whole program. Without it, a function
  in `ob-core` called from `ob-agg` can't be inlined across the crate boundary. On a hot
  path, an un-inlined call blocks constant propagation and vectorisation, not just the
  call itself.
- **`codegen-units = 1`:** by default rustc splits a crate into 16 chunks and optimises
  them in parallel. That's faster to build, but each chunk only sees part of the code.
  With one unit, LLVM sees everything. The cost is slower release builds, which is
  acceptable.
- **`panic = "abort"`:** removes the unwinding machinery (landing pads, unwind tables)
  from every function that might panic. That means smaller code and better instruction
  cache use. It's also the right behaviour: a trading process in an unknown state should
  die and be restarted by a supervisor, not limp on. (Tests still unwind; Cargo forces
  that for the test harness.)
- **`debug = "line-tables-only"`:** profilers (Instruments on macOS, `perf` on Linux) can
  map samples to source lines. It only adds metadata; the machine code is identical.
- **`target-cpu=native` is *not* in the default build.** It would let LLVM use AVX2/BMI2
  on this i7-8569U, but the binary could then crash with an illegal instruction on an
  older CPU. So it's opt-in, via `scripts/bench.sh`. Step 20 measures whether it actually
  helps.

`[profile.dev.package."*"] opt-level = 2`: our own code stays debuggable, while
dependencies are optimised, so debug-mode replay tests (step 16) don't crawl.

### 3. Lints: rules the compiler enforces
- **`unsafe_op_in_unsafe_fn = "deny"`** and **`clippy::undocumented_unsafe_blocks =
  "deny"`:** every unsafe operation needs its own `unsafe {}` block and a `// SAFETY:`
  comment saying why it's sound. Steps 8 (seqlock) and 12 (network buffers) will contain
  the only unsafe code, and each block will be justified where it stands, which is exactly
  what an interviewer will ask about.
- **`cast_possible_truncation` / `cast_sign_loss` / `cast_possible_wrap`:** prices become
  `i64` in step 2. A silent `as u32` or `as usize` on a price or quantity is a real bug
  class. These lints make every narrowing cast visible.
- **`unwrap_used = "warn"`, `todo = "deny"`, `dbg_macro = "deny"`:** no hidden panics or
  leftover debugging in library code.
- **`missing_docs`, `unreachable_pub`:** public items are documented, and nothing is
  `pub` by accident.
- **`unexpected_cfgs` with `cfg(loom)`:** pre-registers the configuration used by the loom
  concurrency model checker in step 8.

### 4. `ob-core` is `no_std`
```rust
#![cfg_attr(not(test), no_std)]
```
`no_std` removes the standard library, including the heap types (`Vec`, `String`, `Box`)
and I/O. **Why:** the plan promises that the book's hot path never allocates. Here the
compiler enforces it. Proof (run on this machine):
```
#![no_std]
pub fn f() -> usize { let v: Vec<u8> = Vec::new(); v.len() }

error[E0425]: cannot find type `Vec` in this scope
```
Under `cfg(test)` the standard library is back, so tests can use `Vec`, `BTreeMap` and
so on as a reference model (step 4).

### 5. Pinned toolchain
`channel = "1.100.0"`. A different compiler ships a different LLVM, which can change
inlining decisions and therefore benchmark numbers. Pinning keeps results reproducible
across machines and over time. Reviewers running `cargo` get the same compiler
automatically (rustup reads this file).

### 6. CI
| Job | Why |
|---|---|
| `fmt --check` | Consistent formatting; no style noise in review |
| `clippy -D warnings` | All the lints above, enforced |
| `test` on Linux + macOS, debug and release | Optimised code can expose bugs that debug hides (e.g. integer overflow checks are off in release) |
| `doc` with `-D warnings` | Broken doc links fail the build |

`RUSTFLAGS=-D warnings` makes any compiler warning fail CI. Locally, warnings stay
warnings so development isn't blocked. Jobs for Miri, loom, fuzzing and the Linux
feature are added in the steps that need them, so CI never runs empty jobs.

## Alternatives considered
| Option | Why not |
|---|---|
| A single crate | Layering only by convention; harder to benchmark parts in isolation; any file could import the network code |
| `opt-level = 3` only (default LTO off, 16 codegen units) | Loses cross-crate inlining; measurably slower on small hot functions |
| `target-cpu=native` everywhere | Binaries become machine-specific; CI and reviewer machines may differ |
| Global `#![forbid(unsafe_code)]` | The seqlock and zero-copy buffers need a small amount of unsafe; better to allow it and force every block to be justified |
| Unpinned `stable` toolchain | Benchmarks drift whenever Rust updates |

## Benefits (measured)
Nothing to measure yet; there is no code on the hot path. The profile settings are
measured once there is something to measure: step 4 (book benchmarks) and step 20
(PGO, `target-cpu=native`, LTO on vs off).

Verified now:
- `cargo fmt --check`: clean
- `cargo clippy --workspace --all-targets -D warnings`: clean
- `cargo test --workspace` (debug and release): pass (0 tests yet)
- `cargo doc` with warnings as errors: clean

## Java counterpart
The Java skeleton's `build.gradle` has no equivalent of most of this:
```groovy
plugins { id 'java' }
dependencies {
    compileOnly 'org.projectlombok:lombok:1.18.24'   // fails to compile on JDK 21+
    implementation 'com.fasterxml.jackson.core:jackson-databind:2.15.2'
    ...
}
test { useJUnitPlatform() }
```
- **Optimisation happens at run time in Java,** not build time. The JIT (C2) compiles hot
  methods after thousands of calls, so the first messages after start-up run interpreted
  (slow), and a de-optimisation can happen later. Rust does all of it ahead of time: the
  first message is as fast as the millionth. The closest Java equivalents are JVM flags
  (`-XX:+AlwaysPreTouch`, `-XX:CompileThreshold`, a warm-up phase) or GraalVM native image.
- **No toolchain pin:** the skeleton depends on whichever JDK the reviewer has. Lombok
  1.18.24 breaks on JDK 21+. Java's equivalent fix is a Gradle toolchain block:
  ```groovy
  java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }
  ```
- **No compile-time "no allocation" guarantee:** Java has no `no_std`. Any method can call
  `new`. Allocation-free Java is checked after the fact with JMH `-prof gc` or
  allocation profilers, not by the compiler.
- **Lints:** the Java equivalent would be Error Prone, SpotBugs or Checkstyle, and the
  skeleton has none of them.

## How to verify it yourself
```bash
cd rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace && cargo test --workspace --release
cargo doc --workspace --no-deps --open
```
To see the `no_std` guard work, temporarily add this to `crates/ob-core/src/lib.rs` and
run `cargo build`; it must fail with `cannot find type Vec`. Then remove it:
```rust
pub fn oops() -> usize { let v: Vec<u8> = Vec::new(); v.len() }
```
To see the profile settings in effect, run `cargo build --release -v`. It prints the
`rustc` command lines with `-C opt-level=3 -C codegen-units=1 -C panic=abort
-C debuginfo=line-tables-only`. You won't see an LTO flag yet. LTO only runs when a
final binary or benchmark is linked, and `ob-core` is a library. It shows up from
step 15 (the `ob-app` binary) and in benchmarks from step 4.

## Likely interview questions
1. **Why `codegen-units = 1`, if it makes the build slower?**
   Build time is paid once; run time is paid on every message. One unit lets LLVM inline
   and optimise across the whole crate instead of 16 isolated chunks.
2. **What does `panic = "abort"` buy you, and what's the risk?**
   Smaller code, with no unwind tables or landing pads, so better instruction-cache use.
   The risk is that any panic kills the process. In trading that's the desired fail-fast
   behaviour; we run under a supervisor and rebuild books from snapshots on restart.
3. **Why isn't `target-cpu=native` the default?**
   Portability. A binary built with AVX2 crashes on a CPU without it. Production at a
   real firm targets a known fleet (e.g. `-C target-cpu=icelake-server`); here we opt in
   for benchmarks only and measure the difference.
4. **How do you *guarantee* the core never allocates?**
   `no_std` at compile time: there is no heap API to call. Step 17 adds a run-time proof
   for the crates that do use `std`: a counting global allocator asserts zero allocations
   per message.
5. **Why pin the compiler?**
   Benchmark reproducibility. A new rustc means a new LLVM, which can change inlining and
   code layout, and therefore latency.
6. **Why deny undocumented `unsafe` instead of forbidding it?**
   The seqlock and zero-copy buffers legitimately need a little unsafe. Forbidding it
   would push us to slower designs. Requiring a `SAFETY:` justification on every block
   keeps it small and auditable.

## Appendix: follow-up questions

### What LTO is, demonstrated
Normally rustc compiles each crate separately into machine code, and the linker only
glues the finished pieces together. A call from crate `app` into crate `pricing` stays a
real `call` instruction, because when `app` was optimised the compiler couldn't see
inside `pricing`. **Link-time optimisation (LTO)** keeps each crate's compiler
intermediate form (LLVM IR) and runs the optimiser again over *all* crates together at
link time. Cross-crate calls can then be inlined, constants pushed through, and dead code
removed across the whole program.

Experiment on this machine: a two-crate demo (`app` calls `pricing::notional` once per
level, over 1M levels × 200 rounds), release build, `codegen-units = 1`. The only
difference is `lto`:

| | `lto = false` | `lto = "fat"` |
|---|---|---|
| `call pricing::notional` in the hot loop | yes, one per level | **gone**: inlined |
| `notional` exists as its own function | yes | no; merged into the caller |
| Run time (3 runs) | 692–693 ms | **499–513 ms (~27% faster)** |

Caveat found during the experiment: the first three, smaller versions of `notional` were
inlined **even without LTO**. Rust already ships enough information across crates to
inline generic functions, `#[inline]` functions, and functions the compiler judges small
enough. LTO pays off for everything else: larger functions, non-generic code in
dependencies, and whole-program dead-code removal. On our hot path, a book update calls
the parser and aggregator crates, which is exactly that case.

`lto = "thin"` does a cheaper, parallel version that gets most of the benefit with faster
builds. `"fat"` is the maximum; step 20 measures both.

### Does `no_std` block more than allocation?
Yes, and that's intended for `ob-core`. `std` = `core` + `alloc` + operating-system
features. `no_std` keeps only `core`:

| Removed by `no_std` | Why it doesn't matter for `ob-core` |
|---|---|
| Heap: `Vec`, `String`, `Box`, `BTreeMap`, `format!` | The point of the guard |
| `HashMap` | Needs the heap and OS randomness; never wanted on a hot path anyway |
| I/O, files, sockets, `println!` | The book is pure data; I/O lives in `ob-net` |
| Threads, `std::sync::Mutex`, `thread_local!` | Concurrency lives in `ob-sync`; the book is single-writer |
| Clocks (`Instant`, `SystemTime`) | Timestamps come in as integers, so tests are deterministic |
| Stable `sort` (needs a buffer) | `sort_unstable` is in `core` |

Still available, at identical speed: `Option`, `Result`, iterators, slices, binary
search, `sort_unstable`, `i128`, atomics, `core::arch` SIMD intrinsics, `spin_loop`, and
`core::fmt` formatting into a fixed buffer. `std::option::Option` *is*
`core::option::Option` re-exported, so it's the same code.

Limits of the guard: it covers only this crate; `extern crate alloc;` could opt back in
(and review would catch it); and it says nothing about the crates that do use `std`.
Step 17's counting-allocator test covers those at run time.
