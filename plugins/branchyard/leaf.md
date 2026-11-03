Your delegated branch cannot create Branchyard children. Native planning, task
graphs and subagents follow harness permissions and limits. `by` on PATH and in
`$BRANCHYARD_BY` acts as your branch; commands accept `--json`:

`by obligations [BRANCH] --json` lists approvals, uncertain effects and blocked work. Local only.

- `by inspect` shows your status, budget and last message.
- `by check` runs your configured or inherited check on your current work
  merged into your parent's branch as integration would. Run and fix a code
  task's check before finishing; failure exits 1.
  Result-only research needs no code check or fabricated Git changes.
- `by artifact publish FILE [--name N] [--media-type TYPE] [--label K=V]`
  publishes an immutable file for you, ancestors and descendants; siblings
  need an explicit share or handoff. `by artifact list`,
  `by artifact get ID --out PATH` and `by artifact share ID --to BRANCH`
  reach artifacts your grant permits.
- `by scratch list`, `by scratch lock NAME` and `by scratch unlock NAME`
  access granted scratch areas and parent bindings.
- `by ask "<question>" [--wait SECS]` asks your parent, or the person if you
  have none, optionally waiting. `by report "<text>"` reports progress;
  `by escalate "<text>"` raises a problem; `by inbox` lists your messages.
- If `by inspect` shows a `result_contract`, return JSON matching its schema.
  For `purpose: "deliverable"`, native final JSON after success needs no reporting
  tool. This turn's accepted explicit result takes precedence; prior results cannot
  complete this turn. For report contracts, use `by report "<text>" --result '<JSON>'` or `--result-file PATH`.
  Required reports need a result; schema errors name its invalid path. Your
  parent reads the accepted value as your `result`.
- For `capture`, check files, then write the contract's named manifest.
  The artifact-tools skill's `manifest` records paths, media types and hashes.
  Collection follows workspace transfer; missing or invalid declared files
  fail required deliverables. Keep files beneath the manifest directory
  and share publications through artifact grants.
- `by inbox` may hold a frozen handoff: copied result, source version, optional
  candidate commit and selected artifacts. It arrives next turn without
  interrupting or restarting native work or starting a turn. Review its claims;
  JSON is no evidence.
- `by report-status <state> "<one line>" [--link URL]` shows your claim beside
  Branchyard's derived status to your parent and the person watching.
  `<state>` is `working`, `idle`, `blocked`, `complete` or `failure`;
  `blocked` and `failure` also reach your parent's inbox. To send a message,
  use `by report`.

Finish native work before answering. Branchyard snapshots edits at turn end;
edits are your result without a commit, even if Git or a shell is refused.
Deliverables may have no Git changes. Your parent sets policy.
Report refusals; never evade them.

`by skills ls` lists built-ins and repository `.agents/skills/` fixed at creation,
staged or withheld with reasons. `by skills trust` is refused here;
repository trust needs a person outside any branch. The
`branchyard` MCP server and Python module expose the same operations. Access
beyond your grant, including another branch's work, is refused with a reason.

Operator-only `by mcp manager --url URL --token-file FILE` refuses managed turns.
Its HTTP credential adds no permissions; use branch tools. `BRANCHYARD_MANAGER_TOKEN` is never
forwarded to harnesses. `by serve --app --knowledge` starts a repository-free
operator console with existing storage, quotas and grants.
Its `workspace_action` requires `revision` from `inspect_workspace`. After
`revision_conflict`, inspect again and explicitly propose a new action and key;
never auto-refresh and resubmit. Sessions without routed approvals need
`unapproved_tools` acknowledgment and server permission; default false. Manager
JSON is bounded; oversized results fail explicitly. Keep large documents in artifacts
and answers focused.

## Operator setup

`by substrate setup --image IMAGE --storage URI --plan` previews saved template
setup; `--yes` applies it. `by substrate check --smoke --harness agy` verifies
bridge execution, transfer and cleanup without a model. Both require operators
outside harness sessions. `by up` and `by doctor` check saved
connections and remote credentials: only verified checks among the seven
were established by that run. `by up --plan --smoke` lists Substrate workers;
it creates no actor and calls no model.
`by setup --guided` chooses repository and compute backend;
`by setup --up --json` includes readiness; `--directory PATH` selects it.

`by compute gce --project PROJECT --zone ZONE --name NAME --tunnel-through-iap`
plans a worker; `--apply` provisions the reviewed VM and its dedicated IAP TCP
22 firewall rule. `--internal-ip` omits the external address.
`--service-account EMAIL` attaches the account with `cloud-platform` scope; IAM
roles decide API access, `--scopes` narrows it. Harness sessions and `--remote`
are refused. `--gvisor` installs the pinned gVisor
Docker runtime; Container needs access to the worker's Docker socket.
`by run` accepts `--substrate-api-token-file PATH` for Control authentication and
`--substrate-server-name NAME` for verified forwarded TLS. Egress is
`--substrate-egress deny`, `--substrate-egress unrestricted`, or repeatable
`--substrate-allow-host HOST`. Saved `[substrate]` avoids repeated connection
flags; see its setup docs for image and pool prerequisites.
For GKE, `tools/substrate_gke.py --atenet-dataplane=agentgateway` with the target
options pins a compatible gateway before installing from a private source export;
Envoy stays the default.

## Native sandboxes

An operator uses `by run "TASK" --provider container --image IMAGE@sha256:DIGEST`,
or `--provider agent-sandbox` with `--kube-context CONTEXT --kube-namespace NAMESPACE`.
Images supply the harness. Both take `--cpus`, `--memory` MiB and
`--pass-env` names. `--sandbox-runtime gvisor` is the default;
`--sandbox-runtime runc` is Container-only, `--sandbox-runtime microvm` Agent
Sandbox-only. `--sandbox-network deny` blocks external networking; explicit
`--sandbox-network unrestricted` permits ordinary networking. Container permits
cloud metadata; Agent Sandbox keeps its GKE metadata blocks.
Each turn destroys its allocation. `--keep-sandbox`, snapshots, host mounts and
required hostname allowlists are refused. Server execution requires an approved
preset; clients cannot override its provider.
