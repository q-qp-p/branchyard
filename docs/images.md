# Toolchain images

A toolchain image is a sandbox image with the harness CLIs already installed at pinned versions, labelled with what it holds, and named by digest. Every sandbox provider (Microsandbox, containers, Agent Sandbox, Substrate) can run one, so a sandboxed turn never installs a harness at launch, and the router knows what an image holds without starting it.

> **Status.** Implemented and tested hermetically: a fake `docker` on `PATH` (it records its argument vectors, emits a digest and answers `image inspect` with the labels of what it "built"), a mock registry served by `branchyard-testkit`'s `MockHttp`, and the fake ACP agent for turns. No model was called and no network was used. Not done: a real `docker buildx` build or push, a real registry, and measured launch times (the claim that a turn on a toolchain image starts faster than one that installs a harness is not measured).

```sh
by images build --harness codex --harness pi --tag ghcr.io/me/tc:1 --push
by images list
by images show ghcr.io/me/tc:1
by images pin ghcr.io/me/tc:1@sha256:... --provider container   # the provider's default image
by harnesses --image ghcr.io/me/tc:1@sha256:...                 # what it holds, from its labels
```

## Building

`by images build` writes a build context (a `Dockerfile` and the files it copies) and builds it.

| Flag | Meaning |
|---|---|
| `--provider microsandbox\|container\|agent-sandbox\|substrate` | The provider the image is for (default `container`) |
| `--harness ID` | A harness to install; repeat it. Default: the harnesses `[harnesses] allow` names, else those `[fleet]` and `[defaults]` configure |
| `--tag REF` | The image name and tag (default `branchyard-toolchain:` and a prefix of the toolchain hash) |
| `--push` | Push to the registry and read the digest back |
| `--out DIR` | Where the context goes (default `.branchyard/images/` under the repository) |
| `--allow-unpinned` | Install a harness whose catalog command takes no version; its label then reads `unpinned` |
| `--base IMAGE` | The base image, which needs npm (default `node:22-bookworm-slim`) |
| `--by-binary PATH` | The Linux `by` binary to put in the image (default: this one) |
| `--json` | Print JSON |

- Each harness is installed with the catalog's package-manager command, pinned to one version, chosen in this order: the **qualified pin** (`by harnesses update ID --qualify`, [harness-lifecycle](harness-lifecycle.md)) when the harness has one; else the version its default profile was checked against (the **catalog** pin); else none, which is refused as below. A harness whose only install is a piped script, or that has no recorded version, is refused with exit code 4 unless `--allow-unpinned`; the script is then used and the image says so in its label and in a comment in the `Dockerfile`.
- The version and its evidence come from one function, `branchyard::images::pinned_version(id)`, which reads `branchyard::qualified_pin(id)` and then the profile. A qualified pin newer than the catalog's wins; the command is rebuilt for its version.
- The `by` binary goes in at `/usr/local/bin/by`. When `[connectors] anvil` is configured, the image also runs `anvil install --prefix /opt/anvil`; without it that step is skipped and the build says so.
- **The image holds no credentials.** The `Dockerfile` header says so, nothing in the context is read from a login or environment variable, and a harness logs in inside the sandbox through the usual credential projection.
- The context is deterministic: the same harnesses, versions and `by` give the same `Dockerfile` and the same labels, so building twice records one image.
- With `docker` on `PATH`, `docker buildx build` runs (with `--push` when asked, and the digest is read from its metadata file). Without it the context is left in place and the command is printed.
- Every build is recorded in `.branchyard/images.json`; `by images list` and `by images show REF` read it.

## Labels

| Label | Value |
|---|---|
| `dev.branchyard.harness.<id>` | The pinned version, or `unpinned` |
| `dev.branchyard.harness.<id>.evidence` | Where that version came from: `live` (a qualified pin with a dated run with a model behind it), `hermetic` (a qualified pin from the deterministic checks only) or `catalog` (the version the default profile was checked against; also an unpinned install) |
| `dev.branchyard.by` | The `by` version in the image |
| `dev.branchyard.anvil` | Anvil's digest, when Anvil is installed |
| `dev.branchyard.toolchain` | `sha256:` and the hash of the sorted `key=value` label set above, the evidence labels included, so two images with the same toolchain share it |

## Pinning a default

`by images pin REF@sha256:DIGEST [--provider P]` makes the digest the provider's default image. It writes the key the provider already reads (`[microsandbox] image`, `[container] image`, `[agent_sandbox] image`) or, for a provider with none, `[images] <provider> = "ref@sha256:..."`. A run on that provider with no `--image` falls back to it; a `--image` flag still wins. With neither, the run is refused and the message names `by images pin`. A tag without a digest is refused: a tag moves.

## What an image holds

`by harnesses --image REF [--json]` prints the harnesses and the `by` version an image holds. The labels are read without starting the image, from the first source that has them: the record `by images build` wrote, `docker image inspect`, then the registry's OCI config blob over Branchyard's own HTTP client.

The router ([`fleet::plan`](fleet.md)) reads the same labels for a sandboxed candidate:

- A harness the labels do not list is excluded, with the reason `X is not in the image REF: its labels say it holds ...`.
- An image with no Branchyard labels is not excluded: whether it holds the harness is unknown, so the candidate stays, and the route carries a note saying so (`notes` in the route, `note:` lines in its text).
- A candidate that sets its own `command` is not checked against the image.

## The turn manifest

A sandboxed turn's manifest records the image reference, its digest and its toolchain label. Before the harness starts, the engine compares that digest with the image the provider's receipt names; a different digest fails the turn with `bundle_mismatch: image REF: ...`, like the [bundle check](harness-integration.md), and the sandbox is discarded. A provider that keeps no receipt, and a session admitted by tag, check nothing.

## What is tested

`crates/branchyard-cli/tests/images.rs` (the commands, with the fake `docker`, the mock registry and a fake `by`), `crates/branchyard/tests/images.rs` (the labels, the router and the manifest) and the unit tests in `crates/branchyard/src/images.rs`, `bundles.rs` and `fleet.rs` cover: a deterministic context and its labels; a pinned harness, and an unpinned one refused and then labelled; a tag refused by `pin`; the inventory from a record, the daemon and a registry; the router excluding a harness and passing an unlabelled image; the manifest recording the digest; and a digest mismatch failing before the harness starts.

Not done: a real build or push, real registries, and measured launch times.
