<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Fused continuation discovery: main comparison

On this fixed x86 replay, the fused implementation's lower-tier lookup p50 is
4.1% above main and p99 is 3.7% below main. The large regression reported for
the earlier global-scan implementation is not present in this comparison.
This is directional evidence: it uses a public AgentX-shaped trace, not the
reviewer's exact trace or Grace CPU, and does not establish exact parity.

## Results

Each value is the median of six fresh-process trial statistics. The intervals
are 95% bootstrap intervals for the candidate/main ratio, expressed as changes.
All trials are retained; request samples are not pooled across processes.

| Metric | Main | Candidate | Change [95% interval] |
| --- | ---: | ---: | ---: |
| Lower-tier lookup p50 | 1.162 ms | 1.209 ms | +4.1% [+0.3%, +6.2%] |
| Lower-tier lookup p99 | 3.835 ms | 3.692 ms | −3.7% [−12.3%, +2.6%] |
| Whole lookup p50, device + lower tier | 1.182 ms | 1.230 ms | +4.1% [+0.3%, +6.2%] |
| Whole lookup p99, device + lower tier | 3.892 ms | 3.732 ms | −4.1% [−12.4%, +2.6%] |
| Query scheduled-to-finished p99 | 409.757 ms | 442.559 ms | +8.0% [+0.7%, +20.1%] |
| Observed device event accepted-to-finished p99 | 0.812 ms | 0.794 ms | −2.2% [−7.6%, +5.7%] |
| Final drain | 34.889 ms | 58.136 ms | +66.6% [+56.1%, +87.0%] |

The A/A comparison used the same main executable under two labels, six fresh
processes each. Its lower-tier p50 ratio interval was 0.852–1.111 and its p99
interval was 0.781–1.265. The A/B p50 increase is nominally significant within
that sample (5/6 adjacent pairs increased; exact Mann–Whitney p=0.041), but its
magnitude is smaller than the A/A resolution. Treat it as observed small
overhead, not proof of parity or a noise-qualified speedup. The p99 decrease
is inconclusive. Scheduled-to-finished p99 also increased in 5/6 pairs, with a
nominal p=0.026; its 8% point change is within the wider A/A variation.

The drain increase is consistent in all six pairs and exceeds its A/A range:
about 23 ms more at the end of a 120-second replay. The workload is bursty;
maximum queued queries reached 1,129 on main and 1,171 on candidate. Main had
2–9 queries queued at producer stop, versus 50–54 on candidate. Passing the
keep-up gate therefore does not mean queues stayed empty.

Both arms passed all generator and keep-up checks, including the two scouts
and twelve A/A runs (26 processes total). The harness requires exact completion,
issue span within 101% of the offered window, and completion within 110% for
keep-up. Every process ran 8,160 queries and 70,298,545 block operations. Final
CPU-tier and KVCR index memberships matched the generator in every process:
524,288 and 3,185,464 respectively.

The offered rate was fixed at 585,821 block operations/s. Achieved rates,
including final drain, were 585,607/s on main and 585,494/s on candidate
(−0.019%). This run does not locate a capacity knee or demonstrate increased
sustained throughput. It does not simulate serving or fetch benefits.

## Reproduction and scope

- Main: `670019d94846c73826feb931d438f69d94b19733`.
- Candidate: the production delta at `b33081734f7946a6cb6592757a4fa2f24f73d7c9`
  applied to that same main revision. The subsequent PR commit `36779226f0`
  only removes redundant tests and a comment; runtime code is unchanged.
- Identical Rust 1.96.1 release builds, dependencies, allocator, trace, placement
  and flags. [Provenance](provenance.json) records source and binary hashes.
- Intel Xeon 6975P-C, 16 physical cores, one NUMA node. Backend cores 2–13;
  event and query issuers on 14 and 15; SMT siblings unused by the experiment.
- 128 sessions, a shared 16k-token prefix, 16 workers, 16-token blocks,
  GPU/CPU/KVCR capacities of 131,072/32,768/262,144 blocks per worker,
  CRTC with four event workers, and two query lanes. See [README](README.md).
- Two scouts, then twelve A/A and twelve A/B processes, each phase in fixed
  ABBA order at 120,000 ms. No runs were discarded or added after looking at
  the result.
- Across the 24 repeated trials, maximum foreign CPU time averaged 0.371 of
  one core over a whole process, and total steal time was 0.58 seconds.
  Those accounting windows include trace preparation, so they are not
  lower-tier CPU-cost measurements.

This measures lookup under concurrent events, including fused continuation
discovery. It excludes source-summary construction, destination selection,
peer fetches, engine scheduling, and inference. Lower-tier events do not have
per-event completion observations; the event-latency row describes device
events. Correctness evidence for the production change and real-KVCR CPU
byte transfers is recorded in the PR and the separate fetch harness.

[Complete exported trials](results/), [analysis](results/analysis.json), and
[checksums](results/SHA256SUMS) preserve all outcomes. The export removes
operational paths and command lines and attributes the frozen executable
hash rather than the runtime loader's hash.
