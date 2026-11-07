# Prepare a GCE worker

`by compute gce` prepares a dedicated Ubuntu 24.04 machine for Branchyard's
[native SSH remote](remote-ssh.md). The default command prints an offline JSON
plan. Add `--apply` after reviewing the machine, image, SSH changes and ongoing
compute charges. The helper installs released binaries; it does not compile
Branchyard or call a model.

## Prepare access

Use Python 3.10 or newer, Git, OpenSSH and an authenticated Google Cloud CLI.
The project needs billing and the Compute Engine API. The operator needs
permission to create and inspect instances, connect through OS Login, and use
passwordless sudo on the created machine. Selecting a service account also
requires permission to attach that existing account.

The helper attaches no service account by default and grants no project IAM
roles. An optional `--service-account` must name an existing account in the
selected project. The VM then gets the `cloud-platform` access scope, so
harnesses on the worker reach Google APIs as that account through the VM's
metadata server, as on a Cloud Workstation, and the account's IAM roles decide
what each request may do. `--scopes` replaces the default with a comma-separated
list of scope names or URLs, or `none` for no scopes. It is refused without
`--service-account`; a VM without an account gets no scopes.

For Vertex AI, Google's documentation also requires the Vertex AI API
(`aiplatform.googleapis.com`) enabled on the project and the Vertex AI User role
(`roles/aiplatform.user`) granted to the account. Branchyard's `gs://` stores and
Cloud KMS keys need their own roles ([identity.md](identity.md)). The helper
enables no API, grants no role and checks neither.

Model credentials can be delivered through the explicit option below. No model
key is embedded in VM metadata, the bootstrap script or an image.

The default uses an external SSH address, direct SSH and the existing `default`
network and subnet. It requires your existing SSH firewall access.

