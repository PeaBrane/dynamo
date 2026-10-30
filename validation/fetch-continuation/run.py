# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Closed-loop router hints -> real KVCR fetches, with CPU byte-copy NIXL."""

import argparse
import ctypes
import json
import queue
import select
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from contextlib import ExitStack, contextmanager
from pathlib import Path
from urllib.error import HTTPError

import _kvcr_test_utils as support
import kvcr
from kvcr.config import LocalDramOptions, RemoteFWDramOptions
from kvcr.control_channels import ZmqPeerControlChannel
from kvcr.types import BlockKey, MemoryRef, QueryStatus

KVCR_REVISION = "34bb11986e096e249c76186cd60e28091f2fcb2e"
BLOCK_BYTES = 16


class KeyAdapter:
    def encode(self, value):
        return BlockKey(int(value).to_bytes(8, "little"))

    def decode(self, value):
        return int.from_bytes(value, "little")


class CpuAgent(support.FakeNixlAgent):
    """Only the NIXL boundary is simulated; copies and notifications are real.

    Each peer lives in this process, so registered host pointers are usable by
    ctypes. KVCR still resolves descriptors, pins sources and handles completion.
    """

    def __init__(self, name, peers):
        super().__init__(name.encode())
        self.peers = peers
        peers[name] = self
        self.state = "DONE"
        self.notifications = queue.SimpleQueue()
        self.copy_count = 0

    def add_remote_agent(self, metadata):
        name = metadata.decode()
        assert name in self.peers, name
        return name

    def transfer(self, handle):
        op, local, indices, remote, remote_agent, notification = self.xfers[handle - 1]
        assert op in ("READ", "WRITE")
        for i in indices:
            src, dst = (local[i], remote[i]) if op == "WRITE" else (remote[i], local[i])
            assert src[1] == dst[1] == BLOCK_BYTES
            ctypes.memmove(dst[0], src[0], src[1])
            self.copy_count += 1
        self.transfers.append(handle)
        if notification:
            self.send_notif(remote_agent, notification)
        return "DONE"

    def send_notif(self, name, message):
        if isinstance(name, bytes):
            name = name.decode()
        self.peers[name].notifications.put((self.name, message))

    def get_new_notifs(self):
        result = {}
        while True:
            try:
                name, message = self.notifications.get_nowait()
            except queue.Empty:
                return result
            result.setdefault(name, []).append(message)


class Peer:
    def __init__(self, name, agents, hashes, resident, sources=()):
        self.name = name
        self.sources = sources
        self.agent = CpuAgent(name, agents)
        self.framework = ctypes.create_string_buffer(20 * BLOCK_BYTES)
        self.cache = ctypes.create_string_buffer(20 * BLOCK_BYTES)
        self.adapter = KeyAdapter()
        self.keys = [self.adapter.encode(h) for h in hashes]
        for i in range(20):
            ctypes.memset(
                ctypes.addressof(self.framework) + i * BLOCK_BYTES, i + 1, BLOCK_BYTES
            )
        listener = socket.create_server(("127.0.0.1", 0))
        port = listener.getsockname()[1]
        control = ZmqPeerControlChannel("127.0.0.1", port, "127.0.0.1")
        control.adopt_listener(listener.detach())
        self.endpoint = control.endpoint
        # No framework fallback: only deposited source blocks may satisfy a fetch.
        self.pinning = support.FakePrimaryPinning(prefix_length=0)
        self.kvcr = support._new_kvcr(
            self.agent,
            self.pinning,
            control,
            name=name,
            key_adapter=self.adapter,
            framework_regions=[support._buffer_region(self.framework, BLOCK_BYTES)],
            local_dram=LocalDramOptions(
                [("", ctypes.addressof(self.cache), len(self.cache))]
            ),
            remote_options=RemoteFWDramOptions(eager_ctrl_connect=False),
        )
        if resident:
            operation = self.kvcr.deposit(
                {self.keys[i]: [self.ref(i)] for i in resident}
            )
            assert all(result.success for result in self.wait(operation).values())
        expected = set(resident)
        for i, (status, _) in enumerate(self.kvcr.query(self.keys)):
            assert (status == QueryStatus.HIT) == (i in expected), (name, i, status)

    def ref(self, i):
        return MemoryRef(end_point_name=self.name, label="", element_index=i)

    def wait(self, operation):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            for source in self.sources:
                list(source.kvcr.poll_completed())
            for handle, entries in self.kvcr.poll_completed():
                if handle == operation:
                    return entries
            time.sleep(0.001)
        raise TimeoutError(f"KVCR operation {operation} did not complete")

    def verify_fetch(self, hint, local, end, request_id, *, success=True):
        self.kvcr.submit_hint(hint, request_id=request_id)
        # The root-aligned hint includes already-local blocks. KVCR must skip
        # these rather than asking the suffix-only source for absent ancestors.
        results = self.wait(self.kvcr.fetch(self.keys[:end], request_id=request_id))
        if not success:
            assert any(not result.success for result in results.values()), results
            return
        assert set(results) == set(self.keys[:end]), results
        assert all(result.success for result in results.values()), results
        ctypes.memset(ctypes.addressof(self.framework), 0, len(self.framework))
        delivered = self.wait(
            self.kvcr.deliver(
                {key: [self.ref(i)] for i, key in enumerate(self.keys[:end])}
            )
        )
        assert all(result.success for result in delivered.values())
        assert self.framework.raw[: end * BLOCK_BYTES] == b"".join(
            bytes([i + 1]) * BLOCK_BYTES for i in range(end)
        )
        assert all(
            status != QueryStatus.HIT for status, _ in self.kvcr.query(self.keys[end:])
        )
        self.kvcr.discard_hint(request_id)

    def close(self):
        self.kvcr.close()


