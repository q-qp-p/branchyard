# Memory that becomes skills

Written 7 November 2026. Status: **design; nothing on this page is built.** It is written for the engineers who build it next, with types, fields, commands and routes, and for each slice the hermetic tests and the evidence the skill-discovery eval must give. What exists today is [repository knowledge](knowledge.md): short rules distilled from a finished branch, reviewed by a person, and *prompted* into every matching turn within 1,500 tokens. This page is the rest of memory: what a harness learns in a run, what a team learns across runs, and the consolidation ("dreaming") that turns the record of many runs into durable knowledge that agents **find by search**, through the [skill registry](skill-registry.md), instead of being told every turn.

## The idea in four lines

- A run writes down what happened: the event log already does, and nothing in it is lost.
- A scheduled job reads many runs, proposes memories with provenance to the events they came from, merges repeats and lets stale ones fade.
- A person approves what becomes shared. Never silently.
- Approved memory is published as skills and references into the same registry and index as every other skill, so a turn that needs it finds it with `tools search`, and a turn that does not pays nothing.

The boundary with knowledge is size and shape: **a rule that fits one line and applies to every turn of a scope is a knowledge entry and stays prompted; anything longer, conditional, or procedural is memory and is found.** Consolidation proposes to both: a candidate under 600 characters with repository-wide scope becomes a proposed knowledge entry through the existing store, everything else a memory on this page.

## Today

| Piece | Where | What it does |
|---|---|---|
| Knowledge entries | `crates/branchyard/src/knowledge.rs`, `knowledge` table | `KnowledgeEntry {id, scope, text ≤ 600, source: Person \| Branch{branch, turn, via}, status: Proposed \| Adopted \| Rejected, created_ms, adopted_by, decided_ms, note}`; `KnowledgeBackend` on SQLite and PostgreSQL with one conformance case |
| The extractor | `knowledge.rs` | Deterministic: sends after the first, steers, `by review` comments and `by pr --watch` comments, read from `Activity::Feedback` records (`FeedbackItem {kind: comment \| check, path, line, author, body}`) and, for old logs, the prose |
| The harness distiller | `knowledge.rs`, any `Judge` | A read-only scratch branch, a strict JSON answer of at most five entries, fallback to the extractor, `KnowledgeActivity::Distilled {ids, duplicates, by, fallback, trigger}` |
| Triggers | `docs/triggers.md` | Cron and interval schedules, `deliver` to one branch, prechecks, exactly-once runs keyed per trigger; they start tasks, not internal jobs |
| The record of a turn | `Activity` in `crates/branchyard/src/lib.rs` | `Prompt`, `Harness(Event)`, `Decision`, `Delegation`, `Provisioned`, `Steered`, `Message`, `Checkpoint`, `PullRequest`, `Feedback`, `ConnectorCall`, `TurnCost`, `Status`, `Warning`, plan and goal activities (`PlanActivity`, `GoalActivity {verdict: met, evidence, missing}`), fleet and judge marks |
| Native memory in a private home | `crates/branchyard/src/tooltree.rs` (`link_native`), provisioning | Claude Code's and Codex's own folders exist per branch home and are discarded with it; nothing reads or seeds them |

## 1. What is captured

Nothing is captured that the branch did not already record. Consolidation reads the event log and derives **observations**: small, typed, scrubbed records that name the events they came from. Observations are the raw material; they are not memories and no harness ever sees one.

