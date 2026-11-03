# Native GKE provider qualification, 2 November 2026

Live on 2 November 2026: the real Rust `AgentSandboxProvider` passed allocation,
inspect, binary exec, archive transfer, missing-program refusal, cancellation,
sibling isolation, network and owned cleanup checks on both managed GKE
runtimes. The dedicated cluster ran GKE `1.37.1-gke.3020000` and Agent Sandbox
controller `v1.0.9`. Each run used the public digest-pinned image recorded in
[manifest.json](manifest.json), with one CPU and 512 MiB requested and limited.
No fake binary was used in these live runs. Not done: model calls, engine or
console execution, IPv6 probing, and active guest seccomp filtering were not
qualified by this record; the optional Kata OOM stress result remains
inconclusive.

| Runtime | Network | Final record | Result |
| --- | --- | --- | --- |
| gVisor | Deny | [verification](gvisor-deny/verification.json) | Required conformance checks and cleanup passed. |
| gVisor | Unrestricted | [verification](gvisor-unrestricted/verification.json) | Required conformance checks and cleanup passed. |
| Kata MicroVM | Deny | [verification](microvm-deny/verification.json) | Required conformance checks and cleanup passed. Optional OOM stress unqualified. |
| Kata MicroVM | Unrestricted | [verification](microvm-unrestricted/verification.json) | Required conformance checks and cleanup passed. |

Each case records its actual Sandbox and Pod UIDs, node UID, kubelet version,
runtime label, immutable image identity, Pod resources and effective guest
security fields. The gVisor guest reported kernel `4.19.0-gvisor`; Kata reported
`6.6.94-g7b99f7ef5fbf`, while the backing nodes reported `6.18.55+`.
The RuntimeClasses used handlers `gvisor` and `kata-clh` respectively.

## What ran

The rerunnable [Rust example](../../../crates/branchyard-agent-sandbox/examples/gke_conformance.rs)
uses the public `SandboxProvider` API for lifecycle and execution. Read-only
kubectl calls capture the actual Kubernetes objects. Its
[guest probe](../../../crates/branchyard-agent-sandbox/examples/gke_probe.py) and
[resource probe](../../../crates/branchyard-agent-sandbox/examples/gke_resources.py)
run through the provider, with no model or harness conversation.

Each final case demonstrated:

- Repeated `ensure` and `inspect` of the same owned allocation, with exact
  Sandbox, Pod, runtime, node and image checks.
- UID/GID `1000:1000`, effective capabilities zero, and `NoNewPrivs=1` for both
  PID 1 and exec. Root was read-only; workspace, home and temporary directories
  were writable. No service-account token, Docker socket or KVM device was
  visible.
- Non-UTF-8 binary stdin/stdout/stderr, exit status `7`, and a 1,024-byte
  upload/download archive round trip. Missing programs failed before execution.
- Cancellation reaching a recorded descendant while a sibling exec survived.
  The unrestricted cases also stopped an allocation containing a child that
  escaped its original exec group, then performed idempotent destruction.
- Default-deny refusing external TCP and both metadata addresses. Unrestricted
  mode resolved DNS and received HTTPS `200` from `example.com`, while metadata
  at `169.254.169.254` and `169.254.169.252` stayed unreachable. Only project-id
  metadata was requested; no credential or token endpoint was queried.
- All owned allocations and allocation-specific policies absent after cleanup.
  The cluster, controller and pools were retained for subsequent validation.

## Failures found and fixed

The preserved [Kata failure](failed/microvm-deny-verification.json) reported
exec `NoNewPrivs=0` despite the Pod security settings. Commit `7e96ed06` starts
both the keepalive and every UID-guarded exec through
`setpriv --no-new-privs --`. Every final record verifies the effective flag for
both PID 1 and exec, while retaining the original Pod security settings.

Both runtimes reported guest `Seccomp=0` in the final probes. The Pod's
`RuntimeDefault` request is retained, but this record does not demonstrate an
active guest seccomp filter.

The preserved [unrestricted DNS failure](failed/gvisor-unrestricted-verification.json)
occurred because the policy selected kube-dns while the actual GKE Dataplane V2
endpoint used `k8s-app=node-local-dns`. The fix permits only TCP/UDP port 53 to
either DNS role in `kube-system`. The IPv4 rule still excludes both metadata
addresses. The final unrestricted cases passed DNS/HTTPS and metadata refusal.

The first live binary exit-status check exposed kubectl's appended nonzero-exit
diagnostic on stderr. The bounded relay now removes exactly its owned trailer
for the actual return code. Hermetic regressions preserve non-UTF-8 output,
guest lookalike/duplicate messages, exit `0`, `7` and `255`, and verify that
startup failure and timeout kill the relay's kubectl and plugin descendants.
Python 3.8 or newer is checked before cloud allocations.

## Resource observations

Both runtimes' four-worker CPU probes stayed within the one-CPU ceiling. Kata
also exposed actual guest cgroup values `cpu.max=100000 100000` and
`memory.max=536870912`.

The gVisor touched-memory probe reached 464 MiB of application allocation before
the sandbox ended; Kubernetes explicitly reported `reason=OOMKilled` under its
512 MiB limit. The Kata probe ended with `ttrpc: closed` after reporting 8 MiB
allocated. Its immediately sampled Pod still reported Running, and no later OOM
reason was captured before cleanup. The example's negative allocation check
passed, but that transport closure is insufficient to qualify Kata's hard OOM
behavior. The raw observation remains in its final verification record.

## Replay

Build without running a live test:

```sh
CARGO_TARGET_DIR=/tmp/branchyard-gke-native-target \
  cargo build -p branchyard-agent-sandbox --example gke_conformance --locked --offline
```

Then explicitly select the real context, prepared namespace, runtime, network,
private receipt directory, dedicated evidence directory and immutable image:

```sh
KUBECONFIG=/private/operator/kubeconfig \
  /tmp/branchyard-gke-native-target/debug/examples/gke_conformance \
  CONTEXT branchyard-gvisor gvisor deny \
  /private/qualification/state/gvisor-deny \
  /private/qualification/evidence/gvisor-deny \
  ghcr.io/vamsiramakrishnan/branchyard-agy@sha256:46e177cc5e874614a04cbe2b06b6df374ab50296557f27ef77246f609dbc158d
```

Repeat with `unrestricted`, then namespace `branchyard-microvm` and runtime
`microvm`, using separate state/evidence directories. The operator host needs
real kubectl, its credential plugin, Python 3.8 or newer, and tar. The guest
image needs the documented provider utilities including setpriv, plus Python 3
for these probes. Run one case at a time per namespace when checking namespace
emptiness. The example always attempts receipt-owned cleanup after a failed
case; it never deletes the namespace, controller or cluster.

Source and binary hashes, the kubectl version and each case's start timestamp
are in [manifest.json](manifest.json). Records contain resource identities and
runtime/network observations; kubeconfig contents, keys, tokens and model
payloads were neither collected nor committed.
