#!/usr/bin/env python3
"""Plan or prepare a dedicated GKE cluster for isolated Agent Sandbox workloads.

The default prints an offline plan. ``--apply`` creates only the named, owned
cluster and pools, then installs checksum-pinned controllers and runtime policies.
It never adopts foreign resources, changes IAM, or creates network/NAT resources.
"""

import argparse
import hashlib
import ipaddress
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

CONTROLLER_VERSION = "v1.0.9"
CONTROLLER_URL = (
    "https://github.com/kubernetes-sigs/agent-sandbox/releases/download/"
    f"{CONTROLLER_VERSION}/sandbox-with-extensions.yaml"
)
CONTROLLER_SHA256 = "4bf9ee5558e9a663f91cfa5963e96f2591bfaa1dcf79f03bfab65bc5d65cc699"
CONTROLLER_IMAGE = (
    "registry.k8s.io/agent-sandbox/agent-sandbox-controller"
    "@sha256:ef1a1a3076afc996e711c44b388fbb7ffb7cd6858953d7e685166b299d77bd38"
)
OWNER_LABEL = "branchyard-deployment"
CONFIG_LABEL = "branchyard-config"
RUNTIME_LABEL = "branchyard-runtime"
NETWORK_POLICY_LABEL = "branchyard-network-policy"
RUNTIMES = ("gvisor", "microvm")
NODE_SCOPES = (
    "https://www.googleapis.com/auth/devstorage.read_only",
    "https://www.googleapis.com/auth/logging.write",
    "https://www.googleapis.com/auth/monitoring",
    "https://www.googleapis.com/auth/service.management.readonly",
    "https://www.googleapis.com/auth/servicecontrol",
    "https://www.googleapis.com/auth/trace.append",
)


class SetupError(Exception):
    """An actionable setup failure that does not expose command output."""


@dataclass(frozen=True)
class Pool:
    """One fixed-size pool and the managed runtime it supplies."""

    name: str
    runtime: str
    machine_type: str
    nodes: int = 1
    disk_gb: int = 100


@dataclass(frozen=True)
class Config:
    """The complete identity of a dedicated, private qualification cluster."""

    project: str
    zone: str
    cluster: str
    version: str
    service_account: str
    master_cidr: str
    state_dir: Path
    network: str = "default"
    subnet: str = "default"
    rollout_timeout: int = 600

    @property
    def region(self) -> str:
        return self.zone.rsplit("-", 1)[0]

    @property
    def context(self) -> str:
        return f"gke_{self.project}_{self.zone}_{self.cluster}"

    @property
    def kubeconfig(self) -> Path:
        return self.state_dir / "kubeconfig"

    @property
    def pools(self) -> tuple[Pool, ...]:
        return (
            Pool("default-pool", "system", "e2-standard-2"),
            Pool("branchyard-gvisor", "gvisor", "n2-standard-4"),
            Pool("branchyard-microvm", "microvm", "n2-standard-4"),
        )

    @property
    def fingerprint(self) -> str:
        fields = {key: value for key, value in asdict(self).items() if key not in {"state_dir", "rollout_timeout"}}
        fields.update(
            controller_sha256=CONTROLLER_SHA256,
            pools=[asdict(pool) for pool in self.pools],
            datapath_provider="ADVANCED_DATAPATH",
        )
        return hashlib.sha256(json.dumps(fields, sort_keys=True).encode()).hexdigest()[:16]

    @property
    def labels(self) -> dict[str, str]:
        return {OWNER_LABEL: self.cluster, CONFIG_LABEL: self.fingerprint}

    def validate(self) -> None:
        for key in ("project", "cluster", "network", "subnet"):
            if not re.fullmatch(r"[a-z][a-z0-9-]{0,61}[a-z0-9]", getattr(self, key)):
                raise SetupError(f"--{key} must be a lowercase Google Cloud resource name")
        if not re.fullmatch(r"[a-z]+-[a-z]+\d-[a-z]", self.zone):
            raise SetupError("--zone must be a zone such as us-central1-a")
        match = re.fullmatch(r"1\.(\d+)\.(\d+)-gke\.(\d+)", self.version)
        if not match or tuple(map(int, match.groups())) < (37, 0, 4713000):
            raise SetupError("--version must pin a complete GKE release at least 1.37.0-gke.4713000")
        if not re.fullmatch(r"[a-z0-9][a-z0-9._-]+@[a-z0-9.-]+\.gserviceaccount\.com", self.service_account):
            raise SetupError("--service-account must name an existing Google service account")
        try:
            cidr = ipaddress.IPv4Network(self.master_cidr)
        except ValueError:
            raise SetupError("--master-cidr must be a private IPv4 /28 network") from None
        private_ranges = ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")
        if cidr.prefixlen != 28 or not any(cidr.subnet_of(ipaddress.IPv4Network(value)) for value in private_ranges):
            raise SetupError("--master-cidr must be a private IPv4 /28 network")
        if not self.state_dir.is_absolute() or self.state_dir.is_symlink() or self.rollout_timeout < 1:
            raise SetupError("--state-dir must be an absolute non-symlink path; --rollout-timeout must be positive")


