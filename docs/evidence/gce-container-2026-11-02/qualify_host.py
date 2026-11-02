#!/usr/bin/env python3
"""Run the bounded, credential-free Docker/runsc qualification on the owned GCE host."""

import hashlib
import io
import json
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from datetime import UTC, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent
OWNER = "gce-container-2026-11-02"
DOCKER = ["sudo", "docker", "--host", "unix:///var/run/docker.sock"]
REGISTRY = "registry@sha256:d4032d4cc323c1731c707931ecada07a53076eac38e3b61ae57533f170f2c6c3"
BASE = (
    "ghcr.io/vamsiramakrishnan/branchyard-agy@sha256:46e177cc5e874614a04cbe2b06b6df374ab50296557f27ef77246f609dbc158d"
)
created = []
report = {"date": datetime.now(UTC).isoformat(), "owner": OWNER, "checks": {}, "cleanup": []}


def docker(*args, payload=None, timeout=45, check=True):
    result = subprocess.run([*DOCKER, *args], input=payload, capture_output=True, timeout=timeout, check=False)
    if check and result.returncode:
        raise RuntimeError(
            f"Docker {args[0]} failed ({result.returncode}): {result.stderr.decode(errors='replace')[:2000]}"
        )
    return result


def inspect(identity):
    return json.loads(docker("container", "inspect", identity).stdout)[0]


def create(name, args):
    existing = docker("container", "inspect", name, check=False)
    if existing.returncode == 0:
        raise RuntimeError(f"named container already exists; inspect before retry: {name}")
    identity = (
        docker("container", "create", "--name", name, "--label", f"io.branchyard.qualification={OWNER}", *args)
        .stdout.decode()
        .strip()
    )
    created.append(identity)
    return identity


def execute(identity, *args, payload=None, check=True, timeout=45):
    return docker(
        "container",
        "exec",
        "-i",
        "--user",
        "1000:1000",
        "--env",
        "HOME=/branchyard/home",
        identity,
        *args,
        payload=payload,
        check=check,
        timeout=timeout,
    )


def boundary(identity):
    value = inspect(identity)
    host = value["HostConfig"]
    keys = (
        "Runtime",
        "NetworkMode",
        "Privileged",
        "ReadonlyRootfs",
        "PidMode",
        "IpcMode",
        "CgroupnsMode",
        "NanoCpus",
        "Memory",
        "MemorySwap",
        "PidsLimit",
        "CapDrop",
        "CapAdd",
        "SecurityOpt",
        "Binds",
        "Devices",
        "Tmpfs",
    )
    return {
        "id": value["Id"],
        "name": value["Name"],
        "image": value["Config"]["Image"],
        "image_id": value["Image"],
        "user": value["Config"]["User"],
        "working_dir": value["Config"]["WorkingDir"],
        "host_config": {key: host.get(key) for key in keys},
        "mounts": [{key: mount.get(key) for key in ("Type", "Name", "Destination", "RW")} for mount in value["Mounts"]],
    }


def cgroup(identity):
    pid = inspect(identity)["State"]["Pid"]
    group = next(
        line.removeprefix("0::")
        for line in Path(f"/proc/{pid}/cgroup").read_text().splitlines()
        if line.startswith("0::")
    )
    path = Path("/sys/fs/cgroup") / group.lstrip("/")
    if ".." in path.parts:
        raise RuntimeError("unexpected cgroup path")
    result = {"path": str(path)}
    for name in ("memory.max", "memory.swap.max", "pids.max", "cpu.max", "memory.events", "cpu.stat"):
        result[name] = (path / name).read_text().strip()
    return result


def counters(text):
    return {line.split()[0]: int(line.split()[1]) for line in text.splitlines()}


