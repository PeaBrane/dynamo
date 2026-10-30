#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Analyze every recorded trial; no latency-based exclusions or pooled-request CIs."""
import argparse
import hashlib
import itertools
import json
from pathlib import Path
import random
import statistics as st


def trials(directory):
    arms = {}
    for filename in sorted(directory.glob("[0-9][0-9]-*.json")):
        if filename.name.endswith(".host.json"):
            continue
        arm = filename.stem.split("-", 1)[1]
        value = json.loads(filename.read_text())
        arms.setdefault(arm, []).append(value)
    return arms


def comparison(a, b):
    rng = random.Random(17657)
    boot = sorted(st.median(rng.choices(b, k=len(b))) / st.median(rng.choices(a, k=len(a)))
                  for _ in range(10000))
    values = a + b
    ranks = [1 + sum(w < v for w in values) + (sum(w == v for w in values) - 1) / 2
             for v in values]
    expected = len(a) * (len(values) + 1) / 2
    observed = abs(sum(ranks[:len(a)]) - expected)
    perms = list(itertools.combinations(ranks, len(a)))
    p = sum(abs(sum(r) - expected) >= observed - 1e-10 for r in perms) / len(perms)
    return {"ratio": st.median(b) / st.median(a), "ci95": [boot[249], boot[9749]],
            "mann_whitney_exact_p": p,
            "paired_increase": sum(y > x for x, y in zip(a, b)),
            "n": [len(a), len(b)]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    args = parser.parse_args()
    provenance = json.loads((args.results / "provenance.json").read_text())
    for phase in ["aa", "ab", "allocations"]:
        manifest = json.loads((args.results / phase / "manifest.json").read_text())
        for binary, digest in manifest["binary_sha256"].items():
            assert digest == provenance["binary_sha256"][binary]
        for line in (args.results / phase / "SHA256SUMS").read_text().splitlines():
            digest, filename = line.split("  ", 1)
            assert hashlib.sha256((args.results / phase / filename).read_bytes()).hexdigest() == digest
    aa, ab, allocations = (trials(args.results / p) for p in ["aa", "ab", "allocations"])
    assert {a: len(v) for a, v in aa.items()} == {"a": 6, "b": 6}
    assert {a: len(v) for a, v in ab.items()} == {"base": 6, "off": 6, "on": 6}
    assert {a: len(v) for a, v in allocations.items()} == {"alloc-off": 2, "alloc-on": 2}
    rows = []
    for index, ref in enumerate(ab["off"][0]["results"]):
        for arms in [aa, ab, allocations]:
            for runs in arms.values():
                assert all(v["results"][index]["case"] == ref["case"] for v in runs)
        # Discrete outcomes are deterministic across all repeats and baseline/off.
        for arm in ["base", "off"]:
            assert all(v["results"][index]["validation"] == ref["validation"] for v in ab[arm])
        for arm in ["a", "b"]:
            assert all(v["results"][index]["validation"] == ref["validation"] for v in aa[arm])
        for arm in ["off", "on"]:
            oracle = ab[arm][0]["results"][index]["validation"]
            assert all(v["results"][index]["validation"] == oracle for v in ab[arm])
            assert all(v["results"][index]["validation"] == oracle for v in allocations[f"alloc-{arm}"])
        row = {"case": ref["case"], "validation": {a: ab[a][0]["results"][index]["validation"]
                                                   for a in ["off", "on"]}, "metrics": {}}
        for phase in ["lookup", "ranking", "pipeline"]:
            row["metrics"][phase] = {}
            for metric in ["p50_ns", "p99_ns", "mean_ns"]:
                values = {a: [v["results"][index]["metrics"][phase][metric] for v in ab[a]]
                          for a in ["base", "off", "on"]}
                noise = {a: [v["results"][index]["metrics"][phase][metric] for v in aa[a]]
                         for a in ["a", "b"]}
                row["metrics"][phase][metric] = {
                    "medians": {a: st.median(v) for a, v in values.items()},
                    "ranges": {a: [min(v), max(v)] for a, v in values.items()},
                    "aa": comparison(noise["a"], noise["b"]),
                    "off_vs_base": comparison(values["base"], values["off"]),
                    "on_vs_off": comparison(values["off"], values["on"]),
                }
        row["allocations"] = {}
        for arm in ["off", "on"]:
            runs = [v["results"][index]["metrics"] for v in allocations[f"alloc-{arm}"]]
            assert runs[0] == runs[1]
            row["allocations"][arm] = runs[0]
        rows.append(row)
    host = [json.loads(p.read_text()) for p in args.results.glob("*/*.host.json")]
    timed_host = [json.loads(p.read_text()) for phase in ["aa", "ab"]
                  for p in (args.results / phase).glob("*.host.json")]
    summary = {"schema": 1, "rows": rows, "host": {
        "timed_trials": len(timed_host),
        "max_timed_foreign_fraction": max(h["foreign_cpu_seconds"] / h["wall_seconds"] for h in timed_host),
        "trials": len(host),
        "foreign_cpu_seconds_range": [min(h["foreign_cpu_seconds"] for h in host),
                                      max(h["foreign_cpu_seconds"] for h in host)],
        "max_foreign_fraction": max(h["foreign_cpu_seconds"] / h["wall_seconds"] for h in host),
        "steal_seconds_total": sum(h["steal_seconds"] for h in host),
    }}
    (args.results / "analysis.json").write_text(json.dumps(summary, indent=2) + "\n")
    print("case | pipeline p50 us base/off/on | on/off 95% CI | p99 off/on us | alloc calls off/on | bytes off/on")
    for row in rows:
        m = row["metrics"]["pipeline"]
        p50 = m["p50_ns"]
        medians = p50["medians"]
        ci = p50["on_vs_off"]["ci95"]
        alloc = row["allocations"]
        print(f"{row['case']['name']} | "
              + "/".join(f"{medians[a]/1000:.2f}" for a in ["base", "off", "on"])
              + f" | {ci[0]:.3f}–{ci[1]:.3f} | "
              + "/".join(f"{m['p99_ns']['medians'][a]/1000:.2f}" for a in ["off", "on"])
              + " | " + "/".join(str(alloc[a]["allocations"]) for a in ["off", "on"])
              + " | " + "/".join(str(alloc[a]["allocated_bytes"]) for a in ["off", "on"]))


if __name__ == "__main__":
    main()
