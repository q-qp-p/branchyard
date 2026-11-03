# Durability primitives: built in Branchyard, shared by contract

Written 2 November 2026. Branchyard's engine already journals every branch operation under a fenced lease and never replays a model turn ([durability](../../durability.md)). Two neighbouring projects push the same idea further in different directions: **Tape** (`vamsiramakrishnan/durable-agents`), a durable-execution substrate for agents you write, whose treatise names the decision, effect and obligation ledgers; and **Durable Actors** (`TerseAI/durable-actors`), stateful serverless actors whose state lives in object storage and that any host can activate. This page records the decision on where their ideas go, what this change built, and what is left, in order. The normative rules are in the [durability contract](../../durability-contract.md).

## The decision

**Build the primitives inside Branchyard. Share the concepts and the test scenarios with Tape, not code. Connect the two later with an adapter, not a dependency.**

Considered and rejected: making Tape a library, or a service, that Branchyard calls for effects, timers or obligations.

| Reason | Detail |
|---|---|
| One transaction is the guarantee | Branchyard's fence check and the write it guards commit in one transaction of one store (`BEGIN IMMEDIATE` on SQLite, `SERIALIZABLE` on PostgreSQL). That is why a fenced-out engine writes nothing. A second journal in another store would need either two-phase commit or the loss of fencing between them, which is the gap both projects exist to close |
| Different layers | Tape journals inside an agent you write: an ADK plugin sees every decision and tool call (`BeginEffect`). Branchyard orchestrates harnesses it does not write (Claude Code, Codex), journals around their turns, and sees effects at the connector gateway. The primitives share names, not shapes |
| Tape is not a library | `tape-server` is a binary crate behind a 51-RPC gRPC contract, shaped around ADK, pinning `rusqlite` 0.32 where Branchyard pins 0.39. Embedding it means redesigning it, and putting a second code base under Branchyard's `--locked --offline` builds and ratchets |
| The machinery is already here | The `Backend` trait, two backends and their conformance suite, and an effect ledger whose states are Tape's. Every gap below extends a table or a step that exists |

What is shared instead: the [durability contract](../../durability-contract.md), written so either project can implement it; conformance scenarios stated as data, each project running them against its own store; and, from Tape, deterministic simulation (madsim) and fail points, which find exactly the lease, takeover and claim races Branchyard's hand-written race tests probe. When a Tape-backed ADK agent runs as a Branchyard harness, an adapter can name Tape's run as the harness session, read Tape's journal to see inside the turn, and take Tape's effect reports into the ledger in the `_meta.effect` shape the gateway already speaks.

Revisit extraction when both projects implement one primitive the same way: then a small crate of pure state-machine logic and validation, no storage and no transport, can be pulled out.

## Built in this change

> **Status.** Hermetic: the fake ACP agent and the mock gateway, on SQLite, on Linux, on 2 November 2026. Not done: a run with a real harness, PostgreSQL for the new tests (the projections read through the same `Backend` and `EffectBackend` traits the conformance suite already runs on both), `by --remote` for obligations.

| Capability | From | What it does | Evidence |
|---|---|---|---|
| The turn after a lost one is told which calls it made | Tape: *the key names the decision, not the call* | The `<branchyard-recovered>` block lists each connector call of the lost turn with what the ledger knows (happened, may have happened, did not happen, staged) and says not to repeat one. A re-sampled model's repeat would carry a new key the upstream cannot match | `tests/effects.rs` `the_turn_after_a_lost_one_is_told_which_calls_it_made` |
| Liveness | Durable Actors' residency (`live`, `dormant`, `unknown` with a known-until) | `BranchInfo::liveness`: `live`, `unknown` once lease renewals stop, `lost` once the lease no longer protects the turn, with `heard_ms` and `known_until_ms`; `by ls` and `by watch` say `running (no heartbeat for 14s)` | `liveness.rs` unit tests; `tests/durable.rs` (live from another yard, lost before recovery); the CLI's render test |
| Obligations | Tape: the obligation ledger, the one no coding harness keeps | `Yard::obligations`, `by obligations [BRANCH]`: approvals, unknown and staged effects, lost turns and engines, blocked, ready and waiting children, parked parents and undo windows, each with the command that deals with it; a projection that stores nothing ([obligations](../../obligations.md)) | `obligations.rs` unit tests; `tests/effects.rs` `obligations_list_what_a_branch_owes_until_its_source_settles`; the CLI's render test |

Considered and not changed: making a repeated `by merge` return the recorded merge, as a Durable Actors request returns the result committed with its state. The server already commits each operation's result with its idempotency binding, so a client that lost the answer reads it by key (`GET /v1/operations?idempotency_key=K`), and the merge step is journaled; `already_merged` is a refusal of a *new* request, part of the CLI, server and SDK error contract, and changing it would break callers for no new guarantee.

## What is left, in order

Each is a change of its own, with its tests and docs.

1. **Durable timers.** One due time per entity in a `timers` table on both backends, claimed with a revision as Durable Actors' alarms are (`claim_due`, `finish`), at-least-once to an idempotent handler. First consumers: per-entry reconciliation backoff for unknown effects (today one 30-second sweep), and waking a branch at a time. Replaces some of the fixed polling the durability page lists.
2. **Decision keys for effects.** Let a call carry a key the agent states for the decision (Tape's `business_key`), journaled before the call, so a repeat in a later turn reaches the upstream with the same key. Needs the gateway contract (Anvil) to carry it.
3. **Safety when a tool is built.** An operation that declares no class, lookup, key or compensation is irreversible with no way to settle an unknown. Tape refuses to build such a tool; Branchyard can stage it behind an approval by default, which the outbox already supports.
4. **Sealed drain.** On SIGTERM or a deploy, end each turn at its next tool-call boundary and seal the branch, so it resumes rather than ending `interrupted`; only an unsealed owner is fenced and killed (Durable Actors' sealed versus unsealed activation).
5. **Leases on the database's clock** for engine turns on PostgreSQL, as operation claims already are, removing the need for synchronized clocks across hosts.
6. **A declared state taxonomy for a workspace.** Persisted (the snapshot), ephemeral (what setup rebuilds, keyed by the prepared environment) and resource (the harness, sandbox and port, reattached on resume), so a rewind across an environment change rebuilds the environment instead of leaving code at one time and dependencies at another.
7. **Portable activation.** Seal a dormant branch's state (refs, session transcript, sandbox snapshot) as one version in object storage so any worker can activate it by a compare-and-swap on its owner record, which lifts the cross-host limits the durability page lists.
8. **Projections derivable from the log.** A conformance check that folding a branch's events reproduces its stored record.

## What was not taken

Durable Actors' Pi adapter replays its whole commit log into memory on every resume; that suits a chat, and its own README notes the cost. Branchyard keeps its rule that a turn is never replayed: resuming a coding agent reproduces the situation and lets it re-decide. Durable Actors keeps no effect ledger (an interleaved call to an outside service is simply awaited), so on external effects Branchyard is the one ahead.