def gcloud(config: Config, *args: str) -> list[str]:
    """An explicitly scoped cloud command, independent of the active project."""
    return ["gcloud", *args, f"--project={config.project}", "--quiet"]


def kubectl(config: Config, *args: str) -> list[str]:
    """A command that cannot use the operator's current kubeconfig context."""
    return ["kubectl", f"--kubeconfig={config.kubeconfig}", f"--context={config.context}", *args]


def pool_labels(config: Config, pool: Pool) -> dict[str, str]:
    """Ownership and runtime labels carried by every node in a pool."""
    return {**config.labels, RUNTIME_LABEL: pool.runtime}


def create_command(config: Config, pool: Pool) -> list[str]:
    """The exact cloud creation command for the system cluster or a runtime pool."""
    if pool.runtime == "system":
        args = [
            "container",
            "clusters",
            "create",
            config.cluster,
            f"--zone={config.zone}",
            f"--cluster-version={config.version}",
            "--release-channel=None",
            "--enable-ip-alias",
            "--enable-dataplane-v2",
            "--enable-private-nodes",
            "--enable-private-endpoint",
            f"--master-ipv4-cidr={config.master_cidr}",
            f"--network={config.network}",
            f"--subnetwork={config.subnet}",
            f"--workload-pool={config.project}.svc.id.goog",
            "--labels=" + ",".join(f"{key}={value}" for key, value in config.labels.items()),
        ]
    else:
        args = [
            "container",
            "node-pools",
            "create",
            pool.name,
            f"--cluster={config.cluster}",
            f"--zone={config.zone}",
            f"--node-version={config.version}",
            f"--sandbox=type={pool.runtime}",
            "--enable-private-nodes",
        ]
        if pool.runtime == "microvm":
            args.append("--enable-nested-virtualization")
    args += [
        f"--machine-type={pool.machine_type}",
        f"--num-nodes={pool.nodes}",
        f"--disk-size={pool.disk_gb}",
        "--image-type=COS_CONTAINERD",
        "--no-enable-autoupgrade",
        "--workload-metadata=GKE_METADATA",
        f"--service-account={config.service_account}",
        "--scopes=" + ",".join(NODE_SCOPES),
        "--node-labels=" + ",".join(f"{key}={value}" for key, value in pool_labels(config, pool).items()),
    ]
    return gcloud(config, *args)


