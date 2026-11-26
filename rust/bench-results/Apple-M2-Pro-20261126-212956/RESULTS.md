# Benchmark results: Apple M2 Pro

## Machine
model:      Mac14,9 (arm64)
cpu:        Apple M2 Pro
cores:      8 performance + 4 efficiency
caches:     P-cluster L2 16 MiB, E-cluster L2 4 MiB, L1d 128 KiB (per OS report)
memory:     16 GiB
os:         macOS 27.2.1
power:      AC Power
rustc:      rustc 1.100.0 (0ef1bc386 2026-11-09)
commit:     7955906 (uncommitted changes)
date:       Thu Nov 26 21:29:56 GMT 2026
load avg:   { 4.17 5.96 5.96 } (at start)
mode:       full
tests: 294 passed, 0 failed, 7 ignored
load avg:   { 3.34 4.99 5.57 } (after)

## Validity
- All tests passed: passed — `294 passed, 0 failed, 7 ignored`
- Every feed scenario stayed on the normal path: passed — no `INVALID RUN` matches
- Parser: passed — `SIMD level parser: OFF: scalar parser (build without target-cpu x86-64-v2, or not x86-64)`
- Machine quiet: failed — start load `{ 4.17 5.96 5.96 }`; end load `{ 3.34 4.99 5.57 }`; neither meets the ~2 start target
- Live default streams: passed — `gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0`, for both venues
- Live full depth: passed — `gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0`, for both venues
- Criterion ran every suite: passed — `74` benchmark lines

## A. Our code per message (tails.txt §1 and §2, row "read start → published")
| Stream | Warm p50 / p99 / p99.9 | Cold p50 / p99 / p99.9 |
| binance depth20+bookTicker | 208 ns / 250 ns / 1.08 µs | 958 ns / 1.96 µs / 6.42 µs |
| bybit orderbook.50+1 | 333 ns / 3.83 µs / 7.38 µs | 1.12 µs / 4.96 µs / 9.46 µs |
| binance full depth | 3.29 µs / 28.09 µs / 32.34 µs | 4.83 µs / 29.50 µs / 32.43 µs |

## B. The whole receive path (tails.txt §3)
| | read p50 / p99 / p99.9 | read start → published p50 / p99 / p99.9 | sent → published p50 / p99 / p99.9 |
| Binance, busy | 4.13 µs / 8.88 µs / 21.34 µs | 4.42 µs / 9.79 µs / 23.05 µs | 14.13 µs / 24.38 µs / 43.71 µs |
| Bybit, busy | 3.83 µs / 9.17 µs / 21.47 µs | 4.71 µs / 11.34 µs / 24.13 µs | 14.42 µs / 25.47 µs / 56.90 µs |
| Binance, sparse | 10.05 µs / 36.51 µs / 50.81 µs | 10.38 µs / 37.31 µs / 52.51 µs | 33.82 µs / 68.48 µs / 89.47 µs |
| Bybit, sparse | 10.29 µs / 36.64 µs / 47.68 µs | 10.92 µs / 37.76 µs / 50.59 µs | 34.85 µs / 69.06 µs / 90.05 µs |

## C. The read stage, taken apart (tails.txt §5), p50 / p99 / p99.9
| | Binance | Bybit |
| recv, data waiting | 1.67 µs / 6.29 µs / 12.71 µs | 1.75 µs / 5.96 µs / 13.29 µs |
| recv, nothing yet | 250 ns / 583 ns / 791 ns | 250 ns / 666 ns / 833 ns |
| rustls: all three | 208 ns / 333 ns / 4.33 µs | 250 ns / 708 ns / 5.25 µs |
| ours: decrypt in place | 83 ns / 166 ns / 250 ns | 83 ns / 375 ns / 458 ns |
| AES-128-GCM alone | 83 ns / 125 ns / 208 ns | 83 ns / 375 ns / 458 ns |

## D. Slowest messages (tails.txt §6): copy the two tables verbatim

```text
  bybit orderbook.50+1 (parse is 58% of all handling time)
    line 32                        2274     109    4.12 µs    2.21 µs      1.92 µs   0.97 ns
    line 122                       1992      95    3.75 µs    1.92 µs      1.83 µs   0.96 ns
    line 48                        1633      76    3.00 µs    1.62 µs      1.38 µs   1.00 ns
    line 112                       1624      75    2.79 µs    1.62 µs      1.17 µs   1.00 ns
    line 0                         2256     100    2.21 µs    2.04 µs       167 ns   0.90 ns
  binance full depth (parse is 48% of all handling time)
    line 86                       24875    1177   28.38 µs   22.38 µs      6.00 µs   0.90 ns
    line 85                       15698     740   18.83 µs   14.12 µs      4.71 µs   0.90 ns
    line 99                       15622     736   18.62 µs   14.08 µs      4.54 µs   0.90 ns
    line 88                       12554     590   15.33 µs   11.33 µs      4.00 µs   0.90 ns
    line 92                       11355     533   13.92 µs   10.21 µs      3.71 µs   0.90 ns
```

