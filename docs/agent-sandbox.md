# Native GKE Agent Sandbox provider

`branchyard-agent-sandbox` implements `SandboxProvider` against the Kubernetes
SIGs `agents.x-k8s.io/v1beta1` Sandbox API. It selects the managed GKE `gvisor`
or `microvm` RuntimeClass. GKE's managed microVM runtime uses Kata Containers
with Cloud Hypervisor and requires GKE `1.37.0-gke.4713000` or later. This is
separate from the Agent Substrate HTTP provider and the upstream example that
installs `kata-qemu` itself. See the [managed GKE runtime
guide](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/sandbox-pods)
and [runtime architecture](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/sandbox-pods).

Prepare the cluster and namespaces with `tools/agent_sandbox_gke.py` as described
in `agent-sandbox-gke.md`. The provider requires an explicit operator kubeconfig
context, a prepared namespace and a private receipt directory. It invokes the
host's `kubectl` with fixed argument vectors. Python 3.8 or newer must also be
on that host's `PATH`; readiness checks refuse allocation before cloud calls if
it is unavailable. A bounded stdio relay removes exactly the kubectl-owned
nonzero-exit trailer while preserving binary guest stderr and exit status.
It leaves other post-startup diagnostics intact, so transport errors can still
appear with workload output. Relay, kubectl and ordinary credential-plugin
children share the local transport process group for cancellation and startup
timeout cleanup. Kubeconfig, credential plugins,
service-account tokens and Kubernetes sockets stay outside the workload.

Live on 2 November 2026: the real Rust provider passed both managed runtimes in
deny and unrestricted network modes on GKE `1.37.1-gke.3020000`, with binary
exec/transfer, cancellation, effective UID/caps/no-new-privileges, metadata
denial, DNS/HTTPS and owned cleanup recorded in the
[dated qualification](evidence/gke-native-2026-11-02/README.md). The record also
preserves the fixed failures and the inconclusive optional Kata OOM stress
observation. Not done: this record does not qualify model calls, engine/console
execution, IPv6 probes or active guest seccomp filtering.

```rust,ignore
use branchyard_agent_sandbox::{AgentSandboxProvider, Config, Network, Runtime};
use branchyard_sandbox::{SandboxProvider, SandboxSpec};

let config = Config {
    context: "gke_project_zone_cluster".into(),
    namespace: "branchyard-gvisor".into(),
    runtime: Runtime::Gvisor,
    network: Network::Deny,
};
config.validate_spec(&spec)?; // Pure admission before allocating anything.
let provider = AgentSandboxProvider::new(config, private_state_dir)?;
provider.ensure(&spec)?;
// Run/copy through SandboxProvider::exec, then capture output before destroy.
provider.destroy(&spec.name)?;
```

