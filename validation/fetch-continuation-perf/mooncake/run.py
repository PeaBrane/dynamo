#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Run frozen Mooncake binaries in fresh processes; preserve every outcome.

Stage bins/{main,candidate}, runtime/, traces/agentx128.jsonl, and run-one.sh
under --root. SSH requires an existing control socket. Output contains raw
operational paths and logs; sanitize it before publishing.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--ssh")
    parser.add_argument("--socket")
    parser.add_argument("--phase", choices=["scout", "aa", "ab"], required=True)
    parser.add_argument("--duration-ms", type=int, required=True)
    args = parser.parse_args()
    if bool(args.ssh) != bool(args.socket):
        parser.error("--ssh and --socket must be supplied together")
    args.out.mkdir(parents=True, exist_ok=False)

    def execute(command, *, check=True):
        if args.ssh:
            subprocess.run(["ssh", "-S", args.socket, "-O", "check", args.ssh],
                           capture_output=True, text=True, check=True)
            command = ["ssh", "-S", args.socket, "-o", "BatchMode=yes", args.ssh, command]
        else:
            command = ["bash", "-c", command]
        return subprocess.run(command, capture_output=True, text=True, check=check)

    orders = {"scout": ["main", "candidate"],
              "aa": ["a", "b", "b", "a"] * 3,
              "ab": ["main", "candidate", "candidate", "main"] * 3}
    order = orders[args.phase]
    inputs = ["bins/main", "bins/candidate", "traces/agentx128.jsonl", "run-one.sh"]

    def identities():
        return {name: execute("sha256sum " + shlex.quote(f"{args.root}/{name}")).stdout.split()[0]
                for name in inputs}

    frozen = identities()
    manifest = {"schema": 1, "phase": args.phase, "order": order,
                "duration_ms": args.duration_ms, "sha256": frozen}
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for index, arm in enumerate(order):
        if identities() != frozen:
            raise RuntimeError("frozen inputs changed")
        variant = "main" if arm in {"a", "b"} else arm
        name = f"{index:02d}-{arm}.json"
        remote_out = f"{args.root}/{args.phase}-{name}"
        command = shlex.join(["bash", f"{args.root}/run-one.sh", args.root,
                              variant, str(args.duration_ms), remote_out])
        result = execute(command, check=False)
        (args.out / (name + ".log")).write_text(result.stdout + result.stderr)
        (args.out / (name + ".exit")).write_text(str(result.returncode) + "\n")
        for suffix in ["", ".host.json"]:
            raw = execute("cat " + shlex.quote(remote_out + suffix), check=False)
            if raw.returncode:
                raise RuntimeError(f"trial {index} exited {result.returncode}; missing {suffix or 'result'}")
            parsed = json.loads(raw.stdout)
            (args.out / (name + suffix)).write_text(json.dumps(parsed, indent=2) + "\n")
        print(f"{args.phase} {index + 1}/{len(order)} {arm}: exit {result.returncode}", flush=True)
        if result.returncode:
            raise RuntimeError(f"trial {index} failed; outputs retained")
        time.sleep(0.2)
    (args.out / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n"
        for p in sorted(args.out.iterdir()) if p.is_file()))


if __name__ == "__main__":
    main()