def plan(config: Config) -> dict[str, Any]:
    """Return the complete offline plan without commands, files or network calls."""
    config.validate()
    return {
        "mode": "plan",
        "project": config.project,
        "zone": config.zone,
        "cluster": config.cluster,
        "gke_version": config.version,
        "service_account": config.service_account,
        "node_scopes": list(NODE_SCOPES),
        "ownership_labels": config.labels,
        "network": {
            "name": config.network,
            "subnet": config.subnet,
            "private_nodes": True,
            "private_endpoint": True,
            "datapath_provider": "ADVANCED_DATAPATH",
            "runtime_namespace_policy": "default-deny-ingress-and-egress",
            "metadata_egress": {
                "default": "denied",
                "per_workload_allow_must_exclude": ["169.254.169.254/32", "169.254.169.252/32"],
            },
            "master_cidr": config.master_cidr,
            "requires_existing_nat": True,
            "creates_network_resources": False,
        },
        "footprint": {
            "nodes": 3,
            "vcpus": 10,
            "memory_gib": 40,
            "boot_disk_gb": 300,
            "pools": [asdict(pool) for pool in config.pools],
        },
        "controller": {
            "version": CONTROLLER_VERSION,
            "url": CONTROLLER_URL,
            "sha256": CONTROLLER_SHA256,
            "image": CONTROLLER_IMAGE,
        },
        "namespaces": {runtime: f"branchyard-{runtime}" for runtime in RUNTIMES},
        "kubeconfig": str(config.kubeconfig),
        "context": config.context,
        "creates_iam_grants": False,
        "creation_commands": [create_command(config, pool) for pool in config.pools],
        "preflight": [
            "Verify source subnet, Private Google Access, existing all-subnet NAT and visible CIDR ranges",
            "Refuse a master CIDR overlapping subnet, secondary, non-default route or other cluster ranges",
            "Refuse foreign or incompatible clusters, node pools and Kubernetes setup objects",
        ],
        "next": "Review this plan; --apply installs controllers and policies but runs no workload or model",
    }


