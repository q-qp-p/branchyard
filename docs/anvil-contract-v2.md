# Anvil contract v2

Written 5 November 2026.

**Status, 5 November 2026: built in Anvil (PR #91, "Implement Branchyard's gateway contract v2", merged 5 November 2026, 22:22 UTC, merge commit `875f14e`; the PR's last commit is `5c06b7d`); hermetic there, not run live.** Evidence, in three kinds:

- **Anvil's own tests, hermetic (fakes only), not rerun here.** Anvil's `docs/branchyard.md` records them: in-memory fakes of Firestore and Cloud KMS, a scripted `fetch` for their REST clients, loopback sockets for the gateway, directories on disk for `install`, an in-process loopback registry for `package oci`.
- **The built Anvil CLI, run for this page on 5 November 2026** (version `0.1.0`, branch `branchyard-contract-v2` at `5c06b7d`, no network, no model): `compile`, `approve`, `package harness`, `connectors index`, `connectors compose`, `install` and `capabilities --json` on a small fixture spec, with Branchyard's strict `tools` reader run on the result and the mock compared with it. The record is [`tests/test_anvil_real.py`](../tests/test_anvil_real.py), which anyone with a built Anvil reruns. It is skipped, with its reason, where there is no built Anvil (`ANVIL_BIN`, else `/home/user/anvil/packages/cli/dist/bin-anvil.js`) or no `node`, so a CI without Anvil stays green and checks only the mock's own tests.
- **Branchyard's mock** ([the mock](#the-mock)), hermetic, now shaped after that run and no longer Branchyard's reading of a proposal.

Not done, in Anvil: any run against real Firestore, Cloud KMS, Cloud Run or a registry such as Artifact Registry; `anvil vault migrate` (item 1 asked for it; it is not built); `--from` taking an OCI layout or a registry reference (it takes a directory only); a self-contained toolchain (the installed `bin/anvil` runs the CLI of the Anvil that installed it); an upstream revocation on disconnect. Not done, in Branchyard: the pull side of an OCI package; the console's Disconnect action; setting `ANVIL_GATEWAY_AUTH=proxy` in a sandbox; any live run on GCP.

This page was what Anvil had to add so that the tools model in `docs/tools-model.md` works remotely. Anvil has built it; Branchyard now builds against Anvil, with the mock for tests that run where Anvil is not. Each item gives what it is, why Branchyard needs it, the wire shape, **what Anvil built and how it differs from the proposal**, how Branchyard tests, and the acceptance test.

Today's contract is in [connectors](connectors.md). This page extends it. Nothing here changes a claim that connectors.md already makes.

| Item | In Anvil | Not done |
|---|---|---|
| [1. Firestore vault with KMS](#1-vault-backend-firestore-envelope-encrypted-with-cloud-kms) | built, hermetic | real Firestore and KMS, the emulator, key rotation, `anvil vault migrate` |
| [2. Disconnect](#2-disconnect) | built, hermetic | upstream revocation (AIR declares no revocation endpoint); a run against a live gateway |
| [3. Search index and lean skill](#3-operation-search-index-and-a-lean-skillmd) | built; run against the CLI | none known |
| [4. Host install and OCI](#4-host-install-oci-packages-and-a-per-host-toolchain) | `install` and `package oci` built; `install` run against the CLI | `--from` an OCI layout or registry, a registry pull or push, a self-contained toolchain |
| [5. Gateway on Cloud Run](#5-gateway-on-cloud-run) | files and the drain built, hermetic | a deployment, the image build, two instances on one ledger |
| [6. Token renewal](#6-token-renewal-a-sidecar-that-holds-the-token) | option C (`ANVIL_GATEWAY_AUTH=proxy`) built, hermetic | Branchyard does not set it yet |
| [7. `by_manifest`](#7-turn-manifest-claim-by_manifest) | built, hermetic | none known |
| [8. Connector icon](#8-connector-icon-id) | built; run against the CLI | none known |

## Where Anvil stands

Some audit notes were out of date. These corrections matter for the items below. The table is the state before PR #91; each item below says what is built now.

| Topic | At `8e990cb`, before PR #91 | Consequence |
|---|---|---|
| Token file | The SDKs (`packages/generators/src/sdk/gateway.ts`) and the CLI (`packages/cli/src/gateway-mode.ts`) read `ANVIL_GATEWAY_TOKEN_FILE` on every call. | A token file that a sidecar renews already works. Item 6 covers the case where the harness should not hold a token at all. |
| SKILL.md | `skill.ts` writes one line per capability with its operation count. The full table is in `reference/operations.md` (`docs.ts`). | The audit said SKILL.md lists every operation. It does not. Item 3 is still needed for search, but the lean mode is a smaller change. |
| Inbound `branchyard` mode | Built. It accepts an `https:` or `file:` JWKS (`packages/mcp-runtime/src/inbound-auth.ts`). | Item 5 needs configuration, not code. |
| Owner key | `by:v1:sha256([iss, by_tenant, sub])` (`branchyardCredentialOwner`), used as the vault `sub` in `serve-fleet-http.ts`. | Item 1 keeps it. |
| Vault | `ConnectionVault` writes one AES-256-GCM file per `(sub, connector, account)`, named by the SHA-256 of the coordinates. `delete` exists (`vault.ts`) and no route calls it. | Items 1 and 2. |
| Health | The fleet listener serves `/healthz` and `/readyz` (`serve-fleet-http.ts`). | Item 5 uses them as they are. |
| Shutdown | `SIGTERM` is handled in the single-bundle server (`mcp-runtime/src/serve.ts`). The fleet listener registers no handler that this review found. | Item 5. |
| Idempotency | A Firestore ledger exists (`runtime/src/idempotency.ts`). The Cloud Run generator sets `ANVIL_LEDGER` (`generators/src/deploy.ts`). This review did not confirm that the fleet listener selects it. | Item 5: confirm. |
| Packs | `anvil pack build`, `verify` and `install` exist. They write content-addressed, digest-checked packs (`packages/cli/src/commands/pack.ts`, `packages/system-pack`). | Item 4 builds on them. |
| Cloud Run | The generator emits one service per bundle. There is no fleet gateway image and no Dockerfile in the repository. | Item 5. |
| Icons | None in the compiler or the package output. | Item 8. |
| Disconnect | No route. | Item 2. |

## 1. Vault backend: Firestore, envelope-encrypted with Cloud KMS

**What.** A second vault backend. Each connection is one Firestore document. Each write uses a fresh data key. Cloud KMS wraps that key. Several Cloud Run instances read and write the same vault.

**Why.** The tools model moves the gateway to Cloud Run, where instances do not share a disk. The file vault needs a shared volume today. The file vault also needs a 32-byte key file (`ANVIL_VAULT_KEY_FILE`, refused when missing in `gateway.ts`). KMS removes that file.

**Wire.**

| Name | Value |
|---|---|
| `ANVIL_VAULT_BACKEND` | `file` (default, unchanged) or `firestore` |
| `ANVIL_VAULT_KMS_KEY` | `projects/P/locations/L/keyRings/R/cryptoKeys/K`, required with `firestore` |
| `ANVIL_VAULT_FIRESTORE_COLLECTION` | Optional. Default `anvil_vault` |
| `ANVIL_VAULT_FIRESTORE_DATABASE` | Optional. Default `(default)` |

With `firestore`, `ANVIL_VAULT_KEY_FILE` and `ANVIL_VAULT_DIR` are refused when set. Anvil does not mix the two backends.

Document:

- Id: the hex SHA-256 of the coordinates JSON, `[sub, connector, account]`. This is the id the file vault already uses, so migration keeps names.
- `sub` is the owner key (`by:v1:sha256(...)`). It is a hash, not a person's identity.
- Fields: `v` (2), `sub`, `connector`, `account`, `kind`, `status`, `created_at`, `updated_at`, `wrapped_dek` (bytes), `kms_key_version`, `iv`, `ciphertext`, `tag`. Secret fields, such as tokens and keys, live only in `ciphertext`.
- Encryption: AES-256-GCM with a new 32-byte data key per write. The data key is wrapped by KMS `Encrypt`. The additional authenticated data is the coordinates JSON, as in `vault.ts` today. A document copied to other coordinates fails to decrypt.
- Reads: KMS `Decrypt` once per document, and an in-process cache of the unwrapped key for at most 60 seconds.
- Writes: a Firestore transaction on the document. Two refreshes never overwrite each other silently.
- Failure: a Firestore or KMS error fails the call with `vault_unavailable`. It never returns a stale secret. A document that will not decrypt gives the existing `credential_unavailable`.

**Migration.** `anvil vault migrate --from <dir> --key-file <file> --to firestore --project P --kms-key K [--dry-run] [--remove-source]`:

- Reads each file record with the old key.
- Writes each as a document with the same id.
- Reads it back and decrypts it.
- Skips a document whose `updated_at` is not older than the source.
- Keeps the source files unless `--remove-source` is given.
- Reports failures by the record's id. It never prints a secret.
- Exits non-zero when any record fails.

**Built in Anvil (PR #91, hermetic).** `ANVIL_VAULT_BACKEND=firestore` with the variables and the document above. Where it differs from the proposal: a lease document (`<collection>_leases`, default `anvil_vault_leases`) serializes refreshes and `POST /connect/disconnect` across instances, so a rotating refresh token is spent once and a disconnect cannot be undone by a refresh that already read the record; a caller waits up to 30 seconds for another instance's lease, then fails as `vault_unavailable`. The coordinates JSON is the additional authenticated data of both the record and the KMS wrap, and a document wrapped by a different KMS key is refused. A Firestore or KMS failure gives a turn `upstream_unavailable` (retryable) with `details.code: "connector/vault_unavailable"`, and a connect route `503 vault_unavailable`. The clients call the REST APIs with `fetch` and an access token from the metadata server only (Cloud Run, GCE); other Application Default Credentials sources are not read. Evidence: Anvil's tests with in-memory fakes of Firestore and KMS (a round trip across two instances, ciphertext only in the fake Firestore, a wrong KMS key, tampered fields, a document moved to other coordinates, outages and the 60-second key cache, the connect routes over a real socket). Not done: real Firestore or Cloud KMS, the Firestore emulator, key rotation, a transaction conflict between two real instances, and **`anvil vault migrate`, which is not built**, so a file vault has no tool to move it.

**Mock before Anvil shipped.** Branchyard does not read the vault. Its own tests use the stand-in gateway, which answers the connect routes from a JSON fixture.

**Acceptance.**

- Instance A writes a connection. Instance B reads the same connection for the same coordinates.
- A document copied to other coordinates does not decrypt.
- With KMS unreachable, a call fails with `vault_unavailable`. It does not use a cached secret past its 60 seconds.
- Migrating N file records gives N documents that decrypt. A second run writes none.
- A scan of the stored documents finds no token or key in plain text.
- With `firestore`, the gateway starts without `ANVIL_VAULT_KEY_FILE`.

## 2. Disconnect

**What.** `POST /connect/disconnect` removes the person's stored connection for one connector and account.

**Why.** The console's Connections list needs a disconnect action. `ConnectionVault.delete` exists (`vault.ts`) and nothing calls it. A person must be able to revoke a connection that Branchyard's gateway holds for them.

**Wire.**

- Auth: a connect token only (`by_purpose: "connect"`). A turn token gets `403 connect_token_required`, as the other connect routes do.
- Request: `{"connector": "github", "account": "work"}`. `account` defaults to `default`, as in `anvil connect`.
- Response `200`: `{"connector": "github", "account": "work", "disconnected": true}`. When nothing was stored, the response is `200` with `"disconnected": false`. The route is idempotent.
- Order: the local record is deleted first. An upstream revocation, when the connector declares a revocation endpoint in its auth requirement, runs after it and is best effort. A failed revocation does not return an error. It appears in the audit line.
- Audit: one line, `operation` `connect.disconnect`, `decision` `allowed`, with `disconnected` true or false.
- Effect on turns: a later call to that connector fails with the existing unconnected-account error, which names the connector. The connect link goes to the person, never to the model.

**Built in Anvil (PR #91, hermetic).** The route as above: the owner comes from the verified connect token, never from the body, so a person cannot remove another person's record; no token gets `401`; the audit line carries `connect.disconnect`, `allowed` and `disconnected`, and no secret; a later call fails with `connector/not_connected`. `anvil capabilities --json` lists `connect.disconnect`. Where it differs: **no upstream revocation runs**, because AIR declares no revocation endpoint in Anvil's tree, so only the local record is removed. With the Firestore vault the delete takes the lease of item 1. Not done: a run against a live gateway or a provider. Branchyard's console does not call the route yet (`docs/companion.md`).

**Mock before Anvil shipped.** The stand-in gateway gets a `/connect/disconnect` route that removes an entry from its JSON store. Branchyard's console tests run against it.

**Acceptance.**

- After disconnect, a turn's call to that connector fails and names the connector.
- A second disconnect returns `disconnected: false`.
- A turn token gets `403 connect_token_required`.
- `vault.get` returns nothing for those coordinates.

## 3. Operation search index, and a lean SKILL.md

**What.** `anvil package harness` also writes `search.json` beside `SKILL.md`. It lists every approved operation with the fields a ranker needs. `--skill-mode lean` writes a small `SKILL.md`.

**Why.** Branchyard's `tools search` ranks operations with BM25 (`docs/tools-model.md`, section 3). Today the only index is `reference/operations.md`, which is a full table, or `--help`. A ranker should not read a full table to find one operation.

**Lean mode.** The default stays `full`. `--skill-mode lean` writes a `SKILL.md` of at most 4 KB: frontmatter, the safety rules, one line per capability with its operation count, and pointers to `search.json` and `reference/`. The per-capability workflow line moves out.

**Wire.** `search.json`:

```json
{
  "schema": "anvil.search/v1",
  "connector": "github",
  "bundle_hash": "sha256:…",
  "icon": null,
  "operations": [
    {
      "id": "github.issues.list",
      "title": "List issues",
      "summary": "List issues in a repository, filtered by state.",
      "effect": "read",
      "risk": null,
      "confirm": false,
      "idempotency": "safe",
      "resource": "issues",
      "tags": ["issues"],
      "terms": ["issue", "repository", "state"],
      "schema": "schemas/github.issues.list.input.json",
      "cli": "bin/github issues list"
    }
  ]
}
```

- `summary` is the first sentence of the operation's `description`, at most 160 characters, on one line.
- `resource` is the id of the capability that owns the operation (the AIR capability, not a new concept).
- `terms` are the tokens of the id, title, tags and parameter names, computed by Anvil.
- Operations are sorted by `id`. The same bundle gives the same bytes.
- Only approved operations are listed. Pending ones stay out of the search.
- `schema` is a path inside the package.
- A JSON Schema for the file ships as `schemas/anvil-search-v1.json`.

**The fields, pinned.** The wire above leaves some fields open. Branchyard's reader (`plugins/branchyard/skills/find-tools/scripts/tools.py`) holds them to the following, and refuses a `search.json` that does not meet them with the file and field named. Anvil accepted these rules and ships a JSON Schema for them (`schemas/anvil-search-v1.json`); a package the built Anvil wrote on 5 November 2026 passes the reader (`tests/test_anvil_real.py`).

| Field | Branchyard's rule |
|---|---|
| `schema` | exactly `"anvil.search/v1"` |
| `connector` | the package's directory name (the fleet connector id) |
| `bundle_hash` | `sha256:` and 64 lower-case hex digits |
| `icon` | present; `null`, or a slug matching `^[a-z0-9]+(-[a-z0-9]+)*$` of at most 64 characters (item 8) |
| `operations` | a list, possibly empty; ids unique and sorted ascending (byte order) |
| `id` | `CONNECTOR.` and one or more dotted names of letters, digits, `_` and `-` |
| `title` | a non-empty string |
| `summary` | a non-empty string on one line, at most 160 characters |
| `effect` | one of Branchyard's effect classes (`docs/effects.md`): `read`, `reversible`, `compensable`, `irreversible` |
| `risk` | `null`, or a non-empty string whose vocabulary is Anvil's (Branchyard does not read it) |
| `confirm` | a boolean: whether the gateway needs `"confirm": "allow"` in the grant entry |
| `idempotency` | `safe` (repeating the call is harmless), `keyed` (the operation takes an idempotency key) or `none` |
| `resource` | a non-empty string, the capability that owns the operation |
| `tags`, `terms` | lists of non-empty strings; each term is lower-case letters and digits |
| `schema` (per operation) | a relative path inside the package, without `..`, to a file that exists |
| `cli` | a non-empty string |

Fields not listed are ignored, so Anvil may add some without breaking the reader. A package without a `search.json` is read tolerantly (see `docs/tools-model.md`); a package with one is read only from it, and a refused one is never read as if it were absent.

**Built in Anvil (PR #91; run against the CLI, 5 November 2026).** `anvil package harness BUNDLE --out DIR [--workspace DIR] [--connector ID] [--skill-mode full|lean]` writes `search.json` and, per operation, `schemas/<id>.input.json`. What the run showed, and where it differs from the proposal:

- **The connector id defaults to the fleet id**: the bundle's path under `--workspace`, folded as the fleet folds it (`<workspace>/shipping/v2` is `shipping_v2`); without `--workspace` the bundle is taken as served alone and named by its directory. `--connector` overrides both. A bundle the workspace does not serve is refused.
- **Operation ids are the connector id and the AIR id with its service-id prefix stripped** (`payments.customers.get` under connector `acme` is `acme.customers.get` in `search.json`). `harness.json` keeps the AIR id (`payments.customers.get`) and puts the connector in `tool` and `cli`, so the two files name one operation differently when the connector is not the service id. The strict reader reads `search.json` only; the tolerant one, used only without a `search.json`, strips the connector prefix from a `harness.json` id and so would show such an operation under its AIR id.
- **`resource` is the id of the owning AIR capability** (`payments.customers`), not a bare noun; **`risk` is `null` for AIR `none`**, else AIR's word (`medium`, `financial`); **`idempotency`** maps AIR `natural` to `safe`, `key_supported`, `client_id` and `required` to `keyed`, `none` to `none`; **`tags` may be empty**. `effect` is the effect class; `terms` split camelCase and are sorted.
- Only approved, directly callable operations are listed. A bundle with none is refused (`has no approved, callable operations`); an operation Anvil only generated, or marked `review_required`, is not approved until `anvil approve`.
- `--skill-mode lean` writes a `SKILL.md` of at most 4 KB (1,150 bytes for a four-operation bundle).
- The package is larger than the proposal: beside `SKILL.md`, `search.json`, `harness.json`, `reference/operations.md` and `bin/<connector>` it holds `reference/errors.md` (and `capabilities.md`, `idempotency.md`, `workflows.md`), `examples/<stem>.json`, `schemas/<stem>.schema.json` (named by tool, not by operation id), `schemas/anvil-search-v1.json`, the Python SDK (`python/anvil_<connector>/`) and the TypeScript SDK (`typescript/src/`) as source trees, and `bin/package.json`.
- `harness.json` is `schemaVersion`, `connector`, `service {id, version}`, `bundleHash`, `bundle_hash`, `icon`, `gateway`, `entrypoints`, `requires` and `operations`. **It keeps `bundleHash` (bare hex) and adds `bundle_hash` (`sha256:` and the same hex)**, the form `search.json` uses; each operation is `{id, tool, cli, effect, effect_class, confirm}` and, when it has them, `undo` and `draft`. **It has no per-operation `description`**, which the mock once wrote. Branchyard's Rust never reads `harness.json` for a hash: it keys its package cache and the tool store by its own BLAKE3 hash of the bundle directory (`crates/branchyard/src/connectors/packager.rs`), and the Python reader takes `bundle_hash` from `search.json`. Neither needed to change.

**Mock before Anvil shipped.** [The mock](#the-mock) writes a `search.json` for any bundle it packages, from fixture bundles under `tests/fixtures/anvil/bundles/`. The ranker is tested on its output, and the mock's output is itself tested against the table above and, where a built Anvil exists, against the real package's shape.

**Acceptance.**

- The file validates against `schemas/anvil-search-v1.json`.
- For `examples/github-mini`, every approved operation appears once, and every `schema` path exists in the package.
- Two runs over one bundle give identical bytes.
- The lean `SKILL.md` for the largest example bundle is 4 KB or less.

## 4. Host install: OCI packages and a per-host toolchain

**What.** Two commands.

- `anvil package oci <package-dir> --out <dir>` writes an OCI image layout. `--push <ref>` pushes it to a registry, such as Artifact Registry.
- `anvil install --prefix /opt/anvil [--from <dir>]` installs the CLI, the Python SDK, the TypeScript SDK and the compose SDK once per host. (The proposal said `<ref or dir>`; **Anvil takes a directory only**, see below.)

**Why.** The tools model installs toolchains on each host when the host is set up, not in every turn (`docs/tools-model.md`, section 5). Today `anvil package harness` writes the CLI and both SDKs into every turn's package. Branchyard copies them per turn.

**Build on the pack.** `anvil pack verify` already checks each digest and the pack digest. `install` checks them before any file lands. The new commands use the same check.

**Layout.**

```text
/opt/anvil/
  current -> versions/<digest>        atomic symlink, switched last
  versions/<digest>/
    manifest.json                     every file's digest, the source ref, the versions
    bin/anvil
    python/                           site-packages with the Python SDK
    typescript/                       the TypeScript SDK and its modules
    compose/                          the compose SDK (Python and TypeScript)
    env.sh                            PATH and PYTHONPATH for the SDKs
  connectors/<bundle_hash>/           read-only connector packages, pulled by hash
```

- `anvil install` writes into a new `versions/<digest>` directory, then switches `current`. An interrupted install leaves the old version.
- Running the same digest again changes nothing.
- `anvil install --prune` removes versions that `current` does not name.
- The OCI image holds one blob per file, named by digest, and an index with the annotations `dev.anvil.bundle-hash`, `dev.anvil.connector` and `dev.anvil.version`.
- The pull side is Branchyard's job. It uses the registry's HTTP API with the host's identity. Anvil may add `anvil package pull` later; it is not required.

**Built in Anvil (PR #91).** `install` run against the built CLI on 5 November 2026; `package oci` by Anvil's tests only. What it does, and where it differs:

- **`--from` takes a directory only**: a `manifest.json` (`{"files": {path: sha256}}`) and the files it lists. A registry reference or an OCI layout is refused as `<from> is not an Anvil toolchain (manifest.json)`, exit 1; Anvil's help says to pull the image first and pass the directory. The same refusal covers a manifest with a digest that is not 64 lower-case hex digits, and one that lists no files.
- **Two names are reserved**: `manifest.json` and `env.sh` may not appear in the file list, nor an absolute path, a backslash, or a `..` part. Each is refused, before any file is written, as `digest mismatch for <name>; the path is not allowed. Nothing was written`. A tampered file is `digest mismatch for <path>; nothing was written`. Both exit 1 and leave the prefix as it was.
- **Commander's own messages**: `anvil install --help` prints `Usage: anvil install [options]` and the option list; a missing `--prefix` prints `error: required option '--prefix <dir>' not specified` and **exits 1** (not 2). The guard in `tools/session_setup.sh` and `by compute gce` (`anvil install --help`) works with both. **The real Anvil has no `ANVIL_MOCK_ROOT`**: a test that wants `/opt/anvil` under a directory uses the mock, where it is a test-only knob.
- **`env.sh` puts both SDK trees on `PYTHONPATH`**: `<version>/python` and `<version>/compose/python`, after `<version>/bin` on `PATH`.
- **The launcher**: the installed `bin/anvil` prints `anvil <digest>` for `--version` only; any other arguments run the Anvil CLI (`node <the installing Anvil's bin-anvil.js> "$@"`). Without `--from`, the toolchain is the one the installing Anvil carries: the launcher, `python/anvil_sdk` and `typescript/` (the gateway settings the clients share) and `compose/` (the composite runtime). **It is not self-contained**: the launcher needs the installing Anvil's tree to stay where it was.
- The version directory is named by the SHA-256 of the file map as `json.dumps(files, sort_keys=True)` writes it; a second run of one digest writes nothing, and `--prune` removes the versions `current` does not name.
- `anvil package oci <package-dir> --out <dir> [--push <ref>]` writes an OCI layout (`oci-layout`, `index.json`, `blobs/sha256/`), one blob per file, with `dev.anvil.bundle-hash`, `dev.anvil.connector` and `dev.anvil.version` on the index and the manifest, byte for byte the same for one package; `--push` uploads the blobs the registry lacks and sends nothing for a digest it already holds, with a bearer token from `ANVIL_OCI_TOKEN_FILE`. Anvil tested it against an in-process loopback registry. Not done: a pull or push against Artifact Registry or any real registry, a registry that wants a token exchange, and `install --from` an OCI layout. The pull side stays Branchyard's job and is not built.

**Mock before Anvil shipped.** [The mock](#the-mock) implements `anvil install --prefix` (with `--from` and `--prune`) with the layout and the checks above, and `anvil install --help` for the guard. Branchyard's host-setup tests (`tools/session_setup.sh --tools-only` and `by compute gce`) run the guarded install against it and look at what lands under the prefix. OCI packaging (`anvil package oci`) and the pull side are not mocked: a test would serve an OCI layout from an in-process HTTP server (`branchyard_testkit::MockHttp`), and none does yet.

**Acceptance.**

- `anvil install --prefix <tmp> --from <dir>` run twice leaves the second run with no changes. The manifest's mtime does not move.
- A tampered blob is refused before any file is written. The error names the path.
- In a clean shell, `source env.sh` makes `import` of the Python SDK work, and `bin/anvil --version` prints the digest.
- The OCI layout passes a standard OCI layout check. Pushing the same digest twice is a no-op.

## 5. Gateway on Cloud Run

**What.** A container image that runs the fleet gateway, with the parts Cloud Run needs.

**Why.** The tools model runs the gateway on Cloud Run (`docs/tools-model.md`, sections 2 and 6). Today there is no fleet image. The generator emits one service per bundle.

**Parts.**

| Part | Wire | State today |
|---|---|---|
| Entrypoint | `anvil serve mcp <bundles> --fleet --http "$PORT" --host 0.0.0.0` | `--http` and `--host` exist. The image passes `PORT` (Cloud Run sets it). |
| Image | A Dockerfile under `deploy/gateway/`, built on the item 4 toolchain, with the bundles from the content store | Missing |
| Public URL | `ANVIL_GATEWAY_PUBLIC_URL=https://gateway.example.com`. The OAuth callback is `/connect/callback` on it. | Built (`gateway.ts`). Required when the public URL is a custom domain. |
| Inbound auth | `ANVIL_INBOUND_AUTH_MODE=branchyard`, `ANVIL_INBOUND_ISSUER=<Branchyard URL>`, `ANVIL_INBOUND_AUDIENCE=<gateway>/mcp`, `ANVIL_INBOUND_JWKS_URI=https://<Branchyard>/.well-known/jwks.json` | Built. The JWKS cache lasts 5 minutes, so a key rotation takes up to 5 minutes to reach verification. Keep the old key published until then (`rotate-key --keep`). |
| Health | `/healthz` (liveness), `/readyz` (per-bundle certification) | Built. `/readyz` should also fail when the vault backend is unreachable (item 1). |
| Shutdown | On `SIGTERM`: stop accepting connections, let in-flight calls finish within `ANVIL_SHUTDOWN_GRACE_SECONDS` (default 8, under Cloud Run's 10-second default), close sessions, exit 0 | The fleet listener has no handler. Missing. |
| Idempotency | Firestore ledger with `ANVIL_LEDGER` | Built in the runtime. Confirm that the fleet listener selects it. |

**Built in Anvil (PR #91, hermetic).** `anvil deploy gateway --out <dir> --image <ref> --project <id> --public-url <url> --branchyard-url <url>` writes `deploy/gateway/Dockerfile` and `deploy/gateway/service.yaml` and calls nothing. The Dockerfile runs `anvil serve mcp /srv/bundles --fleet --http "$PORT" --host 0.0.0.0` under `exec`, so Node receives `SIGTERM`; the service definition carries the probes (`/readyz` startup, `/healthz` liveness), the inbound-auth variables, `ANVIL_GATEWAY_PUBLIC_URL`, `ANVIL_LEDGER=firestore://<project>/<database>/<namespace>`, `ANVIL_SHUTDOWN_GRACE_SECONDS=8` and `ANVIL_VAULT_KEY_FILE` from a mounted Secret Manager secret (the file vault). The fleet listener now drains on `SIGTERM`: a running call finishes, a new connection is refused, a call past the grace period is cut, and the process exits 0. `ANVIL_LEDGER` selects the Firestore ledger from its URI, and a bad URI stops the start. The image expects a prebuilt Anvil tree in `anvil/` and the reviewed bundles in `bundles/` of the build context; it resolves nothing from a registry. Anvil's docs do not say that `/readyz` fails when the vault backend is unreachable; this page has not checked. Evidence: Anvil's tests on loopback sockets and the generated files' contents. Not done: a deployment on Cloud Run, building the image, a call against real Firestore, and two gateway instances on one ledger.

**Mock before Anvil shipped.** Branchyard's local gateway already runs the same entrypoint with a `file:` JWKS (`by gateway start`). The Cloud Run parts are tested in the deployment phase (`docs/tools-model.md`, phase 4), not in unit tests.

**Acceptance.**

- `/healthz` returns 200. `/readyz` returns 200 when every bundle has passed.
- A token signed with Branchyard's key is accepted. A token signed with another key gets `401`. A token whose `aud` is another URL gets `401`.
- `SIGTERM` during load: the process exits 0 within the grace period, and calls that started before the signal complete.
- Two instances with the same idempotency key on one call run the upstream call once.
- A live deployment on Cloud Run is recorded with its date and the revision name. Until then it is not described as run live.

## 6. Token renewal: a sidecar that holds the token

**What.** The harness never holds a gateway token. A proxy inside the sandbox adds the token to each call and renews it.

**Today.** The SDKs and the CLI read `ANVIL_GATEWAY_TOKEN_FILE` on every call. A sidecar that replaces the file by rename is already supported. A missing or empty token file is an error (`auth_required`). That is the gap: a proxy that adds the header itself has no file to point at.

**Options.**

- **A (no change).** The sidecar writes the file by rename, and the SDK reads it per call. The harness can read the file, so it holds the token.
- **B.** `ANVIL_GATEWAY_TOKEN_URL=http://127.0.0.1:<port>/token`. The SDK asks the local proxy for the token on each call. Loopback only. A two-second timeout. A failure gives `auth_required` and names the variable.
- **C (recommended).** `ANVIL_GATEWAY_AUTH=proxy`. The SDK and the CLI send no `Authorization` header and need no token file. The process at `ANVIL_GATEWAY_URL` adds the header. Anvil refuses this mode unless `ANVIL_GATEWAY_URL` names a loopback host.

Branchyard uses C. The harness then holds no token, and the proxy renews it. A and B stay valid for other setups.

**Built in Anvil (PR #91, hermetic).** Option C: `ANVIL_GATEWAY_AUTH` is `token_file` (the default) or `proxy`; any other value is refused. With `proxy` the CLI (generated and packaged `bin/<connector>`) and the Python and TypeScript SDKs send no `Authorization` header and read no token file, and Anvil refuses the mode, before any request, unless `ANVIL_GATEWAY_URL` names a loopback host (`localhost`, `127.x.x.x`, `::1`). Go and Java clients have no gateway mode. Not done in Branchyard: its sandbox path hands the harness `ANVIL_GATEWAY_URL` (the proxy's loopback address) and no token file, and **does not set `ANVIL_GATEWAY_AUTH=proxy` yet**, so a packaged client there reads a token file by default, and the sandbox writes none. That is the next Branchyard change; no test runs a real Anvil client behind the credential proxy.

**Mock before Anvil shipped.** Branchyard's renewing proxy (`docs/tools-model.md`, phase 3) is tested with a fake gateway that checks the `Authorization` header. The Anvil side is tested with a stub server.

**Acceptance.**

- File form: write token A, call; write token B by rename, call. The server sees B on the second call.
- Proxy form: with no token file, the call goes out with no `Authorization` header, and the response comes back through a proxy that adds one.
- Proxy form with a non-loopback `ANVIL_GATEWAY_URL` is refused before any call.

## 7. Turn manifest claim: `by_manifest`

**What.** An optional token claim, `by_manifest`, carries the digest of the turn's manifest (`docs/tools-model.md`, section 4). Anvil accepts it and writes it to every audit line.

**Wire.**

- Token: `"by_manifest": "<algorithm>:<hex>"`. Text, at most 256 characters, like `by_turn`. Optional. Branchyard writes `blake3:<hex>`; Anvil treats the value as opaque text.
- Audit line: `"by_manifest": "…"`, the claim as it came, or `null` when the claim is absent.
- Claim check: a value longer than 256 characters fails the token check with `401`, as the other `by_*` claims do.

**Limits.** `by_manifest` is recorded, not enforced. It does not change the owner key, the grants or the decision. The grant still decides.

**Built in Anvil (PR #91, hermetic).** `by_manifest` is accepted and written to every audit line (`null` when absent); `anvil capabilities --json` lists `manifest.claim`. It is recorded, not enforced. Evidence: Anvil's tests on loopback sockets. Not done: a check, in Branchyard, that a real gateway's audit line carries the digest Branchyard signed.

**Mock before Anvil shipped.** Branchyard signs test tokens with `by_manifest` and checks the stand-in gateway's audit line.

**Acceptance.**

- A token with `by_manifest` gives an audit line with the same value.
- A token without it gives `null`.
- A 257-character value gives `401`.
- Two tokens that differ only in `by_manifest` get the same owner key and the same decision.

## 8. Connector icon id

**What.** An optional `service.icon` in the bundle's AIR document: a Simple Icons slug. Anvil copies it into `harness.json`, `search.json` and the output of `anvil connectors index`.

**Why.** Every row of the console's Tools page shows the connector's icon (`docs/tools-model.md`, section 5). Branchyard keeps the icon sprite and maps slugs to marks. Anvil does not ship an SVG and does not fetch one. The audit found no icon anywhere in Anvil's output.

**Wire.**

- Field: `service.icon`, a string that matches `^[a-z0-9]+(-[a-z0-9]+)*$`, at most 64 characters.
- Absent: `"icon": null` in every output.
- Upper case, spaces or a leading hyphen: refused at compile time.
- An unknown slug is accepted. Branchyard shows its generic icon for a slug its sprite does not hold.

**Built in Anvil (PR #91, hermetic; the absent-icon outputs below were run against the CLI on 5 November 2026).** `service.icon` is validated at compile time (an invalid slug is refused and the error names `service.icon`) and copied to `harness.json` (`icon`), `search.json` (`"icon": null` when absent) and, in `anvil connectors index`, to an `- **Icon:**` line (`` `github` `` or `none`). Anvil ships and fetches no image. `anvil capabilities --json` lists `bundle.icon`. The index, written by the run on a bundle without an icon, said `- **Icon:** none`, and the package said `null` in both JSON files.

**Mock before Anvil shipped.** Branchyard's sprite tool (`tools/connector_icons.py`) is tested on a fixture bundle with `icon: github`, and on one without.

**Acceptance.**

- `icon: GitHub` is refused at compile time, and the error names the field.
- `icon: github` appears in `harness.json`, `search.json` and the index.
- A bundle without `icon` gives `null` in all three.

## The connector index and the composite

Two more commands Branchyard runs when it places a turn's connectors (`crates/branchyard/src/connectors/packager.rs`). The contract did not describe them; the built Anvil writes them as follows (run on 5 November 2026).

- **`anvil connectors index --grants FILE --out FILE [--skills-root DIR] [--workspace DIR] BUNDLE...`** reads a grant (a JSON array of `{connector, operations, mode, account?, confirm?}`, or an object with a `grants` array) and writes `INDEX.md`. It is not one line per bundle. After a short preamble it writes **one block per granted connector**, headed `## <id> — <title>`, with lines `- **For:**`, `- **Use when:**`, `- **Granted:**` (the mode, then the operation ids in parentheses unless they are `*`, then `, account <name>`), `- **Icon:**` (`` `slug` `` or `none`) and `- **Skill:**` (the path of the connector's `SKILL.md` relative to the index, and its CLI). Ungranted bundles are left out. When two or more connectors are granted and `_compose/SKILL.md` exists under the skills root, a final `## Spanning connectors` block points at it. `--skills-root` names the directory that holds the packaged connectors (default: the directory of `--out`), and `<id>` is the fleet id of the bundle under `--workspace`.
- **`anvil connectors compose --out DIR [--skills-root DIR] [--workspace DIR] [--layout harness|bundle] BUNDLE...`** writes `SKILL.md`, `compose.json`, `python/anvil_compose` and `typescript/`. **It needs the harness packages**: with `--layout harness` (the default) each connector's package must already be under `--skills-root` (default: the directory of `--out`), one directory per connector id, or it stops with `<dir> is not a harness package; run anvil package harness ... first, or pass --skills-root` (exit 1); with `--layout bundle` it wraps each bundle's own `sdk/` instead. The id `_compose` is reserved. **`compose.json` is a catalog**: `{"schema": "anvil.compose-catalog/v1", "connectors": [{id, title, service, python {path, package, client}, typescript {import, client}, operations [{id, title, method, safety, effect, effectClass, confirm, inputs, ...}]}]}`. It has no top-level `operations` and no `flow.plan` or `flow.run` entry: flows are methods of the composite's Python and TypeScript `Flow` class, not catalog rows. Branchyard's placement runs compose with `--skills-root` set to the staging directory that holds the packages, then the index; the index points at the composite only when it is already there.

Branchyard's readers took neither file's format: `tools` finds a connector's description in its `SKILL.md` frontmatter and falls back to the index only when that is empty, and now reads a `## id — title` block for it (and, as before, a one-line `- id (mode): ...` entry); it reads `compose.json` only as a tolerant `operations` list or map, which the real catalog does not have, so `_compose` is listed as a connector by its `SKILL.md` and has no operations of its own (`tests/test_find_tools.py`, `RealShapes`). No Rust code parses either file: the index and the composite are placed, not read.

## The mock

`tests/fixtures/anvil/anvil` is a Python script (standard library only) that stands in for the `anvil` CLI in hermetic tests, where no built Anvil is. Put it on `PATH` as `anvil`, or name it in `[connectors] anvil`. It is the one fake Anvil: the tests of `tools_store`, `session_setup.sh`, `by compute gce` and the `tools` search script use it, so a drift between them and the real Anvil shows up in one place. It compiles nothing. **It is the real Anvil's shape, not the real Anvil**: `tests/test_anvil_real.py` runs both on the same commands when a built Anvil is present and compares the key sets of `search.json` and `harness.json`, the package's top-level entries, `capabilities --json`, `install --help`, the missing-`--prefix` error and the `--from` refusals.

| Command | What it does |
|---|---|
| `anvil --version` | `0.0.0-mock` (the real CLI prints its bare version; the installed launcher prints `anvil <digest>`) |
| `anvil capabilities --json` | the nine features the real Anvil lists, sorted: `bundle.icon`, `connect.disconnect`, `install.prefix`, `manifest.claim`, `package.oci`, `package.search`, `skill.lean`, `token.proxy`, `vault.firestore`. The mock implements `package.search`, `skill.lean`, `install.prefix` and `bundle.icon` only (see [Compatibility](#compatibility)) |
| `anvil package harness BUNDLE --out DIR [--workspace ROOT] [--connector ID] [--skill-mode full\|lean]` | writes a package (below); the connector defaults to the fleet id; failures exit 1 |
| `anvil connectors compose --out DIR [--skills-root DIR] [--workspace ROOT] [--layout harness\|bundle] BUNDLE...` | needs the packages under the skills root (or `--layout bundle`); writes `compose.json` as a catalog and a `SKILL.md` |
| `anvil connectors index --grants FILE --out FILE [--skills-root DIR] [--workspace ROOT] BUNDLE...` | writes `INDEX.md` in `## id — title` blocks with the Granted, Skill and Icon lines |
| `anvil install --prefix DIR [--from DIR] [--prune]` | item 4: a `versions/<digest>/` directory, `current` switched by rename, idempotent; `--from` a directory only, reserved names and tampered files refused before anything is written; Commander's help and required-option error (exit 1) |

A bundle is a directory with `air.json` (the mock's own small source: `service.id`, `title`, `description`, `icon`, and `operations` with the AIR `id` (`github.issues.list`), `title`, `description`, `effect` (Branchyard's class), `risk` (AIR's word, `none` for none), `confirm`, `idempotency` (AIR's `natural`, `key_supported`, `client_id`, `required`, `none`), `capability` (the owning capability's id), `tags`, `input_schema`, and `approved: false` for a pending one) or `air.yaml` (only the file's presence is read; the package gets one operation, `<service id>.status.get`). The fixture bundles are `github` and `linear`. The mapping to `search.json` is the real one: operation ids are the connector id and the AIR id without its service-id prefix, `resource` is the capability id, `risk` `none` is `null`, `natural` is `safe`, the three key modes are `keyed`, and `tags` may be empty.

The package `anvil package harness` writes is the layout the real Anvil writes (the mock's SDK trees are stand-ins):

```text
SKILL.md                    frontmatter, one line per capability with its operation count; lean mode is at most 4 KB
harness.json                schemaVersion, connector, service, bundleHash (bare hex), bundle_hash (sha256:), icon,
                            gateway, entrypoints, requires, operations: [{id, tool, cli, effect, effect_class, confirm}]
search.json                 item 3, with the pinned fields; sorted by id; the same bytes for the same bundle
schemas/<id>.input.json     one per approved operation
schemas/<stem>.schema.json  the same schema, named by tool (as Anvil copies it from the bundle)
schemas/anvil-search-v1.json  the JSON Schema of search.json
examples/<stem>.json        one example input per operation
reference/operations.md     the full table
reference/errors.md         the error codes
bin/<connector>             a launcher; bin/package.json
python/anvil_<connector>/   typescript/src/   stand-ins for the SDKs
```

Two environment variables serve tests, and **neither exists in the real Anvil**: `ANVIL_MOCK_LOG` appends `anvil ARGS` for each call to a file, and `ANVIL_MOCK_ROOT` re-roots an absolute `--prefix` under a directory, so a test can run the guarded `anvil install --prefix /opt/anvil` without touching `/opt`. Only the mock honours them; a test that needs the real Anvil's install writes under a directory it chose.

What it does not do: validate an AIR document, call an upstream, sign anything, or serve a gateway; mock items 1, 2, 5, 6 and 7; write the SDKs, the OCI layout (`anvil package oci`) or the other `reference/` files; or compose real flows. Its tests are `tests/test_anvil_mock.py`; the reader's are in `tests/test_find_tools.py`; the comparison with the real Anvil is `tests/test_anvil_real.py`, skipped with its reason where no built Anvil is (`ANVIL_BIN`, else `/home/user/anvil/packages/cli/dist/bin-anvil.js`).

## Compatibility

- Each item is optional and off by default. Today's behaviour does not change.
- Branchyard needs to know what an Anvil supports. `anvil capabilities --json` lists the features this contract adds, by name, sorted: **nine** in the built Anvil: `bundle.icon`, `connect.disconnect`, `install.prefix`, `manifest.claim`, `package.oci`, `package.search`, `skill.lean`, `token.proxy`, `vault.firestore`. The mock lists the same nine though it implements four. Nothing in Branchyard reads the list yet: `by doctor` and the console do not hide what an older Anvil lacks. The guard in `tools/session_setup.sh` and `by compute gce` is `anvil install --help` succeeding, not the list.

## Order

| Needed by (`docs/tools-model.md`, section 6) | Items | Branchyard runs against |
|---|---|---|
| Phase 1: console Connections | 2 | The stand-in gateway (the console does not call the real route yet) |
| Phase 2: mounted tool tree and search | 3, 8 | The mock, and a package from the built Anvil when there is one |
| Phase 3: in-sandbox proxy and manifests | 6, 7 | A fake gateway and the stand-in |
| Phase 4: GCP | 1, 4, 5 | A fake KMS, an OCI layout server, the local gateway |

## Not done

Everything on this page is built in Anvil (PR #91, merged 5 November 2026) and none of it has run live. Anvil's tests are hermetic; the run for this page used the built CLI on a fixture spec, with no network and no model. Not run: Firestore, Cloud KMS, Cloud Run, a registry, a provider's sign-in, a model-driven turn that calls a packaged connector through a Branchyard-placed home with the new package. Not built in Anvil: `anvil vault migrate`, `install --from` an OCI layout or registry, a self-contained toolchain, an upstream revocation on disconnect. Not built in Branchyard: the console's Disconnect, `ANVIL_GATEWAY_AUTH=proxy` in a sandbox, the pull side of an OCI package, reading `capabilities --json`. The acceptance tests above that the mock can answer (determinism, the 4 KB lean skill, idempotent install, a tampered file refused) run against the mock and, where a built Anvil exists, against Anvil; the rest are tests to write, not results.
