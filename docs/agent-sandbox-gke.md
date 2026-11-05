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

## Mount the tool store

A turn can link its skills and connector packages from a shared, read-only tool store instead of copying them into each allocation ([tools model](tools-model.md#4-one-turn-remotely-lifecycle-and-delivery)). On GKE the store is a Cloud Storage bucket, filled with `by tools publish --gcs gs://BUCKET/PREFIX`, and mounted with the Cloud Storage FUSE CSI driver. Set it in the worker's configuration:

```toml
[tools]
store = "/opt/branchyard/store"      # the same path on the worker and in the pod
store_bucket = "BUCKET/PREFIX"       # or BUCKET
```

(A server uses `tools.store` and `tools.store_bucket` in its JSON file.) With `store_bucket` set, each pod the provider builds gets one more volume and one more mount on the runtime container:

| Where | Value |
|---|---|
| Pod annotation | `gke-gcsfuse/volumes: "true"`, so GKE injects the driver's sidecar |
| Volume `tool-store` | `csi` with `driver: gcsfuse.csi.storage.gke.io`, `readOnly: true`, `volumeAttributes: {bucketName: BUCKET, mountOptions: "implicit-dirs,file-mode=755,dir-mode=755,file-cache:enable-parallel-downloads:true"}`; a `PREFIX` adds `,only-dir=PREFIX` |
| Mount | `readOnly: true` at `store` |

The provider accepts the sidecar GKE injects (a container or an init container named `gke-gcsfuse-sidecar`, and volumes named `gke-gcsfuse-*`) when it checks the live pod, and only those. Without `store_bucket` the pod is what it was, and a turn copies.

What the cluster needs, none of which the setup helper does:

1. The driver: `gcloud container clusters update CLUSTER --update-addons=GcsFuseCsiDriver=ENABLED` (GKE 1.27 or later).
2. Read access for the namespace's workload identity. The pod's service account is the namespace's `default`; grant it `roles/storage.objectViewer` on the bucket:

   ```sh
   gcloud storage buckets add-iam-policy-binding gs://BUCKET \
     --role roles/storage.objectViewer \
     --member "principal://iam.googleapis.com/projects/PROJECT_NUMBER/locations/global/workloadIdentityPools/PROJECT_ID.svc.id.goog/subject/ns/NAMESPACE/sa/default"
   ```
3. A route to Cloud Storage from the pod. The sidecar runs in the pod's network, so it needs one even under `network = "deny"` (the default). The provider now adds it: with `store_bucket` set, the allocation's egress always allows Cloud Storage, `deny` included, and nothing else. See [Egress](#egress) for what it generates. The subnet must have Private Google Access (the setup helper already requires it) for the IP ranges to be useful; the name rule works through Cloud NAT.

Not done: no pod mounted a bucket on a live cluster. Whether the driver works in the gVisor and the Kata runtimes of this provider, and the policies below, are unverified. A bucket keeps no file modes, so the mount sets `file-mode=755` and `dir-mode=755` (already in the options above) to keep connector launchers (`bin/<id>`) runnable; read-only, so nothing writes through it.

## Egress

A sandbox may reach the gateway, the model endpoint, the package mirrors and, when the tool store is a bucket, Cloud Storage, and nothing else ([tools model](tools-model.md#4-one-turn-remotely-lifecycle-and-delivery), step 5). The namespace's default deny stays; the provider adds, per allocation, objects generated from the turn and owned by its Sandbox:

| `network` | What the allocation gets |
|---|---|
| `deny`, nothing to reach | No policy of its own: DNS, metadata and the Internet are denied, as before |
| `deny`, with something to reach | A `NetworkPolicy` (egress only) and, when it lists hosts, a GKE `FQDNNetworkPolicy` |
| `unrestricted` | A `NetworkPolicy` for the IPv4 Internet except the GKE metadata addresses, plus cluster DNS, as before; no FQDN policy |

Under `deny` with something to reach, the `NetworkPolicy` allows:

- cluster DNS (`kube-dns` and `node-local-dns` in `kube-system`, port 53), which the FQDN policy needs to resolve names;
- the gateway's pods, when `gateway_service` names an in-cluster Service: pods in `namespace` labelled `app.kubernetes.io/name=NAME`, on `port` (a `NetworkPolicy` selects pods, not a Service, so the Service's pods must carry that label);
- with `store_bucket`, the Private Google Access ranges `199.36.153.4/30` (`restricted.googleapis.com`) and `199.36.153.8/30` (`private.googleapis.com`), TCP 443, as IP blocks. There is no `0.0.0.0/0`.

The `FQDNNetworkPolicy` (`networking.gke.io/v1alpha1`, selecting the pod by its owner label) allows TCP 443 to:

- `egress_hosts` of `[agent_sandbox]`: the model endpoint, for example `api.anthropic.com`;
- `[network] mirrors` that are a name or `*.suffix` on 443 (a mirror on another port, or an address, is not listed);
- the host of `[connectors] sandbox_gateway` when it is a name;
- with `store_bucket`, `storage.googleapis.com`.

```toml
[agent_sandbox]
context = "gke_my-project_us-central1-a_branchyard-sandbox"
namespace = "branchyard-gvisor"
image = "us-docker.pkg.dev/my-project/by/worker@sha256:..."
network = "deny"
egress_hosts = ["api.anthropic.com"]
gateway_service = { namespace = "branchyard-system", name = "anvil-gateway", port = 8931 }

[network]
allow = []                                   # the branch's own policy: nothing
mirrors = ["pypi.org", "*.pythonhosted.org"]

[tools]
store = "/opt/branchyard/store"
store_bucket = "BUCKET/PREFIX"               # Cloud Storage is allowed because of this
```

`egress_hosts` and `gateway_service` are the worker's configuration (its `[agent_sandbox]` section, as `Yard::configure` reads it), not part of a branch's provider options: a yard that was not configured has neither, and then only the mirrors, if any, and Cloud Storage are listed. A server has no such keys yet. A change between two uses of one allocation fails the receipt check, and the allocation is destroyed and made again. Each object is bound to the receipt, as the Sandbox and the Pod are. Before every exec the provider reads the namespace's policies and refuses the allocation if its own `NetworkPolicy` or `FQDNNetworkPolicy` has drifted (a host, port, range or rule added or removed), if one is missing, or if another policy that could select the pod exists. The FQDN policies are read only when the allocation has one. Removing the allocation deletes both by UID.

What the cluster needs, none of which the setup helper does: Dataplane V2 with FQDN network policy enabled (`gcloud container clusters update CLUSTER --enable-fqdn-network-policy`; check the GKE release notes for the minimum version, which this page has not verified) and the `FQDNNetworkPolicy` API. Hosts are limited to 32 per allocation.

Not done: no pod ran under these policies on a live cluster. Whether Dataplane V2 enforces the FQDN list for a gVisor or a Kata pod, and whether the sidecar reads a bucket through the ranges, are unverified. Evidence: hermetic, the fake `kubectl` of `crates/branchyard-agent-sandbox/tests/fake_kubectl.py`.

## The tool tree at `/tools`

Each pod mounts an `emptyDir` volume `tools` at `/tools` (64 MiB). The pod copies its home in, so the engine copies the tree it wrote under the home (`.branchyard/tools`) into this volume once the home is in, and sets `BRANCHYARD_TOOLS=/tools` for the harness. The volume is not read-only: the root filesystem is, and the engine writes through the same container; what a harness writes there is dropped with the pod. Container sandboxes get the same, as a private volume. Not done: a turn on a live cluster reading the tree there.

## Credential proxy sidecar

A harness in a pod should hold no gateway token. With the server's `connectors.sandbox_server` set ([server](server.md#turn-tokens-for-sandboxes)), each pod the provider builds gets a second container, `credential-proxy`, beside `runtime` ([connectors](connectors.md#the-credential-proxy-in-the-sandbox)):

| Where | Value |
|---|---|
| Container `credential-proxy` | image `connectors.sandbox_proxy_image` (`repository@sha256:digest`, with `by` on its `PATH`) or the allocation's own; command `by credential-proxy --config /branchyard/home/.branchyard/credential-proxy.json --wait`; `GCE_METADATA_HOST=169.254.169.254`; 50m CPU and 64Mi requested, 250m and 128Mi at most; no privilege escalation, read-only root, all capabilities dropped |
| Mount | the `home` volume at `/branchyard/home`, `readOnly: true`: the sidecar finds its settings once the engine has copied the home in, and can change nothing in it |
| Network policy | the metadata server `169.254.169.254/32` is no longer excepted from the pod's egress (the node-local `169.254.169.252/32` still is) |
| Harness environment | `ANVIL_GATEWAY_URL=http://127.0.0.1:18900/mcp`; no token file, no token |

The sidecar and the harness share the pod's network, so the harness reaches the proxy on loopback; the settings file (`.branchyard/credential-proxy.json`) holds the server URL, the gateway URL, the branch, turn and manifest digest, and no secret. The sidecar proves the turn as the pod's Workload Identity: it asks the metadata server for an ID token for the server's audience and the server checks the service account against `connectors.sandbox_identities`.

Refused up front: `credential_proxy` with `network = "deny"` (the sidecar must reach the server and the metadata server) and an unpinned `sandbox_proxy_image`. The provider's live-pod checks accept exactly these two containers and apply the same limits to both; a third fails verification.

What the cluster needs, none of which the setup helper does:

1. Workload Identity on the cluster and the namespace's `default` Kubernetes service account bound to a Google service account (`roles/iam.workloadIdentityUser`), listed in the server's `sandbox_identities` (with its `tenant` if it serves one).
2. A route from the pod to the server and to Cloud Storage if the store is mounted: `network = "unrestricted"`.
3. An image with `by` that a pod can pull.

A pod's containers share one network policy: with the sidecar the metadata server is open to the harness as well. The token it could ask for there is the turn's own, bound to its manifest, refused when the turn ends, and lives five minutes; it cannot read a token from a file, because none exists.

Not done: no pod ran this on a live cluster. Workload Identity ID tokens from the pod's metadata server, the NetworkPolicy exception, the sidecar in the gVisor and Kata runtimes, and the sidecar's image digest check on the live pod are unverified; hermetic tests (a fake `kubectl`) cover the pod spec, the policy and the verification only.

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