class Runner:
    """Run bounded argument vectors and record only commands and exit codes."""

    def __init__(self, config: Config):
        self.config = config
        self.env = {**os.environ, "KUBECONFIG": str(config.kubeconfig), "CLOUDSDK_CORE_DISABLE_PROMPTS": "1"}

    def call(self, args: list[str], *, optional: bool = False, timeout: int = 90) -> str | None:
        try:
            result = subprocess.run(args, env=self.env, text=True, capture_output=True, timeout=timeout, check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise SetupError(f"{args[0]} did not finish: {type(error).__name__}; inspect the tool and rerun") from None
        with (self.config.state_dir / "commands.jsonl").open("a") as log:
            log.write(json.dumps({"argv": args, "exit_code": result.returncode}) + "\n")
        if result.returncode:
            if optional and re.search(r"\b(?:NOT_FOUND|NotFound|404)\b", result.stderr):
                return None
            raise SetupError(
                f"{args[0]} {' '.join(args[1:4])} failed (exit {result.returncode}); inspect access and rerun"
            )
        return result.stdout.strip()

    def json(self, args: list[str], *, optional: bool = False) -> Any:
        text = self.call(args, optional=optional)
        if text is None:
            return None
        try:
            return json.loads(text)
        except ValueError:
            raise SetupError(f"{args[0]} returned invalid JSON; inspect the installed tool") from None


def check_labels(labels: dict[str, str], config: Config, name: str) -> None:
    """Refuse ownership or configuration changes instead of adopting an object."""
    if any(labels.get(key) != value for key, value in config.labels.items()):
        raise SetupError(f"{name} is foreign or has a different configuration; choose another dedicated deployment")


def check_pool(pool: dict[str, Any], config: Config, desired: Pool) -> None:
    """Verify every property that could change a pool's isolation or footprint."""
    node = pool.get("config", {})
    check_labels(node.get("labels", {}), config, f"node pool {desired.name}")
    expected = {
        "machineType": desired.machine_type,
        "diskSizeGb": desired.disk_gb,
        "imageType": "COS_CONTAINERD",
        "serviceAccount": config.service_account,
    }
    if (
        pool.get("name") != desired.name
        or pool.get("version") != config.version
        or pool.get("status") != "RUNNING"
        or pool.get("initialNodeCount") != desired.nodes
        or any(node.get(key) != value for key, value in expected.items())
        or node.get("labels", {}).get(RUNTIME_LABEL) != desired.runtime
        or pool.get("management", {}).get("autoUpgrade")
        or pool.get("autoscaling", {}).get("enabled")
        or node.get("spot")
        or node.get("preemptible")
        or node.get("workloadMetadataConfig", {}).get("mode") != "GKE_METADATA"
    ):
        raise SetupError(f"node pool {desired.name} differs from the plan; no pool was changed")
    runtime = node.get("sandboxConfig", {}).get("type", "UNSPECIFIED").lower()
    expected_runtime = desired.runtime if desired.runtime != "system" else "unspecified"
    nested = node.get("advancedMachineFeatures", {}).get("enableNestedVirtualization", False)
    if runtime != expected_runtime or bool(nested) != (desired.runtime == "microvm"):
        raise SetupError(f"node pool {desired.name} has a different sandbox runtime or nested-virtualization setting")
    if set(NODE_SCOPES) != set(node.get("oauthScopes", [])):
        raise SetupError(f"node pool {desired.name} differs from its planned node scopes")


def check_cluster(cluster: dict[str, Any], config: Config) -> None:
    """Allow only the exact owned Standard cluster, including partial setup."""
    check_labels(cluster.get("resourceLabels", {}), config, "cluster")
    private = cluster.get("privateClusterConfig", {})
    if (
        cluster.get("name") != config.cluster
        or cluster.get("location") != config.zone
        or cluster.get("status") != "RUNNING"
        or cluster.get("autopilot", {}).get("enabled")
        or cluster.get("currentMasterVersion") != config.version
        or cluster.get("network", "").rsplit("/", 1)[-1] != config.network
        or cluster.get("subnetwork", "").rsplit("/", 1)[-1] != config.subnet
        or not private.get("enablePrivateNodes")
        or not private.get("enablePrivateEndpoint")
        or private.get("masterIpv4CidrBlock") != config.master_cidr
        or cluster.get("workloadIdentityConfig", {}).get("workloadPool") != f"{config.project}.svc.id.goog"
        or cluster.get("networkConfig", {}).get("datapathProvider") != "ADVANCED_DATAPATH"
        or cluster.get("releaseChannel", {}).get("channel", "UNSPECIFIED") not in {"UNSPECIFIED", "NONE"}
    ):
        raise SetupError("cluster differs from the pinned private Standard plan; no cluster was changed")
    wanted = {pool.name: pool for pool in config.pools}
    for pool in cluster.get("nodePools", []):
        if pool.get("name") not in wanted:
            raise SetupError("cluster has an unplanned node pool; no pool was changed")
        check_pool(pool, config, wanted[pool["name"]])


def preflight(config: Config, run: Runner) -> dict[str, Any] | None:
    """Inspect all cloud boundaries before creating a cluster or node pool."""
    for binary in ("gcloud", "kubectl", "curl"):
        if not shutil.which(binary):
            raise SetupError(f"install {binary} before applying this plan")
    version = run.json(["gcloud", "version", "--format=json"]).get("Google Cloud SDK", "")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version) or int(version.split(".")[0]) < 590:
        raise SetupError("Google Cloud SDK 590 or newer is required for managed microvm node pools")
    account = run.json(gcloud(config, "iam", "service-accounts", "describe", config.service_account, "--format=json"))
    if account.get("disabled") or account.get("email") != config.service_account:
        raise SetupError("the explicit node service account is disabled or does not match")
    cluster = run.json(
        gcloud(config, "container", "clusters", "describe", config.cluster, f"--zone={config.zone}", "--format=json"),
        optional=True,
    )
    if cluster:
        check_cluster(cluster, config)
    subnets = run.json(gcloud(config, "compute", "networks", "subnets", "list", "--format=json"))
    matching = [s for s in subnets if s.get("network", "").rsplit("/", 1)[-1] == config.network]
    selected = [
        s
        for s in matching
        if s.get("name") == config.subnet and s.get("region", "").rsplit("/", 1)[-1] == config.region
    ]
    if len(selected) != 1 or not selected[0].get("privateIpGoogleAccess"):
        raise SetupError("the selected subnet is absent or lacks Private Google Access; no network was changed")
    routers = run.json(gcloud(config, "compute", "routers", "list", "--format=json"))
    if not any(
        router.get("network", "").rsplit("/", 1)[-1] == config.network
        and router.get("region", "").rsplit("/", 1)[-1] == config.region
        and any(
            nat.get("sourceSubnetworkIpRangesToNat") == "ALL_SUBNETWORKS_ALL_IP_RANGES"
            for nat in router.get("nats", [])
        )
        for router in routers
    ):
        raise SetupError("private nodes require existing regional all-subnet Cloud NAT; no router was changed")
    clusters = run.json(gcloud(config, "container", "clusters", "list", "--format=json"))
    routes = run.json(gcloud(config, "compute", "routes", "list", "--format=json"))
    ranges = [s.get("ipCidrRange") for s in matching]
    ranges += [r.get("ipCidrRange") for s in matching for r in s.get("secondaryIpRanges", [])]
    ranges += [
        r.get("destRange")
        for r in routes
        if r.get("network", "").rsplit("/", 1)[-1] == config.network
        and r.get("destRange") != "0.0.0.0/0"
        and not (cluster and r.get("destRange") == config.master_cidr)
    ]
    for other in clusters:
        if other.get("network", "").rsplit("/", 1)[-1] != config.network:
            continue
        if other.get("name") == config.cluster and other.get("location") == config.zone:
            continue
        ranges += [
            other.get("privateClusterConfig", {}).get("masterIpv4CidrBlock"),
            other.get("clusterIpv4Cidr"),
            other.get("servicesIpv4Cidr"),
        ]
    for value in filter(None, ranges):
        try:
            observed = ipaddress.ip_network(value)
        except ValueError:
            raise SetupError("cloud networking returned an invalid CIDR; inspect it before applying") from None
        if observed.version == 4 and ipaddress.ip_network(config.master_cidr).overlaps(observed):
            raise SetupError(f"--master-cidr overlaps existing range {value}; choose a non-overlapping /28")
    return cluster