```rust
pub struct Observation {
    pub id: u64,                       // store-assigned
    pub tenant: String,
    pub repo: String,
    pub branch: String,
    pub turn: u32,
    pub events: Vec<u64>,              // event ids on the branch's log; at least one
    pub kind: ObservationKind,
    pub text: String,                  // scrubbed (section 5); ≤ 4,000 characters
    pub paths: Vec<String>,            // repository paths the observation is about
    pub task_kind: Option<TaskKind>,
    pub harness: String,
    pub model: Option<String>,
    pub person: Option<String>,        // the subject whose correction or decision it was
    pub at_ms: u64,
    pub scrubbed: Vec<ScrubKind>,      // what the scrubber removed, by kind, never the value
}

pub enum ObservationKind {
    Correction,      // a send after the first, a steer, a review or pull-request comment (the extractor's inputs)
    CheckFailed,     // a `--check` or CI check that failed, with the command and the tail of its output
    ToolDenied,      // Activity::Decision with allowed: false, the tool and the rule
    GoalMissing,     // GoalActivity verdict not met: the `missing` list
    JudgeVerdict,    // a judge's reasons for and against a candidate (fleet JudgeMark text)
    PlanRejected,    // PlanActivity rejected, with the reason
    Escalation,      // a Message of class escalation, or report_status blocked/failure
    Outcome,         // the branch's end: merged, failed with reason, budget_exceeded, discarded
    RepeatedCommand, // the same shell command failing twice or more in one turn (harness tool_started + the next event)
    HarnessMemory,   // a line the harness's own memory file gained during the turn (section 6)
    SelfReport,      // `by memory note "..."` run by the harness inside a turn (open decision 5)
}
```

What each kind reads, and what it deliberately leaves out:

| Kind | From | Not from |
|---|---|---|
| `Correction` | The extractor's sources, as data: `Feedback` items of kind `comment` (scoped to `path`), sends and steers after the first | Branchyard's own prompts (plan mode, goal follow-ups, wakes), continuation replies (`yes`, `continue`, `lgtm`), inbox blocks: the extractor's skip list |
| `CheckFailed` | `PullRequestActivity` check runs that failed; a `--check` whose exit was non-zero, with the last 40 lines of output | A check that passed; output beyond the tail |
| `ToolDenied` | `Decision {allowed: false, tool, source}` | The arguments of the call, which can hold a secret or a file's contents |
| `GoalMissing`, `JudgeVerdict`, `PlanRejected`, `Escalation` | Their activities' texts, as recorded | A judge's scores alone (a number teaches nothing) |
| `Outcome` | The closing `Status` and the turn's `TurnCost` | Cost as a signal of anything beyond "this took N turns and $M" |
| `RepeatedCommand` | `tool_started` for a shell tool whose command text repeats in the same turn, each followed by a failure the harness reported | Commands that succeeded on the first try |
| `HarnessMemory` | The diff of the harness's own memory files in the private home between the turn's start and end (section 6) | Anything outside the harness's memory directory |

An observation is derived **once per turn, at the turn's end**, by the engine that ran it (local or worker), from the events of that turn only, and written with an idempotency key `(branch, turn, kind, blake3(text))`, so a turn recovered and settled twice yields one set. Deriving is deterministic and needs no model; a turn that ends before provisioning derives nothing. `[memory] capture = false` turns it off for a repository; a server's tenant setting can turn it off for a tenant.

## 2. Where it lives

Two tables and a link table, behind one trait, on SQLite and PostgreSQL with one conformance case each, keyed by tenant and repository like every other table:

