# Fetch-continuation cost at PR #17657

Measured production source at `6e43c00daff9fd4ddc420b4808c3a7d3f48c4282` against base
`008d356966dc3cd77198d7f47b6f9d949ab44d6a`. The feature remains **off by default**.

The default-off comparison showed no resolved regression: combined-component p50 ratios
versus base ranged from 0.978 to 1.024, and every 95% interval included 1.0. A/A p50 intervals
across the matrix spanned approximately 0.963–1.044. This bounds what this experiment can
resolve; it does not prove zero overhead.

Enabling continuation has a measurable cost. At 32,768 token equivalents, eight shared sources
and eight destinations, the combined component rose from 2.69 to 35.76 microseconds (p50),
with enabled p99 40.53 microseconds. With 64 staggered sources it reached 90.09 microseconds;
with 64 fragmented sources, 253.60 microseconds. A fully-local control still cost 136.81
microseconds although it could not produce a useful fetch. At 131,072 token equivalents,
64 fragmented sources cost 1.143 ms p50 and 1.165 ms p99.

These are **component service times**, not inference latency, fetch throughput, queueing latency,
or sustained serving capacity. They quantify router overhead, not the prefill work that a
successful fetch might save. No deployment-frequency or net serving-benefit claim is made.

## Measurement contract

- Fifteen fixed synthetic inventories, identical across arms, with globally consistent hashes.
  Every event is applied before timing. Independent membership oracles validate exact intervals,
  source choices, attachment/topology restrictions, and local hit/continuation preservation.
- Six fresh-process trials per arm. A/A uses the same feature/off binary under two labels;
  balanced A/B compares base, feature/off, feature/on. Allocation counting uses a separate
  instrumented binary, twice per mode. All 34 processes and all 510 fixture validations passed;
  every result is retained. Base/off discrete outputs match exactly.
- Rust release builds with identical dependency lockfiles and repository target flags;
  system allocator. Intel Xeon 6975P-C x86_64 VM, 16 physical cores/32 logical CPUs, one NUMA
  node, measured on CPU2, task leaves sibling18 unused. glibc 2.39 in a process-local capsule.
  No builds overlap timing. This does not establish hypervisor isolation.
- Each metric has 30 ms warm-up, at least 150 ms and 256 samples. Table entries are medians of
  six per-process percentiles. CI calculation resamples those process summaries 10,000 times;
  per-operation samples are not treated as independent replicates. Exact Mann–Whitney p-values,
  paired directions, min/max and A/A intervals are in `results/analysis.json`. The matrix is
  exploratory; there is no multiple-comparison-adjusted inference.
- The combined component includes lower-tier lookup, the selection service's current candidate
  clone/drop handoff, source-summary construction and a source choice for every destination.
  Device matches are supplied, not timed. Full worker scoring, reservation, serialization,
  concurrent event load, transport, serving scheduler and inference are outside this measurement.

Whole-process measured-CPU foreign busy time was 0–20 ms per trial, at most 0.24% of a timed process. Steal time was zero. These tick-granular checks include fixture setup, and do not isolate each individual query. Allocation-only processes are much shorter; their fractional estimates are correspondingly coarse.

## Complete combined-component results

B is prompt blocks (16 tokens/block), S exact cache owners, T destinations. I is emitted suffix
intervals with the feature enabled. Times are microseconds. Allocated bytes are total requested
allocation traffic per combined operation, including reallocations and the candidate copy—not
peak or retained heap.

| Case | B | S / T | I | Base p50 | Off p50 | On p50 | Off p99 | On p99 | On/off p50 95% CI | Alloc calls off/on | KiB requested off/on |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|
| 4k_shared | 256 | 8 / 8 | 8 | 2.30 | 2.28 | 7.37 | 2.64 | 7.86 | 3.110–3.265 | 31/43 | 11.7/17.9 |
| 32k_no_sources | 2048 | 0 / 8 | 0 | 2.63 | 2.62 | 24.42 | 2.95 | 28.20 | 9.118–9.650 | 31/31 | 30.9/30.9 |
| 32k_single | 2048 | 1 / 8 | 1 | 2.64 | 2.62 | 59.72 | 2.96 | 65.71 | 22.041–23.126 | 31/40 | 30.9/59.1 |
| 32k_pair | 2048 | 2 / 8 | 2 | 2.68 | 2.65 | 83.00 | 3.00 | 89.01 | 30.493–31.863 | 31/40 | 30.9/59.1 |
| 32k_shared | 2048 | 8 / 8 | 8 | 2.69 | 2.69 | 35.76 | 3.04 | 40.53 | 12.909–13.583 | 31/43 | 30.9/60.8 |
| 32k_64owners | 2048 | 64 / 8 | 64 | 2.69 | 2.70 | 41.84 | 3.04 | 47.45 | 15.170–15.922 | 31/52 | 30.9/79.5 |
| 32k_256owners | 2048 | 256 / 8 | 256 | 2.68 | 2.67 | 61.81 | 3.02 | 67.71 | 22.548–23.622 | 31/58 | 30.9/143.2 |
| 32k_64targets | 2048 | 64 / 64 | 64 | 13.04 | 13.17 | 52.68 | 14.03 | 58.38 | 3.933–4.081 | 58/79 | 80.7/129.3 |
| 32k_root | 2048 | 64 / 8 | 0 | 65.28 | 64.25 | 93.63 | 71.11 | 100.62 | 1.433–1.527 | 107/118 | 194.2/207.3 |
| 32k_staggered | 2048 | 64 / 8 | 64 | 2.74 | 2.71 | 90.09 | 3.06 | 96.50 | 32.130–33.475 | 31/59 | 30.9/83.8 |
| 32k_fragmented | 2048 | 64 / 8 | 3072 | 2.74 | 2.68 | 253.60 | 3.04 | 261.99 | 91.922–96.394 | 31/66 | 30.9/549.9 |
| 32k_fully_local | 2048 | 64 / 8 | 3072 | 2.68 | 2.73 | 136.81 | 3.07 | 143.40 | 49.096–51.728 | 26/53 | 40.0/516.8 |
| 32k_islands | 2048 | 64 / 64 | 64 | 13.04 | 13.18 | 101.77 | 14.20 | 108.35 | 7.585–7.901 | 58/104 | 80.7/142.9 |
| 128k_shared | 8192 | 256 / 64 | 256 | 14.40 | 14.75 | 176.57 | 15.66 | 185.49 | 11.803–12.238 | 58/85 | 146.7/340.1 |
| 128k_fragmented | 8192 | 64 / 64 | 12288 | 14.55 | 14.62 | 1143.16 | 15.61 | 1164.96 | 76.009–79.190 | 58/97 | 146.7/2183.7 |