def policy_objects(config: Config) -> list[dict[str, Any]]:
    """Namespace-scoped runtime and containment rules for Sandboxes and their Pods."""
    result = []
    for runtime in RUNTIMES:
        labels = {**config.labels, RUNTIME_LABEL: runtime}
        result.append(
            {
                "apiVersion": "v1",
                "kind": "Namespace",
                "metadata": {
                    "name": f"branchyard-{runtime}",
                    "labels": {**labels, NETWORK_POLICY_LABEL: "enforced-v2"},
                },
            }
        )
        result.append(
            {
                "apiVersion": "networking.k8s.io/v1",
                "kind": "NetworkPolicy",
                "metadata": {"name": "branchyard-default-deny", "namespace": f"branchyard-{runtime}", "labels": labels},
                "spec": {"podSelector": {}, "policyTypes": ["Ingress", "Egress"]},
            }
        )
        for kind, group, resource, path in (
            ("pod", "", "pods", "object.spec"),
            ("sandbox", "agents.x-k8s.io", "sandboxes", "object.spec.podTemplate.spec"),
        ):
            name = f"{config.cluster}-{runtime}-{kind}"
            checks = [
                (
                    f"has(variables.pod.runtimeClassName) && variables.pod.runtimeClassName == '{runtime}'",
                    "runtimeClassName must match this namespace's managed runtime",
                ),
                (
                    f"has(variables.pod.nodeSelector) && 'sandbox.gke.io/runtime' in variables.pod.nodeSelector && "
                    f"variables.pod.nodeSelector['sandbox.gke.io/runtime'] == '{runtime}'",
                    "nodeSelector must select this namespace's managed runtime",
                ),
                (
                    "has(variables.pod.tolerations) && variables.pod.tolerations.exists(t, "
                    f"t.key == 'sandbox.gke.io/runtime' && has(t.value) && t.value == '{runtime}' && "
                    "has(t.effect) && t.effect == 'NoSchedule' && (!has(t.operator) || t.operator == 'Equal'))",
                    "tolerations must select this namespace's managed runtime",
                ),
                (
                    "has(variables.pod.automountServiceAccountToken) && !variables.pod.automountServiceAccountToken",
                    "service account token automount must be disabled",
                ),
                (
                    "has(variables.pod.securityContext) && has(variables.pod.securityContext.runAsNonRoot) && "
                    "variables.pod.securityContext.runAsNonRoot && has(variables.pod.securityContext.runAsUser) && "
                    "variables.pod.securityContext.runAsUser == 1000 && "
                    "has(variables.pod.securityContext.runAsGroup) && "
                    "variables.pod.securityContext.runAsGroup == 1000",
                    "workloads must run as non-root UID/GID 1000",
                ),
                (
                    "(!has(variables.pod.hostNetwork) || !variables.pod.hostNetwork) && "
                    "(!has(variables.pod.hostPID) || !variables.pod.hostPID) && "
                    "(!has(variables.pod.hostIPC) || !variables.pod.hostIPC)",
                    "host namespaces are forbidden",
                ),
                (
                    "!has(variables.pod.volumes) || variables.pod.volumes.all(v, !has(v.hostPath) && "
                    "(!has(v.projected) || v.projected.sources.all(s, !has(s.serviceAccountToken) && "
                    "!has(s.podCertificate))))",
                    "host paths and projected identity credentials are forbidden",
                ),
                (
                    "!has(variables.pod.securityContext.sysctls) || variables.pod.securityContext.sysctls.size() == 0",
                    "custom sysctls are forbidden",
                ),
                (
                    "variables.containers.size() > 0 && variables.containers.all(c, has(c.securityContext) && "
                    "(!has(c.securityContext.runAsUser) || c.securityContext.runAsUser == 1000) && "
                    "(!has(c.securityContext.runAsGroup) || c.securityContext.runAsGroup == 1000) && "
                    "(!has(c.securityContext.runAsNonRoot) || c.securityContext.runAsNonRoot) && "
                    "has(c.securityContext.allowPrivilegeEscalation) && !c.securityContext.allowPrivilegeEscalation && "
                    "(!has(c.securityContext.privileged) || !c.securityContext.privileged) && "
                    "has(c.securityContext.capabilities) && has(c.securityContext.capabilities.drop) && "
                    "'ALL' in c.securityContext.capabilities.drop && (!has(c.securityContext.capabilities.add) || "
                    "c.securityContext.capabilities.add.size() == 0) && (!has(c.securityContext.procMount) || "
                    "c.securityContext.procMount == 'Default'))",
                    "containers must drop all capabilities and privileges",
                ),
                (
                    "variables.containers.all(c, has(c.resources) && has(c.resources.limits) && "
                    "'cpu' in c.resources.limits && 'memory' in c.resources.limits && "
                    "quantity(string(c.resources.limits['cpu'])).compareTo(quantity('0')) > 0 && "
                    "quantity(string(c.resources.limits['memory'])).compareTo(quantity('0')) > 0)",
                    "every container must have positive CPU and memory limits",
                ),
                (
                    "variables.containers.all(c, !has(c.ports) || c.ports.all(p, !has(p.hostPort) || p.hostPort == 0))",
                    "host ports are forbidden",
                ),
                (
                    "variables.containers.all(c, !has(c.volumeMounts) || c.volumeMounts.all(m, "
                    "!m.mountPath.startsWith('/var/run/branchyard-identity') || "
                    "(m.mountPath == '/var/run/branchyard-identity' && has(m.readOnly) && m.readOnly && "
                    "!has(m.subPath) && !has(m.subPathExpr) && has(variables.pod.volumes) && "
                    "variables.pod.volumes.exists(v, v.name == m.name && has(v.downwardAPI) && "
                    "v.downwardAPI.items.exists(i, i.path == 'pod-uid' && has(i.fieldRef) && "
                    "i.fieldRef.fieldPath == 'metadata.uid')))))",
                    "the protected Pod identity mount must be a read-only Downward API volume",
                ),
            ]
            result.append(
                {
                    "apiVersion": "admissionregistration.k8s.io/v1",
                    "kind": "ValidatingAdmissionPolicy",
                    "metadata": {"name": name, "labels": config.labels},
                    "spec": {
                        "failurePolicy": "Fail",
                        "matchConstraints": {
                            "namespaceSelector": {"matchLabels": labels},
                            "resourceRules": [
                                {
                                    "apiGroups": [group],
                                    "apiVersions": ["*"],
                                    "operations": ["CREATE", "UPDATE"],
                                    "resources": [resource, "pods/ephemeralcontainers"]
                                    if kind == "pod"
                                    else [resource],
                                }
                            ],
                        },
                        "variables": [
                            {"name": "pod", "expression": path},
                            {
                                "name": "containers",
                                "expression": "variables.pod.containers + (has(variables.pod.initContainers) ? "
                                "variables.pod.initContainers : []) + (has(variables.pod.ephemeralContainers) ? "
                                "variables.pod.ephemeralContainers : [])",
                            },
                        ],
                        "validations": [{"expression": expr, "message": message} for expr, message in checks],
                    },
                }
            )
            result.append(
                {
                    "apiVersion": "admissionregistration.k8s.io/v1",
                    "kind": "ValidatingAdmissionPolicyBinding",
                    "metadata": {"name": name, "labels": config.labels},
                    "spec": {"policyName": name, "validationActions": ["Deny"]},
                }
            )
    return result