```rust
pub trait MemoryBackend: Send + Sync {
    fn observe(&self, obs: &[NewObservation]) -> Result<Vec<u64>>;          // idempotent per key
    fn observations_since(&self, scope: &Scope, watermark: u64, limit: usize) -> Result<Vec<Observation>>;
    fn memories(&self, filter: &MemoryFilter) -> Result<Vec<Memory>>;
    fn propose(&self, m: &NewMemory) -> Result<u64>;
    fn decide(&self, id: u64, from: MemoryStatus, to: Decision) -> Result<bool>; // compare-and-swap, as knowledge
    fn merge(&self, into: u64, from: u64) -> Result<()>;                     // provenance union, `seen` summed
    fn forget(&self, id: u64) -> Result<Forgotten>;                           // hard delete, plus what to unpublish
    fn purge_person(&self, tenant: &str, subject: &str) -> Result<Purged>;
}

pub struct Memory {
    pub id: u64,
    pub scope: Scope,                 // exactly one owner
    pub kind: MemoryKind,             // Rule, Procedure, Fact, Pitfall
    pub title: String,                // ≤ 80 characters, one line
    pub body: String,                 // Markdown, ≤ 8,000 characters
    pub paths: Vec<String>,           // globs it applies to; empty for the whole repository
    pub task_kinds: Vec<TaskKind>,
    pub harnesses: Vec<String>,       // empty for all; a harness-scoped memory names one
    pub status: MemoryStatus,         // Proposed, Approved, Rejected, Faded, Forgotten
    pub provenance: Vec<u64>,         // observation ids; never empty
    pub seen: u32,                    // how many consolidations found evidence for it
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub strength: f32,                // seen × e^(−age/half_life); recomputed by each consolidation
    pub decided_by: Option<String>,
    pub decided_ms: Option<u64>,
    pub note: Option<String>,         // the distiller's why, or the rejection's reason
    pub published: Option<Published>, // {skill, digest, path} once it is in a bundle
}

pub enum Scope {
    Repo { repo: String },                                  // team memory of one repository
    Team { tenant: String, group: Option<String> },         // across repositories, for a group or the tenant
    Person { tenant: String, subject: String, repo: Option<String> },
    Harness { harness: String, repo: Option<String> },      // what one harness needs to hear (section 6)
}
```

The scope is the owner and the audience at once: a `Person` memory is read only by that person's turns and `by memory` run as that person (an administrator sees that it exists, its scope and its size, not its body, and the read is audited); a `Harness` memory is given only to turns of that harness; `Repo` and `Team` are the shared memories, and are the only ones that need a reviewer other than the subject. Rows outlive branches, as knowledge rows do, and are deleted with the repository's store.

## 3. Consolidation: dreaming on a schedule

A **consolidation** is an internal job, not a task: no worktree, no harness turn of its own, run by the trigger dispatcher of a `by serve` or `by worker` process, exactly once per schedule time by the same claim that makes a schedule's task run once ([exactly once](triggers.md#exactly-once)). Locally, `by memory consolidate` runs one now.

```toml
[memory]
capture = true                                   # derive observations at each turn's end (default true)
consolidate = "0 3 * * *"                        # a cron, in the server's zone; "" for only `by memory consolidate`
half_life_days = 30                              # strength decay
fade_below = 0.5                                 # a memory under this strength is faded, not shown
keep_observations_days = 90                      # observations older than this are deleted at the next consolidation
distiller = { harness = "claude-code", model = "small", effort = "low" }   # optional; without it, the deterministic pass only
max_candidates = 20                              # per consolidation, per scope
budget_usd = 1.0                                 # the distiller's turn
```

A consolidation runs these steps for each scope that has new observations since its watermark (`memory_watermarks(scope) → last observation id`):

1. **Gather.** Observations since the watermark, at most 500 per scope per run; older ones wait for the next run, so a backlog is worked off in order.
2. **Cluster, deterministically.** Normalize text (case, whitespace, numbers to `#`), group by `(kind, normalized text)`; then merge groups whose BM25 similarity over text and paths passes a threshold (the ranker of `tools.py`, which the engine ships, run here over observations). Each cluster carries its observation ids and the paths, task kinds and harnesses it spans.
3. **Match to existing memories.** A cluster whose text matches an existing memory (any status) the same way is **evidence**, not a new candidate: `seen` rises, `last_seen_ms` moves, `provenance` grows, `strength` is recomputed. A match to a `Rejected` memory stays rejected and only records the evidence, so a rejection sticks as a knowledge rejection does. A match to a `Faded` one revives it to its previous status.
4. **Distill.** Without a distiller, a cluster of two or more observations of kind `Correction`, `CheckFailed` or `RepeatedCommand` becomes one candidate verbatim (title: the first 80 characters; body: the observations, each with its branch and turn), as the extractor proposes corrections as written. With a distiller, a `Judge` runs read-only on a scratch branch, as the knowledge distiller does, with the clusters, the memories already approved for the scope, and the existing knowledge entries, and must answer one JSON object:

   ```json
   {"memories": [{"title": "Run the parser benches with the 1 MB fixture",
                  "body": "…", "kind": "procedure", "paths": ["crates/parser/**"], "task_kinds": ["perf"],
                  "harnesses": [], "confidence": 0.8, "evidence": [412, 418, 455]}],
    "knowledge": [{"text": "Run cargo fmt before you finish.", "path": null, "kind": null, "evidence": [401]}]}
   ```

   Parsed strictly: exactly `memories` and `knowledge`; at most `max_candidates` each; `evidence` non-empty and every id one of the observations given to this run, else the whole answer is refused and the deterministic pass's candidates are used, with the reason recorded. A candidate whose `evidence` the distiller invented is the failure this rule exists for.
