"""Bounded CPU and touched-memory probes; no network or model calls."""

import json
import os
import sys
import time
from pathlib import Path

mode = sys.argv[1]
if mode == "cpu":
    children = []
    started = time.monotonic()
    for _ in range(4):
        reader, writer = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(reader)
            cpu_started = time.process_time()
            deadline = time.monotonic() + 3
            value = 0
            while time.monotonic() < deadline:
                value = (value + 1) % 1000003
            os.write(writer, str(time.process_time() - cpu_started).encode())
            os.close(writer)
            os._exit(0)
        os.close(writer)
        children.append((pid, reader))
    total = 0
    for pid, reader in children:
        _, status = os.waitpid(pid, 0)
        if status:
            raise RuntimeError("CPU worker failed")
        total += float(os.read(reader, 128))
        os.close(reader)
    print(json.dumps({"workers": 4, "wall_seconds": time.monotonic() - started, "cpu_seconds": total}))
elif mode == "memory":
    chunks = []
    for _ in range(128):
        chunk = bytearray(8 * 1024 * 1024)
        chunk[::4096] = b"x" * (len(chunk) // 4096)
        chunks.append(chunk)
        print(f"allocated_mib={len(chunks) * 8}", flush=True)
    print("unexpected_one_gib_allocation", flush=True)
elif mode == "observe":
    values = {}
    for name in ("memory.max", "memory.events", "memory.peak", "cpu.max", "cpu.stat"):
        path = Path("/sys/fs/cgroup") / name
        if path.exists():
            values[name] = path.read_text().strip()
    print(json.dumps(values))
else:
    raise ValueError("expected cpu, memory or observe")