Supply an immutable image reference, `repository@sha256:<64 lowercase hex
digits>`, in `SandboxSpec.image`. The image must provide `sh`, `cat`, `tail`,
`ps`, `tr`, `awk`, `kill`, `setsid`, `setpriv --no-new-privs`, `tar`, and the
selected harness. Programs
and their libraries must be readable/executable by UID/GID 1000 without a
root entrypoint. The root image is read-only. EmptyDir mounts provide private
writable `/workspace`, `/branchyard/home` and `/tmp`, and a fourth, `/tools`, that
the engine fills with the turn's tool tree after the home (`BRANCHYARD_TOOLS=/tools`;
see [tools model](tools-model.md#3-context-a-mounted-tool-tree-searched-not-pasted));
`HOME` is `/branchyard/home`. No host mount, additional container, PodSpec override,
credential volume or privileged initialization is accepted. Unset resources
select one CPU and 1024 MiB; requests equal limits. Explicit limits must be
positive. Mounted workspace/home data is copied out before allocation deletion.

The keepalive and every exec enter through `setpriv --no-new-privs --`, so both
PID 1 and payload/control scripts enforce the flag directly. They also retain
the Pod's `allowPrivilegeEscalation=false` and `RuntimeDefault` settings.
The adapter fails closed when the image lacks the required utility.

`Network::Deny` denies ingress and egress, including DNS. The provider verifies
the setup-owned namespace identity, `enforced-v2` label, exact default deny
policy and ready GKE `anetd` DaemonSet. Because Kubernetes policies are additive,
it also refuses other policies that could grant this allocation access.
`Network::Unrestricted` creates an allocation-owned policy permitting IPv4
egress except GKE metadata addresses `169.254.169.254` and `169.254.169.252`,
plus TCP/UDP port 53 to the `kube-system` kube-dns and node-local-dns Pods.
Dataplane V2 excludes
Pod traffic from IPBlock rules, so DNS requires that explicit selector rule.
Ingress and IPv6 Internet egress stay denied. This choice does not implement Branchyard's
required proxy-egress policy; the provider does not declare that capability.

The receipt directory must have mode 0700. Allocation locks serialize local
recovery, and mode-0600 receipts are atomically written and fsynced. A creation
intent records a random owner nonce, target, namespace UID and specification
digest before Kubernetes creation. Recovery accepts only resources carrying
that ownership and records their immutable Sandbox, Pod and policy UIDs.
The receipt also binds the observed RuntimeClass UID and records the backing
node UID, name and kubelet version. Managed handlers must be exactly `gvisor`
and `kata-clh`, respectively, with the matching RuntimeClass scheduling selector.
Requested image, effective limits, non-root security, actual Ready Pod/node
runtime, GCE provenance and running image digest are checked before execution.
The runtime classification is a checked configuration claim; live qualification
also tests the runtime's actual isolation properties.

Every startup and control exec first checks a protected read-only DownwardAPI
Pod UID file against the receipt. A same-name replacement receives no payload.
There is no TTY or user-argument shell interpolation. The shared process layer
preserves separate binary stdin/stdout/stderr and waits for a guest startup
acknowledgement. Its teardown reaches its process group; a process can escape
that group. Host credential-plugin diagnostics before acknowledgement and
in control errors are replaced with a generic message; guest stderr after
acknowledgement remains byte-exact. The engine must destroy the whole allocation
after normal output capture, cancellation or failure. `stop` and `destroy` both delete the allocation
with Kubernetes UID preconditions and foreground garbage collection, without
force deletion. Failed cleanup retains the receipt for retry. A name collision
or recorded UID mismatch blocks adoption and cleanup. Cleanup verifies the
recorded Pod and allocation policy identities before requesting foreground
deletion of their Sandbox; garbage collection then removes its controller-owned
children as part of the owned allocation. Pause, retention, snapshots,
ingress, host mounts and write scopes are not offered.

The host principal needs get/create/watch/delete on Sandboxes, Pods and
NetworkPolicies in the selected namespace, create on `pods/exec`, and read
access to the Namespace, RuntimeClass, Nodes and `kube-system/anetd` DaemonSet.
Direct `kubectl exec` avoids the Python SDK's sandboxd port-forward limitation
on gVisor and Kata. See the [upstream client
documentation](https://github.com/kubernetes-sigs/agent-sandbox/tree/main/clients/python/agentic-sandbox-client).

Replayed evidence: `crates/branchyard-agent-sandbox/tests/gke_runtimeclasses.json`
records the class UIDs, handlers and scheduling captured from GKE
`1.37.1-gke.3020000` on 2026-11-02. Not done: live adapter qualification from
this configuration fixture.

Hermetic evidence: `cargo test -p branchyard-agent-sandbox --locked --offline`
uses a fake kubectl with no network or model. It covers lost create responses,
intent recovery, receipt ownership, target and Pod replacement, UID deletion
races, effective runtime/image/resource/security drift, additive network-policy
refusal, binary pipes and startup UID binding. Not done: a live GKE qualification
or a real model run in this crate's hermetic tests.
