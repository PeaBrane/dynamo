# Fetch-continuation component cost probe

This standalone harness measures the incremental router CPU cost in
[ai-dynamo/dynamo#17657](https://github.com/ai-dynamo/dynamo/pull/17657).
See [REPORT.md](REPORT.md) for the recorded comparison, [results/](results/) for every trial,
and [../fetch-continuation/](../fetch-continuation/) for the separate byte-verified CPU fetch harness.

## Scope

Each process builds a fixed, hash-consistent HostPinned inventory through normal store events.
The device match is a valid, precomputed result. It then times three components separately:

1. `query_lower_tiers_with_options`, including continuation discovery when enabled.
2. `KvTransferSourceSummary::new` plus `source_for` for every eligible target, from a saved lookup.
3. The combined lookup, candidate clone/drop handoff used by the selection service, summary,
   and per-target source choices, including destruction of temporary results.

The third measurement is called `pipeline` in JSON. It excludes token hashing, the device index
lookup, overlap analysis, full worker scoring/reservations, hint JSON serialization, queues,
event/query contention, fetch transport, and inference. This is a warm-cache, serial component
service-time probe, not an end-to-end request or sustained serving-throughput benchmark.
No runtime discovery services are required.

The ordinary binary uses the system allocator with no instrumentation. The separately compiled
`allocations` binary counts allocation/reallocation calls and total requested bytes for the
combined component on the calling thread. Requested bytes include reallocation's new size;
they are allocation traffic, not peak memory or retained heap. Timing from that binary is not used.

## Inventories and correctness

The fixed matrix has 15 cells, defined in `src/main.rs`. Blocks represent 16 tokens. Prompt
length is 256, 2,048, or 8,192 blocks (4,096, 32,768, or 131,072 token equivalents). Exact cache
owner counts are 0, 1, 2, 8, 64, or 256; destination counts are 8 or 64. These are synthetic scale
points, not claims about a production distribution.

- `root`: all sources hold the full chain; existing root-prefix hints already work.
- `suffix`: all sources hold the second half of the chain.
- `staggered`: source starts vary across the middle half; ends vary across the final eighth.
- `fragmented`: owners alternate 16-block present/absent runs with opposite parities, beginning
  at one-quarter depth. The shared chain remains discoverable, but each source has real gaps.
- `fully_local`: the fragmented inventory, with every destination already holding the full chain.
- `no_sources`: an allocated but empty host index, as after all memberships have been evicted.
- `islands`: staggered sources split across eight topology islands.

Ordinary destination frontiers span one-half to eleven-sixteenths of the prompt. One-quarter of
sources are attached to a destination and must be excluded for that destination. Others are
unattached; advertised topology still constrains source eligibility. Destination workers consume
hints but do not publish source endpoints, keeping the independent owner and target axes explicit.

Every process validates source choices against an independent boolean-membership oracle,
including tie-breaking, attachment and topology restrictions. Enabled runs also compare the full
set of emitted intervals against maximal contiguous bitmap ranges. Root-aligned hashes and
unchanged local hits/continuations are checked. Analysis requires identical discrete outcomes
across repeats, allocations builds, and base/default-off arms. Correctness failures stop the run.

This is a static-inventory regression check, not the campaign's write-path soak or eviction model.
The separate existing CPU fetch harness validates payload consumption and actual copied bytes.

## Build and reproduce

Use two checkouts of the commits recorded in `results/provenance.json`. Place this identical
standalone crate at `validation/fetch-continuation-perf` in both. It owns its lockfile and does
not require changing the repository workspace manifest. Preserve `.cargo/config.toml` and use
the same compiler and target flags in both checkouts.

From the feature checkout:

```bash
cargo build --release --locked --manifest-path validation/fetch-continuation-perf/Cargo.toml
# Copy target/release/fetch-continuation-perf from this standalone crate to MEASURE/bins/feature.
cargo build --release --locked --features allocations \
  --manifest-path validation/fetch-continuation-perf/Cargo.toml
# Copy this separate instrumented executable to MEASURE/bins/alloc.
```

From the base checkout:

```bash
cargo build --release --locked --no-default-features \
  --manifest-path validation/fetch-continuation-perf/Cargo.toml
# Copy this executable to MEASURE/bins/base.
```

`continuation` only adapts the harness to the new API field. Feature/off and feature/on use the
same frozen executable; the command-line mode selects the runtime flag. Base rejects `on`.
The baseline and feature dependency lockfiles are identical.

Copy `run-one.sh` to `MEASURE/`, freeze the three binaries, finish all builds, and check host
load/topology. The default measured CPU is 2; choose an available physical core and leave its
SMT sibling unused by the task. All event writes finish before timing; their lane is then idle.
Create a Python environment in the repository root; the runner and analysis use only stdlib.

```bash
.venv/bin/python validation/fetch-continuation-perf/run.py \
  --root MEASURE --out RESULTS/aa --phase aa --ms 150 --cpu 2
.venv/bin/python validation/fetch-continuation-perf/run.py \
  --root MEASURE --out RESULTS/ab --phase ab --ms 150 --cpu 2
.venv/bin/python validation/fetch-continuation-perf/run.py \
  --root MEASURE --out RESULTS/allocations --phase allocations --cpu 2
.venv/bin/python validation/fetch-continuation-perf/analyze.py RESULTS
```

`--ssh HOST` and optional `--socket SOCKET` run the same frozen files on a verified remote host.
`--capsule` uses a process-local loader and libraries in `MEASURE/runtime/` when needed; it never
changes system libraries. Otherwise the binary uses the host runtime. Paths and SSH metadata are
not saved in portable results. The recorded run used glibc 2.39 for both arms.

A/A runs feature/off as two labels, six fresh processes each. A/B uses six balanced trials each
of base, feature/off, and feature/on. Every case gets 30 ms warm-up per metric followed by at least
150 ms and 256 observations (at most one million). All trials are retained. A/A is required
before interpreting small deltas. Bootstrap intervals resample per-process summaries, not the
many correlated requests within a process. Multiple cells are exploratory; no familywise
significance claim is made.

`run-one.sh` records process CPU time, total busy time on the pinned CPU, steal, and elapsed time.
Foreign CPU is their difference over the whole process, including fixture setup. Values near zero
can be negative because Linux CPU counters and `/usr/bin/time` are tick/centisecond-granular.
They do not establish exclusive access to the underlying physical machine.
