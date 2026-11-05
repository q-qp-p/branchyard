# Tools: one model for connectors, skills and helpers

Written 5 November 2026. Status: **design; section 3 is built, section 4 is partly built (the manifest, the policy version, renewing tokens, the shared tool store and host install, the credential proxy in the sandbox with the server's token fetch), the rest is not.** It answers four questions from the console work in [#67](https://github.com/vamsiramakrishnan/branchyard/pull/67): how a person's identity and permissions reach Anvil's gateway, how a tool is installed, allowed and chosen per query, how choosing fewer tools saves context, and how all of it runs on GCE, GCE with Microsandbox, GKE and Substrate. The audits it rests on read the code at `af17285d` plus #67, and Anvil at `8e990cb`. Each claim about today names its file.

## The problem in four lines

- **The model is unclear.** A connector, a skill and a harness each have their own word for "may I use it", stored in different places: server JSON, `branchyard.toml`, presets, the browser.
- **Choosing less saves almost nothing.** About 90 KB reaches every turn whatever was chosen: Branchyard's MCP tool list (41.8 KB, 35 tools, never filtered, `crates/branchyard-mcp/src/lib.rs:857`), the delegate skill (38.5 KB) and the operator notes in the MCP instructions (7.5 KB). A turn also copies about 425 KB of skills into a fresh sandbox, every turn.
- **Remote is not closed.** A worker signs gateway tokens with its own key file, made silently if missing (`KeyRing::load_or_create`). The issuer defaults to the listen address. Tokens last at most an hour and are never refreshed. Delegation and the effects proxy are unreachable from every sandbox but Microsandbox (`reaches_host`). No sandbox enforces egress.
- **The console shows it twice.** Connections sit in the sidebar, while tools sit in a form step of their own.

## 1. The model

Every tool is one of three kinds:

- a **connector**: an upstream API through Anvil, called as code;
- a **skill**: instructions and scripts on disk;
- a **helper**: a harness a session may delegate to.

Each kind passes through the same five states. Each state narrows the one before it, and each has one owner:

| State | Means | Owner | Connector | Skill | Helper |
|---|---|---|---|---|---|
| **Available** | It exists here | Operator | The gateway serves the bundle | It is in the skill registry (built in, organisation or repository) | An environment (preset) offers it |
| **Allowed** | This person may use it, at most this much | Administrator | A ceiling grants `read` or `write` | Policy allows the skill or its source | Policy allows delegation to it |
| **Ready** | It is set up for this person | Person | An account is connected in the vault | The skill is installed in their library | It is signed in for the environment |
| **On by default** | Sessions here start with it | Workspace owner | Workspace default | Workspace default | Workspace default |
| **Chosen** | This query uses it | Person, per query | In the session's grant | Staged for the turn | In the turn's envelope |

The rule is that each state's set sits inside the one before: Chosen ⊆ Ready ⊆ Allowed ⊆ Available, and On by default only suggests within Ready. The server computes every row for the caller. A tool is shown at the first state it fails, with that state's one action:

- **Ask your administrator** (not allowed).
- **Connect**, **Install** or **Sign in** (not ready).
- A plain tick (ready).

Admission computes it again, so the browser never decides.

**One API for all three kinds.** It replaces the per-kind routes:

- `GET /v1/tools?workspace=` lists each tool with its `kind`, `state`, `max` (for example `read`), its default, `action` and `reason`.
- `PUT /v1/workspaces/{workspace}/tools` stores the workspace's defaults, guarded by a revision.
- A session start takes one field: `tools: [{kind, id, mode?}]`.

The routes added in #67 become aliases.

**One place for policy.** Policy lives in the server's store (SQLite, or Cloud SQL for PostgreSQL), keyed by tenant and group, not only by principal name. An administrator edits it through `/v1/admin/policy`. Workers do not read it from their own JSON (section 4).

## 2. Identity, end to end

| Hop | Today | Proposed |
|---|---|---|
| Person → console | A bearer token, a paired device token, or a Google ID token matched to an exact `(iss, sub)` binding (`auth.rs:58`) | **IAP** in front of the console (a GKE Gateway or Cloud Run). The server verifies `x-goog-iap-jwt-assertion`. The tenant comes from the domain or a mapping, and groups from the Cloud Identity Groups API (cached for five minutes). Bearer and paired tokens stay for the CLI and devices |
| Console → policy | Ceilings keyed by principal name; presets by tenant | Policy by tenant and group (section 1); the person's effective policy is computed per request |
| Server → gateway | A turn JWT (Ed25519) signed with `<data_dir>/gateway/key`; `iss` defaults to the listen URL | Signed by **Cloud KMS** (`EC_SIGN_ED25519`): server and workers sign through IAM, the JWKS comes from the KMS public key, and no key file exists. `issuer` is required whenever a gateway is remote |
| Gateway → vault | `by:v1:sha256([iss, tenant, sub])`, as a file per connection on one machine | Same owner key. The vault moves to Firestore, envelope-encrypted with Cloud KMS, so Cloud Run instances share it. Adds a **disconnect** route (`ConnectionVault.delete` exists in Anvil, unexposed) |
| Gateway → upstream | OAuth or a stored key | Unchanged; Anvil's `SecretManagerCredentialResolver` already serves operator-owned keys |

Two rules fall out of the owner key:

- The issuer and a person's tenant are part of their identity at the gateway. Changing either orphans their connections.
- So the issuer is set once per deployment and checked at start, and moving a person between tenants is an explicit migration.

## 3. Context: a mounted tool tree, searched, not pasted

**Status: built (5 November 2026), hermetic only.** What is built and measured is listed in "Built" at the end of this section. Not done: any run of Claude Code or Codex with a model against the tree or the native links; Anvil's real package formats (the search reads `harness.json`, `compose.json`, `schemas/` and `reference/` tolerantly, from `connectors.md`); a listing of helpers (the per-turn choice of skills is built: the index and the tree list only the chosen skills, `Provisioning::skills`); `helpers/INDEX.md` is read when present and nothing writes it; a `/tools` link at the guest root (the tree is under the home, `.branchyard/tools/`, not `/tools`).

A turn sees only what was chosen. Everything it sees, past one line, is on disk and found by search.

```text
/tools/                         read-only, per turn
  INDEX.md                      ≤ 4 KB: one line per chosen tool (name: one sentence), and how to search
  bin/tools                     `tools search WORDS` · `tools show ID` · `tools schema CONNECTOR.OPERATION`
  skills/<name>/SKILL.md        frontmatter (name, description) first; references and scripts beside it
  connectors/<id>/SKILL.md      Anvil's package: SKILL.md, reference/, bin/<id>, python/, typescript/
  connectors/_compose/          Anvil's compose SDK, when two or more connectors are chosen
  helpers/INDEX.md              each helper: what it is good at, its cost and how to delegate
```

- **The prompt gets one line:** "Your tools are under /tools: read /tools/INDEX.md, and search with `tools search` before reading a skill." Unchosen tools cost nothing.
- **Connectors are code.** The harness calls Anvil's CLI (`bin/<id> --schema`, `--dry-run`) or its Python and TypeScript SDK through the gateway, as Anvil intends (its `harness-package.ts`). No MCP server is added per connector.
- **Operations are found, not listed.** Anvil's SKILL.md lists capabilities with operation counts; the full table is `reference/operations.md`. `tools search` ranks with BM25 over the frontmatter of the chosen skills, each connector's operations (Anvil's `reference/` and `compose.json`) and each helper. `tools schema` prints one operation's schema. A skill named `find-tools` teaches the habit in about 600 characters. This is the skill-search skill.
- **Native discovery too.** The chosen skills are also linked where a harness discovers skills itself (Claude Code's `.claude/skills/`, Codex's skills directory), so a harness that loads only frontmatter loads only these.
- **Branchyard's own surface follows the choice:**
  - Without helpers, the turn gets `leaf.md` (6.6 KB) instead of the delegate skill (38.5 KB), and the MCP server lists no spawn, delegate or graph tools.
  - Without outputs, it lists no artifact or scratch tools.
  - The operator paragraphs move out of the MCP instructions into a file.
  - The Branchyard surface of a plain Work turn falls from about 90 KB to under 15 KB, enforced by `tools/model_surface_budget.json` per case.

**Built.** `crates/branchyard/src/tooltree.rs` mounts the tree and links native skills (`~/.claude/skills`, `~/.codex/skills`, private homes only); `plugins/branchyard/skills/find-tools/` holds the skill and the `tools` script (BM25, `search`, `show`, `schema`), tested in `tests/test_find_tools.py`; `Grant::has_helpers` picks the delegate skill or `leaf.md`; `operations::Surface` and `BRANCHYARD_MCP_SURFACE` gate `tools/list`; the operator paragraphs moved to [delegation.md](delegation.md#operator-notes). Measured with `cargo test -p branchyard-mcp` and `tools/check_model_surface.py`:

| Case | Before | After | Cap |
|---|---|---|---|
| MCP `tools/list`, leaf (8 tools) | 41,736 B (35 tools) | 5,500 B | 5,800 |
| MCP `tools/list`, leaf with a result contract (17 tools) | 41,736 B | 8,861 B | 9,300 |
| MCP `tools/list`, branch with helpers (35 tools) | 41,736 B | 41,736 B | 43,900 |
| Spawning branch text (delegate skill, instructions, note) | 45,969 chars | 42,314 chars | 44,500 |
| Leaf branch text (`leaf.md`, instructions, note) | 14,352 chars | 8,526 chars | 9,000 |
| Work turn, no delegation: note and the tools line | note 0.4 KB, capabilities line 0.2 KB, connectors line 0.6 KB | 661 chars at the line's cap | 1,000 |
| Skills and connectors lines | two lines, 0.8 KB | one line under 300 chars | 300 |

A plain Work turn with no delegation gets no MCP server at all, so its Branchyard surface is under 1 KB of prompt plus the 4 KB index it reads on demand. A leaf is 8,526 chars plus 5,500 B of tools, about 14 KB. A spawning branch is not smaller in tools; its saving is 3.7 KB of instructions. The delegate skill itself (38.5 KB) is unchanged and still goes to every branch with helpers.

## 4. One turn, remotely: lifecycle and delivery

**Status: partly built.** Three parts, each hermetic. Hermetic (fakes only: the fake ACP agent, a mock gateway, a fake packager, an in-process server; no model, no real sandbox, no GCP), run on 5 November 2026 at the commit that adds this paragraph. Built:

- **The manifest (step 1, in part).** A session resolves a `TurnManifest` in its first turn and keeps it with the branch; every turn uses it until the person changes the session's tools. It holds the actor, grants, chosen skills (or all), each connector's bundle hash, the helpers envelope, models, network, the policy version and the time. Its blake3 digest is written to `~/.branchyard/manifest.json` (`BRANCHYARD_MANIFEST`), carried in the token as `by_manifest`, in the `provisioned` event and on the console's session view. See [the turn manifest](connectors.md#the-turn-manifest).
- **The policy version (steps 1 and 2, on the server).** Every `PUT` and `DELETE` of any policy record raises the tenant's version in the same transaction. A session records the version at admission. When its queued start or send is claimed and the version has moved, the server checks the session's tools against the policy as it stands and refuses `policy_changed` if they are no longer allowed, else records the new version in the session and its manifest. See [tool policy](server.md#tool-policy).
- **Renewing tokens (step 4, in part).** The token file holds a five-minute token that the engine replaces by rename every two and a half minutes while the turn runs, and the effects proxy calls the gateway with the live token whatever the harness presents (its own, an earlier one, or none), so a turn can outlast the hour. See [token renewal](connectors.md#token-renewal).

Not done: the worker does not ask the server to re-admit (step 2): the check runs on the server when the operation is claimed, not on a worker that holds an older manifest, and a worker still reads its own configuration. The digests in the manifest are recorded but a turn does not check its linked store entries against them. Egress (step 5) and sleep and resume (step 7) are as before. No part of this ran against a real Anvil, gateway, sandbox or cluster.

**The credential proxy in the sandbox and the token fetch (step 4), built 5 November 2026, hermetic only:**

- **The route.** `POST /v1/turns/token` with `{manifest, branch, turn}` issues a five-minute gateway token for a *running* turn ([server](server.md#turn-tokens-for-sandboxes)). The caller is a sandbox: a **Google ID token** for the server's audience, of a service account listed in `connectors.sandbox_identities` (GKE Workload Identity, or a GCE VM's service account), verified against Google's keys; or the turn's **per-turn secret**, whose hash the engine recorded with the turn and manifest digest, for a sandbox with no cloud identity (Microsandbox, local, hermetic tests). The server refuses a wrong manifest digest (`manifest_mismatch`), a branch that is not running (`turn_not_running`), another turn's number, an account that is not listed (`identity_not_allowed`) or is for another tenant, and a body that carries any field beyond the three; the claims come from the turn's recorded grant with the person's ceiling applied, as the turn's own token's do.
- **The proxy.** `by credential-proxy` (`connectors/sandbox_proxy.rs`) listens on the sandbox's loopback, fetches the token, renews it at half its life, and forwards the harness's calls to the gateway with it. The harness is given `ANVIL_GATEWAY_URL=http://127.0.0.1:18900/mcp` and nothing else: no token file in its home, no token in its environment.
- **Where it runs, chosen per provider** (`ProviderKind::credential_site`, one declaration each, tested for every provider; `connectors/credential.rs` joins it with the operator's `connectors.sandbox_server`): local process, container and recipe machine keep the host's proxy; Microsandbox gets a process the engine starts in the guest and kills with the turn; a GKE Agent Sandbox gets a `credential-proxy` sidecar container in the pod ([the pod](agent-sandbox-gke.md#credential-proxy-sidecar)); Substrate is configuration only (its template would run `by credential-proxy --config ~/.branchyard/credential-proxy.json --wait`). Without `sandbox_server` every provider keeps the host arrangement. See [the credential proxy in the sandbox](connectors.md#the-credential-proxy-in-the-sandbox).

Evidence: hermetic (mock HTTP servers on loopback, a fake sandbox provider running a stand-in `by` as a host process, a fake `kubectl`, a real server with Google's keys served by a mock and ID tokens signed with a test key; no model, no network beyond loopback, no GCP): `connectors/sandbox_proxy/tests.rs`, `connectors/guest/tests.rs`, `connectors/issue.rs`, `providers/tests.rs`, `crates/branchyard/tests/connectors.rs`, `crates/branchyard-agent-sandbox/src/tests.rs`, `crates/branchyard-cli/tests/credential_proxy.rs`, `crates/branchyard-server/tests/turn_token.rs`. Not done: **a live run on GKE or GCE**: Workload Identity ID tokens from a real metadata server, a pod's sidecar and the NetworkPolicy exception on a cluster (and in the gVisor and Kata runtimes), a Microsandbox guest with a real `by` in its image; a Substrate template that runs the sidecar; the effect-ledger proxy and its approvals inside the sandbox (a turn whose sandbox holds the credentials has none and says so); a cap of the issued token at the turn's deadline (a token lives five minutes whatever the turn's deadline); and Anvil's `ANVIL_GATEWAY_AUTH=proxy` mode. A harness in the same pod can still reach the metadata server and the proxy; what it cannot do is read a token from a file or keep one past five minutes.

**The shared tool store and host install (step 3), built 5 November 2026, hermetic only:**

- **Store.** `[tools] store` in `branchyard.toml` (server: `tools.store`) names a read-only directory that holds `skills/<name>@<digest>/` and `connectors/<id>@<bundle-hash>/`, with a `store.json` index (`id`, `kind`, `digest`, `size`, `path`). A digest is the BLAKE3 hash of the entry's files, the hash `anvil package harness` output is cached under. When the store has an entry for a turn's chosen skill or granted connector, the turn links to it; otherwise it copies, as before. A skill keeps its own directory with a link for each file, so its references to sibling skills (`../artifact-tools/INDEX.md`) still resolve. A connector's package is one link.
- **Which providers link.** A provider links only where the harness sees the store at the same path. A local harness does. Microsandbox gets a read-only volume of the store root. A GKE Agent Sandbox gets a Cloud Storage FUSE CSI volume (`driver: gcsfuse.csi.storage.gke.io`, `readOnly: true`) when `[tools] store_bucket` (server: `tools.store_bucket`) is set, and the pod annotation `gke-gcsfuse/volumes: "true"`; see [Agent Sandbox on GKE](agent-sandbox-gke.md#mount-the-tool-store). Container, Substrate and recipe sandboxes copy the home in, so a link would dangle: they keep the copy.
- **Publishing.** `by tools publish` writes the entries for the built-in skills and for every bundle under `[connectors] bundles` (packaged by `anvil package harness`, cached by bundle hash), then `store.json`. `--gcs gs://BUCKET/PREFIX` mirrors the store with `gcloud storage rsync`. `--oci REGISTRY/REPOSITORY` pushes each entry as an OCI artifact with the `oras` CLI, tagged by its digest (`REGISTRY/REPOSITORY/skills/NAME:DIGEST`, `.../connectors/ID:BUNDLE-HASH`), and the index as `REGISTRY/REPOSITORY/index:latest`. Branchyard links no registry client; it runs `oras` and `gcloud` from `PATH`.
- **Pulling.** `by tools pull --from gs://BUCKET/PREFIX` or `--oci REGISTRY/REPOSITORY` fills a local store, fetches only the entries it lacks, and checks every entry against its digest in the index. A mismatch fails and names the path.
- **Host setup.** `by compute gce --tools-store gs://BUCKET/PREFIX` and `BRANCHYARD_TOOLS_FROM=gs://... sh tools/session_setup.sh --tools-only` pull the store to `/opt/branchyard/store` once. Both also run `anvil install --prefix /opt/anvil` once when `anvil` is on the host and has an `install` command, and say they skipped it when it is not ([Anvil contract v2](anvil-contract-v2.md), item 4). [Deploying `by serve`](deploy.md#tool-store-in-an-image) shows the same two steps for an image.

Evidence: hermetic. Fake `gcloud`, `oras`, `kubectl` and Anvil scripts record their arguments and move files between directories that stand for a bucket and a registry (`crates/branchyard/src/toolstore/tests.rs`, `crates/branchyard-cli/tests/tools_store.rs`, `crates/branchyard-agent-sandbox/src/tests.rs`, `tests/test_compute_gce.py`, `tests/test_session_setup.py`). Not done: any run of `oras` against Artifact Registry or `gcloud` against a bucket; the CSI volume on a live GKE cluster, in the gVisor and Kata runtimes, with a turn reading through it; the Substrate actor-image layer; an `anvil install` from a real Anvil (it does not exist yet, so the call is guarded). Known limits: `gcloud storage rsync` and the FUSE mount do not keep file modes, so a connector's `bin/` launchers are not executable through a bucket (run them through their interpreter, or add `file-mode=755` to the mount options); and with `network = "deny"` the CSI sidecar, which runs in the pod's network, has no route to Cloud Storage.

Admission resolves everything once, and the turn carries the result. A worker runs exactly what was admitted.

1. **Admission (server).** It computes the effective tools (section 1). It resolves each to a content digest, writes a **turn manifest** and stores it with the operation:
   - `{actor, tenant, grants, skills: [digest], connectors: [digest], helpers, model access, network, deadline, policy version}`.
2. **Claim (worker).** The worker checks the manifest's policy version against the server's. If the version is newer, it asks the server to re-admit the turn rather than run stale policy.
3. **Tools in place.** Each digest is an immutable bundle in a content-addressed store:
   - Skills and connector packages are published once to **Artifact Registry** as OCI artifacts, mirrored to a GCS bucket.
   - The turn mounts the store read-only. Its own files are only `INDEX.md` and a tree of links, a few kilobytes.

   | Runs on | Store mounted by | Gateway reached at | Token held by |
   |---|---|---|---|
   | GCE, local process | Host cache directory, filled from GCS | `gateway` | A sidecar on the host (step 4) |
   | GCE with Microsandbox | virtiofs share of the host cache, read-only | `sandbox_gateway` (host address) | The host sidecar, through the address the guest reaches |
   | GKE Agent Sandbox | **Cloud Storage FUSE CSI** volume with the file cache, or an image volume of the toolset | Gateway's internal Service; NetworkPolicy allows only it | A sidecar container in the pod |
   | Substrate | Actor image layer or copy-in of the chosen digests only | Routed ingress URL | A sidecar in the actor |
4. **Credentials (sidecar).** The harness never holds a gateway token.
   - SDK and CLI calls go to `127.0.0.1:<port>` in the sandbox: a small proxy (today's effects proxy, moved into the sandbox).
   - The proxy attaches a five-minute token and renews it before expiry. A turn longer than an hour keeps working, and a leaked token dies in minutes.
   - The proxy fetches tokens from the server as the sandbox's identity:
     - on GKE, a Kubernetes service account through **Workload Identity**;
     - on GCE, the VM's service account.
   - Each fetch is bound to the turn by its manifest id.
   - This also fixes the unreachable delegation and effects endpoints, because the proxy is inside every sandbox.
5. **Egress.** A sandbox may reach the gateway, the model endpoint and the package mirrors, and nothing else:
   - GKE: an FQDN network policy;
   - GCE and Microsandbox: the egress proxy;
   - Substrate: its egress rules.
6. **End.** Results are copied or applied back as today. The token stops renewing, the links go, and the store stays cached for the next turn on that host or node.
7. **Sleep.** A paused Microsandbox or Substrate sandbox keeps its mounts and loses its token. On resume it gets a fresh manifest, so a tool deselected meanwhile is gone.

**Configuration carries over** because it is data, not files:

- The catalog, policy, defaults and bundles live in the server's store and the content store.
- Secrets are Secret Manager names, read by each worker as its own service account.
- A worker's JSON shrinks to: where the server is, who it is, and its labels.

## 5. The console

- **One Tools page.** It is under Settings (`#/settings/tools`), not a sidebar item, with one table for connectors, skills and helpers. Each row has the tool's icon, its state in words and its one action. Administrators see the Allowed column and edit it. The page reads `GET /v1/tools`.
- **One control in the composer.** A **Tools** button sits beside the message box, showing the chosen tools' icons, stacked. It opens a menu with three groups and a search field, matching what the harness itself searches. The separate step in the session form goes away.
- **Icons.** `assets/connector-icons.svg` is a sprite of 57 marks from Simple Icons (CC0), drawn in `currentColor` so they take the page's ink, and regenerated by `tools/connector_icons.py`. Skills and helpers get line icons from the console's own set.

**Status, 5 November 2026.** The first two bullets are built in the console ([the Tools page and the composer's Tools control](companion.md#tools-page)): one table under **Settings > Tools** with an Allowed column and a policy editor for an administrator, and one **Tools** button with a menu (popover wide, modal sheet at 759 pixels or less) that sends a start's choice as `tools` and saves a workspace's defaults on the server. Evidence: hermetic (a real server, a mock gateway and the fake agent, in headless Chromium at 1440 and 390 pixels). Not done: a real gateway, a screen reader, a person's own policy, **Install** for a helper's harness (shown disabled, "Not available yet"), and the helper limit, which `tools` cannot carry, so the server's default applies.

## Decisions (4 November 2026)

- **Policy store:** PostgreSQL, running on **AlloyDB**. Two ways to connect: through the AlloyDB Auth Proxy (a sidecar on GKE, with `--auto-iam-authn`), or directly over TLS (`sslmode=require` with the instance's server CA), with an IAM token from the metadata server as the password at each connection. SQLite stays for one machine.
- **Sign-in:** IAP in front of the console, plus any standard identity federated through GCP: Workforce or Workload Identity Federation tokens, and plain OIDC from a configured issuer. Groups come from a token's claim, or from Cloud Identity when the token carries none.
- **Shared store:** tool bundles are OCI artifacts in **Artifact Registry**, mirrored to a GCS bucket and mounted read-only with **Cloud Storage FUSE**.
- **Manifest:** every session or conversation starts by resolving its turn manifest (section 4). Every turn of it carries that manifest, renewed only when the person changes the session's tools.
- **Host setup:** connector bundles, Anvil's CLI and SDKs, and the compose SDK are installed on each host when it is set up (`by setup`, `by compute gce`, the worker image), not packaged per turn.
- **Anvil:** it evolves to the contract in [Anvil contract v2](anvil-contract-v2.md); Branchyard builds against that contract, with a mock until Anvil ships each part.

## 6. Order of work

| Phase | What | Where |
|---|---|---|
| 1 | The model and its API (`/v1/tools`, server-side defaults, one start field); policy in the store; required issuer; the console's Tools page and composer control | Branchyard |
| 2 | The mounted tool tree, `tools search`, the `find-tools` skill, native skill links, Branchyard's MCP surface following the choice, budgets per case (built; see section 3) | Branchyard |
| 3 | The in-sandbox proxy with renewing tokens; turn manifests; workers re-admitting on a stale policy. **Built:** manifests, the policy version and its check at claim, renewing tokens on the host's proxy, the proxy in the sandbox with the server's token fetch (hermetic). **Not built:** workers re-admitting, the effect ledger in the sandbox | Branchyard |
| 4 | GCP: KMS signing, IAP and groups, Artifact Registry and GCS store, GCS FUSE on GKE, FQDN egress, Workload Identity token fetch. **Built, hermetic:** the Workload Identity token fetch and the GKE sidecar. **Not run live** | Branchyard, deployment |
| 5 | Anvil: Firestore and KMS vault, disconnect route, operation search index in the package, a fleet-gateway image for Cloud Run | Anvil |

## Not done

Everything on this page is a proposal, except section 3 and the tool store and host install and the credential proxy in the sandbox in section 4, which are built and tested hermetically. Nothing else here is built or measured beyond the sizes the audits read.