## E. Reader queries (tails.txt §4), p50 / p99 / p99.9: writer idle | writer busy
| Query | Writer idle | Writer busy |
| timer floor (empty) | 1 ns / 1 ns / 41 ns | 1 ns / 1 ns / 41 ns |
| refresh (copy changed books) | 1 ns / 41 ns / 41 ns | 1 ns / 125 ns / 583 ns |
| best bid + best ask | 41 ns / 41 ns / 83 ns | 41 ns / 41 ns / 125 ns |
| best 10 bid levels | 41 ns / 83 ns / 83 ns | 41 ns / 83 ns / 125 ns |
| optimal execution, buy 1 BTC | 375 ns / 458 ns / 1.88 µs | 83 ns / 458 ns / 541 ns |
| arbitrage scan | 1 ns / 41 ns / 41 ns | 1 ns / 41 ns / 41 ns |

## F. Jitter (tails.txt §7): the gap row and the "interruptions per second" line, verbatim
```text
  gap between consecutive clock reads      4228591616     1 ns      1 ns     41 ns     41 ns     41 ns    1.56 ms
    interruptions per second: 3139 over 1 µs, 325 over 10 µs, 1.2 over 100 µs; CPU time lost: 1.981%
```

## G. Criterion highlights (criterion-summary.txt)
- `pipeline/binance_depth20                                               [59.740 µs 60.482 µs 61.578 µs]`
- `pipeline/bybit_orderbook50                                             [23.473 µs 23.622 µs 23.828 µs]`
- `pipeline/binance_depth20+bookTicker                                    [217.19 µs 219.85 µs 224.02 µs]`
- `pipeline/bybit_orderbook50+1                                           [82.054 µs 82.540 µs 83.490 µs]`
- `pipeline/binance_full_depth                                            [451.48 µs 460.38 µs 476.13 µs]`
- `full_depth_apply/sorted arrays (OrderBook<1024>)                       [158.84 µs 161.08 µs 164.58 µs]`
- `full_depth_apply/price-indexed (TickBook<1024>)                        [32.252 µs 32.981 µs 34.241 µs]`
- `binance_depth20/serde_value                                            [396.66 µs 398.78 µs 401.94 µs]`
- `binance_depth20/serde_typed                                            [157.57 µs 161.56 µs 169.51 µs]`
- `binance_depth20/ob_parse                                               [55.472 µs 56.540 µs 58.108 µs]`
- `read_top/seqlock                                                       [1.7901 ns 1.8139 ns 1.8539 ns]`
- `read_top/mutex                                                         [6.7058 ns 6.7792 ns 6.9003 ns]`
- `read_full/20x20/seqlock                                                [14.646 ns 14.824 ns 15.096 ns]`
- `read_full/20x20/mutex                                                  [24.472 ns 24.754 ns 25.156 ns]`
- `contended/read_top/seqlock                                             [1.7710 ns 1.7939 ns 1.8293 ns]`
- `contended/read_top/mutex                                               [15.996 ns 16.810 ns 17.415 ns]`
- `contended/read_full_20x20/seqlock                                      [15.822 ns 16.174 ns 16.597 ns]`
- `contended/read_full_20x20/mutex                                        [45.250 ns 46.156 ns 48.049 ns]`
- `execution/optimal_execution buy 1 BTC                                  [41.494 ns 42.130 ns 42.993 ns]`
- `execution/optimal_execution buy 10 BTC                                 [259.59 ns 264.37 ns 271.01 ns]`
- `execution/scan_arbitrage (2 venues)                                    [12.793 ns 12.954 ns 13.189 ns]`
- `aggregator/refresh/nothing_changed                                     [2.7104 ns 2.7356 ns 2.7733 ns]`
- `aggregator/best_bids+best_asks                                         [22.108 ns 22.530 ns 23.076 ns]`

## H. Live (live.txt, live-full-depth.txt)
```text
binance  messages 14702 applied 14682 (top of book 14129) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
bybit    messages 1926 applied 1916 (top of book 615) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
```
```text
binance  messages 9437 applied 9404 (top of book 8829) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
bybit    messages 2056 applied 2048 (top of book 537) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
```
Default run stage tables and full-depth stage tables are present verbatim in `live.txt` and `live-full-depth.txt`.

## Notes
The rerun was complete. The benchmark harness itself was changed to invoke each Criterion bench target separately; the original workspace-wide invocation passed Criterion options to libtest targets. Apple Silicon used the scalar parser. Machine load remained above the ~2 target, so OS-involving results and tail numbers should be interpreted with that caveat.

Compared with the development Mac's tail-harness reference: warm CPU-bound pipeline rows on this M2 Pro are substantially faster for small depth and Bybit messages, while the 41.7 ns timer granularity and loaded macOS environment affect sub-100 ns and OS-involving measurements; the scalar parser was used for Apple Silicon.
