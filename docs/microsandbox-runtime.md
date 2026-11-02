# Build and install the patched Microsandbox runtime

Prepared 2 November 2026.

Branchyard's Linux x86_64 runtime release includes a source patch that counts
the kernel mapping inside the configured guest RAM budget. The upstream
Microsandbox 0.7.3 executable exceeds that budget. The SDK starts a separate
executable, so changing only Branchyard's Cargo dependencies cannot fix it.
The [dated GCE record](remote-validation-2026-11-02-gce.md) contains the
before/after memory measurements and real adapter execution results.

## Release assets

The release workflow builds the actual `microsandbox-cli` runner from the
source revision, patch and dependency locks in
[`microsandbox-runtime-build.json`](../tools/microsandbox-runtime-build.json).
It fetches the locked dependencies, then runs the architecture regressions
and compilation with `--locked --offline`. The runtime uses the optimized
`release` profile on Ubuntu 24.04, with the checksum-pinned guest agent and
matching firmware. Supported delivery is Linux x86_64 with glibc, including
the helper's Ubuntu 24.04 GCE workers.

The [build driver](../tools/build_microsandbox_runtime.py) writes a tar archive
whose name includes the runtime version, patch revision, platform and archive
digest. It also writes a checksum sidecar, provenance and
`microsandbox-runtime.json`. The manifest selects the exact archive and records
its checksum, source-plan identity and provenance checksum. An output directory
cannot silently switch its manifest to a different artifact.

The runtime job completes before the shared `SHA256SUMS` file is generated.
That file covers the archive, runtime manifest and provenance alongside the
native Branchyard archives. The release smoke and publication jobs depend on
the runtime build. No debug-build checksum is hardcoded into bootstrap.

The Linux x86_64 smoke job verifies the actual generated payloads without
installation or KVM. The same check can be repeated on downloaded release assets:

```sh
python3 tools/install_microsandbox_runtime.py \
  --version 0.4.0 --verify-artifacts /path/to/release-assets
```

To build locally, follow the pinned-input commands in the
[GCE record](remote-validation-2026-11-02-gce.md#build-the-actual-runtime).
The default build profile is `release`; `--fetch` only populates dependencies
before the offline tests and compilation. The driver leaves the input checkout
and shared Cargo configuration unchanged.
It selects the supported GNU target explicitly and reads that target's output,
so an ambient Cargo target cannot cause an older host binary to be packaged.

## Installation and runtime identity

`by compute gce --backend=microsandbox` uses the
[runtime installer](../tools/install_microsandbox_runtime.py). It resolves all
assets from the requested Branchyard release over HTTPS, verifies the manifest
against that release's checksums, and verifies the archive and provenance
before writing an installation. It refuses an upstream, debug, foreign-platform
or unreviewed build plan, and rejects tar links or unexpected paths. The
executable and firmware must match their recorded digests.

The installer places root-owned binaries under
`/opt/branchyard/microsandbox/<revision>-<complete-archive-digest>/`. It retains
`runtime-provenance.json` beside `bin/msb`, with matching firmware under `lib/`.
A matching installation can be reused; differing bytes or unsafe ownership or
permissions cause a refusal. Existing installed files are never overwritten.

Each artifact has a separate, owner-only runtime home under `/var/lib/`, with
a full-digest identity marker. A prefix collision or foreign directory is
refused. A generated 0600 environment file records `MSB_PATH`,
`MSB_LIBKRUNFW_PATH` and `MSB_HOME`; worker wrappers source it on later SSH
connections. Switching artifacts preserves the previous home and kept guest
records. Recreate guests under the selected artifact rather than adopting old
processes into a replacement installation.

For an already prepared worker, create a private environment-file directory
owned by the worker user, then install from a published Branchyard release:

```sh
state="$HOME/.local/state/branchyard/runtime"
mkdir -p "$state"
chmod 0700 "$state"
sudo python3 tools/install_microsandbox_runtime.py \
  --version 0.4.0 --env-file "$state/microsandbox.env" \
  --uid "$(id -u)" --gid "$(id -g)" --apply
. "$state/microsandbox.env"
"$MSB_PATH" --version
```

The native adapter verifies the SDK-selected runtime pair before creating,
executing or resuming a guest. The provenance must match the reviewed source,
patch and layout pins, and both executable and firmware must match its hashes.
Stock or unidentified runners refuse before allocation or adoption. This check
also applies when callers bypass the GCE installer or change runtime paths.

Checkpoint references include the actual executable, firmware and memory-layout
identity. Capturing requires provenance that matches the resolved executable.
Legacy references and references from another identity refuse restore and
removal before the SDK can act on their absolute paths. Read or export a refused
checkpoint with its original runtime, or create a fresh checkpoint. The refusal
leaves the saved artifact intact.

Live on 2 November 2026: the optimized candidate passed immutable root-owned
installation and replay, all guarded adapter tests, strict guest RAM probes and
cleanup on the GCE worker. The installer consumed locally staged verified
release payloads. See the [optimized runtime record](remote-validation-2026-11-02-gce.md#optimized-runtime-and-installer-replay)
for the exact artifact, commands and evidence. Not done: this replay did not
download a published release, provision a new worker or call a model.

Hermetic on 2 November 2026: installer tests verify checksum and provenance
binding, unsafe archive and path refusals, immutable retries, separate homes,
kept-record preservation and packaged-helper wiring. Release tests verify
runtime-before-checksums ordering and checksum coverage. The dated GCE record
contains the separate live runtime and adapter results. Not done: no published
release was installed by these hermetic tests, no model was called, and the
GitHub release workflow was not run by these tests.
