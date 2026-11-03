# Concepts

This page explains the core terms you will see in `by ls`, `by show`, and `branchyard.toml`. If you have not run `by` yet, start with [Getting started](getting-started.md).

## What a piece of work is

| Word | Meaning | Where to see it |
|---|---|---|
| **Task** | The overall objective you asked for, along with every attempt made to solve it. Every `by run` creates a task. | `by task ls`, `by task show ID` |
| **Branch** (an **attempt**) | One attempt at a task: a Git branch `by/NAME`, an isolated worktree under `.branchyard/worktrees/NAME`, a harness session, and an event log. "Branch" refers to the Git side of an attempt; "attempt" refers to the task side. | `by ls`, `by show NAME` |
| **Base** | The Git revision the branch started from (`--base`, defaulting to `HEAD`). | `by show NAME` |
| **Turn** | One prompt sent to the harness and all the work the harness performs until it stops. `by run` runs turn 1; `by send` adds a follow-up turn. | `by log NAME` |
| **Checkpoint** | The snapshot commit created at the end of every turn. You can roll a branch back to an earlier checkpoint with `by rewind NAME --to N`, or branch off from one with `by fork NAME --at N`. | `by show NAME` |
| **Candidate** | The net change a branch offers: the diff between its base commit and its latest checkpoint. This is what `by diff` displays and what `by merge` integrates. A branch has no candidate until a turn modifies at least one file. | `by diff NAME` |
| **Check** | A verification command (such as `--check "cargo test"`) that must pass on the exact merged tree before `by merge` updates your target branch. | `by merge NAME` |

## Ways to start work

| Command | Who decides | What starts | When to use it |
|---|---|---|---|
| `by run` | You | One branch on a single harness | You have a specific task to run on one agent |
| `by fan` | You | The same prompt across several harnesses (`NAME-HARNESS`), followed by a comparison table (`--judge` adds an automated evaluation) | You want to compare how different harnesses solve the same problem and merge the best result (`by compare --pick`) |
| `by map` | You | One prompt applied to each item in a list, with one branch per item and structured outputs validated against a JSON schema | You want to run the same analysis or change across many inputs ([map](map.md)) |
| `by spawn`, `by delegate` | An agent inside a running branch | Child branches under the parent branch, each with its own harness, prompt, and budget | You want an agent to break down a complex task into subtasks, integrate child results with `by integrate`, and resume automatically when its children finish ([delegation](delegation.md)) |

You can also run `by fork` to start a new branch from an existing branch's candidate commit and conversation history.

## Harness, profile, provider

- **Harness**: A coding agent CLI you run in a terminal, such as `claude-code`, `codex`, or `gemini-cli`. Select one with `--harness ID`, or run `by harnesses` to see which ones are installed and logged in.
- **Profile**: The protocol driver Branchyard uses to communicate with a harness (such as `claude-code-stream-json`, `claude-code-acp`, or `codex-app-server`). Some harnesses support more than one protocol profile. `--harness` accepts either a harness ID or a profile ID, and `by harnesses --profiles` lists all profiles.
- **Provider**: The execution environment where the harness process runs (`--provider`): `local` (the default), `microsandbox`, `substrate`, or `recipe:NAME`. Note that this is separate from the LLM API provider (Anthropic, OpenAI, and others), which is configured inside the harness or via the `[models]` gateway.

## Modes

Branchyard supports five deployment and isolation levels, ranging from running directly on your laptop to running across a multi-worker server cluster. See [`docs/modes.md`](modes.md) for the configuration setting and `by doctor` check for each mode.

- **local**: The harness runs as your local user account inside a Git worktree, with no extra isolation.
- **isolated**: Passing `--isolated` runs the harness with a scrubbed environment and a private `HOME` directory.
- **sandbox**: Passing `--provider microsandbox`, `substrate`, or `recipe:NAME` runs the harness inside a microVM, remote actor, or external sandbox command.
- **server**: Running `by serve` hosts the engine behind an authenticated HTTP API so clients can connect with `--remote`.
- **postgres**: Running `by serve --database URL` stores server state in PostgreSQL so multiple servers and workers can share state (requires a binary built with the `postgres` Cargo feature).

## Branch states

`by ls` prints the status label shown in the second column below, while `by show NAME --json` reports the snake_case identifier in `{"state": "…"}`.

