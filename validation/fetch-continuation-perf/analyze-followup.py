#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Compare the original enabled feature with standard continuation semantics."""
import argparse
import hashlib
import json
import statistics as st

from pathlib import Path

from analyze import comparison, trials


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    args = parser.parse_args()
    provenance = json.loads((args.results / "provenance.json").read_text())
    phases = ["follow-aa", "follow-ab", "follow-allocations"]
    for phase in phases:
        directory = args.results / phase
        manifest = json.loads((directory / "manifest.json").read_text())
        for binary, digest in manifest["binary_sha256"].items():
            assert digest == provenance["binary_sha256"][binary]
        for line in (directory / "SHA256SUMS").read_text().splitlines():
            digest, name = line.split("  ", 1)
            assert hashlib.sha256((directory / name).read_bytes()).hexdigest() == digest
    aa, ab, allocations = [trials(args.results / phase) for phase in phases]
    assert {a: len(v) for a, v in aa.items()} == {"new-a": 6, "new-b": 6}
    assert {a: len(v) for a, v in ab.items()} == {"old": 6, "new": 6}
    assert {a: len(v) for a, v in allocations.items()} == {"old-alloc": 2, "new-alloc": 2}
    rows = []
    for i, ref in enumerate(ab["old"][0]["results"]):
        for arms in [aa, ab, allocations]:
            for runs in arms.values():
                for run in runs:
                    result = run["results"][i]
                    assert result["case"] == ref["case"]
                    assert result["validation"]["choices"] == ref["validation"]["choices"]
        row = {"case": ref["case"], "metrics": {}, "validation": {}, "allocations": {}}
        for arm in ["old", "new"]:
            expected = ab[arm][0]["results"][i]["validation"]
            assert all(run["results"][i]["validation"] == expected for run in ab[arm])
            assert all(run["results"][i]["validation"] == expected for run in allocations[f"{arm}-alloc"])
            row["validation"][arm] = expected
            counted = [run["results"][i]["metrics"] for run in allocations[f"{arm}-alloc"]]
            assert counted[0] == counted[1]
            row["allocations"][arm] = counted[0]
        for runs in aa.values():
            assert all(run["results"][i]["validation"] == row["validation"]["new"] for run in runs)
        for phase in ["lookup", "ranking", "pipeline"]:
            row["metrics"][phase] = {}
            for metric in ["p50_ns", "p99_ns", "mean_ns"]:
                values = {a: [v["results"][i]["metrics"][phase][metric] for v in ab[a]] for a in ab}
                noise = {a: [v["results"][i]["metrics"][phase][metric] for v in aa[a]] for a in aa}
                row["metrics"][phase][metric] = {
                    "medians": {a: st.median(v) for a, v in values.items()},
                    "ranges": {a: [min(v), max(v)] for a, v in values.items()},
                    "aa": comparison(noise["new-a"], noise["new-b"]),
                    "new_vs_old": comparison(values["old"], values["new"]),
                }
        rows.append(row)
    host = [json.loads(p.read_text()) for phase in phases[:2]
            for p in (args.results / phase).glob("*.host.json")]
    result = {"rows": rows, "host": {
        "timed_trials": len(host),
        "max_foreign_fraction": max(h["foreign_cpu_seconds"] / h["wall_seconds"] for h in host),
        "steal_seconds": sum(h["steal_seconds"] for h in host),
    }}
    (args.results / "analysis.json").write_text(json.dumps(result, indent=2) + "\n")
    print("case | pipeline p50 us old/new | new/old CI95 | p99 us old/new | intervals old/new | bytes old/new")
    for row in rows:
        metrics = row["metrics"]["pipeline"]
        p50 = metrics["p50_ns"]
        ci = p50["new_vs_old"]["ci95"]
        print(row["case"]["name"] + " | "
              + "/".join(f"{p50['medians'][a]/1000:.2f}" for a in ["old", "new"])
              + f" | {ci[0]:.3f}–{ci[1]:.3f} | "
              + "/".join(f"{metrics['p99_ns']['medians'][a]/1000:.2f}" for a in ["old", "new"])
              + " | " + "/".join(str(row["validation"][a]["intervals"]) for a in ["old", "new"])
              + " | " + "/".join(str(row["allocations"][a]["allocated_bytes"]) for a in ["old", "new"]))


if __name__ == "__main__":
    main()
