# Anvil contract v2

Written 4 November 2026.

**Status, 4 November 2026: proposed. Nothing here is built in Anvil.** Evidence: a read of Anvil at `8e990cb`, with the file for each claim named in its item. No Anvil build, no run, and no Branchyard test against a mock exists yet. Not done: any Anvil change, any mock in Branchyard, any live run on GCP.

This page is what Anvil must add so that the tools model in `docs/tools-model.md` works remotely. Branchyard builds against this contract now, with a mock for each item, and switches to Anvil when Anvil ships the item. Each item gives what it is, why Branchyard needs it, the wire shape, how Branchyard tests before Anvil ships, and the acceptance test.

Today's contract is in [connectors](connectors.md). This page extends it. Nothing here changes a claim that connectors.md already makes.

## Where Anvil stands

Some audit notes were out of date. These corrections matter for the items below.

| Topic | Today at `8e990cb` | Consequence |
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

**Mock before Anvil ships.** Branchyard does not read the vault. Its own tests use the stand-in gateway, which answers the connect routes from a JSON fixture. Anvil's tests use a local fake KMS (a wrapping key in memory) and the Firestore emulator from `gcloud`.

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

**Mock before Anvil ships.** The stand-in gateway gets a `/connect/disconnect` route that removes an entry from its JSON store. Branchyard's console tests run against it.

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

**Mock before Anvil ships.** The fake packager in Branchyard's tests writes a fixture `search.json` (for example, `crates/branchyard/tests/fixtures/anvil/search/github.json`). The ranker is tested on that fixture. No Anvil run is needed.

**Acceptance.**

- The file validates against `schemas/anvil-search-v1.json`.
- For `examples/github-mini`, every approved operation appears once, and every `schema` path exists in the package.
- Two runs over one bundle give identical bytes.
- The lean `SKILL.md` for the largest example bundle is 4 KB or less.

## 4. Host install: OCI packages and a per-host toolchain

**What.** Two commands.

- `anvil package oci <package-dir> --out <dir>` writes an OCI image layout. `--push <ref>` pushes it to a registry, such as Artifact Registry.
- `anvil install --prefix /opt/anvil [--from <ref or dir>]` installs the CLI, the Python SDK, the TypeScript SDK and the compose SDK once per host.

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

**Mock before Anvil ships.** Branchyard's host-setup tests serve an OCI layout from an in-process HTTP server (`branchyard_testkit::MockHttp`). The fake `anvil` binary on `PATH` stands in for the CLI, as the tests already do.

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

**Mock before Anvil ships.** Branchyard's local gateway already runs the same entrypoint with a `file:` JWKS (`by gateway start`). The Cloud Run parts are tested in the deployment phase (`docs/tools-model.md`, phase 4), not in unit tests.

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

**Mock before Anvil ships.** Branchyard's renewing proxy (`docs/tools-model.md`, phase 3) is tested with a fake gateway that checks the `Authorization` header. The Anvil side is tested with a stub server.

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

**Mock before Anvil ships.** Branchyard signs test tokens with `by_manifest` and checks the stand-in gateway's audit line.

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

**Mock before Anvil ships.** Branchyard's sprite tool (`tools/connector_icons.py`) is tested on a fixture bundle with `icon: github`, and on one without.

**Acceptance.**

- `icon: GitHub` is refused at compile time, and the error names the field.
- `icon: github` appears in `harness.json`, `search.json` and the index.
- A bundle without `icon` gives `null` in all three.

## Compatibility

- Each item is optional and off by default. Today's behaviour does not change.
- Branchyard needs to know what an Anvil supports. A proposed addition: `anvil capabilities --json` lists the features this contract adds, by name (`vault.firestore`, `connect.disconnect`, `package.search`, `skill.lean`, `install.prefix`, `package.oci`, `token.proxy`, `manifest.claim`, `bundle.icon`). `by doctor` reads it, and the console hides what the Anvil in use lacks.

## Order

| Needed by (`docs/tools-model.md`, section 6) | Items | Branchyard runs against |
|---|---|---|
| Phase 1: console Connections | 2 | The stand-in gateway |
| Phase 2: mounted tool tree and search | 3, 8 | Fixture `search.json` and icon files |
| Phase 3: in-sandbox proxy and manifests | 6, 7 | A fake gateway and the stand-in |
| Phase 4: GCP | 1, 4, 5 | A fake KMS, an OCI layout server, the local gateway |

## Not done

Nothing on this page is built in Anvil. No Branchyard mock exists yet for any item. No item has been run against a real Cloud Run, Firestore or KMS. Each item's acceptance is a test to write, not a result.
