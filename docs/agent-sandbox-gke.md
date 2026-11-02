# Prepare Agent Sandbox on GKE

`tools/agent_sandbox_gke.py` prepares a dedicated private GKE Standard cluster
for the native Kubernetes Agent Sandbox API. Its default command prints an
offline JSON plan. `--apply` creates the planned cluster and node pools, installs
the pinned controller, and establishes runtime and network policies. It runs no
workload or model.

This is separate from [Agent Substrate on GKE](substrate-gke.md). The managed
`microvm` runtime uses Kata Containers and Cloud Hypervisor. It does not require
installing a custom Kata shim on nodes or deploying Substrate's `WorkerPool` and
`SandboxConfig` APIs.

## Prepare the cloud prerequisites

Use an authenticated Google Cloud CLI, a recent `kubectl`, `curl`, and Python
3.11 or newer. The operator needs permission to create and inspect GKE resources,
use the explicitly named node service account, and administer Kubernetes objects
inside the new cluster. The helper grants no IAM roles.

Choose a dedicated cluster name, a complete available GKE version, and an existing
node service account. The selected subnet must have Private Google Access and
regional Cloud NAT covering all subnet address ranges. The helper creates no
network, router, NAT, service account, or IAM binding.

The master CIDR must be a private IPv4 `/28` that overlaps neither the VPC's
subnet and secondary ranges, its visible non-default routes, nor the ranges of
other clusters on that network. The helper checks those observations before
creating a cluster. Also check any connected network whose routes are not
visible to this project's Compute API, including externally managed hybrid
routes. It never changes a route to make a conflicting CIDR work.

The API endpoint is private. Run the apply command from a host that already has
a route to that endpoint. NAT permits node downloads; it does not provide an
operator route into the control plane.

## Review and apply the plan

Replace the example names and verify that the selected GKE release is available
in the zone:

```sh
gcloud container get-server-config \
  --project=my-project --zone=us-central1-a --format=json

python3 tools/agent_sandbox_gke.py \
  --project=my-project \
  --zone=us-central1-a \
  --cluster=branchyard-sandbox-qualification \
  --version=1.37.1-gke.3020000 \
  --service-account=branchyard-nodes@my-project.iam.gserviceaccount.com \
  --master-cidr=172.30.80.0/28 \
  --state-dir=/var/tmp/branchyard-sandbox-qualification
```

The plan reports the exact pool sizes, CPU, memory and boot-disk footprint,
creation commands, controller release and checksums, node scopes, namespaces,
ownership labels and private connection settings. It runs no subprocess and
creates no file. Review ongoing compute and disk charges before repeating the
same command with `--apply`.

GKE needs an unsandboxed system pool as well as the runtime pools. The helper
uses separate managed gVisor and MicroVM pools; nested virtualization is enabled
only on the MicroVM pool. It pins node versions, disables node auto-upgrade,
and leaves node counts fixed. It never upgrades or resizes an existing cluster.
Dataplane V2 is enabled at creation and verified on retries, so Kubernetes
`NetworkPolicy` objects are enforced.

Every cloud resource must have the expected ownership and configuration labels.
An existing cluster or pool with another owner, version, runtime, node size,
scope set or networking configuration is refused before mutation. A partial
creation can be resumed by running the same command. Successful cloud creation
alone is not a successful controller or workload verification.

## Controllers and policies

The helper downloads a release-specific controller manifest and verifies its
committed SHA-256 before creating cloud resources. The controller image is also
pinned by digest. It accepts `kubectl`'s JSON object stream or a Kubernetes
`List`, labels the resulting
objects, checks existing object ownership, then uses server-side apply without
forcing field conflicts. It refuses a foreign controller namespace, CRD, role,
binding or policy.

The managed `gvisor` RuntimeClass must use handler `gvisor`; the managed
`microvm` RuntimeClass must use Kata's Cloud Hypervisor handler `kata-clh`.
An unexpected handler refuses setup.

Each runtime namespace has its own admission policies for both `Sandbox`
resources and their resulting Pods. A namespace accepts only its selected
runtime class, node selector and toleration. Workloads must disable service
account token automounting, run as UID/GID `1000:1000`, drop all capabilities,
deny privilege escalation, and declare positive CPU and memory limits for
every container. The same rules cover init containers and ephemeral containers.
Host namespaces, host paths, host ports, custom sysctls and projected identity
credentials are refused. Container security contexts cannot override the
required non-root identity.

The protected `/var/run/branchyard-identity` mount, when present, must be a
read-only Downward API volume exposing `metadata.uid` as `pod-uid`. A container
cannot substitute a writable or unrelated mount at that path.

Both runtime namespaces start with default-deny ingress and egress policies.
Per-workload policies must explicitly grant needed traffic. Keep the GKE
metadata addresses blocked even for workloads that can reach external services.
The helper's admission rules allow MicroVMs in their own namespace; it does not
apply the upstream example's gVisor-only policy to MicroVM workloads.

The setup marks a runtime namespace `branchyard-network-policy=enforced-v2`
only after checking the cluster's Dataplane V2 configuration and waiting for
its managed `anetd` DaemonSet. A provider should verify that label, the actual
default-deny policy and networking readiness before admitting work.

## Connection and records

The helper writes a private `kubeconfig` under the selected state directory and
passes both that file and the exact context to every Kubernetes command. It
does not select the operator's global context. `deployment.json` records the
applied plan. `commands.jsonl` records command argument vectors and exit codes;
command output and credentials are omitted.

Inspect the result through the private kubeconfig. An applied setup report
means the pools, controller and policies were prepared. Qualify the real
provider separately with file transfer, command execution, runtime identity,
network refusal, resource-limit and cleanup checks. A ready controller is not
evidence that a workload ran inside a MicroVM.

Delete a temporary deployment only after preserving its redacted evidence.
Check its current ownership labels and resource identity before deleting the
dedicated cluster. Deleting a cluster removes its managed pools; leave the
existing network, subnet, NAT, node service account and unrelated clusters
intact. The helper has no delete command.

## Evidence and limits

Hermetic verification on 2 November 2026 uses fake `gcloud`, `kubectl` and `curl`
commands in `tests/test_agent_sandbox_gke.py`. It checks plans without side
effects, ownership refusals, version and runtime pinning, CIDR conflicts,
controller checksum failures, scoped kubeconfig use, policy generation and
recovery after interrupted pool creation. Not done: these tests do not prove
live GKE provisioning, CEL admission decisions, network enforcement, a real
Kata guest, resource ceilings or a model conversation.

Primary deployment references are [managed GKE Sandbox](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/sandbox-pods),
[GKE Sandbox behavior and metadata restrictions](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/sandbox-pods),
[Dataplane V2 network policy enforcement](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/dataplane-v2),
and [the Agent Sandbox releases](https://github.com/kubernetes-sigs/agent-sandbox/releases).
