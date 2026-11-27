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
commit:     22b2365
date:       Fri Nov 27 17:28:39 GMT 2026
load avg:   { 5.43 4.56 2.90 } (at start)
mode:       full
tests: 317 passed, 0 failed, 7 ignored
load avg:   { 8.81 6.44 4.60 } (at end)

## Validity
- All tests passed: passed — `317 passed, 0 failed, 7 ignored`
- Every tail feed scenario stayed on the normal path: passed — no `INVALID RUN` matches
- Parser: passed — `SIMD level parser: OFF: scalar parser (build without target-cpu x86-64-v2, or not x86-64)`
- Machine quiet: failed — load was `{ 5.43 4.56 2.90 }` at start and `{ 8.81 6.44 4.60 }` at end; both above the ~2 target
- Default live runs: passed — both venues had zero resyncs, rejects, reconnects, and disconnects
- Full-depth live run: failed validity check — Binance had `resyncs 1`; Bybit had zero; neither venue had rejects, reconnects, or disconnects
- Criterion: passed — 75 measurement rows across all targets; the summary has 117 rows total because it also contains 42 comparisons with existing Criterion baselines

## A. Our code per message (tails.txt §1 and §2, row "read start → published")
| Stream | Warm p50 / p99 / p99.9 | Cold p50 / p99 / p99.9 |
| binance depth20+bookTicker | 208 ns / 250 ns / 1.08 µs | 1.00 µs / 2.92 µs / 8.42 µs |
| bybit orderbook.50+1 | 333 ns / 4.04 µs / 7.71 µs | 1.08 µs / 5.04 µs / 9.63 µs |
| binance full depth | 2.25 µs / 27.14 µs / 31.38 µs | 3.75 µs / 28.51 µs / 32.02 µs |

## B. The whole receive path (tails.txt §3)
| | read p50 / p99 / p99.9 | read start → published p50 / p99 / p99.9 | sent → published p50 / p99 / p99.9 |
| Binance, busy | 2.96 µs / 8.63 µs / 33.63 µs | 3.25 µs / 9.54 µs / 37.15 µs | 10.46 µs / 26.43 µs / 70.33 µs |
| Bybit, busy | 2.96 µs / 9.05 µs / 42.17 µs | 3.50 µs / 11.17 µs / 45.50 µs | 10.88 µs / 25.97 µs / 76.80 µs |
| Binance, sparse | 10.26 µs / 35.01 µs / 50.88 µs | 10.54 µs / 36.06 µs / 55.94 µs | 34.94 µs / 66.30 µs / 97.60 µs |
| Bybit, sparse | 10.38 µs / 36.80 µs / 56.19 µs | 10.92 µs / 38.24 µs / 60.09 µs | 35.97 µs / 67.45 µs / 92.35 µs |

## C. The read stage, taken apart (tails.txt §5), p50 / p99 / p99.9
| | Binance | Bybit |
| recv, data waiting | 1.62 µs / 6.46 µs / 11.84 µs | 1.67 µs / 5.38 µs / 12.34 µs |
| recv, nothing yet | 250 ns / 416 ns / 875 ns | 250 ns / 416 ns / 791 ns |
| rustls: all three | 208 ns / 333 ns / 4.54 µs | 250 ns / 625 ns / 4.83 µs |
| ours: decrypt in place | 83 ns / 125 ns / 208 ns | 83 ns / 375 ns / 458 ns |
| AES-128-GCM alone | 83 ns / 125 ns / 208 ns | 83 ns / 333 ns / 458 ns |

