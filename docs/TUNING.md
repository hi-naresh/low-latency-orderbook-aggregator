# Running `ob` for the lowest latency

Steps 12–20 measured where the time goes. On this laptop the in-process path is ~50 µs
per message, ~45 µs of it the socket read, which is mostly the cost of waking a cold
thread and cold kernel paths when messages are sparse. **Measured, the network round
trip from here is 234 ms (Binance) and 7–10 ms to Bybit's edge.** Tuning the host is the
second-biggest lever; location is the first.

## 1. Location first: colocation

| Venue | Matching engine | Measured from this laptop |
|---|---|---|
| Binance USDⓈ-M futures | AWS Tokyo (ap-northeast-1) | 234–244 ms round trip (TCP connect) |
| Bybit linear | AWS Singapore (ap-southeast-1) | 7–10 ms to a nearby edge, then the engine behind it |

Running the feed for each venue on an instance in **the venue's own AWS region** turns
hundreds of ms into ~1 ms or less. That's 1,000× more than any setting below. With two
venues in two regions, a cross-venue strategy has to decide where it lives; the
inter-region link (Tokyo–Singapore ≈ 70 ms round trip) is then the latency that matters
for arbitrage.

## 2. The machine (Linux)

**[docs/LINUX.md](LINUX.md)** is the Linux document: what is already in the code,
everything to add or change (host isolation, busy polling, `io_uring`, kernel TLS,
kernel bypass, measurement), the impact to expect from each, and a day-one checklist.
`scripts/tune-linux.sh [--dry-run] 2,3` prints or applies the host settings.

## 3. On macOS (the test bench)

macOS offers far less, and `ob live` reports exactly what it got:

```text
lock memory: Ok(Unsupported("mlockall is Linux-only here"))
binance: real-time priority: Ok(Hint("macOS QoS class user-interactive"))
binance: pin to CPU 1: Ok(Hint("macOS affinity tag (no hard pinning)"))
busy poll 50 µs: Ok(Unsupported("SO_BUSY_POLL is Linux-only"))
kernel timestamps: Ok(Unsupported("no kernel receive timestamps for TCP on this OS"))
```

What helps, measured (local TLS server, 150 messages 20 ms apart, interleaved over 2
rounds): the **user-interactive QoS class** (`--rt`) leaves the median wake-up latency
unchanged (~70–77 µs) but **cuts the tail**. Spin p99 163–213 → 126–130 µs, max 167–883 →
134–148 µs. Block max 760–1,513 → 177–181 µs. Two macOS quirks found by tests:
- set the QoS class **before** the affinity tag, or it fails with EPERM;
- macOS never delivers kernel receive timestamps for TCP, even after accepting
  `SO_TIMESTAMP`.

## 4. Beyond this project

Kernel TLS, `io_uring`, kernel bypass and hardware timestamps, with their expected
impact: [docs/LINUX.md](LINUX.md) §3.
