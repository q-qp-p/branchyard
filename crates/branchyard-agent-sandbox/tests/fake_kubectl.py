#!/usr/bin/python3
"""Hermetic kubectl protocol fixture: no network, credentials or Kubernetes."""

import json
import os
import subprocess
import sys
from pathlib import Path

root = Path(__file__).parent
state_path = root / "state.json"
state = json.loads(state_path.read_text())
args = sys.argv[1:]
assert args[:2] == ["--context", "test-context"]
assert args[2] == "--request-timeout=30s"
args = args[3:]
namespace = None
if args[0] == "--namespace":
    namespace = args[1]
    args = args[2:]
operation = args.pop(0)
body = json.load(sys.stdin) if operation in ("create", "delete") else None
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"operation": operation, "args": args, "body": body}) + "\n")


def save():
    state_path.write_text(json.dumps(state))


def emit(value):
    if value is not None:
        print(json.dumps(value))
    sys.exit(0)


def kind(value):
    if value.startswith("sandboxes"):
        return "sandbox"
    return value


if operation == "get":
    resource = kind(args[0])
    name = args[1] if len(args) > 1 and not args[1].startswith("--") else ""
    runtime = state["runtime"]
    if resource == "namespace":
        emit({"metadata": {"uid": state["namespace_uid"], "labels": {
            "branchyard-runtime": runtime, "branchyard-network-policy": "enforced-v2",
            "branchyard-deployment": "test-cluster", "branchyard-config": "fingerprint"}},
            "status": {"phase": "Active"}})
    if resource == "daemonset":
        emit({"status": {"desiredNumberScheduled": 1, "numberReady": 0 if state.get("dataplane_unready") else 1}})
    if resource == "runtimeclass":
        classes = json.loads((Path(__file__).resolve().parent / "gke_runtimeclasses.json").read_text())
        observed = next(item for item in classes["items"] if item["metadata"]["name"] == runtime)
        if state.get("wrong_handler"):
            observed["handler"] = "runc"
        emit(observed)
    if resource == "node":
        emit({"metadata": {"uid": "node-uid", "labels": {"sandbox.gke.io/runtime": "runc" if state.get("wrong_node") else runtime,
            "cloud.google.com/gke-nodepool": runtime}}, "spec": {"providerID": "gce://test-project/zone/node"},
            "status": {"nodeInfo": {"operatingSystem": "linux", "containerRuntimeVersion": "containerd://2.4.1",
                "kubeletVersion": state.get("version", "v1.37.1-gke.3020000")},
                "conditions": [{"type": "Ready", "status": "True"}]}})
    default = {"metadata": {"name": "branchyard-default-deny", "labels": {
        "branchyard-deployment": "test-cluster", "branchyard-config": "fingerprint"}},
        "spec": {"podSelector": {}, "policyTypes": ["Ingress", "Egress"]}}
    if resource == "networkpolicy" and name == "branchyard-default-deny":
        emit(default)
    if resource == "networkpolicies":
        emit({"items": [default] + [value for key, value in state["objects"].items()
            if key.startswith("networkpolicy/")] + state["extra_policies"]})
    emit(state["objects"].get(f"{resource}/{name}"))

if operation == "create":
    resource = "sandbox" if body["kind"] == "Sandbox" else "networkpolicy"
    name = body["metadata"]["name"]
    key = f"{resource}/{name}"
    if key in state["objects"]:
        sys.exit(1)
    # Prove the durable intent precedes external creation.
    receipts = list(root.glob("*-receipts/*.json")) + list(root.glob("receipts/*.json"))
    assert receipts and any(json.loads(path.read_text())["owner"] == body["metadata"]["labels"]["branchyard.io/owner"] for path in receipts)
    state["counter"] += 1
    body["metadata"]["uid"] = f"uid-{state['counter']}"
    state["objects"][key] = body
    if resource == "sandbox":
        body["status"] = {"conditions": [{"type": "Ready", "status": "True"}]}
        pod = json.loads(json.dumps(body["spec"]["podTemplate"]))
        pod["metadata"].update({"name": name, "namespace": namespace, "uid": "pod-uid",
            "ownerReferences": [{"kind": "Sandbox", "uid": body["metadata"]["uid"], "controller": True}]})
        pod["spec"]["nodeName"] = "test-node"
        pod["status"] = {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [{"ready": True, "state": {"running": {}},
            "imageID": pod["spec"]["containers"][0]["image"]}]}
        state["objects"][f"pod/{name}"] = pod
    save()
    if state.pop("lose_create_response", False):
        save()
        sys.exit(1)
    emit(body)

if operation == "delete":
    path = args[1]
    name = path.rsplit("/", 1)[1]
    resource = "sandbox" if "/sandboxes/" in path else "pod" if "/pods/" in path else "networkpolicy"
    key = f"{resource}/{name}"
    if state.get("replace_on_delete"):
        state["objects"][key]["metadata"]["uid"] = "raced-replacement"
        save()
    if state["objects"][key]["metadata"]["uid"] != body["preconditions"]["uid"]:
        sys.exit(1)
    del state["objects"][key]
    if resource == "sandbox":
        state["objects"].pop(f"pod/{name}", None)
        state["objects"].pop(f"networkpolicy/{name}", None)
    save()
    emit({"status": "Success"})

if operation == "wait":
    sys.exit(0)

if operation == "exec":
    if state.get("diagnostic"):
        print(state["diagnostic"], file=sys.stderr, flush=True)
    assert args[:1] == ["-i"] and args[2:5] == ["-c", "runtime", "--"]
    pod = state["objects"][f"pod/{args[1]}"]
    actual = state.get("exec_replacement_uid", pod["metadata"]["uid"])
    identity = root / "pod-uid"
    identity.write_text(actual)  # Kubernetes DownwardAPI has no trailing newline.
    command = args[5:]
    assert command[:5] == ["setpriv", "--no-new-privs", "--", "sh", "-c"]
    command[5] = command[5].replace("/var/run/branchyard-identity/pod-uid", str(identity))
    os.environ["HOME"] = str(root / "home")
    result = subprocess.run(command, check=False)
    code = result.returncode if result.returncode >= 0 else 128 - result.returncode
    if code:
        print(f"command terminated with exit code {code}", file=sys.stderr, flush=True)
    sys.exit(code)

raise AssertionError(f"unhandled fake kubectl operation: {operation}")