## D. Slowest messages (tails.txt §6): copy the two tables verbatim
```text
  bybit orderbook.50+1 (parse is 58% of all handling time)
    line 32                        2274     109    4.08 µs    2.21 µs      1.88 µs   0.97 ns
    line 122                       1992      95    3.75 µs    1.92 µs      1.83 µs   0.96 ns
    line 48                        1633      76    3.00 µs    1.62 µs      1.38 µs   1.00 ns
    line 112                       1624      75    2.79 µs    1.62 µs      1.17 µs   1.00 ns
    line 0                         2256     100    2.21 µs    2.04 µs       167 ns   0.90 ns
  binance full depth (parse is 63% of all handling time)
    line 86                       24875    1177   27.29 µs   22.38 µs      4.92 µs   0.90 ns
    line 85                       15698     740   17.83 µs   14.12 µs      3.71 µs   0.90 ns
    line 99                       15622     736   17.54 µs   14.08 µs      3.46 µs   0.90 ns
    line 88                       12554     590   14.29 µs   11.33 µs      2.96 µs   0.90 ns
    line 92                       11355     533   12.96 µs   10.21 µs      2.75 µs   0.90 ns
```

## E. Reader queries (tails.txt §4), p50 / p99 / p99.9: writer idle | writer busy
| Query | Writer idle | Writer busy |
| timer floor (empty) | 1 ns / 1 ns / 41 ns | 1 ns / 1 ns / 41 ns |
| refresh (copy changed books) | 1 ns / 41 ns / 41 ns | 1 ns / 125 ns / 333 ns |
| best bid + best ask | 41 ns / 41 ns / 41 ns | 41 ns / 41 ns / 83 ns |
| best 10 bid levels | 41 ns / 83 ns / 83 ns | 41 ns / 83 ns / 125 ns |
| optimal execution, buy 1 BTC | 333 ns / 458 ns / 4.08 µs | 83 ns / 458 ns / 500 ns |

## F. Jitter (tails.txt §7): the gap row and the "interruptions per second" line, verbatim
```text
  gap between consecutive clock reads      4229758976     1 ns      1 ns     41 ns     41 ns     41 ns  226.81 µs
    interruptions per second: 3781 over 1 µs, 221 over 10 µs, 0.5 over 100 µs; CPU time lost: 2.157%
```

## G. Criterion highlights (criterion-summary.txt)
- `pipeline/binance_depth20                                               [58.327 µs 58.603 µs 59.052 µs]`
- `pipeline/bybit_orderbook50                                             [23.147 µs 23.457 µs 23.999 µs]`
- `pipeline/binance_depth20+bookTicker                                    [210.47 µs 213.82 µs 218.30 µs]`
- `pipeline/bybit_orderbook50+1                                           [80.944 µs 81.591 µs 82.640 µs]`
- `pipeline/binance_full_depth                                            [345.51 µs 349.28 µs 354.94 µs]`
- `full_depth_apply/sorted arrays (OrderBook<1024>)                       [165.16 µs 166.55 µs 168.48 µs]`
- `full_depth_apply/price-indexed (TickBook<1024>)                        [32.700 µs 33.584 µs 34.745 µs]`
- `binance_depth20/serde_value                                            [387.38 µs 390.45 µs 394.94 µs]`
- `binance_depth20/serde_typed                                            [156.44 µs 158.40 µs 161.33 µs]`
- `binance_depth20/ob_parse                                               [55.687 µs 56.518 µs 57.726 µs]`
- `binance_depth20/ob_parse+book                                          [57.298 µs 58.320 µs 59.905 µs]`
- `read_top/seqlock                                                       [1.7883 ns 1.8001 ns 1.8226 ns]`
- `read_top/mutex                                                         [6.7134 ns 6.8056 ns 6.9638 ns]`
- `read_full/20x20/seqlock                                                [14.644 ns 14.796 ns 15.056 ns]`
- `read_full/20x20/mutex                                                  [24.556 ns 24.716 ns 25.000 ns]`
- `contended/read_top/seqlock                                             [1.7725 ns 1.8070 ns 1.8503 ns]`
- `contended/read_top/mutex                                               [19.418 ns 19.671 ns 19.975 ns]`
- `contended/read_full_20x20/seqlock                                      [15.689 ns 15.796 ns 15.927 ns]`
- `contended/read_full_20x20/mutex                                        [47.069 ns 47.688 ns 48.571 ns]`
- `execution/optimal_execution buy 1 BTC                                  [40.511 ns 41.218 ns 42.112 ns]`
- `execution/optimal_execution buy 10 BTC                                 [249.72 ns 253.17 ns 259.03 ns]`
- `execution/optimal_execution buy 60 BTC                                 [794.06 ns 796.66 ns 800.96 ns]`
- `execution/ask_price_for_volume 1 BTC (no fees)                         [40.376 ns 40.713 ns 41.165 ns]`
- `aggregator/refresh/nothing_changed                                     [2.7215 ns 2.7399 ns 2.7710 ns]`
- `aggregator/refresh/both_changed (publish+copy 20x20 & 50x50)           [137.00 ns 140.02 ns 145.15 ns]`
- `aggregator/best_bids+best_asks                                         [21.939 ns 22.069 ns 22.270 ns]`
- `aggregator/bid_volume (top level only)                                 [15.299 ns 15.348 ns 15.427 ns]`
- `aggregator/bid_volume ($10 deep)                                       [41.909 ns 42.126 ns 42.619 ns]`

