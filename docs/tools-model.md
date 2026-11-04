# Tools: one model for connectors, skills and helpers

Written 4 November 2026. Status: **design, not built.** It answers four questions from the console work in [#67](https://github.com/vamsiramakrishnan/branchyard/pull/67): how a person's identity and permissions reach Anvil's gateway, how a tool is installed, allowed and chosen per query, how choosing fewer tools saves context, and how all of it runs on GCE, GCE with Microsandbox, GKE and Substrate. The audits it rests on read the code at `af17285d` plus #67, and Anvil at `8e990cb`. Each claim about today names its file.

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
- **Operations are found, not listed.** Anvil's SKILL.md tables every operation, which grows large. `tools search` ranks with BM25 over the frontmatter of the chosen skills, each connector's operations (Anvil's `reference/` and `compose.json`) and each helper. `tools schema` prints one operation's schema. A skill named `find-tools` teaches the habit in about 600 characters. This is the skill-search skill.
- **Native discovery too.** The chosen skills are also linked where a harness discovers skills itself (Claude Code's `.claude/skills/`, Codex's skills directory), so a harness that loads only frontmatter loads only these.
- **Branchyard's own surface follows the choice:**
  - Without helpers, the turn gets `leaf.md` (6.6 KB) instead of the delegate skill (38.5 KB), and the MCP server lists no spawn, delegate or graph tools.
  - Without outputs, it lists no artifact or scratch tools.
  - The operator paragraphs move out of the MCP instructions into a file.
  - The Branchyard surface of a plain Work turn falls from about 90 KB to under 15 KB, enforced by `tools/model_surface_budget.json` per case.

## 4. One turn, remotely: lifecycle and delivery

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

- **One Tools page.** It is under Settings, not a sidebar item, with one table for connectors, skills and helpers. Each row has the tool's icon, its state in words and its one action. Administrators see the Allowed column and edit it. The page reads `GET /v1/tools`.
- **One control in the composer.** A **Tools** button sits beside the message box, showing the chosen tools' icons, stacked. It opens a menu with three groups and a search field, matching what the harness itself searches. The separate step in the session form goes away.
- **Icons.** `assets/connector-icons.svg` is a sprite of 57 marks from Simple Icons (CC0), drawn in `currentColor` so they take the page's ink, and regenerated by `tools/connector_icons.py`. Skills and helpers get line icons from the console's own set.

## 6. Order of work

| Phase | What | Where |
|---|---|---|
| 1 | The model and its API (`/v1/tools`, server-side defaults, one start field); policy in the store; required issuer; the console's Tools page and composer control | Branchyard |
| 2 | The mounted tool tree, `tools search`, the `find-tools` skill, native skill links, Branchyard's MCP surface following the choice, budgets per case | Branchyard |
| 3 | The in-sandbox proxy with renewing tokens; turn manifests; workers re-admitting on a stale policy | Branchyard |
| 4 | GCP: KMS signing, IAP and groups, Artifact Registry and GCS store, GCS FUSE on GKE, FQDN egress, Workload Identity token fetch | Branchyard, deployment |
| 5 | Anvil: Firestore and KMS vault, disconnect route, operation search index in the package, a fleet-gateway image for Cloud Run | Anvil |

## Not done

Everything on this page is a proposal. Nothing here is built or measured beyond the sizes the audits read.