5. **Dedup and merge.** A candidate matching an existing memory merges into it (step 3's rule); two candidates matching each other merge; the rest are proposed. A `knowledge` candidate goes to `KnowledgeBackend` as `Proposed` with `source: Branch {via: "consolidation"}` and the observation ids in its `note`; the knowledge store's own duplicate rule applies.
6. **Decay.** Every memory of the scope gets `strength = seen × e^(−(now − last_seen_ms)/half_life)`; an `Approved` or `Proposed` one under `fade_below` becomes `Faded` (hidden from review and unpublished at the next publish, not deleted), and `last_seen_ms` is what revives it. Observations past `keep_observations_days` are deleted unless a memory's provenance names them.
7. **Record.** `Activity::Memory(MemoryActivity::Consolidated {scope, observations, clusters, proposed: [ids], merged, faded, by, fallback, trigger})` on the repository's event feed (there is no branch), shown by `by log --repo` and `by memory runs`; the watermark moves in the same transaction as the proposals, so a crash repeats nothing and loses nothing.

A consolidation never approves and never publishes. Its only outputs are rows in `Proposed` status and evidence on existing rows.

## 4. Review: a person decides what is shared

```text
by memory list [--scope repo|team|mine|harness ID] [--status proposed|approved|rejected|faded|all] [--json]
by memory show ID [--evidence]                       # the memory, and with --evidence each observation and its events
by memory review [--scope …] [--editor EDITOR] [--json]   # walk proposed memories: a approve, r reject, e edit, p promote, s skip, q quit
by memory approve ID... | reject ID... [--reason TEXT]
by memory edit ID [--title T] [--body-file F] [--path GLOB]... [--kind K] [--harness H]...
by memory promote ID --to repo|team                  # a person's memory offered for sharing: becomes a new Proposed row in the target scope
by memory add --scope … --title T --body-file F      # written by a person; approved at once for `mine`, proposed for a shared scope
by memory forget ID                                  # hard delete and unpublish (section 5)
by memory consolidate [--scope …] [--deterministic]  # run one now
by memory runs                                       # consolidations, with counts
by memory export [--scope …] [--out DIR]             # the approved memories as the skill bundle the publisher would write
```

Who may approve:

| Scope | Approver | Default |
|---|---|---|
| `Person` | The subject, for their own memories | Self-approval; a person's own memory never needs anyone else |
| `Harness` with a `repo` | Anyone who may adopt the repository's knowledge (locally `$USER`; on a server a `run` token of the repository) | The same rule as `by knowledge adopt` |
| `Repo` | The same | The same; `[memory] approvers = ["group:maintainers"]` narrows it on a server |
| `Team` | An `admin` of the tenant, or a group the setting names | Narrow by default: a team memory reaches every repository |

Rules that do not bend: a proposed memory is never published and never given to any turn; `by memory` refuses to approve, reject, edit, promote or forget inside a harness (`BRANCHYARD_BRANCH` set), the rule `by knowledge` has; a `promote` is a new proposal in the target scope and keeps the person's original as it was; who decided, and when, is on the row; `review` shows each memory with the count of its evidence and the branches it came from, so the reviewer sees what the model saw. The console's **Decide** view gains a Memory section with the same actions, reading `GET /v1/repos/{repo}/memory?status=proposed`.

## 5. Publication: memory becomes skills

Approved memories are compiled into **skill bundles** and published to the [registry](skill-registry.md) under source `memory`, so they are indexed, searched, mounted and materialized exactly as every other skill, and recorded in `provisioned.skills` and `SkillActivity` like any other.

**One bundle per (scope, topic).** The publisher groups a scope's approved memories by topic (a `paths` prefix, else a `task_kind`, else `general`) and writes:

```text
memory-<repo-slug>-<topic>/            e.g. memory-branchyard-parser, memory-branchyard-general
  SKILL.md                             frontmatter: name, description (generated: "What this team learned about <topic> in <repo>:
                                       N procedures, M pitfalls; found by search, approved by people"), keywords from the memories' titles
                                       body: one line per memory, `- <title> (references/m<id>.md)`, grouped by kind; ≤ the skill budget
  references/m<id>.md                  the memory's body, then a footer: `Approved by <who> on <date>. Evidence: <repo> <branch> turn <n> (<kind>), …`
  memory.json                          {"schema": "branchyard.memory-bundle/v1", "scope", "memories": [{id, title, kind, paths, task_kinds, harnesses, strength}]}
```

The bundle's digest is the registry's identity; a new consolidation that changes the approved set publishes a new digest and the lockfile's `previous` keeps the old one, so a memory that was wrong is rolled back with `by skills rollback memory-branchyard-parser --previous` while `by memory forget` removes it for good. A `Person` scope publishes `memory-mine-<repo-slug>-<topic>` to that person's user library and nowhere else; a `Harness` scope publishes `memory-<harness>-<repo-slug>` and the registry marks the entry `harnesses: [claude-code]`, which the engine honours by mounting it only in that harness's turns. A `Team` scope publishes to the tenant's registry with the group's `groups`.

**Found, not prompted.** No memory is added to a turn's instructions. The turn's `INDEX.md` lists a memory skill as it lists any skill (chosen, or in the "N more" count), `tools search` ranks its references like any reference, and the `SkillActivity` events record which memory a turn opened, which is how the eval and `by memory usage` learn whether a memory earned its place. The only prompted memory is what consolidation proposed to the **knowledge** store and a person adopted: the one-line rules, within knowledge's budget, as today.

**Publication is part of approval**, not a separate job: `approve` marks the row and queues a republish of its scope's bundles, which the dispatcher performs within a minute (or `by memory publish` performs now), so an approved memory is findable at the next turn. The republish raises the tenant's policy version, as any publish does, so sessions recheck at their next claim.

**Privacy, scrubbing, tenancy, deletion.**

- *Scrubbing happens before storage*, at step 1, in the engine, with one scrubber shared with the driver fixtures' redaction (`crates/branchyard-harness`'s recording redaction is the precedent): secret shapes (`AKIA…`, `ghp_…`, `sk-…`, `xox…`, JWTs, PEM blocks, `Bearer …`, `password=`, 32-plus-character hex or base64 runs after a key-like word), URLs cut at `?`, email addresses outside the repository's committers replaced by `<email>`, and anything matching the names of the turn's secrets (`Provisioning` names them; the values are never in the log, but a harness may have printed one). Each removal is counted in `scrubbed` by kind. An observation that is more than a quarter scrubbed is dropped, with a warning event. The distiller sees only scrubbed text; a published reference carries only what the memory's body says, which a person read.
- *Tenancy*: every row carries `tenant` and `repo`; a `Team` memory names its tenant; a route answers only the caller's tenant, as policy routes do, and a `Person` scope's body is readable by its subject alone.
- *Deletion*: `forget` deletes the memory row, drops it from its bundle, publishes the new digest and marks the old one `yanked`; the observations it named are kept until `keep_observations_days` unless `--with-evidence`. `by memory purge --person SUBJECT` deletes a person's `Person` memories and every observation whose `person` is the subject, then republishes any shared bundle whose provenance named them (the memories stay, their footers lose the entries), for erasure requests. Both record a `MemoryActivity::Forgotten` on the repository feed without the text.
- *A harness never writes memory*: observations are derived by the engine from the log; `SelfReport` (open decision 5) would be the one exception, and it is an observation, never a memory.

