<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Frontier-seeded lookup replay

See [RESULTS.md](RESULTS.md) for the completed six-runs-per-arm comparison,
including A/A noise, queueing, final drain, and all trial outcomes.

This compares main `670019d948` with the continuation feature applied to the same
revision. It uses the existing `mooncake_bench` lower-tier mode with concurrent
device and host events. It measures indexer lookup cost, including continuation
discovery, but excludes scheduler source-summary construction and selection.
It does not simulate peer fetches, inference, or serving throughput.

The input approximates the shape of the reviewer's AgentX workload. It is not
the reviewer's exact trace or a reproduction of the Grace result. Both arms run
on the same x86 CPU host with the same runtime and dependencies.

## Public input

`prepare_agentx.py` reads the four complete public plays at
[`ai-dynamo/aisimulate@63f6074a5fd57fdb3b777372110d676ee7300e8d`](https://github.com/ai-dynamo/aisimulate/blob/63f6074a5fd57fdb3b777372110d676ee7300e8d/python/aisimulate/tools/simulation_perf_gate/fixtures/agentx.jsonl).
The source and generator are Apache-2.0. The generator verifies the fixture hash,
includes nested model requests, replicates the four plays into 128 sessions,
and prepends a shared 16,384-token prefix. It preserves each session's hash
identity and gives different sessions distinct identities after that prefix.
It flattens agent dependencies into fixed request timestamps.

The resulting 8,160 requests contain 1,011,093,504 input tokens and 9,181,344
output tokens. Source hash units are 64 tokens; engine blocks are 16 tokens.
The generated trace SHA-256 is
`e7e25eeff6222d9e3af17ad3d2d8d9e84a789996b5df2cdba78a6eb00c3aecb0`.
The generated trace is not included in this branch.

## Build and run

Use separate checkouts and mutable Cargo targets for main and candidate. For
both arms, the campaign uses the same local-only workspace trim to `lib/bench`
and points `aisimulate-core` at public AISimulate
`c2eb4f640bf87d85b77c559d88e22316268ce284` (`crates/core`). That snapshot supplies
the placement callbacks required by this Dynamo main revision. These build
overrides are excluded from the production PR.

```sh
cargo bench --locked -p dynamo-bench --no-default-features \
  --features mooncake --bench mooncake_bench --no-run
```

Freeze each executable under `bins/main` or `bins/candidate` and record its
SHA-256. Stage the trace as `traces/agentx128.jsonl` and this directory's
`run-one.sh` in a separate measurement directory. The wrapper uses a
process-local runtime capsule under `runtime/`; it does not change system
libraries. On a host with matching libraries the loader prefix can be removed
from the wrapper before freezing it, identically for both arms.

`run-one.sh` records the exact flags: 16 workers, GPU/CPU/KVCR capacities of
131,072/32,768/262,144 blocks per worker, CRTC with four event workers, and two
query lanes. The backend uses physical cores 2–13, with separate event and
query issuers on 14 and 15. Their SMT siblings are unused by this task.

The frozen plan starts with a 120,000 ms scout per arm. If either arm fails
generator or keep-up gates, double the common duration at most twice before
freezing repeated trials. Then run six fresh processes per A/A arm and six
per main/candidate arm in balanced ABBA order. Keep all outcomes.

```sh
# Run from the selected Dynamo checkout's environment.
.venv/bin/python validation/fetch-continuation-perf/mooncake/run.py \
  --root "$MEASUREMENT_ROOT" --out "$RAW_RESULTS/scout" \
  --phase scout --duration-ms 120000
# Repeat with --phase aa and --phase ab, each with its own output directory.
# For a remote host, add --ssh "$VERIFIED_HOST" --socket "$HELD_SOCKET".
```

The runner verifies a held SSH socket before each remote operation and freezes
binary, trace, and wrapper hashes. Raw output includes operational paths and
must stay private. `analyze-mooncake.py` verifies checksums, workload totals,
final memberships and trial counts; retains every outcome; and exports results
without paths, hostnames or command lines. It replaces the harness's loader
hash with the separately frozen executable hash. Run-level bootstrap intervals
and A/A quantify noise; request samples are not pooled across runs.

`soak.rs` is the earlier hash-consistent differential oracle, retained for
reproduction. Its model and checks are unchanged; the added empty
`source_intervals` field only adapts an existing candidate literal. It covers
edge/reverse-map consistency, projected local hits and continuations, root
workers, source root-prefix credits, and dump/replay. Focused production tests
separately check suffix-source coverage against independent owner inventories.
