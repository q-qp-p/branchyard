# Container sandboxes

The container provider runs a turn in a container on the worker's local Docker
daemon. On a GCE worker this provides a filesystem and process boundary on that
VM. It does not provision a GCE VM, mount the host worktree, or treat a private
HOME as isolation.

The default runtime is gVisor's `runsc`, with allocation-wide networking disabled.
An operator can explicitly select `runc` for Linux namespaces sharing the host
kernel, or `unrestricted` networking for Docker's bridge network. The provider
refuses an unavailable runtime instead of falling back. Unrestricted networking
does not promise to block cloud metadata. Neither network choice implements the
per-exec listener or hostname allowlist contract; required per-exec confinement is
refused.

Configuration contains only `runtime` and `network`. The operator selects the
Docker executable and the private receipt directory outside task configuration.
Commands always target `unix:///var/run/docker.sock`; ambient Docker contexts,
remote daemon settings and TLS overrides cannot redirect a task. Images and
resource limits arrive through the sandbox specification. An image must be
already available locally and pinned by its registry SHA-256 digest. Omitted
limits use one CPU and 1024 MiB. Explicit limits must be positive. The provider
checks Docker's actual runtime, image identity, memory and CPU settings before
starting the workload and before every new exec.

Docker transport diagnostics before the guest startup acknowledgement are
omitted from errors. Guest stderr after acknowledgement remains byte-exact.

Containers run as UID/GID `1000:1000`, with a read-only root filesystem, all Linux
capabilities dropped, no-new-privileges, private PID/IPC/cgroup namespaces and a
process-count limit. There are no host binds or Docker socket mounts. `/tmp` is
a bounded, non-executable tmpfs. Two anonymous local volumes supply `/workspace`
and `/branchyard/home`; Docker initializes their ownership and content from the
image. Both directories must already be writable by UID 1000 in the image. The
provider does not repair ownership using a privileged exec.

The image needs `sh`, `tar`, `setsid`, `tail`, the utilities used by the shared
process transport, and the selected harness. The entrypoint keeps the allocation
alive independently of a harness exec. The engine copies the worktree and private
home into the allocation and brings results back through its existing staged
transfer path. Host mounts, retained allocations, pause, snapshots and write-scope
requests are refused.

Creation first writes and syncs an ownership intent containing an unpredictable
owner identifier, the daemon identity and a specification digest. A per-name
filesystem lock covers creation, receipt updates and lifecycle operations. A
retry can discover a container created before the immutable ID was persisted,
but it must match the exact generated name, owner and specification. Before start
or exec, the receipt records the immutable container ID and its anonymous volume
identities. Exec commands address that ID, never the reusable container name.
Changed identities or configuration are errors.

The shared transport carries binary stdin/stdout/stderr and reports a missing
guest command before returning a process. Process teardown signals its recorded
remote process group. An untrusted program can escape that group; allocation
stop and destruction provide the final boundary. Stop ends every container
process while retaining its private volumes. Destruction removes the exact owned
container with its anonymous volumes and verifies their absence. A deletion or
volume-cleanup failure retains the receipt for inspection and retry. The provider
never deletes an unrelated volume by its mutable name. Operators must resolve a
remaining volume when Docker cannot remove it with its owned container.

Evidence is hermetic: `cargo test -p branchyard-container --all-features --locked --offline`
uses a stateful fake Docker binary for ownership recovery, isolation/resource
settings, lifecycle failures, binary exec I/O, process-group teardown and
cleanup. Fake inspection data proves adapter behavior, not a real container's
isolation. Not done: live Docker/gVisor execution, GCE qualification, or model
execution.