## 6. Native memory: Claude Code and Codex

Both harnesses have memory of their own; this design neither replaces it nor ignores it. Written from their documentation; nothing here was run live, and the paths must be confirmed against the installed versions before slice 5.

| | Claude Code | Codex |
|---|---|---|
| Instructions it reads | `CLAUDE.md` at the repository root and in parents and subdirectories, `.claude/rules/*.md`, the user's `~/.claude/CLAUDE.md`; a line beginning `#` in a session adds to one of them | `AGENTS.md` at the root and nested, `~/.codex/AGENTS.md` |
| Memory it writes itself | An auto-memory directory per project under `~/.claude/projects/<slug>/memory/` (a `MEMORY.md` index and topic files), which the model updates during sessions | Per-user state under `~/.codex/`; whether a given version keeps model-written memory, and where, is version-dependent and not documented in one stable place |
| In a Branchyard private home | Both are inside the branch's private `HOME` and are created fresh per branch, discarded with it; `--isolated` leaves the user's `~/.claude` out by design ([harness integration](harness-integration.md)) | The same |
| Skills it loads itself | `~/.claude/skills/<name>/SKILL.md` (frontmatter first), which `link_native` already fills | `~/.codex/skills/`, the same |

**Sync, both ways, through what exists:**

- *Import.* At a turn's end the engine diffs the harness's own memory directory in the private home against the turn's start (Claude Code's `~/.claude/projects/*/memory/`; Codex's, once a path is confirmed) and derives `HarnessMemory` observations, one per added or changed paragraph, scope `Harness {harness, repo}`. What the harness wrote for itself is thereby offered to consolidation, scrubbed, and reaches other turns only once a person approves it. A harness's note is a claim, not a fact, and is labelled so in the reference's footer (`Written by claude-code in <branch> turn <n>; approved by …`).
- *Export, knowledge-sized.* Approved `Harness`-scope memories under 600 characters go, as now, into the managed instructions block (the session channel for Claude Code and Codex, `GEMINI.md` for Antigravity), inside knowledge's budget and after knowledge's entries. Nothing new is written to `CLAUDE.md` or `AGENTS.md` in the worktree: those are the repository's files, and an agent's edit to them is a candidate change in its diff, not a memory.
- *Export, the rest.* `memory-<harness>-<repo>` is a skill, linked by `link_native` into `~/.claude/skills` or `~/.codex/skills`, so the harness's own loader sees its frontmatter and its model opens it when it decides to, which is how both harnesses are built to learn from a skill.
- *Seeding the auto-memory directory* with approved memories (writing `MEMORY.md` in the private home before a turn) is **not** proposed: it would prompt what this design says should be found, and Claude Code's format is its own. Open decision 4.