def controller_asset(config: Config, run: Runner) -> Path:
    """Fetch and verify the pinned controller before creating cloud resources."""
    asset = config.state_dir / "controller.yaml"
    if not asset.exists():
        staged = asset.with_suffix(".download")
        run.call(["curl", "--proto", "=https", "--tlsv1.2", "-fsSL", CONTROLLER_URL, "-o", str(staged)], timeout=300)
        if hashlib.sha256(staged.read_bytes()).hexdigest() != CONTROLLER_SHA256:
            raise SetupError("controller release checksum mismatch; no controller was installed")
        staged.chmod(0o600)
        staged.rename(asset)
    if hashlib.sha256(asset.read_bytes()).hexdigest() != CONTROLLER_SHA256:
        raise SetupError("cached controller checksum mismatch; inspect private state before retrying")
    return asset


def install(config: Config, run: Runner, asset: Path) -> None:
    """Install verified controller objects and ownership-fenced runtime policies."""
    rendered = run.json(
        kubectl(config, "create", "--dry-run=client", "--validate=false", "-f", str(asset), "-o", "json")
    )
    items = rendered.get("items", []) if isinstance(rendered, dict) and rendered.get("kind") == "List" else []
    if not items or not all(isinstance(item, dict) for item in items):
        raise SetupError("kubectl did not render the pinned controller as a JSON List")
    for item in items:
        metadata = item.setdefault("metadata", {})
        metadata.setdefault("labels", {}).update(config.labels)
        if item.get("kind") == "Deployment" and metadata.get("name") == "agent-sandbox-controller":
            for container in item["spec"]["template"]["spec"]["containers"]:
                if container.get("name") == "agent-sandbox-controller":
                    container["image"] = CONTROLLER_IMAGE
    items += policy_objects(config)
    for item in items:
        meta = item["metadata"]
        args = ["get", item["kind"], meta["name"], "--ignore-not-found", "-o", "json"]
        if "namespace" in meta:
            args += ["--namespace", meta["namespace"]]
        value = run.call(kubectl(config, *args))
        if value:
            try:
                observed = json.loads(value)
                check_labels(observed.get("metadata", {}).get("labels", {}), config, f"{item['kind']}/{meta['name']}")
            except ValueError:
                raise SetupError("kubectl returned invalid ownership metadata; no setup object was changed") from None
    manifest = config.state_dir / "setup.json"
    manifest.write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": items}, indent=2) + "\n")
    manifest.chmod(0o600)
    run.call(
        kubectl(
            config, "apply", "--server-side", "--field-manager=branchyard-agent-sandbox-setup", "-f", str(manifest)
        ),
        timeout=config.rollout_timeout,
    )
    run.call(
        kubectl(
            config,
            "rollout",
            "status",
            "deployment/agent-sandbox-controller",
            "-n",
            "agent-sandbox-system",
            f"--timeout={config.rollout_timeout}s",
        ),
        timeout=config.rollout_timeout + 30,
    )
    for runtime in RUNTIMES:
        value = run.json(kubectl(config, "get", "runtimeclass", runtime, "-o", "json"))
        if value.get("handler") != runtime:
            raise SetupError(f"RuntimeClass/{runtime} does not select its expected handler")


