#!/usr/bin/python3
"""A stateful Docker command double. Only the guest shell runs real processes."""

import json
import os
from pathlib import Path
import sys

root = Path(__file__).parent
args = sys.argv[1:]
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")
assert args[:2] == ["--host", "unix:///var/run/docker.sock"]
args = args[2:]
path = root / "docker.json"
state = json.loads(path.read_text()) if path.exists() else {}
mode = (root / "mode").read_text() if (root / "mode").exists() else ""


def save():
    path.write_text(json.dumps(state))


def value(flag):
    return args[args.index(flag) + 1]


if args[0] == "info":
    print(json.dumps({"ID": "other" if mode == "daemon" else "daemon-one",
                      "OSType": "linux", "MemoryLimit": mode != "no-memory",
                      "CpuCfsQuota": True,
                      "Runtimes": {} if mode == "no-runtime" else {"runsc": {}, "runc": {}}}))
elif args[:2] == ["image", "inspect"]:
    print(json.dumps([{"Id": "sha256:" + "b" * 64, "RepoDigests": [args[2]]}]))
elif args[:2] == ["container", "ls"]:
    if "container" in state:
        print(state["container"]["Id"])
elif args[:2] == ["container", "inspect"]:
    current = json.loads(json.dumps(state["container"]))
    if mode == "runtime":
        current["HostConfig"]["Runtime"] = "runc"
    elif mode == "memory":
        current["HostConfig"]["Memory"] = 0
    elif mode == "owner":
        current["Config"]["Labels"]["io.branchyard.owner"] = "foreign"
    elif mode == "replaced":
        current["Id"] = "f" * 64
    elif mode == "mount":
        current["Mounts"][0]["Type"] = "bind"
    elif mode == "privileged":
        current["HostConfig"]["Privileged"] = True
    elif mode == "network":
        current["NetworkSettings"]["Networks"]["foreign"] = {}
    elif mode == "volume-target":
        current["Mounts"][1]["Destination"] = "/workspace"
    print(json.dumps([current]))
elif args[:2] == ["container", "create"]:
    assert "container" not in state
    labels = dict(args[i + 1].split("=", 1) for i, arg in enumerate(args) if arg == "--label")
    mounts = [{"Type": "volume", "Target": target} for target in ["/workspace", "/branchyard/home"]]
    state["container"] = {
        "Id": "c" * 64, "Name": "/" + value("--name"), "Image": "sha256:" + "b" * 64,
        "Config": {"Labels": labels, "Image": args[-3], "User": value("--user"),
                   "WorkingDir": value("--workdir")},
        "State": {"Running": False},
        "NetworkSettings": {"Networks": {value("--network"): {}}},
        "HostConfig": {"Runtime": value("--runtime"), "NetworkMode": value("--network"),
                       "Privileged": False, "ReadonlyRootfs": "--read-only" in args,
                       "PidMode": "", "IpcMode": value("--ipc"), "UTSMode": "",
                       "CgroupnsMode": value("--cgroupns"), "NanoCpus": int(value("--cpus")) * 1000000000,
                       "Memory": int(value("--memory")), "MemorySwap": int(value("--memory-swap")),
                       "PidsLimit": int(value("--pids-limit")), "CapDrop": [value("--cap-drop")],
                       "SecurityOpt": [value("--security-opt")], "Mounts": mounts,
                       "Tmpfs": {"/tmp": value("--tmpfs").split(":", 1)[1]}},
        "Mounts": [{"Type": "volume", "Driver": "local", "RW": True,
                    "Name": letter * 64, "Destination": target}
                   for letter, target in [("d", "/workspace"), ("e", "/branchyard/home")]]}
    state["volumes"] = ["d" * 64, "e" * 64]
    save()
    if mode == "create-gap":
        sys.exit(1)
    print("c" * 64)
elif args[:2] == ["container", "start"]:
    assert args[-1] == state["container"]["Id"]
    if mode == "start-fail":
        sys.exit(1)
    state["container"]["State"]["Running"] = True
    save()
elif args[:2] == ["container", "exec"]:
    assert "c" * 64 in args
    if "--interactive" not in args:
        sys.exit(1 if mode == "ready-fail" else 0)
    if mode == "exec-credentials":
        print("registry password=private-credential-must-not-escape", file=sys.stderr)
        sys.exit(1)
    command = args[args.index("c" * 64) + 1:]
    home = root / "home"
    home.mkdir(exist_ok=True)
    os.environ["HOME"] = str(home)
    os.execvp(command[0], command)
elif args[:2] == ["container", "stop"]:
    assert args[-1] == state["container"]["Id"]
    state["container"]["State"]["Running"] = False
    save()
elif args[:2] == ["container", "rm"]:
    assert args[-1] == state["container"]["Id"]
    assert "--force" in args and "--volumes" in args
    if mode == "delete-fail":
        sys.exit(1)
    del state["container"]
    if mode != "volume-leak":
        state["volumes"] = []
    save()
elif args[:2] == ["volume", "ls"]:
    name = value("--filter").removeprefix("name=^").removesuffix("$")
    if name in state.get("volumes", []):
        print(name)
else:
    raise AssertionError(args)
