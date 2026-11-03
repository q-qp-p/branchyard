# Obligations

A branch can end its turn and still owe something: a connector call whose answer was lost, a staged call nobody has promoted, an undo whose window is closing, a question waiting for you, a turn its engine lost, children it waits for. Each of those is already durable in its own table. `by obligations` reads them all and lists, in one place, what is owed, since when, by when, and the command that deals with each.

> **Status.** Implemented in local mode and tested hermetically: the projection's rules against stand-in effect entries and records (`crates/branchyard/src/obligations.rs`), and an engine test against the mock gateway and the fake ACP agent (`crates/branchyard/tests/effects.rs`): an unknown call and an undo window owed, the window gone past its deadline, a lost turn owed a decision, another branch's obligations kept apart, and the unknown call no longer owed once reconciliation settles it. Not done: `by --remote obligations` and an HTTP route (the command refuses `--remote`), a hook line that tells a session what it owes, and a run with a real harness.

## Why

The treatise this project grew out of keeps three ledgers: decisions (why), effects (what) and obligations (what is still owed). Coding harnesses keep the first, in their transcripts, and a thin version of the second, in their snapshots; none keeps the third. Branchyard keeps the second in the [effect ledger](effects.md) and the facts the third needs in its store, but until now you had to know where to look: `by effects` for an unknown call, `by approvals` for an ask, `by ls` for an interrupted branch, `by children` for a parked parent. An obligation is the question all of those answer: *can this task be called done, and if not, what is left?*

## What is owed

Listed in this order, which puts what waits on a person first, then by branch and age.

| Kind | Owed when | Discharged when | Next |
|---|---|---|---|
| `approval` | an ask waits for an answer; its turn is parked | it is answered | `by approvals allow ID`, `by approvals deny ID` |
| `unknown_effect` | an effect is `unknown`, or `begun` while its branch is not running (its turn ended before its answer) | reconciliation confirms or fails it | `by effects reconcile`, `by effects show ID` |
| `staged_effect` | an effect is `staged`: a draft upstream, or a call held in the outbox | it is promoted or denied | `by effects promote ID`, `by approvals deny --branch B` |
| `lost_turn` | a branch ended `interrupted` because its engine stopped after submitting the prompt ([recovery](durability.md#recovery)) | another turn is sent | `by send B "<prompt>"`, `by send B --retry` |
| `lost_engine` | a branch says `running` and its [liveness](durability.md#liveness) is `lost` | recovery ends the turn (any `by` command recovers it) | `by inspect B` |
| `blocked` | a child cannot start: a prerequisite failed or was interrupted | its graph is changed, or it is discarded | `by graph show PARENT` |
| `ready_child` | a delegated child is `ready`, and was not left to the engine to [land](delegation.md#landing) | its parent integrates or discards it | `by integrate B`, `by discard B` |
| `undo_window` | a confirmed reversible effect has an undo with a deadline still ahead | the deadline passes, or it is undone | `by undo B --plan` |
| `children` | a parent is parked `waiting_on_children` | its children settle and it is woken | `by children B` |
| `prerequisites` | a child is `waiting` for its prerequisites | they settle and it starts | `by graph show PARENT` |

An effect entry outlives its branch, so an unknown call of a removed branch is still owed, under the branch's name. A `begun` entry of a branch that is still running is a call in flight, not an obligation.

## Using it

```sh
by obligations                 # everything owed in this repository
by obligations lead            # lead and every branch delegated below it
by obligations lead --json     # [{kind, branch, about, id?, since_ms?, due_ms?, next}]
```

```text
unknown_effect pay ABCDEFGH: flaky: charge may have happened: its answer was lost, since 01:00
  next: by effects reconcile
  next: by effects show ABCDEFGH
undo_window post: slack: chat_post can be undone until its deadline, until 02:01
  next: by undo post --plan
```

Times are UTC, as `by effects` prints them. `Yard::obligations(branch)` is the same list in Rust; `Yard::obligations_at(branch, now_ms)` evaluates undo deadlines and leases at a given time.

## What it is not

It stores nothing and decides nothing. An obligation exists exactly as long as its source says it does, so there is no second copy to drift, and nothing to recover after a crash. It does not run reconciliation, answer asks or send turns: each `next` is a command you, or a parent harness, choose to run.
