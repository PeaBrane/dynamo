#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Run frozen binaries in balanced fresh-process blocks. All results are retained.

Use a local measurement directory or --ssh with an already staged remote directory.
No hosts, credentials, paths, or command lines are saved in portable result files.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, help="Measurement directory holding bins/ and run-one.sh")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--ssh", help="Verified measurement-host alias, if remote")
    parser.add_argument("--socket", help="Task-owned SSH control socket")
    parser.add_argument("--phase", choices=["aa", "ab", "allocations", "follow-aa", "follow-ab", "follow-allocations"], required=True)
    parser.add_argument("--case", default="all")
    parser.add_argument("--ms", type=int, default=250)
    parser.add_argument("--cpu", type=int, default=2)
    parser.add_argument("--capsule", action="store_true", help="Use runtime/ loader and libraries")
    args = parser.parse_args()
    if bool(args.ssh) != bool(args.socket):
        parser.error("--ssh and --socket must be supplied together")
    args.out.mkdir(parents=True, exist_ok=False)
    ssh = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=12"]
    if args.socket:
        ssh += ["-S", args.socket]
    if args.ssh:
        ssh += [args.ssh]

    def execute(command):
        if args.ssh:
            subprocess.run(["ssh", "-S", args.socket, "-O", "check", args.ssh],
                           capture_output=True, text=True, check=True)
        return subprocess.run(ssh + [command] if args.ssh else ["bash", "-c", command],
                              capture_output=True, text=True, check=True)

    orders = {
        "follow-aa": ["new-a", "new-b", "new-b", "new-a"] * 3,
        "follow-ab": ["old", "new", "new", "old"] * 3,
        "follow-allocations": ["old-alloc", "new-alloc", "new-alloc", "old-alloc"],
        "aa": ["a", "b", "b", "a"] * 3,
        "ab": ["base", "off", "on", "on", "off", "base",
               "off", "on", "base", "base", "on", "off",
               "on", "base", "off", "off", "base", "on"],
        "allocations": ["alloc-off", "alloc-on", "alloc-off", "alloc-on"],
    }
    arms = {"new-a": ("standard", "on"), "new-b": ("standard", "on"),
            "old": ("feature", "on"), "new": ("standard", "on"),
            "old-alloc": ("alloc", "on"), "new-alloc": ("standard-alloc", "on"),"a": ("feature", "off"), "b": ("feature", "off"),
            "base": ("base", "off"), "off": ("feature", "off"), "on": ("feature", "on"),
            "alloc-off": ("alloc", "off"), "alloc-on": ("alloc", "on")}
    order = orders[args.phase]
    manifest = {"schema": 1, "phase": args.phase, "order": order, "case": args.case,
                "milliseconds_per_metric": args.ms, "warmup_ms_per_metric": 30,
                "minimum_samples": 256, "cpu": args.cpu, "capsule": args.capsule,
                "instrumented": args.phase.endswith("allocations"), "binary_sha256": {}}
    for binary in sorted({arms[a][0] for a in order}):
        identity = execute("sha256sum " + shlex.quote(f"{args.root}/bins/{binary}"))
        manifest["binary_sha256"][binary] = identity.stdout.split()[0]
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    marker = Path(f"/tmp/linux-bench-measuring.{os.getpid()}")
    marker.write_text("fetch-continuation component comparison\n")
    try:
        for index, arm in enumerate(order):
            binary, mode = arms[arm]
            name = f"{index:02d}-{arm}.json"
            remote_out = f"{args.root}/{args.phase}-{name}"
            env = [f"BENCH_CPU={args.cpu}"]
            if args.capsule:
                env += ["BENCH_LOADER=" + shlex.quote(f"{args.root}/runtime/ld-linux-x86-64.so.2"),
                        "BENCH_LIBDIR=" + shlex.quote(f"{args.root}/runtime")]
            command = " ".join(env + [shlex.quote(f"{args.root}/run-one.sh"),
                                      shlex.quote(f"{args.root}/bins/{binary}"), mode,
                                      shlex.quote(remote_out), shlex.quote(args.case), str(args.ms)])
            result = execute(command)
            # Private caller logs may contain SSH banners; only validated JSON is portable.
            for suffix in ["", ".host.json"]:
                raw = execute("cat " + shlex.quote(remote_out + suffix)).stdout
                parsed = json.loads(raw)
                (args.out / (name + suffix)).write_text(json.dumps(parsed, indent=2) + "\n")
            if result.returncode:
                raise RuntimeError(f"trial {index} failed")
            print(f"{args.phase} {index + 1}/{len(order)} {arm} complete", flush=True)
            time.sleep(0.2)
    finally:
        marker.unlink()
    files = sorted(args.out.glob("*.json"))
    (args.out / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in files))


if __name__ == "__main__":
    main()