try:
    before = json.loads(docker("info", "--format", "{{json .}}").stdout)
    report["docker_version"] = json.loads(docker("version", "--format", "{{json .Server}}").stdout)
    report["default_runtime_before"] = before["DefaultRuntime"]
    report["runtime"] = before["Runtimes"]["runsc"]["path"]
    report["runsc_version"] = subprocess.run(
        ["sudo", report["runtime"], "--version"], capture_output=True, text=True, timeout=15, check=True
    ).stdout.strip()
    with tempfile.TemporaryDirectory(dir=ROOT) as context:
        (Path(context) / "Dockerfile").write_bytes((ROOT / "Dockerfile").read_bytes())
        build = docker(
            "build", "--pull=false", "--network=none", "-t", "branchyard-qual-container:20261102", context, timeout=180
        )
        (ROOT / "build.log").write_bytes(build.stdout + build.stderr)
    registry = create(
        "by-v040-container-registry-20261102",
        [
            "--publish",
            "127.0.0.1::5000",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges:true",
            "--memory",
            "134217728",
            "--memory-swap",
            "134217728",
            "--cpus",
            "0.5",
            "--pids-limit",
            "64",
            "--mount",
            "type=volume,destination=/var/lib/registry",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,noexec,size=16777216",
            REGISTRY,
        ],
    )
    docker("container", "start", registry)
    port = inspect(registry)["NetworkSettings"]["Ports"]["5000/tcp"][0]["HostPort"]
    deadline = time.monotonic() + 15
    while True:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/v2/", timeout=1) as response:
                if response.status == 200:
                    break
        except (OSError, urllib.error.URLError):
            if time.monotonic() >= deadline:
                raise RuntimeError("owned registry did not become ready") from None
            time.sleep(0.1)
    tag = f"localhost:{port}/branchyard-qualification/container:20261102"
    docker("image", "tag", "branchyard-qual-container:20261102", tag)
    pushed = docker("image", "push", tag, timeout=180)
    (ROOT / "push.log").write_bytes(pushed.stdout + pushed.stderr)
    image = json.loads(docker("image", "inspect", tag).stdout)[0]
    pin = next(
        digest
        for digest in image["RepoDigests"]
        if digest.startswith(f"localhost:{port}/branchyard-qualification/container@sha256:")
    )
    report["image"] = {
        "pin": pin,
        "id": image["Id"],
        "base": BASE,
        "dockerfile_sha256": hashlib.sha256((ROOT / "Dockerfile").read_bytes()).hexdigest(),
    }
    args = [
        "--runtime",
        "runsc",
        "--network",
        "none",
        "--user",
        "1000:1000",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
        "--ipc",
        "private",
        "--cgroupns",
        "private",
        "--pids-limit",
        "256",
        "--cpus",
        "1",
        "--memory",
        "536870912",
        "--memory-swap",
        "536870912",
        "--mount",
        "type=volume,destination=/workspace",
        "--mount",
        "type=volume,destination=/branchyard/home",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,noexec,size=67108864,mode=1777",
        "--env",
        "HOME=/branchyard/home",
        "--workdir",
        "/workspace",
        "--pull",
        "never",
        "--entrypoint",
        "sh",
        pin,
        "-c",
        "exec tail -f /dev/null",
    ]
    primary = create("by-v040-gvisor-deny-20261102", args)
    docker("container", "start", primary)
    (ROOT / "host-only-marker").write_text("host-only-qualification-marker\n")
    report["boundary"] = boundary(primary)
    guest = json.loads(execute(primary, "python3", "-", payload=(ROOT / "guest_probe.py").read_bytes()).stdout)
    report["guest_deny"] = guest
    assert guest["uid"] == guest["gid"] == 1000
    assert guest["cwd"] == "/workspace" and guest["home"] == "/branchyard/home"
    assert guest["root_readonly"] and guest["tmp_noexec"] and all(guest["writable"].values())
    assert not any(
        guest[key] for key in ("host_marker_visible", "docker_socket_visible", "kvm_visible", "external_tcp_reachable")
    )
    assert guest["process_status"] == {"CapEff": "0000000000000000", "NoNewPrivs": "1"}
    assert all(guest["tools"].values()) and not any(value["reachable"] for value in guest["metadata"].values())
    report["harness_version"] = execute(primary, "agy", "--version").stdout.decode().strip()
    binary = bytes(range(256)) * 4
    io_result = execute(
        primary,
        "python3",
        "-c",
        "import sys; data=sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1]); "
        "sys.stderr.buffer.write(b'error\\x00\\xff'); sys.exit(7)",
        payload=binary,
        check=False,
    )
    assert io_result.returncode == 7 and io_result.stdout == binary[::-1] and io_result.stderr == b"error\x00\xff"
    report["checks"]["binary_exec"] = {
        "exit_code": io_result.returncode,
        "stdin_sha256": hashlib.sha256(binary).hexdigest(),
        "stdout_sha256": hashlib.sha256(io_result.stdout).hexdigest(),
        "stderr_sha256": hashlib.sha256(io_result.stderr).hexdigest(),
    }
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        entry = tarfile.TarInfo("binary-transfer.dat")
        entry.size, entry.mode, entry.uid, entry.gid = len(binary), 0o640, 1000, 1000
        archive.addfile(entry, io.BytesIO(binary))
    execute(primary, "tar", "-xf", "-", "-C", "/workspace", payload=stream.getvalue())
    returned = execute(primary, "tar", "-cf", "-", "-C", "/workspace", "binary-transfer.dat").stdout
    with tarfile.open(fileobj=io.BytesIO(returned)) as archive:
        source = archive.extractfile("binary-transfer.dat")
        assert source is not None and source.read() == binary
    report["checks"]["tar_transfer"] = {
        "bytes": len(binary),
        "sha256": hashlib.sha256(binary).hexdigest(),
        "round_trip": True,
    }
    initial = cgroup(primary)
    assert initial["memory.max"] == "536870912" and initial["memory.swap.max"] == "0"
    assert initial["cpu.max"] == "100000 100000" and initial["pids.max"] == "256"
    report["cgroup_before_load"] = initial
    load = b"""import os,time
children=[]
for _ in range(4):
 pid=os.fork()
 if pid==0:
  until=time.monotonic()+4
  value=0
  while time.monotonic()<until:value=(value+1)%1000003
  os._exit(0)
 children.append(pid)
for pid in children:os.waitpid(pid,0)
print('four_cpu_workers_finished')
"""
    started = time.monotonic()
    execute(primary, "python3", "-", payload=load, timeout=20)
    elapsed = time.monotonic() - started
    loaded = cgroup(primary)
    first, last = counters(initial["cpu.stat"]), counters(loaded["cpu.stat"])
    used = (last["usage_usec"] - first["usage_usec"]) / 1_000_000
    throttles = last["nr_throttled"] - first["nr_throttled"]
    assert used <= elapsed * 1.25 and throttles > 0
    report["checks"]["cpu_ceiling"] = {
        "wall_seconds": elapsed,
        "cgroup_cpu_seconds": used,
        "throttled_periods": throttles,
        "workers": 4,
    }
    bridge_args = list(args)
    bridge_args[bridge_args.index("--network") + 1] = "bridge"
    bridge = create("by-v040-gvisor-bridge-20261102", bridge_args)
    docker("container", "start", bridge)
    report["guest_unrestricted"] = json.loads(
        execute(bridge, "python3", "-", payload=(ROOT / "guest_probe.py").read_bytes()).stdout
    )
    oom = execute(
        primary,
        "python3",
        "-c",
        "chunks=[]\nfor _ in range(128):\n chunk=bytearray(8*1024*1024)\n "
        "chunk[::4096]=b'x'*(len(chunk)//4096)\n chunks.append(chunk)\nprint('unexpected_one_gib_allocation')",
        check=False,
        timeout=30,
    )
    state = inspect(primary)["State"]
    report["checks"]["memory_ceiling"] = {
        "exec_exit_code": oom.returncode,
        "container_running": state["Running"],
        "container_oom_killed": state["OOMKilled"],
        "stdout": oom.stdout.decode(errors="replace")[:300],
        "stderr": oom.stderr.decode(errors="replace")[:300],
    }
    group = Path(initial["path"])
    report["checks"]["memory_ceiling"]["cgroup_after"] = {
        name: (group / name).read_text().strip()
        for name in ("memory.max", "memory.peak", "memory.events")
        if (group / name).exists()
    }
    assert oom.returncode != 0 and b"unexpected_one_gib_allocation" not in oom.stdout
    assert state["OOMKilled"] or b"MemoryError" in oom.stderr
    after = json.loads(docker("info", "--format", "{{json .}}").stdout)
    assert after["DefaultRuntime"] == before["DefaultRuntime"]
    report["default_runtime_after"] = after["DefaultRuntime"]
    report["result"] = "passed"
except BaseException as error:
    report["result"] = "failed"
    report["failure"] = f"{type(error).__name__}: {error}"
    raise
finally:
    for identity in reversed(created):
        value = inspect(identity)
        if value["Config"]["Labels"].get("io.branchyard.qualification") != OWNER:
            raise RuntimeError("container ownership changed; refusing cleanup")
        volumes = [mount["Name"] for mount in value["Mounts"] if mount["Type"] == "volume"]
        docker("container", "rm", "--force", "--volumes", identity)
        container_absent = docker("container", "inspect", identity, check=False).returncode != 0
        volumes_absent = all(docker("volume", "inspect", name, check=False).returncode != 0 for name in volumes)
        report["cleanup"].append(
            {"id": identity, "container_absent": container_absent, "volumes": volumes, "volumes_absent": volumes_absent}
        )
        assert container_absent and volumes_absent
    (ROOT / "host-only-marker").unlink(missing_ok=True)
    (ROOT / "verification.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, indent=2, sort_keys=True))
