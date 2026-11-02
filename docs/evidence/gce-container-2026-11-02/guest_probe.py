#!/usr/bin/env python3
"""Emit credential-free observations from the qualification container."""

import json
import os
import platform
import shutil
import socket
import urllib.error
import urllib.request
from pathlib import Path

results = {
    "uid": os.getuid(),
    "gid": os.getgid(),
    "cwd": os.getcwd(),
    "home": os.environ.get("HOME"),
    "kernel": platform.release(),
    "root_readonly": bool(os.statvfs("/").f_flag & os.ST_RDONLY),
    "host_marker_visible": Path("/var/tmp/branchyard-cloud-qual-20261102/evidence/container/host-only-marker").exists(),
    "docker_socket_visible": Path("/var/run/docker.sock").exists(),
    "kvm_visible": Path("/dev/kvm").exists(),
    "tools": {name: shutil.which(name) for name in ("sh", "tar", "setsid", "tail", "python3", "agy")},
    "writable": {},
}
for directory in ("/workspace", "/branchyard/home", "/tmp"):
    path = Path(directory) / "branchyard-qualification-write"
    path.write_text("qualification\n")
    results["writable"][directory] = path.read_text() == "qualification\n"
    path.unlink()
status = Path("/proc/self/status").read_text().splitlines()
results["process_status"] = {
    key: next((line.split(":", 1)[1].strip() for line in status if line.startswith(key + ":")), None)
    for key in ("CapEff", "NoNewPrivs")
}
results["network_interfaces"] = sorted(name for _, name in socket.if_nameindex())
script = Path("/tmp/branchyard-noexec-probe")
script.write_text("#!/bin/sh\nexit 0\n")
script.chmod(0o755)
child = os.fork()
if child == 0:
    try:
        os.execv(str(script), [str(script)])
    except PermissionError:
        os._exit(73)
    except OSError:
        os._exit(74)
_, status = os.waitpid(child, 0)
results["tmp_noexec"] = os.waitstatus_to_exitcode(status) == 73
script.unlink()
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
results["metadata"] = {}
for address in ("169.254.169.254", "169.254.169.252"):
    request = urllib.request.Request(
        f"http://{address}/computeMetadata/v1/project/project-id", headers={"Metadata-Flavor": "Google"}
    )
    try:
        with opener.open(request, timeout=2) as response:
            results["metadata"][address] = {"status": response.status, "reachable": True}
    except (OSError, urllib.error.URLError) as error:
        results["metadata"][address] = {"reachable": False, "error": type(error).__name__}
try:
    with socket.create_connection(("1.1.1.1", 443), timeout=2):
        results["external_tcp_reachable"] = True
except OSError as error:
    results["external_tcp_reachable"] = False
    results["external_tcp_error"] = type(error).__name__
print(json.dumps(results, sort_keys=True))