def apply(config: Config, run: Runner | None = None) -> dict[str, Any]:
    """Apply only the complete, inspected dedicated deployment; run no workload."""
    config.validate()
    config.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    config.state_dir.chmod(0o700)
    run = run or Runner(config)
    cluster = preflight(config, run)
    asset = controller_asset(config, run)
    if cluster is None:
        run.call(create_command(config, config.pools[0]), timeout=1800)
        cluster = run.json(
            gcloud(
                config, "container", "clusters", "describe", config.cluster, f"--zone={config.zone}", "--format=json"
            )
        )
        check_cluster(cluster, config)
    present = {pool["name"] for pool in cluster.get("nodePools", [])}
    for pool in config.pools:
        if pool.name not in present:
            run.call(create_command(config, pool), timeout=1800)
        observed = run.json(
            gcloud(
                config,
                "container",
                "node-pools",
                "describe",
                pool.name,
                f"--cluster={config.cluster}",
                f"--zone={config.zone}",
                "--format=json",
            )
        )
        check_pool(observed, config, pool)
    run.call(
        gcloud(
            config, "container", "clusters", "get-credentials", config.cluster, f"--zone={config.zone}", "--internal-ip"
        )
    )
    config.kubeconfig.chmod(0o600)
    run.call(
        kubectl(
            config, "rollout", "status", "daemonset/anetd", "-n", "kube-system", f"--timeout={config.rollout_timeout}s"
        ),
        timeout=config.rollout_timeout + 30,
    )
    install(config, run, asset)
    result = {**plan(config), "mode": "applied", "workloads_run": False, "models_run": False}
    (config.state_dir / "deployment.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main(argv: list[str] | None = None) -> int:
    """Print a plan by default; return one for invalid or failed preparation."""
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("project", "zone", "cluster", "version", "service-account", "master-cidr"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--network", default="default")
    parser.add_argument("--subnet", default="default")
    parser.add_argument("--state-dir", type=Path)
    parser.add_argument("--rollout-timeout", type=int, default=600)
    parser.add_argument(
        "--apply", action="store_true", help="create only matching owned resources and install policies"
    )
    values = vars(parser.parse_args(argv))
    do_apply = values.pop("apply")
    if values["state_dir"] is None:
        values["state_dir"] = (
            Path.home() / ".local/state/branchyard/agent-sandbox-gke" / values["project"] / values["cluster"]
        )
    try:
        config = Config(**values)
        print(json.dumps(apply(config) if do_apply else plan(config), indent=2))
    except (SetupError, OSError) as error:
        print(f"agent-sandbox-gke: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