## 7. Surfaces

| Operation | SDK | `by` | `by --remote` | HTTP | Console |
|---|---|---|---|---|---|
| List, show, evidence | `Yard::memories`, `memory` | `memory list`, `show` | yes | `GET /v1/repos/{repo}/memory[?scope=&status=]`, `…/memory/{id}`, `…/memory/{id}/evidence` | Decide view, Memory section |
| Approve, reject, edit, promote | `approve_memory`, `reject_memory`, `edit_memory`, `promote_memory` | `memory approve`, `reject`, `edit`, `promote`, `review` | yes, as the token's name | `POST …/memory/{id}/approve`, `…/reject`, `…/edit`, `…/promote` | yes |
| Add | `add_memory` | `memory add` | yes | `POST …/memory` | no |
| Forget, purge | `forget_memory`, `purge_person_memory` | `memory forget`, `purge` | yes (`admin` for purge) | `DELETE …/memory/{id}`, `POST /v1/admin/memory/purge` | forget |
| Consolidate, runs | `Yard::consolidate(scope, &dyn Judge)` | `memory consolidate`, `runs` | yes (the deterministic pass only, as `knowledge distill` is) | `POST …/memory/consolidate`, `GET …/memory/runs` | runs |
| Publish, export | `publish_memory` | `memory publish`, `export` | yes | `POST …/memory/publish` | no |
| Settings | `Yard::use_memory(MemorySettings)` | `[memory]` | the server's | n/a | n/a |