The large ratios for suffix inventories have a small baseline denominator: the disabled mode
cannot discover those sources. They are added cost for added capability, not equivalent-work
slowdowns. `root` already has useful hints without the feature, while `no_sources` and
`fully_local` have no useful continuation result. Every enabled/off p50 comparison clears its
A/A floor, with six of six paired increases; exact two-sided p is 0.00216 per cell (unadjusted).

## Where the cost goes

The phases below are timed independently, so their medians need not sum to the combined median.
Source ranking includes summary construction and all per-target lookups.

| Case | Enabled lower-tier lookup p50, us | Enabled ranking p50, us |
|---|---:|---:|
| 4k_shared | 6.20 | 0.84 |
| 32k_no_sources | 24.00 | 0.31 |
| 32k_single | 58.84 | 0.43 |
| 32k_pair | 81.96 | 0.49 |
| 32k_shared | 34.44 | 0.85 |
| 32k_64owners | 37.23 | 4.19 |
| 32k_256owners | 45.09 | 15.92 |
| 32k_64targets | 45.60 | 6.54 |
| 32k_root | 90.32 | 3.56 |
| 32k_staggered | 84.58 | 4.02 |
| 32k_fragmented | 108.17 | 144.37 |
| 32k_fully_local | 105.62 | 26.55 |
| 32k_islands | 92.87 | 8.37 |
| 128k_shared | 155.99 | 18.44 |
| 128k_fragmented | 487.35 | 603.47 |

One/two owners are not faster than eight in the shared-suffix case: the existing owner-set
fingerprint coalescing applies to Multi entries, while Single/Pair run the per-block path.
This is a code-path explanation consistent with the measurements, not a separate optimized A/B.
Fragmented inventories spend substantial time both discovering and ranking intervals. With all
targets fully local, summary filtering rejects them only after the interval list has been built,
copied, and sorted. The 3,072-interval fully-local fixture requests 516.8 KiB per operation and
produces no fetch choice. The 12,288-interval long fragmented fixture requests 2,183.7 KiB.

## Owner/cardinality model

Let B be the reachable prompt length in blocks, T eligible destination worker/ranks, S distinct
source identities seen along that chain, R distinct owner-set runs, and I emitted suffix intervals.
S includes worker/rank residency owners (including those later rejected for source capability)
and independently advertised cache owners; it is bounded by their
union along the matched chain, not by T and not necessarily by the whole fleet size. A thin target
frontier or round-robin session placement does not by itself prove a thin source inventory.

- Discovery costs O(B + sum of owner counts over changed runs). A stable identical Multi owner
  set coalesces to approximately O(B+S). Changing sets can approach O(B*S). Staggered one-suffix
  owners can require O(S²) visits even though I=S; fragmented sources can make I=O(B*S).
- Source-summary construction allocates I references and sorts them by start, with an
  O(I log I) upper bound, then groups eligible entries by worker type and applicable topology.
  Sources ending no later than every target's local prefix are filtered after that sort.
- Per-target selection checks at most two relevant buckets. Root-only lookup is O(1);
  continuation lookup is O(log distinct starts), not a scan over S. This keeps target lookup
  cheap, but does not eliminate discovery or summary cost.
- Retained request candidates use O(B+P+I) space, where P is root-prefix candidates. Queue depth
  multiplies retained space. Current lookup and refresh handoffs clone candidates before dropping
  their source; the combined measurement includes this copy, but does not measure queue residency.

These results support keeping the feature opt-in. Before broad/default-on exposure, the clearest
cost improvements are to skip discovery that no original destination frontier can use, filter
before sorting, and move candidates at the handoff instead of cloning them. None is implemented
or credited as a speedup in this report. Actual serving benefit still needs a closed-loop run
with fetches, eviction, placement and engine prefill costs.

## Reproduction and evidence

See [README.md](README.md) for build/run instructions and exact workload definitions.
`results/provenance.json` records revisions, compiler, build settings and binary/source hashes.
`results/{aa,ab,allocations}/` contains every raw trial, placement/CPU record, trial order and
checksums. `results/analysis.json` contains the complete statistics. Source, lockfile and frozen
binary identities were verified before execution. Only public OSS inputs and synthetic inventories
are included. No source file in the production PR was changed by this evaluation.
