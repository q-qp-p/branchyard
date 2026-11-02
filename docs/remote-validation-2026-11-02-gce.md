# GCE Microsandbox runtime qualification

Prepared 2 November 2026.

Live on 2 November 2026: the checksum-pinned upstream Microsandbox 0.7.3
runtime reproduced the guest RAM over-allocation on the existing
`branchyard-validation-01` worker in `us-central1-a`. The patched runtime
passed the same strict RAM bounds at 128, 512 and 1024 MiB, along with real
guest execution, CPU count, file write/read and host-file visibility checks.
The guest kernel was `6.12.109`; the worker kernel was `7.0.0-1011-gcp`.
All 26 architecture tests passed on the worker's KVM device. The
[evidence directory](evidence/gce-microsandbox-2026-11-02/) records the output
and runtime provenance. Not done: no model calls, host-process RSS ceiling,
egress confinement, or restoration of snapshots made with the old memory map.

## The RAM failure and fix

For a requested 512 MiB, the official runtime reported `MemTotal: 529224 kB`,
exceeding the strict `524288 KiB` ceiling. The non-TEE x86 low-memory layout in
`msb_krun_arch` allocated the requested size after the kernel mapping, adding
the kernel and low-address prefix to the requested budget. The patch makes
the last address equal the requested size and subtracts that prefix from
the post-kernel region. The existing kernel-fit check remains in effect.
There is no adjustment to the memory value sent by Branchyard's adapter.

The added regression checks the region endpoints and complete memory budget
at 128, 512 and 3328 MiB. It fails against the original source: a 128 MiB
request ends at `182452224` rather than `134217728`. The patched layout tests
pass. Branchyard's existing `limits_reach_the_guest` assertion remains strict.

| Requested RAM | Upstream guest MemTotal | Patched guest MemTotal | Maximum |
| --- | --- | --- | --- |
| 128 MiB | Not measured | 108108 KiB | 131072 KiB |
| 512 MiB | 529224 KiB | 494600 KiB | 524288 KiB |
| 1024 MiB | Not measured | 1009928 KiB | 1048576 KiB |

These are guest physical-memory measurements. The VM runner also uses host
memory for its own process, devices and caches. No host cgroup memory limit
was applied or measured in this run.

## Build the actual runtime

The SDK starts a separate `msb` executable. A patch applied only to Branchyard's
Cargo dependency cannot change that executable. The reviewed build driver
builds `microsandbox-cli`, embeds the pinned guest agent, and carries the
matching firmware from the checksum-pinned upstream archive.

On an x86_64 Linux build machine, install Python 3.12, Rust 1.94.0, a C compiler,
`pkg-config` and `libcap-ng-dev`. Fetch the pinned inputs:

```sh
git clone https://github.com/superradcompany/microsandbox.git /tmp/msb-source
git -C /tmp/msb-source checkout f9f40e1a341922c0f66da36f10914d2aad0ca8be
curl --fail --location --output /tmp/msb-upstream.tar.gz \
  https://github.com/superradcompany/microsandbox/releases/download/v0.7.3/microsandbox-linux-x86_64.tar.gz
curl --fail --location --output /tmp/msb-agentd \
  https://github.com/superradcompany/microsandbox/releases/download/v0.7.3/agentd-x86_64
python3 tools/build_microsandbox_runtime.py \
  --source /tmp/msb-source --upstream-archive /tmp/msb-upstream.tar.gz \
  --agentd /tmp/msb-agentd --output /tmp/msb-artifacts \
  --target-dir /tmp/msb-target --fetch
```

`--fetch` fetches the locked dependency graph first. The architecture tests
and actual runtime compilation always use `--locked --offline`; omit
`--fetch` when the cache is populated. The default build profile is `release`.
The checked archive in this record used `--profile dev`. Building does not
change the input checkout or shared Cargo configuration.

The driver refuses changed payloads, vendored sources, patches or lockfiles.
It emits a content-addressed `.tar.gz`, its `.sha256`, and a provenance JSON
containing the upstream revision, architecture revision, patch and lock
digests, version, platform, profile and binary/archive digests. The tar also
contains the provenance and architecture license notices. The exact reviewed
inputs are in `tools/microsandbox-runtime-build.json`; the source patch is
recorded in `vendor.patches.json` and `patches/msb-krun-arch-memory.patch`.

## Repeat the live checks

Use a Linux worker with read/write KVM access, Docker access and a locally
available `alpine:3.20` image. Copy the generated archive and
`tools/qualify_microsandbox_gce.sh` to the worker. Then run:

```sh
sh tools/qualify_microsandbox_gce.sh \
  /absolute/path/to/microsandbox-ARCHIVE.tar.gz \
  ARCHIVE_SHA256 /var/tmp/branchyard-new-runtime-evidence
```

The evidence directory must be new and absolute. The script verifies the
archive checksum, uses a private temporary `MSB_HOME`, imports the local
image, and runs fresh guests at all three RAM sizes. Every exec closes its
standard input so the runtime cannot consume the rest of a piped SSH script.
It checks the guest kernel, one CPU, strict RAM limits, write/read, host-file
invisibility and an empty sandbox list. Its trap removes test guests and its
own temporary runtime home. The retained evidence includes the image digest
and runtime provenance. No existing runtime or service is changed.

Use a fresh runtime home for this patched memory layout. This record does
not qualify restoring kept full snapshots made by the upstream layout.

Hermetic on 2 November 2026: ten build-driver tests passed with temporary Git
repositories, fake binaries and a fake compiler. Ruff, mypy and the vendor
pin/patch verifier passed. Not done: these hermetic tests do not execute KVM
or qualify cloud provisioning, release asset delivery or model authentication.
