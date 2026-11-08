# Skill-discovery eval

Does an agent find and use the right skill, connector operation or helper through the mounted tool tree? [Tools: one model](tools-model.md#3-context-a-mounted-tool-tree-searched-not-pasted) says a turn sees one line of pointer, a 4 KB `INDEX.md`, and `tools search|show|schema`; [layered skill indexes](knowledge-skills.md#discover-only-what-is-needed) say the same for the built-in skills. Every claim about that was hermetic. This eval is the yardstick for measuring it with a real model, and for comparing one change to the skills, the index or `find-tools` with the one before.

**Status (7 November 2026): the scenarios, the scorer, the runner and a model-free ranking measure ([ranking without a model](#ranking-without-a-model)) are built and tested hermetically; nothing has been measured with a model.** The scorer does not yet credit an opened reference (`tools show SKILL/references/FILE.md`, or a read of the file) as finding a reference scenario's expected id; only `rank` measures those seven scenarios today. The four things that blocked a trustworthy live run are removed in the product: the event log now carries a target for tool calls, `by run` chooses built-in skills (`--skill`) and the delegation envelope's harnesses (`--helpers`), and the runner generates the binary input files. Evidence: hermetic. `python3 tests/test_skill_eval.py` checks the scenarios against the skill directories, the Anvil mock's `search.json` and the harness catalog; scores hand-written logs in the shape `by log --json` emits, with and without targets; generates every input file twice and compares the bytes; and, with a built `by` and `fake-acp-agent` (`REQUIRE_BINARIES=1`, run by `tools/check.py`), runs three scenarios against the fake agent scripted to search and open a skill, through `by run --skill`, `by log --json` and the scorer, now from real `tool_started` targets and not narrated lines. The drivers' targets are tested against recorded payloads for Claude Code, Codex, ACP (Antigravity's and OpenCode's agents) and Amp, and against payloads written from documentation for Pi (labelled so in the test). Not done: no live run yet, with any harness or model; so no record is under `evals/skills/records/`, no number here is a measurement of an agent, and whether a real harness's log shows enough targets (the blind count, [below](#what-a-log-can-and-cannot-show)) is unchecked. Connector scenarios have not run at all, even against the fake agent: the Anvil mock cannot serve the gateway they need. Antigravity's own stream and Pi report no target the drivers could read before this change (Antigravity sends a tool's parameters on a later step), so they stay blind unless they route permission requests.

## Findings that blocked a live run

| Finding | State |
|---|---|
| The log was blind to tool inputs: `tool_started` carried only a tool's name, and inputs appeared only in `permission_requested`. | Resolved. `tool_started` carries an optional `target` ([the event log](#what-a-log-can-and-cannot-show)); the scorer reads it and keeps the blind heuristic only for logs without one. Remaining: Antigravity's stream and Pi (above) and the harness-specific tools that name no file or command (a web search, an image) still log no target. |
| `by run` could not choose built-in skills, so every turn listed all 19. | Resolved. `--skill NAME`, repeatable, on `run`, `fan`, `send`, `fork` (and `map`, `reincarnate`), refused for an unknown name with the known list ([knowledge skills](knowledge-skills.md#choosing-built-in-skills)). The runner passes each scenario's `skills`, and the meta says `skills_chosen_applied: true`. |
| Helper scenarios whose helper is not the run's own harness needed an envelope `by run` could not widen. | Resolved. No configuration or flag widened the envelope's harness list for a top-level run; `--helpers ID,ID` now does ([delegation](delegation.md#the-envelope)). The runner passes the run's own harness and the scenario's `envelope`. |
| 13 scenarios named binary inputs (`contract.docx`, `part1.pdf`) that did not exist, so the agent found the file missing and the scenario had no file check. | Resolved for the discovery question: the runner generates them (below). 8 of the 13 have an output or an edit the scorer can check; the other 5 (`docx-layout-check`, `source-capture-snapshot`, `table-extract-pdf`, `table-extract-docx` and `inspect-layout-preview`) leave their output unspecified in the prompt and are scored on discovery alone. |
| The fake agent's `SH` lines were not tool calls, so the end-to-end test read narrated `$ command` lines. | Resolved. The fake ACP agent reports each `SH` line as an `execute` tool call with the command in its input, and `READ` as a `read` call, so the end-to-end test exercises targets. Narrated lines are still read, marked `narrated`, for harnesses whose agents print them. |
| Connector scenarios cannot run against the Anvil mock. | Remaining. They need `--anvil CMD --bundles DIR` naming an Anvil that serves a gateway. |

## What it measures

For each scenario (a realistic ask that never names the skill) the scorer reads the branch's event log and computes:

| Column | Meaning |
|---|---|
| searched | The agent ran `tools search`, or read an `INDEX.md` (the tree's, a topic's, a skill's, `helpers/INDEX.md`). `searched_command` and `read_index` say which. |
| found | It opened what the scenario expects: a skill's `SKILL.md` (read, `cat`, the native `Skill` tool, or `tools show`), a connector operation (`tools show` or `tools schema CONNECTOR.OP`, `bin/<id> ...` on its CLI line, or a gateway call), a helper (`tools show ID`, or reading `helpers/INDEX.md`). A skill reached only through its `INDEX.md` is routed, not found. |
| wrong opens | Anything opened that was not expected, with `find-tools` itself exempt. `must_not` names the ones that fail the scenario. Operations of another connector are wrong. |
| tokens | From `usage_observed`: per-call observations are added; a `cumulative` total replaces the last one. Unknown stays unknown (`-`), never zero. |
| turn time | First `prompt` to last `turn_ended`, from the log's own clock. |
| success | The last turn completed, everything expected was found, nothing forbidden was opened, and the scenario's checks passed. For a distractor: it completed and opened nothing. `-` when a check could not be run. |

Per harness and overall: discovery recall (expected found over expected), discovery precision (right opens over all opens), searched rate on scenarios that need a tool, needless-search rate on the distractors, tokens per success, median seconds, and the number of blind scenarios.

### What a log can and cannot show

The log has what the harness reported to Branchyard. `tool_started` carries the tool's name and, when the harness reports the call's input, a `target`: `{"kind": "file" | "command" | "mcp" | "skill", "value": ...}`, bounded and scrubbed by the driver ([harness integration](harness-integration.md#tool-call-targets)). A file target is relative to the worktree, `tools/...` for a path in the mounted tool tree, and only the last component of any other absolute path; a command is one line of at most 120 characters with secrets replaced by `[redacted]`; an MCP call is `server.tool`. Never a file's content or a call's full arguments. The scorer reads a file target as a read unless the tool's name says it writes (`Write`, `Edit`, `patch`, ...), a command target as a command, and a `skill` target as the skill opened; so it needs no permission request to see what the agent did. `permission_requested` events still carry full inputs for harnesses that route approvals, and the scorer reads those too; a call refused after its request loses both.

A scenario whose log shows tool calls and no target, no permission input and no narrated command is `blind`: its searched and found are lower bounds, the report says so, and it is counted. A call with no target in a log that has others is not blind (a web search names no file). Drivers that report targets: Claude Code's stream-json, Codex's app-server, every ACP agent that sends the call's `rawInput` or locations (Gemini CLI, Qwen Code, OpenCode, Antigravity over ACP, and the rest), Amp and Pi. The first live run must still check how many scenarios are blind per harness before any rate is believed. Commands the agent narrates in its message as `$ COMMAND` lines are also read, marked `narrated`.

## Scenarios

`evals/skills/scenarios/*.toml`, one file per area, 80 scenarios: documents (16: Word, Excel, PDF, PowerPoint, Google Docs, Sheets and Slides), knowledge (11), design (5), workflows (18: `parallel-deck-large` and, in `workflows.toml`, three realistic asks for each of `fan-judge`, `pr-feedback-loop`, `budgeted-explore` and `issue-pr`, three reference scenarios and two close distractors that expect `xlsx` and `docx` and must not open the workflow skills), runtime (3), connectors (5, one of them an operation no package exposes), helpers (3), references (7: each asks for what one paragraph of a skill's `references/*.md` says, and expects that reference, `SKILL/references/FILE.md`, not the skill), memory (5: each a task a person gives after one correction, with the memory bundle a person wrote from it seeded under `.agents/skills/` or the private `HOME`'s skills folder, expecting the memory's reference, `repository:memory-parser/references/SLUG.md` or `user:memory-reviews/references/SLUG.md`) and distractors (7: arithmetic, a text file, a rename, a question about rebase, and three that borrow the workflow skills' vocabulary: a squash merge, a context-window budget, a bug-report template). A scenario:

```toml
[[scenario]]
id = "docx-status-report"          # lower-case words joined by "-"; the log file is ID.log.json
area = "documents"
prompt = "Write up this week's status as a Word document ..."   # never names the skill
files = { "status.txt" = "..." }   # seeded into the scratch repository; .agents/skills/NAME/SKILL.md seeds a repository skill
home_files = { ".config/branchyard/skills/NAME/SKILL.md" = "..." }   # seeded into the private HOME: a personal skill
skills = ["docx", "pdf", "pptx"]   # the tools chosen for the turn (see below)
mounted = "ready"                  # instead of `skills`: only find-tools chosen, every other ready skill mounted (R2)
connectors = ["github:read"]       # grants, as `by run --connector` takes them
helpers = true                     # the turn is given helpers (`--delegate`)
envelope = ["codex"]               # harnesses the envelope must allow besides the run's own
expect_skills = ["docx"]           # must be opened; "pptx/references/layouts.md" names one reference of a skill
expect_operations = ["github.issues.list"]
expect_helpers = ["codex"]         # a catalog harness id, or "own" for the harness the run uses
must_not = ["pptx"]                # opening one of these fails the scenario
none = true                        # distractor: nothing should be opened
fixtures = ["contract.docx"]       # a binary input the runner generates (see below)
check_files = ["status.docx"]      # exists in the branch's worktree afterwards
check_matches = { "hello.txt" = "hello from" }   # regular expression over the file
check_archive = { "sales.xlsx" = "pivotTable" }  # regular expression over the inside of a docx, xlsx or pptx
check_commands = ["soffice .*flyer"]             # regular expression over the commands in the log
```

`python3 tools/skill_eval.py list` validates every scenario and prints it. The tests refuse a scenario whose expected skill is not one a turn's tool tree lists (the topic indexes' skills; `delegate` and `setup` are not), whose operation is not in the `search.json` the Anvil mock packages, whose helper is not a catalog harness, or that names a skill in its prompt. The mock's `github.drafts.publish` is defined but not approved, so no package exposes it; a scenario uses it to ask whether an agent stretches another operation to fit.

**Tools chosen for the turn.** `skills` is applied: the runner passes one `--skill` for each ([knowledge skills](knowledge-skills.md#choosing-built-in-skills)), so the tool tree lists those skills and the skills they need and the run's meta says `skills_chosen_applied: true`. `mounted = "ready"` with no `skills` passes `--skill find-tools` alone: the index names the search skill and counts every other ready skill, which the engine mounts where linking is free ([registry, R2](skill-registry.md#8-build-plan)); `skills_chosen_applied` is true for it too. Connectors and helpers are applied too (`--connector`, `--delegate=1`, and `--helpers RUN-HARNESS,ENVELOPE...` for a scenario with an `envelope`).

**Seeded skills.** A file under `.agents/skills/NAME/SKILL.md` in `files` seeds the repository skill `repository:NAME`, and one under `.config/branchyard/skills/NAME/SKILL.md` in `home_files` seeds the personal skill `user:NAME` in the run's private `HOME` ([your own skills](knowledge-skills.md#your-own-skills)); `expect_skills` may name either, or one of their references, and the validator checks that the reference is a file the scenario seeds. A repository set is withheld until a person trusts it, so the runner takes the **trust step**, `by skills trust` after the scratch commit and before `by run`, for a scenario that seeds one (`repository_skills_trusted` in the meta; `home_files_seeded` lists the other). A seeded path that is absolute or resolves outside the scratch repository (or the private `HOME`) is refused before anything is written. The memory scenarios seed what `by memory add` wrote and `by memory export` reproduces, so the fixtures are the writer's bytes.

**Input files.** Text inputs are `files`. A binary input is named in `fixtures` (`contract.docx`, `part1.pdf`; 15 files across 16 scenarios, counting the reference scenario `reference-pdf-render-review` and the two workflow distractors that read `sales.xlsx` and `contract.docx`) and generated by `run` with the artifact toolkit's libraries (python-docx, openpyxl, python-pptx, reportlab; `plugins/branchyard/skills/artifact-tools/scripts/requirements.lock`), the same bytes every time: one fixed timestamp, OOXML members rewritten in order, reportlab in invariant mode. A scenario whose libraries are missing is skipped with the reason. The tests refuse a prompt that names a file nobody seeds, generates or expects as output. A generated file is small and plain (a three-paragraph contract with "Acme Ltd" four times, a sales sheet of six rows, a forecast whose totals still sum two of three quarters, a five-slide deck, a PDF with form fields), so a check can look inside it. `check_archive = { "sales.xlsx" = "pivotTable" }` is a regular expression over the member names and text of a zip (docx, xlsx, pptx) in the worktree afterwards.

## Running it

Scoring recorded logs needs nothing:

```sh
python3 tools/skill_eval.py score LOGDIR [--out DIR] [--json]
```

A `LOGDIR` holds `ID.log.json` (the output of `by log BRANCH --json`) and, optionally, `ID.meta.json` (`harness`, `model`, `wall_s`, `checks`), which the runner writes. `--follow --json` output (one object per line) is accepted.

**Live run.** A person starts it; no test does, and it spends model money. In a checkout with a built `by`:

```sh
python3 tools/skill_eval.py run --harness claude-code --model MODEL --out /tmp/skill-eval-claude --record
```

For each scenario it makes a scratch repository and private `HOME` under a temporary directory, seeds the files, commits, runs `by run PROMPT --name ID --harness H --yes --max-minutes 10 [--model M] [--skill S ...] [--delegate=1 [--helpers H,H]] [--connector G]`, saves `by log ID --json`, runs the file checks in the branch's worktree, and finally scores the directory into `report.json` and `report.md`. `--record` also writes `evals/skills/records/DATE-HARNESS[-MODEL].json`. `--scenarios GLOB,GLOB` narrows by id, area or file stem (`--scenarios documents,distractors`). The private `HOME` has no login, so a harness needs its key passed as `*_API_KEY` in the environment (those are copied; nothing else is). Run it three times and look at the spread before trusting a rate; one run per scenario is one sample.

Skipped, and listed in the report with the reason:

- connector scenarios, unless `--anvil CMD --bundles DIR` name an Anvil that can serve the gateway ([connectors](connectors.md)): the mock cannot;
- a scenario whose input files cannot be generated because the toolkit's libraries are not installed.

`--command FAKE --script FILE` runs a fake agent with scripted prompts (`{tools}` and `{repo}` are filled in); such a run is marked SCRIPTED in the report and measures only the pipeline. `tests/fixtures/skill_eval/fake-script.json` is the script the hermetic end-to-end uses.

## Ranking without a model

The ranker is what every change to the skills, their references or `find-tools` moves, so it has a measure that needs no agent:

```sh
python3 tools/skill_eval.py rank [--tree DIR] [--mount own|all] [--chosen NAME,NAME] [--scenarios GLOB] [--plain] [--tools SCRIPT] [--json]
```

For each scenario that expects something, `rank` gives the whole prompt to the shipped `tools` script as the query (`search`, over a tool tree) and reports the rank of each expected id, with the three documents ranked first, and overall the **top-1 and top-3 rates** (the share of expected ids ranked first, or in the first three) and the **mean reciprocal rank**. Without `--tree` it builds the tree `by run` mounts today: every skill `capabilities.json` lists, chosen, with the Anvil mock's packages as connectors, plus, for each scenario, the skills it seeds (`--mount own`, the default: a scenario that seeds a memory bundle is ranked over the tree its repository and `HOME` would mount, and one that seeds nothing over the shipped tree alone; `--mount all` puts every ranked scenario's seeded skills in one tree; a seeded skill is mounted, never chosen, so the tree's `INDEX.md` counts it in its overflow line, and `--chosen NAME,NAME` names the built-in skills the index lists, every one without it); `--tree` points it at a mounted tree (`REPO/.branchyard/capabilities/*/.branchyard/tools`) instead. An expected id with no document in the tree (a helper, since the shipped tree has no `helpers/INDEX.md`) is `absent` and not counted. `--plain` ranks by BM25 alone, without the factors a tool and a chosen tool get. `--tools` ranks with another copy of the script, so the commit before a change and the commit after can be measured over one tree. It is a proxy: a prompt is a longer query than a model writes, and the live `discovery_recall` is the number that counts once a record exists. `tests/test_skill_eval.py` runs it over the shipped tree and holds the top-3 rate of the whole set at 0.95 or better and of the reference scenarios at 0.9 or better.

**Result, 7 November 2026, hermetic** (the shipped skills and the mock's `github` and `linear` packages; no model), at the commit that made references searchable and at its parent (`b0f83eea`, measured with `--tools` on that commit's `tools.py`):

| Measure | Parent | References searchable | `--plain` |
|---|---|---|---|
| documents in the tree | 25 | 32 (7 references) | 32 |
| expected ids ranked (absent: 2 helpers) | 41 | 48 | 48 |
| top-1 | 61% (25) | 65% (31) | 65% (31) |
| top-3 | 98% (40) | 98% (47) | 94% (45) |
| MRR | 0.779 | 0.800 | 0.789 |
| reference scenarios, top-1 / top-3 | not indexed | 6/7, 7/7 | 7/7, 7/7 |

No expected id that was in the top three at the parent left it; one that was a miss already (`claim-verify-numbers`, rank 11) is at 12.

**Result, 7 November 2026, hermetic, memory (M1)**, at the commit that added the five `memory.toml` scenarios, each over the shipped tree plus the bundles it seeds (`--mount own`): the memory scenarios at top-1 4 of 5 and **top-3 5 of 5** (the personal one, `memory-mine-migration-review`, is second behind `github.pulls.merge`); the whole set of 60 at top-1 66% (35 of 53), top-3 98.1% (52) and MRR 0.810; and **no existing scenario moved**, since a scenario that seeds nothing is ranked over the same tree as before (top-3 97.9%, 47 of 48, MRR 0.800, as at `b258cb46`). With every fixture bundle in one tree (`--mount all`: three bundles, 40 documents), the memory scenarios keep their ranks and six existing scenarios rank lower: `docx-layout-check` and `pdf-fill-form` from 1 to 3, `pptx-update-template` and `google-sheets-append` from 2 to 3, `data-clean-dates` from 2 to 6, `claim-verify-numbers` from 12 to 16 (top-3 96.2%, MRR 0.772). The cause is the R1 rule that a reference's body is scored by its content words against the tools' statistics: a memory's body is long and uses everyday words (`report`, `numbers`, `fix`, `check`, `second`), which are rare among the tools' short descriptions, so a memory about reporting bench numbers outranks `pdf` for "fill in the fields and save the result". That is a property of the ranker over any repository that carries chatty references, memory or not, and R2, which mounts every ready skill, is where it is measured and corrected; the writer did not change its output to flatter the number. Not done: a live run; a memory scenario scored from an agent's log; the trust step with a real harness (the fake-agent end-to-end does not seed a repository skill). Without the factor, a reference outranks the expected skill in five scenarios (`calculate-units`, `inspect-layout-preview`, `source-capture-anchors`, `pdf-extract-text`, `artifact-tools-deps`), which is what the factor exists to prevent in a tree where every tool is chosen. Two designs were measured and not shipped: the first draft's field weights (`keywords` ×3 defaulting to the name, the `INDEX.md` line ×2, the first paragraph of `SKILL.md` ×1) lowered the top-1 rate of the existing scenarios from 61% to 41% and MRR from 0.779 to 0.649, because the first paragraph of a built-in skill is a generic command line and the extra weight dilutes the description; and indexing a reference by its title and first paragraph alone found the reference scenarios in the top three 3 of 7 times, because the first paragraph of every shipped reference is its attribution line. What shipped indexes a skill as before (its id, `keywords` when given, description, headings) and a reference by its content words, scored against the tools' statistics. Not done: a live run; a reference scenario scored from an agent's log.

**Result, 7 November 2026, hermetic: R2's ranker, every skill mounted** (the shipped skills, the mock's two packages and the five memory bundles; no model), at the commit that adds it (`python3 tools/skill_eval.py rank` with `--mount own|all`, `--chosen find-tools` and `--plain`) and at its parent (`09bc98c6`, with `--tools` on that commit's `tools.py`). Two things changed in `tools.py` ([registry, section 3](skill-registry.md#3-the-index-and-search)): a reference's name is indexed without English function words and its provenance lines (`Source:`, `License:`, a memory's `Approved by` and `Evidence:`) are not indexed; and the 1.5 factor became a factor on every tool over a reference (`TOOL`), with the factor on a chosen tool over a mounted one (`CHOSEN`) at 1.0 as measured. `--chosen find-tools` is the R2 tree: only the search skill chosen, everything else mounted and counted in the index's overflow line.

| Measure (72 scenarios, 70 ids) | Parent, own | Parent, all | R2, own | R2, all | R2, all, only find-tools chosen | R2, own, only find-tools chosen | R2, `--plain` own / all |
|---|---|---|---|---|---|---|---|
| top-1 | 70% (49) | 66% (46) | 70% (49) | 69% (48) | 69% (48) | 70% (49) | 61% (43) / 57% (40) |
| top-3 | 100% (70) | 95.7% (67) | 100% (70) | 98.6% (69) | 98.6% (69) | 100% (70) | 92.9% (65) / 84.3% (59) |
| MRR | 0.836 | 0.798 | 0.836 | 0.825 | 0.825 | 0.836 | 0.762 / 0.723 |

Over its own tree no scenario moved (every rank identical to the parent's). With every bundle in one tree, five of the six that had dropped are back (`data-clean-dates` 6 to 1, `pdf-fill-form` 3 to 1, `pptx-update-template` and `google-sheets-append` 3 to 2, `workbook-compare-not-fan` 4 to 3) and one stays at 3 (`docx-layout-check`: `table-extract` and the `user:memory-reviews` reference on `second` score within 0.2 of `docx`). With only `find-tools` chosen the ranks are the all-chosen tree's, rank for rank, which is R2's acceptance (within 0.02 top-3) met exactly. Measured and not shipped, each over the parent's fields: scoring a reference against the references' own idf (own average length or the tools') lowered six reference scenarios over their own tree (`reference-pdf-render-review` 1 to 6); against the union corpus lowered three; damping the body field by half lowered six; and each `CHOSEN` above 1.0 (1.1, 1.2, 1.5) lowered one to five reference scenarios over their own tree (`reference-judge-scoring` 1 to 2 at 1.1; `reference-pdf-render-review` 1 to 6 at 1.5) while recovering nothing. The factor on chosen tools alone, R1's design, gives top-3 80.0% (56) with only `find-tools` chosen in the all-mounted tree: a reference outranks its own unchosen skill in 13 scenarios. Not done: a live run; a scenario with `mounted = "ready"` run by an agent (the key is accepted and the runner passes `--skill find-tools`, but no scenario file sets it yet).

**Result, 8 November 2026, hermetic: R2 built, a tree `by skills index` wrote** (no model), at the commit that adds the registry's commands. `by skills index --out DIR --skill find-tools` wrote the tree a turn in this checkout would mount (23 built-in skills, no repository or user skill here, no connectors): `INDEX.md` of 484 bytes naming `find-tools` and counting `- ...22 more`; `by skills index --out DIR` without a choice wrote the all-chosen tree (3,393 bytes, every skill named). `python3 tools/skill_eval.py rank --tree DIR/.branchyard/tools` over each: **identical, rank for rank** (61 ids present, 11 absent as the tree has no connector or seeded bundle: top-1 73.8%, top-3 98.4%, MRR 0.854 on both), which is the slice's acceptance met exactly, as the `--chosen find-tools` measurement over the shipped tree predicted (the ranker sees the same documents, and `CHOSEN` is 1.0). Over the shipped tree with every bundle mounted (`--mount all`) the numbers of the paragraph above hold at this commit: top-3 98.6%, MRR 0.825, identical with only `find-tools` chosen; over each scenario's own tree (`--mount own`) every rank is the parent's (`09bc98c6`, measured with `--tools` on its `tools.py`): top-1 70%, top-3 100%, MRR 0.836. The two scenarios that rank lower with every bundle mounted than over their own tree are `docx-layout-check` (1 to 4: `user:memory-reviews`, its reference on `second`, and `table-extract` score above `docx`) and `inspect-layout-preview` (1 to 2, behind `docx`); the paragraph above said `docx-layout-check` stays at 3, which was the count before the workflow skills joined the tree. The index tree's ranks differ from the shipped tree's for ten ids in both directions (`docx-layout-check` 4 to 2, `pdf-extract-text` 2 to 3) because the corpus differs: no connector operations, so the term statistics are the skills' alone. Not done: a live run; a scenario with `mounted = "ready"` run by an agent; `rank --tree` over a repository that trusts memory bundles, where the index tree would carry them too.

**Result, 7 November 2026, hermetic: the workflow skills** (`fan-judge`, `pr-feedback-loop`, `budgeted-explore`, `issue-pr`; no model), at the commit that added them and at its parent (`b258cb46`, measured with `python3 tools/skill_eval.py rank --json` on each tree):

| Measure | Parent | Workflow skills | `--plain` |
|---|---|---|---|
| documents in the tree | 32 | 43 (4 skills, 7 references) | 43 |
| scenarios that expect something | 50 | 67 (17 new) | 67 |
| expected ids ranked (absent: 2 helpers) | 48 | 65 | 65 |
| top-1 | 65% (31) | 69% (45) | 60% (39) |
| top-3 | 98% (47) | 100% (65) | 91% (59) |
| MRR | 0.800 | 0.833 | 0.753 |
| the 17 new scenarios, top-1 / top-3 | n/a | 11/17, 17/17 | 9/17, 15/17 |

**Result, 7 November 2026, hermetic: memory and the workflow skills together**, at the merge of M1 onto the workflow skills (`python3 tools/skill_eval.py rank --json`, `--mount own`): the 72 scenarios at top-1 70%, **top-3 100%** and MRR 0.836; the memory scenarios at top-1 4 of 5 and top-3 5 of 5, the personal one (`memory-mine-migration-review`) now third, behind `github.pulls.merge` and a workflow skill; every scenario that seeds nothing ranks exactly as over the shipped tree. With every fixture bundle in one tree (`--mount all`): top-3 95.7% (67 of 70), MRR 0.798, for the reason given under memory above. Not done: a live run.

No existing scenario's expected id ranks lower than at the parent. Three things made that true, in the order they were found. First, the ids: `issue-to-pr` and `review-fix-pr` scored above `docx` and `pdf` for any prompt containing "to" or "fix", because an id is weighted four times and the ranker does not drop stop words from it; the skills are named `issue-pr`, `pr-feedback-loop`, `fan-judge` and `budgeted-explore`. Second, the keywords: the first draft's generic words (`check`, `fix`, `branch`, `result`, `review`, `limit`, `stop`, `second`) weighted three times moved eleven existing scenarios down (`docx-layout-check` from 1 to 8, `pdf-extract-text` from 3 to 6, `claim-verify-numbers` from 12 to 30); the shipped keywords are four to eight distinctive words per skill (`fan`, `judge`, `ci`, `jira`, `dollars`). Third, the references: twelve reference files in the first draft each carried generic English the tools' statistics find rare (`check`, `fix`, `changed`, `you`, `what`) and outranked `docx` for `docx-layout-check`; they are seven tighter files in command-first prose. `claim-verify-numbers`, a miss at rank 12 already, fell to 19 under the new documents until `claim-verify` got two keywords (`numbers`, `inference`), which put it at 1; three more keywords on it (`memo`, `traces`, `confirm`) lowered `claim-verify-quote` from 1 to 2 by lengthening its document, so they are not there. The scenarios are in `evals/skills/scenarios/workflows.toml`; `distractors.toml` has three asks that borrow the vocabulary and need no skill. `--plain` ranks six fewer ids in the top three than the default because the 1.5 factor is what keeps a reference below its skill. Not done: a live run, so whether an agent opens these skills when it should, and not when it should not, is unmeasured; the three `none` distractors and the two `must_not` rows are scored only from an agent's log, and `rank` does not look at them.

## Reading a result

- Low **searched** with high **found** means the agent knew the skill without the tree: the tree is not what is being exercised. Compare with the distractors.
- Low **recall** with high **searched** means the index or the descriptions do not route: read the missed scenario's log, then the skill's description and the `tools search` ranking for its prompt.
- Low **precision** is wrong opens: tokens spent on the wrong skill. `must_not` names the confusable pairs (xlsx against google-sheets, pptx against parallel-deck).
- **Tokens per success** is the number to compare across changes. It is only meaningful for harnesses that report usage; `tokens_missing` counts the rest. ACP agents report none, so compare time and opens for them.
- Needless-search rate on distractors is what discovery costs a turn that needed nothing.
- A `blind` count above zero caps everything else.

A change to a skill, the index or `find-tools` is compared by running the same scenarios, same harness, same model before and after, and reading the two records side by side.

## What a turn pays for statically

Measured 7 November 2026 at v0.5.0 (`ab7537cf`) with `python3 tools/skill_eval.py sizes` and `tools/check_model_surface.py`. The first is the cost of a plain Work turn with no helpers; the rest is paid only on the path named.

| Text | Bytes | Paid when |
|---|---|---|
| prompt line pointing at the tree, with the note (`work-turn-no-helpers`, cap 1,000) | 661 chars | every turn |
| `tools/INDEX.md` as mounted, 19 skills, no helpers (cap 4,096) | 2,894 | read first, on every discovery path |
| `tools/INDEX.md` with the `## Helpers` pointer | 3,041 | a turn with helpers |
| `tools/helpers/INDEX.md`, default envelope (cap 2,048) | 185 | a turn with helpers that reads it |
| `find-tools/SKILL.md` | 769 | a turn that opens the skill |
| `find-tools/scripts/tools.py` | 25,493 | never read; run |
| `skills/INDEX.md` and the five topic indexes, shipped | 723 + 594, 1,209, 1,174, 444, 289 | the layered path (root, topic, then the skill) |
| a built-in skill's `SKILL.md` | 769 (`find-tools`) to 4,009 (`setup`), mostly 1,000 to 1,650 | the turn opens it |
| `plugins/branchyard/leaf.md` | 6,845 | a leaf (no helpers) |
| the delegate skill `SKILL.md` | 38,969 | a branch with helpers |
| spawning branch, all its text (cap 44,500) | 42,875 chars | a branch with helpers |
| leaf branch, all its text (cap 9,000) | 8,878 chars | a leaf |
| generic `AGENTS.md` (cap 1,500) | 1,262 chars | a harness with no plugin |

So the discovery path for one skill costs about 3.7 KB (index, `find-tools`, the skill's description) before the skill's own 1 to 1.6 KB; the layered path costs 0.7 KB, 1 KB and the skill. A turn with helpers pays the delegate skill's 39 KB whether or not it searches; that is the number a split of that skill should move (it did, on 7 November 2026: the core is about 7.5 KB, and `sizes` lists each of its 14 reference files as read on demand). Numbers drift as shipped text changes: re-measure with the command, do not edit this table to match.

## Files

- [`tools/skill_eval.py`](../tools/skill_eval.py): `list`, `score`, `run`, `sizes`, `rank`.
- [`evals/skills/scenarios/`](../evals/skills/scenarios): the scenarios (`references.toml` expects references). [`evals/skills/records/`](../evals/skills/records): dated live results (none yet).
- [`tests/test_skill_eval.py`](../tests/test_skill_eval.py), [`tests/fixtures/skill_eval/`](../tests/fixtures/skill_eval): the hermetic tests, the hand-written logs (in the shape of `crates/branchyard-cli/src/json.rs`'s `recorded`) and the fake agent's script.
