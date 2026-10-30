# Standard continuation: follow-up cost comparison

The standard-mode implementation reduces avoidable interval materialization, sorting and copies.
Against the original **enabled** implementation, combined component p50 fell 52.4% on the 32k
fragmented fixture and 56.9% on the 128k fragmented fixture. The fully-local fragmented control
fell from 137.51 to 2.23 µs because it skips the additional suffix walk. These three effects exceed
the observed A/A noise. This is router component cost, not serving throughput or net fetch benefit.

The remaining cells changed by roughly −1.3% to +3.4%; this is not a uniform speedup. The
single-owner cell increased from 60.26 to 62.29 µs (+3.4%, six of six paired directions).
All small movements are reported below without a general improvement claim.

## Compared code and method

- Old: `6e43c00daff9fd4ddc420b4808c3a7d3f48c4282`, continuation enabled, original frozen binaries.
- New production code: `e58769846168585de889b29bd9c31dcf59666014`, standard continuation, no new flag.
  Subsequent changes on the PR document the local/remote ordering and extend its regression test;
  measured production code is unchanged. Exact source and binary hashes are in
  [provenance.json](results-standard/provenance.json).
- Same 15 hash-consistent inventories, 16 tokens per block, compiler/settings, CPU2 on a
  16-physical-core Xeon 6975P-C VM, one NUMA node, SMT sibling18 unused by this task, glibc 2.39.
- 12 same-binary A/A processes, 12 balanced old/new processes (six each), four separate allocation
  processes. Each metric gets 30ms warmup, 150ms timing, at least 256 samples. No trial exclusions.
  All 420 process/fixture validations passed; every target's source and credited end agree
  across both implementations. Interval membership is checked against the original frontiers.
- Table values are medians of per-process percentiles. Ratio CIs bootstrap process medians,
  never pooled request samples. A/A p50 ratios span 0.993–1.014, with CI envelope 0.962–1.053.
  Full intervals and all raw trials are in [results-standard](results-standard/).
- Maximum timed foreign busy fraction was 0.24%; recorded steal time was zero. This is not a
  dedicated-hypervisor guarantee. Allocation bytes are cumulative allocator traffic, not peak heap.

## Results

| Inventory | Sources / targets | p50 old/new (µs) | p99 old/new (µs) | Intervals old/new | Allocation bytes old/new |
|---|---:|---:|---:|---:|---:|
| 4k_shared | 8 / 8 | 7.38 / 7.56 | 8.01 / 8.05 | 8 / 8 | 18,376 / 15,880 |
| 32k_no_sources | 0 / 8 | 24.71 / 24.48 | 29.03 / 28.78 | 0 / 0 | 31,664 / 20,240 |
| 32k_single | 1 / 8 | 60.26 / 62.29 | 66.56 / 68.45 | 1 / 1 | 60,512 / 43,952 |
| 32k_pair | 2 / 8 | 83.03 / 85.09 | 89.51 / 92.12 | 2 / 2 | 60,560 / 43,952 |
| 32k_shared | 8 / 8 | 36.09 / 35.62 | 42.21 / 41.20 | 8 / 8 | 62,280 / 45,448 |
| 32k_64owners | 64 / 8 | 42.13 / 41.69 | 48.58 / 47.25 | 64 / 64 | 81,368 / 62,744 |
| 32k_256owners | 256 / 8 | 61.74 / 61.52 | 67.71 / 67.82 | 256 / 256 | 146,680 / 121,912 |
| 32k_64targets | 64 / 64 | 52.78 / 52.97 | 58.81 / 58.99 | 64 / 64 | 132,376 / 112,856 |
| 32k_root | 64 / 8 | 94.14 / 93.72 | 101.86 / 100.77 | 0 / 0 | 212,296 / 193,704 |
| 32k_staggered | 64 / 8 | 89.83 / 89.21 | 96.39 / 95.67 | 64 / 57 | 85,784 / 67,160 |
| 32k_fragmented | 64 / 8 | 256.46 / 122.19 | 266.46 / 129.21 | 3072 / 128 | 563,096 / 69,272 |
| 32k_fully_local | 64 / 8 | 137.51 / 2.23 | 145.46 / 2.56 | 3072 / 0 | 529,204 / 24,280 |
| 32k_islands | 64 / 64 | 102.05 / 100.94 | 109.05 / 108.24 | 64 / 57 | 146,296 / 125,816 |
| 128k_shared | 256 / 64 | 175.28 / 174.95 | 185.62 / 184.70 | 256 / 256 | 348,216 / 273,400 |
| 128k_fragmented | 64 / 64 | 1149.69 / 495.93 | 1185.56 / 514.73 | 12288 / 128 | 2,236,120 / 220,760 |

The measured pipeline is lower-tier lookup, candidate ownership handoff, source summary and
per-target choice. It excludes device matching, full worker scoring, queues, concurrent events,
network fetch, inference and end-to-end serving latency. Inventories are synthetic stress/control
shapes, not an estimate of their production frequency.

## Why the pruning is safe

Destination frontiers are the original contiguous device-plus-local-host prefixes. Detached local
host segments do not extend them. A suffix interval is retained only if some original frontier
lies in `[start, end)`. Its full bounds are preserved. Root-prefix candidates remain available to
cold destinations, including workers absent from the hit map. When there is no positive partial
frontier, only the additional suffix discovery walk is skipped.

The router first extends L1 through local L2, then considers one remote L2 source. It does not
plan a remote fetch and then resume a detached local segment, or chain another remote source.
The receiver may skip blocks that are actually resident; routing credit remains conservative.

When suffix discovery runs, hash-chain traversal and source run tracking are unchanged. It can
still approach O(B*S) owner visits in changing-owner inventories. Pruning reduces emitted intervals
and downstream work; it does not remove that traversal bound. Source cardinality is still not
bounded by destination count. The source summary filters unusable intervals before sorting and
the selection/refresh handoff moves candidates rather than cloning them.
