# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Make a repeatable Mooncake cost fixture from four public AgentX plays.

This is a workload-shape comparison, not John's exact 128-session Grace input.
The four plays are replicated into 128 independent sessions. Hash identities are
preserved within each session and shared only for the prepended 16,384 tokens.
No peer fetch, agent dependency graph or serving-latency fidelity is claimed.
"""
import argparse
import hashlib
import json
from pathlib import Path

SOURCE_SHA256 = "51fd311c4f00acd6d4d457fba29653af03613cd1ee2611833dc40dfbe988dd60"


def model_requests(entries):
    for entry in entries:
        if "in" in entry:
            yield entry
        else:
            assert entry["type"] == "subagent"
            yield from model_requests(entry["requests"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="AISimulate's pinned public fixtures/agentx.jsonl")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    data = args.source.read_bytes()
    assert hashlib.sha256(data).hexdigest() == SOURCE_SHA256
    plays = [json.loads(line) for line in data.splitlines()]
    assert len(plays) == 4
    rows = []
    next_hash = 256
    for session in range(128):
        play = plays[session % 4]
        assert play["block_size"] == 64 and play["hash_id_scope"] == "local"
        requests = sorted(model_requests(play["requests"]), key=lambda r: r["t"])
        start = min(r["t"] for r in requests)
        mapping = {}
        for request in requests:
            hashes = []
            for original in request["hash_ids"]:
                if original not in mapping:
                    mapping[original] = next_hash
                    next_hash += 1
                hashes.append(mapping[original])
            assert len(hashes) == (request["in"] + 63) // 64
            assert next_hash < 2**32
            rows.append({
                "session_id": f"agentx-{session}",
                "timestamp": (request["t"] - start) * 1000,
                "input_length": request["in"] + 16384,
                "output_length": request["out"],
                "hash_ids": list(range(256)) + hashes,
            })
    assert len(rows) == 255 * 32
    assert sum(row["input_length"] - 16384 for row in rows) == 27418752 * 32
    assert sum(row["output_length"] for row in rows) == 286917 * 32
    rows.sort(key=lambda row: row["timestamp"])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w") as stream:
        for row in rows:
            stream.write(json.dumps(row, separators=(",", ":")) + "\n")
    print(json.dumps({"source_sha256": SOURCE_SHA256,
        "output_sha256": hashlib.sha256(args.output.read_bytes()).hexdigest(),
        "sessions": 128, "rows": len(rows), "trace_block_size": 64,
        "common_prefix_tokens": 16384,
        "input_tokens": sum(row["input_length"] for row in rows),
        "output_tokens": sum(row["output_length"] for row in rows)}, sort_keys=True))


if __name__ == "__main__":
    main()