@contextmanager
def router(binary, sources):
    process = subprocess.Popen(
        [str(binary), *sources],
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        readable, _, _ = select.select([process.stdout], [], [], 30)
        if not readable:
            raise TimeoutError("router fixture did not start")
        metadata = json.loads(process.stdout.readline())
        yield metadata
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        assert process.returncode == 0, process.returncode


def book(metadata, target, request_id):
    request = urllib.request.Request(
        metadata["url"] + "/select_and_reserve",
        data=json.dumps(
            {
                "model_name": "fixture",
                "routing_group": "default",
                "selection_id": request_id,
                "token_ids": metadata["tokens"],
                "pinned_worker": {"worker_id": target, "dp_rank": 0},
            }
        ).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            value = json.load(response)
    except HTTPError as error:
        raise RuntimeError(error.read().decode()) from error
    assert value["worker_id"] == target
    return value.get("kv_hint")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--requests", type=int, default=32)
    args = parser.parse_args()
    kvcr_root = Path(support.__file__).resolve().parents[2]
    revision = subprocess.check_output(
        ["git", "-C", str(kvcr_root), "rev-parse", "HEAD"], text=True
    ).strip()
    assert (
        revision == KVCR_REVISION
    ), "revalidate the public test adapter after a KVCR revision change"
    assert not subprocess.check_output(
        ["git", "-C", str(kvcr_root), "status", "--porcelain"], text=True
    ).strip(), "KVCR checkout must be clean"
    loaded_root = Path(kvcr.__file__).resolve().parent
    source_root = kvcr_root / "src" / "kvcr"

    # A wheel and the test helpers can come from different checkouts. Verify the
    # installed package's bytes, not just the helper checkout's revision.
    def package_files(root):
        return {
            file.relative_to(root): file.read_bytes() for file in root.rglob("*.py")
        }

    assert package_files(loaded_root) == package_files(
        source_root
    ), "loaded KVCR package differs from the pinned source"
    assert all(
        Path(module.__file__).resolve().is_relative_to(loaded_root)
        for name, module in sys.modules.items()
        if name == "kvcr" or name.startswith("kvcr.")
    ), "KVCR modules were loaded from mixed package locations"
    # Ask the router for its real hash chain before allocating peer inventories.
    with router(args.probe, ["tcp://unused:1", "tcp://unused:2"]) as fixture:
        hashes = fixture["hashes"]
    agents = {}
    report = {
        "kvcr_revision": revision,
        "successful_fetches": 0,
        "rejected_wrong_sources": 0,
    }
    with ExitStack() as stack:
        source_b = Peer("source-b", agents, hashes, range(10, 15))
        stack.callback(source_b.close)
        source_c = Peer("source-c", agents, hashes, range(15, 20))
        stack.callback(source_c.close)
        endpoints = [source_b.endpoint, source_c.endpoint]
        with router(args.probe, endpoints) as metadata:
            assert metadata["hashes"] == hashes
            for i in range(args.requests):
                target, local, end, endpoint = (
                    (1, 10, 15, endpoints[0])
                    if i % 2 == 0
                    else (2, 15, 20, endpoints[1])
                )
                request_id = f"fetch-{i}"
                hint = book(metadata, target, request_id)
                assert len(hint["actions"]) == 1
                action = hint["actions"][0]
                assert action["action_type"] == "kv.fetch"
                assert action["payload"] == {
                    "source_control_endpoint": endpoint,
                    "block_hashes": hashes[:end],
                }
                peer = Peer(
                    f"target-{i}",
                    agents,
                    hashes,
                    range(local),
                    sources=(source_b, source_c),
                )
                try:
                    peer.verify_fetch(hint, local, end, request_id)
                finally:
                    peer.close()
                report["successful_fetches"] += 1
            # A deliberate wrong-source hint must fail: this guards against a
            # mock that claims every hash exists or never checks transfer bytes.
            hint = book(metadata, 1, "wrong-source")
            hint["actions"][0]["payload"]["source_control_endpoint"] = endpoints[1]
            peer = Peer(
                "negative-target",
                agents,
                hashes,
                range(10),
                sources=(source_b, source_c),
            )
            try:
                peer.verify_fetch(hint, 10, 15, "wrong-source", success=False)
            finally:
                peer.close()
            report["rejected_wrong_sources"] += 1
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
