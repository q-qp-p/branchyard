# Install the pinned gVisor Docker runtime

`tools/install_container_runtime.py` prepares an existing Linux x86_64 Docker
host. Its default command prints an offline JSON plan. Review it, then add
`--apply` under sudo to install gVisor and register the `runsc` runtime.
Use Python 3.11 or newer. It runs no containers or models.

```sh
python3 tools/install_container_runtime.py
sudo python3 tools/install_container_runtime.py --apply
```

The installer pins gVisor `release-20261026.0` and the official x86_64
[`gvisor.tar.bz2` archive](https://storage.googleapis.com/gvisor/releases/release/20261026.0/x86_64/gvisor.tar.bz2).
Its SHA-512, published in the
[official checksum file](https://storage.googleapis.com/gvisor/releases/release/20261026.0/x86_64/gvisor.tar.bz2.sha512), is:

```text
26e4a327565f5da0f73261a6387e2c20f4695a232e330095a3111b70efd604fdf09d734e8cc8f714dbb5c5607ce8c87ea80b26df549d83a146f2efd5a1af448a
```

The archive contains `runsc`, the containerd shim, and `gvisor-bin` sidecars.
Current gVisor requires those sidecars beside `runsc`; copying a single binary
is incomplete. See the [gVisor installation guide](https://gvisor.dev/docs/user_guide/install/).
The installer checks the pinned checksum before extracting any executable.
It rejects links, special files and paths outside the bundle.

Files go into `/opt/branchyard/gvisor/release-20261026.0/x86_64`. Docker's
`runtimes.runsc.path` points to that directory's `runsc`. Existing system
`runsc` binaries stay in place. A foreign installation directory, existing
`runsc` registration, symlink, or changed owned executable causes a refusal.
Retries verify all installed file hashes. They preserve unrelated Docker
settings and runtime registrations.

The installer validates the merged Docker configuration, writes
`/etc/docker/daemon.json` atomically, and runs `systemctl reload docker`.
It never restarts Docker or changes `default-runtime`. Docker documents
`runtimes` as a [reloadable setting](https://docs.docker.com/reference/cli/dockerd/#configuration-reload-behavior).
Success requires the local daemon's `docker info` to report the exact runtime
path and its original default runtime. The command ignores remote Docker
contexts and `DOCKER_HOST` by selecting `/var/run/docker.sock` explicitly.

Ownership, a pre-change configuration copy, and the deployment report live in
`/var/lib/branchyard/container-runtime`, mode `0700`, with files mode `0600`.
The ownership record precedes installation so a failed download or reload can
be retried with the same options. A file lock rejects concurrent applies.
`--install-dir`, `--state-dir`, and `--daemon-config` select absolute,
non-overlapping paths for an operator-managed layout. The selected config must
be the file used by the running local Docker daemon. Restore a saved config
only after reviewing other changes made since installation, then reload Docker.
There is no automatic uninstall or daemon rollback.

## GCE bootstrap and task images

`python3 tools/compute_gce.py ... --gvisor` includes this installer in the
bootstrap of a newly owned GCE worker. The selection is included in the VM's
configuration fingerprint and deployment report. It can coexist with
`--backend=microsandbox`. Installing runsc leaves task-provider selection
unchanged; select the Container provider and its `runsc` runtime in the task
configuration when that is the intended boundary.

The Container provider requires a digest-pinned harness image with the
required harness executable and `/bin/sh`. Its guest workspace is `/workspace`,
its home is `/branchyard/home`, and the process runs as UID/GID `1000`.
Prepare those directories in the image with ownership `1000:1000`; the provider
mounts the task's workspace and home there. Include writable `/tmp` support for
the harness. Image publication and harness installation happen separately from
this runtime installer. Verify the configured image's digest and tool versions
in each live qualification record.

`--isolated` selects Branchyard's process-level guard for local execution.
It does not select a Docker container or gVisor. A container boundary requires
the Container provider; a gVisor boundary also requires its `runsc` runtime.

Hermetic on 2 November 2026: `tests/test_install_container_runtime.py` uses fake
curl, Docker and systemctl commands. It checks archive and ownership refusal,
configuration merging, retries, reload verification and default-runtime
preservation. `tests/test_compute_gce.py` checks the explicit prerequisite flag
for both worker backends and validates the generated Bash. The official archive
was downloaded and its SHA-512 matched the pinned checksum on this date.
Not done: no live host installation, container execution, resource ceiling or
model call was performed by these checks.
