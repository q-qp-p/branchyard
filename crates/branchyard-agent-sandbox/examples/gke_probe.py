"""Credential-free guest observations for the real provider conformance example."""

import json
import os
import platform
import socket
import sys
import urllib.error
import urllib.request
from pathlib import Path

network = sys.argv[1]
result = {
    "uid": os.getuid(),
    "gid": os.getgid(),
    "kernel": platform.release(),
    "cwd": os.getcwd(),
    "home": os.environ.get("HOME"),
    "pod_uid": Path("/var/run/branchyard-identity/pod-uid").read_text().strip(),
    "root_readonly": bool(os.statvfs("/").f_flag & os.ST_RDONLY),
    "docker_socket_visible": Path("/var/run/docker.sock").exists(),
    "kvm_visible": Path("/dev/kvm").exists(),
    "service_account_token_visible": Path("/var/run/secrets/kubernetes.io/serviceaccount/token").exists(),
    "network_interfaces": sorted(name for _, name in socket.if_nameindex()),
    "pid_one": Path("/proc/1/comm").read_text().strip(),
    "resolv_conf": Path("/etc/resolv.conf").read_text(),
    "writable": {},
    "cgroups": {},
}
for directory in ("/workspace", "/branchyard/home", "/tmp"):
    path = Path(directory) / "branchyard-live-write"
    path.write_text("qualification\n")
    result["writable"][directory] = path.read_text() == "qualification\n"
    path.unlink()
status = Path("/proc/self/status").read_text().splitlines()
result["process_status"] = {
    key: next((line.split(":", 1)[1].strip() for line in status if line.startswith(key + ":")), None)
    for key in ("CapEff", "NoNewPrivs", "Seccomp", "Seccomp_filters")
}
pid_one_status = Path("/proc/1/status").read_text().splitlines()
result["pid_one_status"] = {
    key: next((line.split(":", 1)[1].strip() for line in pid_one_status if line.startswith(key + ":")), None)
    for key in ("CapEff", "NoNewPrivs", "Seccomp", "Seccomp_filters")
}
for name in ("memory.max", "memory.events", "memory.peak", "cpu.max", "cpu.stat"):
    path = Path("/sys/fs/cgroup") / name
    if path.exists():
        result["cgroups"][name] = path.read_text().strip()
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
result["metadata"] = {}
for address in ("169.254.169.254", "169.254.169.252"):
    request = urllib.request.Request(
        f"http://{address}/computeMetadata/v1/project/project-id", headers={"Metadata-Flavor": "Google"}
    )
    try:
        with opener.open(request, timeout=2) as response:
            result["metadata"][address] = {"reachable": True, "status": response.status}
    except urllib.error.HTTPError as error:
        result["metadata"][address] = {"reachable": True, "status": error.code}
    except OSError as error:
        result["metadata"][address] = {"reachable": False, "error": type(error).__name__}
try:
    with socket.create_connection(("1.1.1.1", 443), timeout=2):
        result["external_tcp_reachable"] = True
except OSError as error:
    result["external_tcp_reachable"] = False
    result["external_tcp_error"] = type(error).__name__
if network == "unrestricted":
    try:
        result["dns_resolved"] = bool(socket.getaddrinfo("example.com", 443, type=socket.SOCK_STREAM))
    except OSError as error:
        result["dns_resolved"] = False
        result["dns_error"] = type(error).__name__
    if result["dns_resolved"]:
        try:
            with opener.open("https://example.com/", timeout=10) as response:
                result["external_https_status"] = response.status
        except OSError as error:
            result["external_https_error"] = type(error).__name__
print(json.dumps(result, sort_keys=True))
