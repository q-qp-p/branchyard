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


POOL_KINDS = {"sandboxtemplates": "sandboxtemplate", "sandboxwarmpools": "sandboxwarmpool",
              "sandboxclaims": "sandboxclaim"}


def kind(value, named=True):
    if value.startswith("sandboxes"):
        return "sandbox"
    if value.split(".")[0] in POOL_KINDS:
        return POOL_KINDS[value.split(".")[0]]
    if value.startswith("fqdnnetworkpolicies"):
        return "fqdnnetworkpolicy" if named else "fqdnnetworkpolicies"
    return value


if operation == "get":
    name = args[1] if len(args) > 1 and not args[1].startswith("--") else ""
    resource = kind(args[0], bool(name))
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
    if resource == "pods":
        emit({"items": [value for key, value in state["objects"].items() if key.startswith("pod/")]})
    if resource == "sandboxwarmpool" and f"{resource}/{name}" in state["objects"]:
        # The controller's view: how many pods of the pool are ready.
        pool = state["objects"][f"{resource}/{name}"]
        pool["status"] = {"readyReplicas": state.get("pool_ready", 0)}
        emit(pool)
    if resource == "networkpolicies":
        emit({"items": [default] + [value for key, value in state["objects"].items()
            if key.startswith("networkpolicy/")] + state["extra_policies"]})
    if resource == "fqdnnetworkpolicies":
        emit({"items": [value for key, value in state["objects"].items()
            if key.startswith("fqdnnetworkpolicy/")] + state.get("extra_fqdn_policies", [])})
    emit(state["objects"].get(f"{resource}/{name}"))

if operation == "create":
    resource = {"Sandbox": "sandbox", "FQDNNetworkPolicy": "fqdnnetworkpolicy", "SandboxTemplate": "sandboxtemplate",
                "SandboxWarmPool": "sandboxwarmpool", "SandboxClaim": "sandboxclaim"}.get(body["kind"], "networkpolicy")
    name = body["metadata"]["name"]
    key = f"{resource}/{name}"
    if key in state["objects"]:
        sys.exit(1)
    # Prove the durable intent precedes external creation. The shared template and pool are no
    # allocation's: they carry no owner.
    if resource not in ("sandboxtemplate", "sandboxwarmpool"):
        receipts = list(root.glob("*-receipts/*.json")) + list(root.glob("receipts/*.json"))
        assert receipts and any(json.loads(path.read_text())["owner"] == body["metadata"]["labels"]["branchyard.io/owner"] for path in receipts)
        if resource == "sandboxclaim":
            assert any((json.loads(path.read_text()).get("pooled") or {}).get("template") == body["spec"]["sandboxTemplateRef"]["name"]
                       for path in receipts), "the claim's intent is durable before the claim exists"
    state["counter"] += 1
    body["metadata"]["uid"] = f"uid-{state['counter']}"
    state["objects"][key] = body
    if resource == "sandboxclaim":
        # The controller adopts a ready pod of the pool: a Sandbox for the claim, owning that pod.
        template = state["objects"][f"sandboxtemplate/{body['spec']['sandboxTemplateRef']['name']}"]
        sandbox_name = state.get("claimed_sandbox_name", name)
        state["counter"] += 1
        sandbox = {"apiVersion": "agents.x-k8s.io/v1beta1", "kind": "Sandbox",
            "metadata": {"name": sandbox_name, "namespace": namespace, "uid": f"uid-{state['counter']}",
                "ownerReferences": [{"kind": "SandboxClaim", "uid": body["metadata"]["uid"], "controller": True}]},
            "spec": {"podTemplate": json.loads(json.dumps(template["spec"]["podTemplate"]))},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}}
        if state.get("claimed_foreign_owner"):
            sandbox["metadata"]["labels"] = {"branchyard.io/owner": "someone-else"}
        state["objects"][f"sandbox/{sandbox_name}"] = sandbox
        for index in range(state.get("claimed_pods", 1)):
            pod = json.loads(json.dumps(template["spec"]["podTemplate"]))
            pod_name = f"{state.get('warm_pod_name', 'warm-pod')}-{index}"
            state["counter"] += 1
            pod["metadata"].update({"name": pod_name, "namespace": namespace, "uid": f"pod-{state['counter']}",
                "ownerReferences": [{"kind": "Sandbox", "uid": sandbox["metadata"]["uid"], "controller": True}]})
            pod["spec"]["nodeName"] = "test-node"
            pod["status"] = {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}],
                "containerStatuses": [{"name": c["name"], "ready": True, "state": {"running": {}},
                "imageID": state.get("claimed_image_id") or c["image"]} for c in pod["spec"]["containers"]]}
            state["objects"][f"pod/{pod_name}"] = pod
        body["status"] = {"sandbox": {"name": sandbox_name}, "conditions": [{"type": "Ready", "status": "True"}]}
    if resource == "sandbox":
        body["status"] = {"conditions": [{"type": "Ready", "status": "True"}]}
        pod = json.loads(json.dumps(body["spec"]["podTemplate"]))
        pod["metadata"].update({"name": name, "namespace": namespace, "uid": "pod-uid",
            "ownerReferences": [{"kind": "Sandbox", "uid": body["metadata"]["uid"], "controller": True}]})
        pod["spec"]["nodeName"] = "test-node"
        pod["status"] = {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [{"name": c["name"], "ready": True, "state": {"running": {}},
            "imageID": c["image"]} for c in pod["spec"]["containers"]]}
        state["objects"][f"pod/{name}"] = pod
    save()
    if state.pop("lose_create_response", False):
        save()
        sys.exit(1)
    emit(body)

