<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# CPU fetch-continuation validation

This harness checks Dynamo's standard fetch-continuation semantics against the public
KVCR implementation. It needs no GPU, model weights, etcd, or NATS. It makes no
latency, throughput, GPU, RDMA, or real-inference claim.

## What runs

The Rust fixture uses Dynamo's real device/lower-tier indexers, default worker
selection policy, selection/reservation service, and hint construction. A small
HTTP adapter accepts the production `SelectAndReserveRequest` wire format. The
inventory is deterministic: destination A has blocks 1–10, source B has only
11–15, destination D has 1–15, and source C has only 16–20. The fixture injects
normal `RouterEvent` records and checks its dump before accepting requests.

Python uses real KVCR objects, managed DRAM residency, hint parsing, descriptor
resolution, peer-control messages over loopback ZMQ, fetch and deliver lifecycles,
and terminal completion handling. Sources contain only the stated suffixes in
KVCR-managed DRAM. Framework pinning returns misses, preventing an accidental
fallback from inventing missing source blocks.

The engine is replaced by the Python driver, CPU framework buffers, and a pinning
callback that always reports misses. NIXL is replaced by a byte-copy adapter.
Each peer has registered CPU buffers. The adapter implements
NIXL's transfer/notification boundary using `ctypes.memmove`, checks transfer sizes,
and sends the notification attached to each completed transfer. KVCR remains
responsible for choosing and pinning the source, resolving source/destination
memory, requesting transfers, and marking blocks ready. A successful status alone
is insufficient: the test delivers the result into a separate framework buffer
and checks every byte against an independent per-block pattern.

The driver pins the destination in each routing request, and the HTTP fixture
releases its reservation before returning the hint. It checks per-destination
source selection, not load balancing or reservation lifetime through inference.

This is a semantic integration test. It does not exercise a Dynamo frontend,
engine launcher, discovery/state-agent pipeline, real model execution, or native
NIXL. The public mocker work in
[ai-dynamo/dynamo#16423](https://github.com/ai-dynamo/dynamo/pull/16423) instead
exercises online hint delivery but deliberately ignores the hints. This harness
fills the fetch-consumer validation gap without claiming full serving coverage.

## Assertions

- A gets one B hint through block 15; D gets one C hint through block 20.
  A never gets an implicit B-then-C transfer plan.
- Every payload contains the exact root-aligned request hashes. KVCR skips its
  locally resident prefix and fetches only the missing suffix.
- Successful fetches produce the expected bytes; later uncredited blocks remain
  absent. Each request uses a fresh destination so repetition cannot turn this
  into a warm-cache-only test.
- A deliberately wrong source (C for A's missing blocks 11–15) fails. This is a
  negative control against a mock that pretends every block exists.

The driver is closed-loop with one outstanding request: it books, fetches, verifies,
and closes that destination before issuing the next request. Its request count is
coverage, not a performance measurement.

A stale-prefix control books the same hint for A while the real destination retains
only blocks 1–8. It asserts failures and subsequent `MISS` for blocks 9–10,
successful fetches for 11–15, and exact delivered bytes on both sides of the gap.
This tests KVCR's partial-success contract, not an engine's recomputation path.

The public vLLM 0.32.0 consumer was also inspected at
`5df63a7203cdb9328d824a27b682e77c7b9928be`: its KVCR tier reports successful keys
individually, the tiering manager commits those promotions and removes failed
ones, and the offloading scheduler stops prefix credit at the first `MISS`.
The core scheduler then computes the remaining tokens. That is a source-code
check of one pinned consumer, not an executed vLLM or SGLang integration test.

## Reproduce

Use a Linux CPU host and a virtual environment belonging to this Dynamo checkout.
The public KVCR test adapter is pinned to commit
`34bb11986e096e249c76186cd60e28091f2fcb2e`; the script rejects another revision.
Use that same revision for the installed KVCR package and its test helpers. The
checkout must be clean. The driver also verifies that the loaded KVCR package's
Python sources match that checkout and rejects mixed package locations.
Its normal package dependencies include NIXL; this harness
imports the package but replaces agent creation before any native agent starts.
Install dependencies using the package source configured for your environment.

```sh
# KVCR_ROOT points to a clean public ai-dynamo/kvcr checkout at the pinned commit.
uv venv .venv
uv pip install --python .venv/bin/python "$KVCR_ROOT" numpy pytest
cargo build --locked --manifest-path validation/fetch-continuation/Cargo.toml
PYTHONPATH="$KVCR_ROOT/tests/unit" .venv/bin/python \
  validation/fetch-continuation/run.py \
  --probe validation/fetch-continuation/target/debug/fetch-continuation-validation \
  --requests 32
```

All listeners bind loopback on kernel-assigned ports. The driver owns each router
process and closes it by recorded process handle; KVCR peers are closed before
buffer owners go away. No services are left running.

Validated result: 32 successful suffix byte fetches,
one rejected wrong-source transfer, and one stale-prefix partial-success check.
See `result.json`. Production correctness tests
also cover gaps/eviction/dump replay, topology required/preferred modes, cache-owner
attachment exclusion, and a seeded source-inventory oracle. There is no continuation configuration switch. The older opt-in harness and its results
remain available at validation-branch commit `1356e31ee4`.