Reading needs `read`; deciding and adding `run`; purging `admin`. Delegation has no memory operations, and no MCP tool is added: a harness proposes nothing except through `SelfReport` if that decision goes that way, and even then by a `by` command, not a tool on the capped list.

## 8. Phased plan

Each slice ships alone and is tested hermetically against the fake ACP agent, in-process stores on SQLite (and PostgreSQL in CI's postgres jobs), fixture event logs, and fake binaries; no model is called. The eval column names what the skill-discovery eval ([skill registry, the eval](skill-registry.md#the-eval)) must show, and what live evidence would show the memory helps once a run with a model is recorded.

| Slice | What ships | Size (days) | Hermetic tests | Evidence |
|---|---|---|---|---|
| **1. Observations** | `Observation`, `ObservationKind`, the derivation at turn end for every kind but `HarnessMemory` and `SelfReport`, the scrubber, the idempotency key, `MemoryBackend::observe` and `observations_since` on both stores, `[memory] capture`, `by memory observations` (hidden; for the tests and the curious) | 5 | `crates/branchyard/src/memory/tests.rs`: each kind from a fixture log, the extractor's skip list honoured, a turn settled twice yielding one set, every scrub pattern on a corpus of fake secrets (no real one; the test fixtures are generated), a quarter-scrubbed observation dropped with its warning; conformance `memory_observations` on both stores | None from the eval yet (nothing is searched). Before and after: `tests/test_knowledge*.py` and `crates/branchyard/tests/knowledge.rs` unchanged |
| **2. Memories and review** | `Memory`, `Scope`, `MemoryStatus`, propose/decide/merge/forget on both stores, `by memory list \| show \| review \| approve \| reject \| edit \| add \| forget`, the routes, the client, approver rules, refusal inside a harness | 5 | `crates/branchyard/tests/memory.rs`: compare-and-swap decisions (two deciders, one wins), a `Person` body hidden from another principal over HTTP, `promote` making a new proposal and leaving the original, `forget` recording no text; `crates/branchyard-cli/tests/memory.rs`: `review` on stdin; `crates/branchyard-server/tests/memory.rs` over HTTP | None yet |
| **3. Consolidation, deterministic** | Clustering, matching, the verbatim candidates, decay and fading, watermarks, the `knowledge` hand-off for short candidates, `MemoryActivity::Consolidated`, `by memory consolidate --deterministic`, `[memory] consolidate` as a dispatcher job with exactly-once claims | 6 | Two consolidations over one fixture log proposing once and recording evidence the second time; a rejected memory gaining evidence and staying rejected; a faded memory revived by new evidence; a short repository-wide candidate landing in the knowledge table as `Proposed`; two dispatchers racing one schedule time (the triggers' test shape); a crash between proposal and watermark replayed without duplicates | None yet |
| **4. Publication into the registry** | The bundle writer, `memory.json`, the per-scope topics, `Person` and `Harness` audience rules in the registry entry, republish on approve and on forget, `by memory publish \| export`, `provisioned.skills` naming memory skills, `by memory usage` from `SkillActivity` | 5 (needs registry slices 1 to 3) | A bundle's digest reproducible from the same approved set; a memory skill mounted for one harness and not another; a `Person` memory skill mounted for its subject only; `forget` republishing without the reference and yanking the old digest; `tools search` over a tree with a memory skill finding its reference by the memory's title; `check_model_surface.py`: a generated SKILL.md within the `skill:memory-*` cap | **Eval, before:** the eval set over a tree without memory skills. **After:** the same set plus `evals/skills/memory/` cases whose queries are corrections from fixture logs and whose expected result is the memory reference consolidated from them: recall@3 ≥ 0.8, and no existing case worse |
| **5. The harness distiller and native import** | The `Judge`-based distiller with strict parsing and fallback, `[memory] distiller`, `HarnessMemory` observations from the private home's memory directory (paths confirmed per harness version, recorded in the compatibility table), knowledge-sized export into the managed block | 6 | The distiller's protocol with the fake agent answering from files (a valid answer used, an invented evidence id refusing the whole answer and falling back, a budget exceeded falling back); a fixture private home whose memory file changed yielding `HarnessMemory` observations and an unchanged one yielding none | **Live, when run:** one dated record under `evals/skills/live/` of a real harness and model over a repository with ten approved memories: for each of ten tasks whose fixture corrections the memories came from, whether the turn's `Searched` found and `Opened` the memory before repeating the mistake, against a control run with the memory skills unmounted. The number that says it helps is corrections per task, before and after, from the `Correction` observations themselves |
| **6. Team scope and purge** | `Scope::Team`, `admin` approval, the group audience, `purge --person`, `keep_observations_days`, the console's Memory section | 4 | A `Team` memory reaching two repositories' turns of the group and none of another's; purge removing a subject's rows and republishing a shared bundle without their footers; the console at 1440 and 390 pixels | Unchanged eval numbers; the live record extended to a second repository |

Documentation moves with each slice: this page's status paragraph, `knowledge.md` (the hand-off), `surfaces.md`, `server.md`, `triggers.md` (the consolidation job), `observability.md` (the `Consolidated` event and any metric), and the registry page.

## Open decisions

1. **Where consolidation runs locally.** On a server it is a dispatcher job. Locally, `by memory consolidate` by hand, or also a step `by run` and `by merge` take when `[memory] consolidate` is set and the last run is older than the cron's period? The second gives a laptop user dreaming without a daemon and makes a merge slower.
2. **Self-approval of `Repo` memories.** Knowledge lets anyone with `run` adopt. Keep that for `Repo` memories (a repository's maintainers already decide what is merged), or require `approvers` on a server by default?
3. **Decay parameters.** A 30-day half-life and `fade_below = 0.5` mean a memory seen once fades in about 30 days unless evidence recurs; a memory approved by a person could be exempt from fading (`approved_decays = false`). Which?
4. **Seeding native memory.** Write approved `Harness` memories into Claude Code's auto-memory directory in the private home before a turn, so the harness treats them as its own? Rejected above in favour of the skill link and the knowledge block; confirm.
5. **`SelfReport`.** Let a harness run `by memory note "…"` inside a turn to leave itself a note, captured as an observation with the branch and turn? It is cheap, and it is also the one path by which a model writes toward memory; everything else is derived from what it did. Recommendation: build it, scope `Harness`, proposed like any observation, never approved by itself.
6. **The scrubber's email rule.** Replace every email outside the repository's committers, or every email? Committers' addresses are already in the git history the agent reads.
7. **Topic grouping.** By `paths` prefix (one bundle per crate or directory), by `task_kind`, or by the distiller's own `topic` field? Path prefixes are deterministic and match knowledge's scopes; the distiller's topics read better. Recommendation: paths, with the distiller allowed to name a topic that the publisher uses only when it names an existing bundle.