if operation in ("label", "annotate"):
    resource = kind(args[0])
    key, value = args[2].split("=", 1)
    metadata = state["objects"][f"{resource}/{args[1]}"]["metadata"]
    field = metadata.setdefault("labels" if operation == "label" else "annotations", {})
    if field.get(key, value) != value:  # no --overwrite: another value stays
        sys.exit(1)
    field[key] = value
    save()
    sys.exit(0)

if operation == "patch":
    resource = kind(args[0])
    state["objects"][f"{resource}/{args[1]}"]["spec"].update(json.loads(args[5])["spec"])
    save()
    sys.exit(0)

if operation == "delete":
    path = args[1]
    name = path.rsplit("/", 1)[1]
    resource = ("sandboxclaim" if "/sandboxclaims/" in path else "sandbox" if "/sandboxes/" in path
                else "pod" if "/pods/" in path
                else "fqdnnetworkpolicy" if "/fqdnnetworkpolicies/" in path else "networkpolicy")
    key = f"{resource}/{name}"
    if state.get("replace_on_delete"):
        state["objects"][key]["metadata"]["uid"] = "raced-replacement"
        save()
    if state["objects"][key]["metadata"]["uid"] != body["preconditions"]["uid"]:
        sys.exit(1)
    del state["objects"][key]
    if resource in ("sandbox", "sandboxclaim"):
        # Garbage collection: whatever the deleted object owns goes too, and so on down.
        def collect(uid):
            for other, value in list(state["objects"].items()):
                if any(ref["uid"] == uid for ref in value["metadata"].get("ownerReferences", [])):
                    state["objects"].pop(other, None)
                    state["objects"].pop(f"networkpolicy/{value['metadata']['name']}", None)
                    state["objects"].pop(f"fqdnnetworkpolicy/{value['metadata']['name']}", None)
                    collect(value["metadata"]["uid"])
        collect(body["preconditions"]["uid"])
    if resource == "sandbox":
        state["objects"].pop(f"pod/{name}", None)
        state["objects"].pop(f"networkpolicy/{name}", None)
        state["objects"].pop(f"fqdnnetworkpolicy/{name}", None)
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