If a running turn is waiting on a user [approval](effects.md#approvals), `by ls` displays `waiting for approval: …` along with the request details (and the JSON output includes an `awaiting` object with the request ID and timestamp). A branch waiting on an approval stays paused until someone responds and is never marked as stalled. By contrast, an active turn that produces no harness output for longer than its stall window shows `(stalled)` next to `running`.

After every `by run`, `by send`, and `by fork`, the CLI prints `next` lines with suggested commands for the branch's current state. If a branch stops in an error or interrupted state, its worktree and session are preserved until you run `by rm`; see [troubleshooting](troubleshooting.md) for recovery steps.

| State | Shows as | Meaning | Do next |
|---|---|---|---|
| `running` | running | A turn is in progress. | `by watch`; `by log NAME -f`; `by cancel NAME` |
| `waiting` | waiting | A delegated child whose prerequisites have not all settled. It has no worktree and has run no turn. | `by graph show PARENT` |
| `blocked` | blocked: REASON | A child that will not start because a prerequisite failed, was interrupted, hit a limit, is blocked itself or was removed. | Fix or drop the prerequisite; `by graph show PARENT`; `by rm NAME` |
| `ready` | ready | The last turn completed and produced a candidate. | `by diff NAME`, then `by merge NAME` |
| `awaiting_plan_approval` | awaiting plan approval | A planning turn proposed a plan, and nothing changes until it is approved, edited or rejected. | `by plan show NAME`; `by plan approve NAME`; `by plan reject NAME --reason "…"` |
| `waiting_on_children` | waiting on children | A delegating branch whose turn ended while its children still ran. It is parked and starts its next turn by itself when they settle. Not finished. | `by children NAME`; `by wait --all CHILD…` |
| `no_changes` | no changes | The last turn completed without changing any file. | `by send NAME "…"` to ask again; `by rm NAME` |
| `interrupted` | interrupted | The turn stopped before it finished: you cancelled it, or the engine running it stopped. The session is kept. | `by log NAME`; `by send NAME "…"` to continue; `by rm NAME` |
| `budget_exceeded` | over budget: LIMIT | A limit you set (`--budget-usd`, `--max-turns`, `--max-minutes`) stopped it. | `by diff NAME` for what it has; `by send NAME "…" --max-turns N` with a higher limit |
| `failed` | failed: REASON | The turn ended in failure: the harness errored or died, or a plan was rejected. | `by log NAME`; `by send NAME "…"` to try again; `by rm NAME` |
| `merged` | merged into TARGET | The candidate was integrated. The worktree stays until you remove it. | `by rm NAME` |
| `discarded` | discarded: REASON | Set aside on purpose with `by discard`: never integrated, record and cost kept. | `by rm NAME` |

## What a branch needs from you

One function in the engine classifies every branch for the console, `GET …/decisions`, hook lines, the status line and bare `by`. The result is derived each time a branch is listed or looked up and never stored. `by show NAME --json` and `GET …/branches` carry it as `stage`, such as `{"lane": "needs_you", "reason": "approval"}`. The rows are in precedence order, and the first row that matches a branch gives its reason and lane.

<!-- fact:stage.table -->

| Reason | Lane | Condition |
|---|---|---|
| `approval` | `needs_you` | Its running turn is parked on an approval ask. It moves only when you allow or deny it. |
| `question` | `needs_you` | It sent a question or escalation that nobody has answered, and it is running or it asked you. |
| `stalled` | `needs_you` | It is running, and its harness has been silent for its stall window. |
| `parent` | `running` | It is a delegated child that settled without being merged or discarded, and its parent still runs or waits. Its parent decides what comes next. |
| `awaiting_plan_approval` | `needs_you` | Its plan waits for you to approve, edit or reject it. |
| `failed` | `needs_you` | Its turn failed. |
| `interrupted` | `needs_you` | Its turn was cancelled, or the engine running it stopped. |
| `budget_exceeded` | `needs_you` | A limit you set stopped it. |
| `blocked` | `needs_you` | A prerequisite did not succeed, so it will not start until its graph changes. |
| `running` | `running` | A turn is in progress. |
| `waiting_on_children` | `running` | Its turn ended while its children still ran. It starts again on its own when they settle. |
| `waiting` | `running` | It waits for its prerequisites to settle. |
| `ready` | `ready` | Its candidate waits to be merged. |
| `no_changes` | `done` | Its last turn changed no file. |
| `merged` | `done` | Its candidate was integrated. |
| `discarded` | `done` | It was set aside on purpose. |

<!-- /fact -->

## What "qualified" means

Branchyard drives each harness through a protocol profile. Throughout the CLI and documentation (`by harnesses`, `by doctor`, [compatibility](compatibility.md), and [driver qualification](qualification/README.md)), Branchyard distinguishes between having a protocol driver that passes mock tests and having verified that driver against the real harness binary:

| Term | Meaning |
|---|---|
| **live-qualified** | The profile's driver has been run through the nine-scenario qualification suite against the real harness binary, and the report records the pass count, date, and harness version (for example, `9 of 9 pass, 2026-10-16, codex-cli 0.157.1`). `by doctor` displays `qualified 9/9 on 2.1.283`. |
| **unqualified** | A driver exists and passes its protocol tests against mock processes (shown as "not qualified" in the compatibility table), but no live run against the real binary is recorded yet. |
| **drivable** | `by run --harness ID` can launch and control the harness because it has a driver profile. Harnesses without a driver are listed under `by harnesses --all` as "knows about". |
| **inform-only** | Branchyard can install hooks so an interactive session in that harness receives branch status updates, but Branchyard does not have a driver to launch it directly (currently Droid and Kiro). See [harness hooks](harness-hooks.md). |

Qualification tests the protocol driver when launching the harness as a local process. It does not test sandbox isolation, network egress policy, credential injection, or multi-node recovery.

Not done: `drivable` and `inform-only` are this page's names for what the catalog and the hook packaging show; the qualification pages use neither word. The rows for `waiting`, `blocked`, `awaiting_plan_approval` and `waiting_on_children` come from the enum's comments and the delegation, graph and plan pages, not from a live run; the others were run against the fake agent.
