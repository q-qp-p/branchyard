# Skill-discovery eval

Does an agent find and use the right skill, connector operation or helper through the mounted tool tree? [Tools: one model](tools-model.md#3-context-a-mounted-tool-tree-searched-not-pasted) says a turn sees one line of pointer, a 4 KB `INDEX.md`, and `tools search|show|schema`; [layered skill indexes](knowledge-skills.md#discover-only-what-is-needed) say the same for the built-in skills. Every claim about that was hermetic. This eval is the yardstick for measuring it with a real model, and for comparing one change to the skills, the index or `find-tools` with the one before.

**Status (7 November 2026): the scenarios, the scorer and the runner are built and tested hermetically; nothing has been measured with a model.** The four things that blocked a trustworthy live run are removed in the product: the event log now carries a target for tool calls, `by run` chooses built-in skills (`--skill`) and the delegation envelope's harnesses (`--helpers`), and the runner generates the binary input files. Evidence: hermetic. `python3 tests/test_skill_eval.py` checks the scenarios against the skill directories, the Anvil mock's `search.json` and the harness catalog; scores hand-written logs in the shape `by log --json` emits, with and without targets; generates every input file twice and compares the bytes; and, with a built `by` and `fake-acp-agent` (`REQUIRE_BINARIES=1`, run by `tools/check.py`), runs three scenarios against the fake agent scripted to search and open a skill, through `by run --skill`, `by log --json` and the scorer, now from real `tool_started` targets and not narrated lines. The drivers' targets are tested against recorded payloads for Claude Code, Codex, ACP (Antigravity's and OpenCode's agents) and Amp, and against payloads written from documentation for Pi (labelled so in the test). Not done: no live run yet, with any harness or model; so no record is under `evals/skills/records/`, no number here is a measurement of an agent, and whether a real harness's log shows enough targets (the blind count, [below](#what-a-log-can-and-cannot-show)) is unchecked. Connector scenarios have not run at all, even against the fake agent: the Anvil mock cannot serve the gateway they need. Antigravity's own stream and Pi report no target the drivers could read before this change (Antigravity sends a tool's parameters on a later step), so they stay blind unless they route permission requests.

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

`evals/skills/scenarios/*.toml`, one file per area, 48 scenarios: documents (16: Word, Excel, PDF, PowerPoint, Google Docs, Sheets and Slides), knowledge (11), design (5), workflows (1), runtime (3), connectors (5, one of them an operation no package exposes), helpers (3) and distractors (4: arithmetic, a text file, a rename, a question about rebase). A scenario:

```toml
[[scenario]]
id = "docx-status-report"          # lower-case words joined by "-"; the log file is ID.log.json
area = "documents"
prompt = "Write up this week's status as a Word document ..."   # never names the skill
files = { "status.txt" = "..." }   # seeded into the scratch repository
skills = ["docx", "pdf", "pptx"]   # the tools chosen for the turn (see below)
connectors = ["github:read"]       # grants, as `by run --connector` takes them
helpers = true                     # the turn is given helpers (`--delegate`)
envelope = ["codex"]               # harnesses the envelope must allow besides the run's own
expect_skills = ["docx"]           # must be opened
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

**Tools chosen for the turn.** `skills` is applied: the runner passes one `--skill` for each ([knowledge skills](knowledge-skills.md#choosing-built-in-skills)), so the tool tree lists those skills and the skills they need and the run's meta says `skills_chosen_applied: true`. Connectors and helpers are applied too (`--connector`, `--delegate=1`, and `--helpers RUN-HARNESS,ENVELOPE...` for a scenario with an `envelope`).

**Input files.** Text inputs are `files`. A binary input is named in `fixtures` (`contract.docx`, `part1.pdf`; 14 files across 13 scenarios) and generated by `run` with the artifact toolkit's libraries (python-docx, openpyxl, python-pptx, reportlab; `plugins/branchyard/skills/artifact-tools/scripts/requirements.lock`), the same bytes every time: one fixed timestamp, OOXML members rewritten in order, reportlab in invariant mode. A scenario whose libraries are missing is skipped with the reason. The tests refuse a prompt that names a file nobody seeds, generates or expects as output. A generated file is small and plain (a three-paragraph contract with "Acme Ltd" four times, a sales sheet of six rows, a forecast whose totals still sum two of three quarters, a five-slide deck, a PDF with form fields), so a check can look inside it. `check_archive = { "sales.xlsx" = "pivotTable" }` is a regular expression over the member names and text of a zip (docx, xlsx, pptx) in the worktree afterwards.

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

So the discovery path for one skill costs about 3.7 KB (index, `find-tools`, the skill's description) before the skill's own 1 to 1.6 KB; the layered path costs 0.7 KB, 1 KB and the skill. A turn with helpers pays the delegate skill's 39 KB whether or not it searches; that is the number a split of that skill should move. Numbers drift as shipped text changes: re-measure with the command, do not edit this table to match.

## Files

- [`tools/skill_eval.py`](../tools/skill_eval.py): `list`, `score`, `run`, `sizes`.
- [`evals/skills/scenarios/`](../evals/skills/scenarios): the scenarios. [`evals/skills/records/`](../evals/skills/records): dated live results (none yet).
- [`tests/test_skill_eval.py`](../tests/test_skill_eval.py), [`tests/fixtures/skill_eval/`](../tests/fixtures/skill_eval): the hermetic tests, the hand-written logs (in the shape of `crates/branchyard-cli/src/json.rs`'s `recorded`) and the fake agent's script.
