# Notes for AI agents working in this repository

- **Asked to run the benchmarks?** Follow [`docs/BENCHMARKING.md`](docs/BENCHMARKING.md)
  exactly: prerequisites, one command (`cd rust && scripts/run-benchmarks.sh`), the
  validity checks, then `RESULTS.md` in the results folder. Don't change source code,
  scripts or build settings; if a step fails, stop and report it.
- **The project:** a low-latency order book aggregator in Rust (`rust/`); the Java
  skeleton in `src/` is the original assignment, untouched. Start with
  [`README.md`](README.md); the design log is [`docs/steps/`](docs/steps/).
- **Checks before any change:** from `rust/`: `cargo fmt --check`,
  `cargo clippy --workspace --all-targets --release`, `cargo test --workspace --release`.