## H. Live (live.txt, live-full-depth.txt)
Default run:
```text
binance  messages 85754 applied 85622 (top of book 85046) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
bybit    messages 5618 applied 5594 (top of book 2818) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0

binance
stage (ns)                     count       p50       p90       p99     p99.9        max
read (socket+TLS+WS)           85754        41     16127     28335     77631    6504447
handle (parse+apply)           85622       166       291      1583      3751      13127
publish (seqlock)              85622        41        83       208       541       8631
total                          85622       250     16511     29583     80767    6504447
exchange → us                  85622    123711    156031    238719    360191     483327   (µs; includes clock skew; 0 negative)

bybit
stage (ns)                     count       p50       p90       p99     p99.9        max
read (socket+TLS+WS)            5618     13791     27887     68543    154879    2306047
handle (parse+apply)            5594       916      2375      5791      8631      30543
publish (seqlock)               5594       208       375       958      5251       6127
total                           5594     16127     30335     71551    157951    2312191
exchange → us                   5594    111679    148223    195967    314367     414207   (µs; includes clock skew; 0 negative)
```
Full-depth run:
```text
binance  messages 83216 applied 82464 (top of book 81892) gaps 0 rejected 0 resyncs 1 reconnects 0 disconnects 0 | log drops 0
bybit    messages 5506 applied 5487 (top of book 2754) gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0

binance
stage (ns)                     count       p50       p90       p99     p99.9        max
read (socket+TLS+WS)           83216        41     15879     28095     70847     409087
handle (parse+apply)           82464       166       291      2000     14007      58399
publish (seqlock)              82464       958      1000      1333      2791     147583
total                          82464      1166     17263     31583     76479  525336575
exchange → us                  82464    119807    197375    353279    435967     502527   (µs; includes clock skew; 0 negative)

bybit
stage (ns)                     count       p50       p90       p99     p99.9        max
read (socket+TLS+WS)            5506     17135     28223     65727    129599    2027519
handle (parse+apply)            5487       916      2417      6083      9127      17167
publish (seqlock)               5487       208       375       833      5375       6083
total                           5487     18383     30431     69823    130559    2038783
exchange → us                   5487    113791    151295    199039    474111     572927   (µs; includes clock skew; 0 negative)
```

## Notes
The full run completed. The machine was not idle: load exceeded the guide's ~2 target
at both start and finish, and this session's active VS Code/Copilot processes were among
the busiest visible processes. I did not terminate them because that would end the
running session and risk interrupting the user's editor work. The scalar parser ran as
expected on Apple Silicon. Binance recorded one resync in the full-depth live run; the
default live run had none. No `INVALID RUN` marker appeared in the tail harness.

Compared with the development Intel Mac's warm pipeline reference (step 23, which had
lower recorded start load but different code and SIMD capability), the M2 Pro's
`read start → published` p50s were lower: 208 ns vs 355 ns for Binance depth20 plus
bookTicker, 333 ns vs 560 ns for Bybit orderbook.50 plus 1, and 2.25 µs vs 2.98 µs
for Binance full depth. These CPU-bound comparisons are indicative rather than a
strict A/B: the code versions and parser implementations differ, and the M2 run had
high system load.
