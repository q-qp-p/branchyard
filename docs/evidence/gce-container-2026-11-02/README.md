# GCE Docker and gVisor qualification, 2 November 2026

Live on 2 November 2026: the pinned installer from commit `2f02aabc` installed
gVisor `release-20261026.0`, OCI spec `1.2.1`, on the existing owned
`branchyard-validation-01` GCE VM in `us-central1-a`. The host ran Docker
`29.1.3`, containerd `2.2.1`, runc `1.3.4-0ubuntu1~24.04.1` and kernel
`7.0.0-1011-gcp`. Direct Docker/runsc commands then passed the image, exec,
filesystem, resource, network and cleanup checks recorded in
[verification.json](verification.json). Not done: no model call, native
Container adapter or engine execution, fresh GCE bootstrap, or release binary
installation was performed in this qualification.

## Installer

[plan.json](plan.json) is the offline plan reviewed before the authorized apply.
[install.json](install.json) is its live result. The exact official complete
archive SHA-512 is recorded in both files. The installed runtime lives at
`/opt/branchyard/gvisor/release-20261026.0/x86_64/runsc`, with its required
`gvisor-bin` sidecars beside it. The source SHA-256 is in
[installer-source.sha256](installer-source.sha256).

The VM initially had no `/etc/docker/daemon.json` and no registered `runsc`.
The installer added only its runtime registration. Docker's default remained
`runc`. The [identical repeat result](installer-repeat.json) verifies the
idempotent apply. [docker-service.txt](docker-service.txt) records Docker's main
PID `2117` and its start on 17 September 2026, before this installation; the
daemon was reloaded without restarting. [permissions.txt](permissions.txt)
records root ownership, private state mode `0700`, and record/config modes
`0600`.

## Image and direct runtime checks

[Dockerfile](Dockerfile) derives from the existing digest-pinned Branchyard agy
image and adds the required UID/GID `1000:1000` workspace and home ownership.
The image contains `sh`, `tar`, `setsid`, `tail`, Python 3 and `agy` `1.4.9`.
The harness version was inspected without calling a model. Source hashes,
[build.log](build.log), [push.log](push.log) and the resolved image identity are
recorded alongside the probes.

The resulting image remains available locally on this VM as:

```text
localhost:32769/branchyard-qualification/container@sha256:23780f5c249ea8be5532d08fc3296c7785ac022c714ea445da68940d86405f0e
```

This is a genuine Docker `RepoDigest`, suitable for the native adapter's
`--pull=never` contract on this VM. The temporary loopback-only registry and its
anonymous data volume were deleted. That registry address is no longer a pull
source; another host must build and publish its own image.

[qualify_host.py](qualify_host.py) executes the same creation settings as the
native Container provider. [guest_probe.py](guest_probe.py) records only local
boundary observations and metadata HTTP status. It requests only the public
project-id metadata path and never requests a token, service-account document,
or credential.

| Check | Live observation |
| --- | --- |
| Runtime | Guest kernel `4.19.0-gvisor`; Docker inspected runtime `runsc` at the pinned path. |
| Identity and filesystems | UID/GID `1000:1000`; writable `/workspace`, `/branchyard/home`, `/tmp`; read-only root and non-executable `/tmp`. |
| Host access | Host-only marker, Docker socket and `/dev/kvm` absent; effective capabilities zero and `NoNewPrivs=1`. No host bind mounts. |
| Binary exec | Non-UTF-8 stdin/stdout/stderr preserved, with exit status `7`. SHA-256 values are recorded. |
| File transfer | A 1,024-byte binary payload survived a tar upload/download round trip with the same SHA-256. |
| CPU | Actual cgroup quota `100000 100000`; four workers consumed `4.127` CPU seconds in `4.167` wall seconds, with `41` throttled periods. |
| Memory | Actual `memory.max=536870912`, swap disabled; touched allocation exceeded the ceiling and reached an exact 512 MiB peak, with `oom=1`, `oom_kill=1` and Docker `OOMKilled=true`. |
| Default network deny | Only loopback visible; external TCP and metadata addresses `169.254.169.254` and `169.254.169.252` unreachable. |
| Explicit unrestricted network | Bridge networking reached external TCP and the GCE project-id endpoint at `169.254.169.254` with HTTP `200`; it provides no metadata block. |
| Cleanup | All three owned containers and all five anonymous volumes verified absent. [containers-after.txt](containers-after.txt) is empty. |

The memory test returned Docker exec status `128` with a runsc `WaitPID` EOF
after the kernel killed the sandbox. Docker's immediately sampled state still
said `Running=true` while `OOMKilled=true`. The hard ceiling is established by
the host cgroup limit, peak and OOM counters. Cleanup removed the exact owned
container and its volumes successfully.

## Replay

From the owned host's evidence directory, run `python3 qualify_host.py` with
passwordless sudo and the recorded base/registry images available. The script
creates only labeled qualification containers, uses a Docker-assigned
loopback port for its temporary registry, checks each immutable container's
owner before deletion, and verifies the associated anonymous volumes are gone.
It leaves the resulting image for a separate native adapter/engine replay.

The first observation attempt assumed `/sys/class/net` existed. gVisor omits
that path, so the probe now uses the standard `socket.if_nameindex()` API.
That attempt's containers and volumes were verified removed before the recorded
passing replay. No installer or product change was needed.
