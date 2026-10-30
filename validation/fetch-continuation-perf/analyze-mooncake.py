#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Compare all frozen Mooncake trials and export results without operational paths."""
import argparse
import hashlib
import json
from pathlib import Path
import statistics as st

from analyze import comparison


def write_json(filename, value):
    filename.write_text(json.dumps(value, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("raw", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    phases = {}
    discrete = None
    hashes = None
    host = []
    for phase in ["scout", "aa", "ab"]:
        src = args.raw / phase
        dst = args.output / phase
        dst.mkdir()
        for line in (src / "SHA256SUMS").read_text().splitlines():
            digest, name = line.split("  ", 1)
            assert hashlib.sha256((src / name).read_bytes()).hexdigest() == digest
        manifest = json.loads((src / "manifest.json").read_text())
        if hashes is None:
            hashes = manifest["sha256"]
        assert manifest["sha256"] == hashes
        write_json(dst / "manifest.json", manifest)
        arms = {}
        for index, arm in enumerate(manifest["order"]):
            name = f"{index:02d}-{arm}.json"
            result = json.loads((src / name).read_text())
            assert (src / (name + ".exit")).read_text().strip() == "0"
            # The harness records the runtime loader and private paths in provenance.
            # Attribute the actual executable from the separately frozen manifest.
            result.pop("provenance", None)
            result.pop("backend_timing_report", None)
            variant = "main" if arm in {"a", "b"} else arm
            result["binary_sha256"] = hashes[f"bins/{variant}"]
            result["trace_sha256"] = hashes["traces/agentx128.jsonl"]
            lower = result["lower_tier"]
            observed = {key: value for key, value in result.items() if key.startswith("total_")}
            observed.update({tier: lower[tier] for tier in ["cpu_tier", "kvcr"]})
            if discrete is None:
                discrete = observed
            assert observed == discrete, (phase, index, "workload changed")
            for tier in ["cpu_tier", "kvcr"]:
                assert lower[f"{tier}_indexed_memberships"] == lower[tier]["live_memberships"]
            write_json(dst / name, result)
            host_result = json.loads((src / (name + ".host.json")).read_text())
            write_json(dst / (name + ".host.json"), host_result)
            if phase != "scout":
                host.append(host_result)
            arms.setdefault(arm, []).append(result)
        phases[phase] = arms
    aa, ab = phases["aa"], phases["ab"]
    assert {a: len(v) for a, v in aa.items()} == {"a": 6, "b": 6}
    assert {a: len(v) for a, v in ab.items()} == {"main": 6, "candidate": 6}
    gates = {phase: {arm: {"runs": len(runs),
                          "generator_valid": sum(r["generator_valid"] for r in runs),
                          "kept_up": sum(r["kept_up"] for r in runs),
                          "failures": [r["failure_reasons"] for r in runs]}
                     for arm, runs in arms.items()} for phase, arms in phases.items()}
    metrics = {}
    for metric in ["lower_tier.query_service.p50_ns", "lower_tier.query_service.p99_ns",
                   "query_service.p50_ns", "query_service.p99_ns",
                   "query_scheduled_to_finished.p99_ns",
                   "update_accepted_to_finished.p50_ns", "update_accepted_to_finished.p99_ns",
                   "achieved_block_ops_per_sec", "drain_ns"]:
        def get(record):
            for key in metric.split("."):
                record = record[key]
            return record

        values = {arm: [get(run) for run in runs] for arm, runs in ab.items()}
        noise = {arm: [get(run) for run in runs] for arm, runs in aa.items()}
        result = {"medians": {a: st.median(v) for a, v in values.items()},
                  "ranges": {a: [min(v), max(v)] for a, v in values.items()}}
        if all(min(v) > 0 for v in list(values.values()) + list(noise.values())):
            result["aa"] = comparison(noise["a"], noise["b"])
            result["candidate_vs_main"] = comparison(values["main"], values["candidate"])
        metrics[metric] = result
    report = {"gates": gates, "metrics": metrics,
              "host": {"timed_trials": len(host),
                       "max_foreign_core_fraction": max(h["foreign_cpu_seconds"] /
                            h["whole_process_wall_seconds"] for h in host),
                       "steal_seconds": sum(h["steal_seconds"] for h in host)}}
    write_json(args.output / "analysis.json", report)
    files = sorted(p for p in args.output.rglob("*") if p.is_file())
    (args.output / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.relative_to(args.output)}\n" for p in files))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