Add `--tunnel-through-iap` to connect through Google Cloud Identity-Aware Proxy
(IAP). On apply, the helper creates or verifies one deployment-owned firewall
rule. It allows TCP 22 from `35.235.240.0/20` only to the deployment's unique VM
tag. A conflicting rule is refused. Your account needs IAP tunnel access, such
as `roles/iap.tunnelResourceAccessor`, and permission to create the firewall rule.
The helper grants no IAM roles. See [Google's IAP TCP forwarding requirements](https://docs.cloud.google.com/iap/docs/using-tcp-forwarding).

IAP is optional and is not live qualified in the tested project. Its managed
firewall enforcer removes the dedicated allow rule, preventing the tunnel from
reaching SSH. An organization or project policy can therefore block IAP even
when your account can create the rule. Resolve that policy with the project
administrator or select an already permitted route. The helper does not retry
by opening broader ingress.

`--internal-ip` omits the VM's external address. Combine it with
`--tunnel-through-iap` to reach a private-only VM through IAP, or provide your own
private route for direct SSH. A private-only VM also needs existing outbound
access, such as Cloud NAT, for package and image downloads. The helper creates
no network, subnet or NAT and leaves other firewall rules unchanged.

## Review and apply

Run from a clean, committed project repository and replace the example names:

```sh
by compute gce \
  --project=my-project \
  --zone=us-central1-a \
  --name=branchyard-worker \
  --directory="$PWD" \
  --tunnel-through-iap \
  --version=0.5.0 \
  --image=ghcr.io/vamsiramakrishnan/branchyard-agy:0.5.0
```

The planner contacts no cloud service, runs no provider commands and creates no
files. Repeat with `--apply` to prepare the worker. Pin `--image` by
its published `@sha256:` digest when reproducing a qualification record. A
release-version tag is also accepted; `latest` is refused. The release binaries
and worker image must already be published and accessible to the machine.

`by compute` is an operator command. It refuses harness sessions and `--remote`.
The packaged helper needs no Branchyard source checkout. From a source checkout,
`python3 tools/compute_gce.py` runs the same helper; its source-repository option
is named `--repo` instead of the CLI's `--directory`.

The helper creates an `n2-standard-4` VM with a 100 GB balanced persistent disk.
`--machine-type`, `--disk-size`, `--network` and `--subnet` select another
reviewed footprint. Existing machines must carry the matching
`branchyard-deployment` label and exact configuration fingerprint. An unlabeled,
foreign, stopped or differently configured machine is refused. The helper
does not adopt or replace it. Access scopes are part of the fingerprint. A
worker that Branchyard 0.4.0 or earlier made with a service account has none:
without `--scopes` it is reused as it is, and `deployment.json` reports
`access_scopes: []` with an `access_scopes_note` saying that its account cannot
reach Vertex AI, `gs://` or Cloud KMS; run with another `--name`, or delete it
and apply again, for a worker with `cloud-platform`. `--scopes none` reuses it
without the note, and any other `--scopes` refuses it as differently configured.

If a zone has no capacity for the selected machine type, review a different
`--zone` or a compatible `--machine-type` and repeat the plan. Deployment
identity includes the zone, so another zone uses separate state and resources.

Bootstrap installs Git, curl, Docker and native runtime libraries using Ubuntu's
package manager, then installs the exact released `by`, server and bridge into
`/usr/local`. It pulls the worker image and extracts its standalone `agy`
executable. This uses Docker for artifact delivery; a local-process task on the
worker has the worker user's permissions and makes no container isolation claim.

Add `--gvisor` to install the pinned complete runsc bundle and register it with
Docker through a reload. It also adds the dedicated SSH worker to Docker's
group; the following configuration step uses a fresh SSH login. The reviewed
plan names this permission change. The flag is part of the deployment fingerprint and can
coexist with `--backend=microsandbox`. It installs prerequisites and leaves task
provider selection unchanged. See [the runtime installer](container-runtime.md)
for the pin, ownership checks, image contract and hermetic evidence.

`--directory` transfers a git bundle of committed `HEAD`; uncommitted and untracked
files cause a refusal before VM creation. On retry, the remote `HEAD` must match
the source. The helper never resets existing remote work to a newer source
commit. Without `--directory`, it initializes an empty repository with a first commit.
Each remote project lives under `/srv/branchyard/NAME`.

## Connect and resume

The helper uses `gcloud compute ssh` with the selected transport for initial
authentication and host-key registration. With IAP, the saved Host entry uses
gcloud's IAP tunnel for later native SSH connections too. It then writes a deployment-owned Host entry under
`~/.ssh/branchyard/` and prepends one Include line to `~/.ssh/config`, preserving
existing content. The alias is derived from the project, zone and machine, so
the same machine name in another project does not share its entry. An alias
already mapped by another configuration is refused. Strict host-key checking
stays enabled.

The owner-readable `deployment.json` in the state directory records `remote`,
`ssh_config`, `remote_command`, source commit, image and version. The default
state directory is
`~/.local/state/branchyard/compute-gce/PROJECT/ZONE/NAME`; `--state-dir` overrides
it. Bootstrap output stays in its private `install.log`; terminal errors omit
captured command output. Rerun the same command after fixing a reported failure.
A matching existing VM is reused and completed steps can run again.

Use the recorded URL with `by --remote URL`, or save it as `[remote] url` in the
local project configuration. The native remote starts and authenticates its own
Unix-socket server. It introduces no public Branchyard HTTP port. `by setup --up`
then reports the seven [readiness checks](up.md#readiness-checks) from the worker's
own inventory. A worker made by guided setup is also offered to new projects
under *Existing remote environment*; one made by `by compute gce` keeps its record
under `compute-gce/` and is not.

Operations act on the remote copy. After reviewing and merging a candidate
there, fetch its parent branch through the recorded SSH alias if the local
checkout should receive it too:

```sh
git fetch SSH_ALIAS:/srv/branchyard/NAME main
git merge FETCH_HEAD
```

Availability of `agy --version` proves installation only. To reuse a model API
key already in your environment, add `--forward-secret=GEMINI_API_KEY` or
`--forward-secret=GOOGLE_API_KEY` to the reviewed command. Only the variable name
appears in the plan. Apply sends its value over SSH standard input to the
worker's native `by harnesses login antigravity --api-key` command. That command
stores canonical `GEMINI_API_KEY` in a 0600 file, outside the repository, and
names it in the deployment's private user configuration. Values never appear
in arguments, deployment records or bootstrap logs. A missing or malformed
selected source stops before cloud mutation; reapplying can rotate it without
replacing the machine.

The helper configures the remote project and server through `by init project`
and `by init server`, then validates both. Tasks use private homes. The server's
secret registry references the stored key file and fixes agy to the installed
executable. Its worker policy permits agy's tool execution; clients still make
the explicit `--allow-unapproved-tools` choice for a task. A deployment wrapper
keeps this server configuration and private keystore selected for later SSH
sessions. Existing explicit `by serve --config` arguments remain usable.

Without `--forward-secret`, no model key is copied. The server names its future
key file so credentials can be configured there later with the same native
login command. Model authentication remains unverified until a real task
completes. Workstation OAuth files are not transferred.

## Use Microsandbox on the worker

Add `--backend=microsandbox`. This requires an `n2-standard` or `c3-standard`
machine with nested virtualization. The helper enables nested virtualization,
installs the release's Linux runtime build with `--runtime`, adds the SSH user
to `kvm`, and installs Branchyard's patched Microsandbox 0.7.3 runtime and its
matching firmware. It verifies the selected Branchyard release's `SHA256SUMS`,
`microsandbox-runtime.json`, archive and provenance before installation. It
never installs the upstream runtime archive with the reproduced RAM failure.

The root-owned runtime lives in an immutable directory under
`/opt/branchyard/microsandbox/` named by its revision and complete archive digest.
Provenance stays beside the canonical executable. A short, private
`/var/lib/bm-<digest-prefix>` avoids Unix socket limits under long OS Login home
paths. A full-digest ownership marker refuses a conflicting home. A new artifact
gets a separate home; old runtimes, kept guests and checkpoints are retained.
The owner-only deployment file `microsandbox.env` records the exact runtime,
firmware and home. The server wrapper and fresh-login checks source that file.
The worker image is imported into that home. See
[runtime delivery](microsandbox-runtime.md) for build and installation details.

The installed CLI is probed and a fresh SSH session checks read/write KVM access.
The exact Microsandbox 0.7.3 CLI exposes `context`, `doctor` and `image load`;
it has no `init` command. These probes call no model and do not qualify a VM
lifecycle. Setup does not measure a guest RAM limit or a host-process memory
ceiling. The [older runtime results](remote-validation-2026-10-28.md) retain the
upstream failure for comparison.

The [2 November runtime qualification](remote-validation-2026-11-02-gce.md)
reproduces the upstream guest RAM over-allocation and records a runtime-source
fix with strict RAM, CPU, execution and cleanup checks on GCE. Its pinned build
driver emits a separate patched runtime archive with checksums and provenance.
Those results apply to that patched archive. Checkpoint references bind the
actual runtime, firmware and layout; legacy or mismatched references refuse
before restore or removal and explain how to recover without changing the saved
artifact. Use a fresh runtime home when changing artifacts.

## Install the shared tool store and Anvil

Skills and connector packages are published once as a shared tool store, then pulled onto the worker when it is set up, so a turn links them instead of packaging or copying them ([tools model](tools-model.md#4-one-turn-remotely-lifecycle-and-delivery)). Build and mirror the store from a machine that has the connector bundles:

```sh
by tools publish --store ./store --gcs gs://BUCKET/PREFIX
```

Then ask the worker's setup for it:

```sh
by compute gce --project PROJECT --zone ZONE --name NAME --tools-store gs://BUCKET/PREFIX
```

Setup runs `by tools pull --from gs://BUCKET/PREFIX --store /opt/branchyard/store` on the VM, checks every entry against the index, and writes `tools.store` and `tools.store_bucket` into the worker's `server.json` so its server links turns to the store. The plan lists the step. The VM needs `gcloud`, which setup checks before it pulls, and its service account needs `roles/storage.objectViewer` on the bucket; the helper grants no roles. Without `--tools-store` nothing changes and turns copy.

Setup also installs Anvil's CLI and SDKs once under `/opt/anvil` with `anvil install --prefix /opt/anvil` ([Anvil contract v2](anvil-contract-v2.md), item 4) when `anvil` is on the VM and has an `install` command (Anvil has had one since its PR #91; the installed launcher runs the installing Anvil's CLI, so that Anvil's tree must stay where it is). When it does not, setup says so and goes on. Anvil is not installed by this helper.

Hermetic evidence: `tests/test_compute_gce.py` checks the generated script and plan, and runs the Anvil guard with fake `anvil` and `sudo`. Not done: no worker was set up with `--tools-store`, and no real `anvil install` ran.

## Remove a temporary worker

Stop its Branchyard SSH server with `by remote ssh stop URL`, retain redacted
evidence, and delete only the dedicated VM:

```sh
gcloud compute instances delete NAME --project=PROJECT --zone=ZONE
```

If you selected IAP, read the owned firewall name from `deployment.json` and
delete that rule too:

```sh
gcloud compute firewall-rules delete IAP_FIREWALL --project=PROJECT
```

Remove that deployment's owned SSH Host file and private state directory. Keep
the shared Include line while other Branchyard Host files use it. The helper
does not delete resources automatically or change an existing service account.

## Verification

Live on 28 October 2026, finishing at 12:56:28 UTC: the patched helper completed
setup and resume on a fresh dedicated worker in `us-central1-b` over an existing
private VPC route. It installed the published `by`, server and bridge 0.1.0,
AGY 1.4.9 and Microsandbox 0.7.3, and imported the worker image. Native project
and server initialization, configuration validation, `serve --check`, KVM
access and the managed SSH alias passed. The result is recorded in
`/tmp/branchyard-install-20261028/gce-qual-published/private-b-final-apply-result.json`
with exit 0. Its `private-b-state/` directory contains the deployment record
and install log. See
the [dated validation record](remote-validation-2026-10-28.md) for the broader
qualification. Not done: no model was called or model credentials forwarded in
this setup run, no existing project was transferred, and no complete Microsandbox
task or hard memory ceiling was qualified. IAP remains unqualified because the
project's managed firewall enforcer removed its allow rule.

Hermetic on 28 October 2026: `tests/test_compute_gce.py` and
`tests/test_cloud_setup.py` use fake provider commands and a temporary Git
repository. The CLI's `setup_cmd` tests run real offline plans and a guided
terminal review, then cancel before cloud mutation. They cover VM and firewall
ownership refusals, private and IAP transport selection, named credential
forwarding, resumable SSH configuration and repository transfer. The exact
downloaded Microsandbox 0.7.3 archive was checksum-verified and its CLI help
inspected while implementing the bootstrap. See the
[dated live validation record](remote-validation-2026-10-28.md) for
real-environment evidence. Not done: these hermetic checks do not qualify a live
GCE bootstrap, model authentication or a complete Microsandbox lifecycle.

Hermetic on 2 November 2026: `tests/test_compute_gce.py` checks the create
command with fake `gcloud`: `--scopes=https://www.googleapis.com/auth/cloud-platform`
with a service account by default, `--no-scopes` with `--scopes none` or with no
account, an explicit list passed through, and `--scopes` refused without an
account. The CLI's `cloud_setup` unit test checks that `by compute gce` forwards
`--scopes`. Not done: no worker was created and no model was called for this
change, so no harness has reached Vertex AI, `gs://` or Cloud KMS through a
worker's metadata server.


Public 0.1.1 follow-up, 28 October 2026: live provisioning through the published
standalone `by compute gce --apply` also passed from a non-Git directory, using
its embedded helper. A fresh private worker passed binary/version, native
configuration, KVM, authenticated remote connection and real MicroVM execution
checks. See the [public verification record](remote-validation-2026-10-28.md#public-011-verification-28-october-2026).
Not done: model execution, the MicroVM hard memory ceiling, and IAP under this
managed network policy.
