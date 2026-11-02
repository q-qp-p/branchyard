# The durability contract

The rules a durable agent runtime keeps, stated so that more than one implementation can keep them: Branchyard's engine, which orchestrates harnesses it does not write, and Tape (`vamsiramakrishnan/durable-agents`), which journals agents you write. Each rule says what Branchyard does today and where; a rule marked *planned* is not built yet. Why the rules are shared and the code is not is in the [design record](superpowers/specs/2026-11-02-durability-primitives-design.md); how Branchyard implements them is in [durable execution](durability.md).

The model in one sentence: **one append-only journal, with projections of what is true now.** The journal says what happened; the effect ledger, the obligations and liveness say what is true. A projection is never a second source of truth: it is read from the journal and the tables beside it, or it is wrong.

## 1. Turns

| # | Rule | Branchyard |
|---|---|---|
| T1 | A model turn is an effect, never replayed. Recovery never submits a prompt again | `recover.rs`; [recovery](durability.md#recovery) |
| T2 | A turn whose prompt was submitted and whose end was not recorded has an *unknown* outcome, and says so | `End::Lost`; the branch ends `interrupted` |
| T3 | Resuming is re-deciding with what was known, not replaying: the next turn is told what happened, including every effect of the lost turn and what is known of it | `<branchyard-recovered>`, `wake::recovered_note` |
| T4 | Every step records its intent before its effect and its outcome after, keyed by (branch, turn, step); a step with a recorded outcome is not repeated | `steps` table, `begin_step`/`finish_step` |

## 2. Effects

An effect is one call that changes something outside the machine.

```
staged ──promote──▶ begun ──answer──▶ confirmed ──▶ undone | compensated | undo_failed | expired
   │                  │  └──answer───▶ failed
   └──deny──▶ failed  └──answer lost / turn ended──▶ unknown ──lookup──▶ confirmed | failed
```

| # | Rule | Branchyard |
|---|---|---|
| E1 | An effect's entry is written, durably, before the call | the effect proxy writes `begun` and commits before it forwards |
| E2 | The entry's id is the call's idempotency key upstream | `EffectEntry::id` |
| E3 | `unknown` is a state, not an error. An unknown call is settled by asking the upstream (a lookup by key or captured id), never by calling it again | `reconcile.rs`; an entry with no lookup stays `unknown` and is shown |
| E4 | Every effect has a class (`reversible`, `compensable`, `irreversible`, `read`), declared by the operation, never guessed; one that declares nothing is `irreversible` | `EffectClass`; [effects](effects.md#effect-classes) |
| E5 | An undo is recorded on the original entry, with its deadline | `Undo::deadline_ms` |
| E6 | *Planned.* A key may name the decision rather than the call, so a repeat in a later turn reaches the upstream with the same key | design record, step 2 |
| E7 | *Planned.* An operation with no lookup, no key and no compensation is staged behind an approval by default | design record, step 3 |

## 3. Ownership

| # | Rule | Branchyard |
|---|---|---|
| O1 | One owner at a time: a lease with a generation, and every write of the owner checks the generation in the same transaction | `leases`, `Fence`; [leases and fencing](durability.md#leases-and-fencing) |
| O2 | A takeover writes a new generation; exactly one contender wins | `take_over` updates only the observed generation |
| O3 | A fenced-out owner writes nothing more and stops its work | `Error::Fenced`; the heartbeat notices within one beat |
| O4 | Liveness is reported as known, unknown or lost, with when the owner was last heard and until when the lease holds; a reader is never told `running` without it | `BranchInfo::liveness`; [liveness](durability.md#liveness) |
| O5 | *Planned.* A graceful stop seals: the owner ends at a boundary and releases with a checkpoint; only an unsealed owner is fenced and killed | design record, step 4 |
| O6 | Lease expiry is written and judged on one clock, the store's: on a shared database, the database's, so owners on several hosts need no synchronized clocks | `Backend::clock_ms`; [leases and fencing](durability.md#leases-and-fencing) |

## 4. Signals and time

| # | Rule | Branchyard |
|---|---|---|
| S1 | A signal (cancel, steered input, an approval's answer) is a durable row bound to the turn it was meant for, never a flag in memory | `cancels`, `steers`, the asks |
| S2 | A deadline is stored with what it limits | `max_duration` with the lease |
| S3 | A timer is a row with a due time and a revision; a due timer is claimed for a lease, delivered at least once to an idempotent handler, and finished only by the revision claimed | `timers` / `by_timers`; [timers](durability.md#timers); its first user paces effect lookups |

## 5. Obligations

| # | Rule | Branchyard |
|---|---|---|
| B1 | What is still owed is one projection over the durable state: approvals, unknown and staged effects, lost turns and owners, blocked, ready and waiting children, parked parents, undo windows | `Yard::obligations`; [obligations](obligations.md) |
| B2 | An obligation is discharged exactly when its source changes; it is never stored separately | `obligations.rs` stores nothing |
| B3 | Each obligation names what deals with it | `Obligation::next` |

## 6. The record and the log

| # | Rule | Branchyard |
|---|---|---|
| P1 | Every write that changes a branch's status appends its status event in the same transaction, so the record's status is the log's last | `Recorder::finish`, and each status write; `Yard::drift` checks it ([the record and the log](durability.md#the-record-and-the-log)) |

## Conformance scenarios

Each is a scenario either implementation can run against its own store; the Branchyard test that runs it is named.

| Scenario | Rules | Branchyard test |
|---|---|---|
| An owner killed after submitting its prompt: recovered, its processes killed, the prompt received once | T1, T2 | `tests/durable.rs` `a_killed_engine_is_recovered_its_harness_killed_and_nothing_resubmitted` |
| The turn after a lost one is told which calls it made and what is known of each | T3 | `tests/effects.rs` `the_turn_after_a_lost_one_is_told_which_calls_it_made` |
| A recorded step is used, not taken again | T4 | `tests/durable.rs` `a_recorded_snapshot_is_used_when_recovery_finishes_a_turn` |
| A lost answer is unknown, never retried, and settled by the lookup | E1–E3 | `tests/effects.rs` `a_lost_answer_is_unknown_until_the_lookup_settles_it` |
| A call the gateway describes nothing about is irreversible | E4 | `tests/effects.rs` `a_call_the_gateway_describes_nothing_about_is_irreversible` |
| Two owners never drive one branch; a cancel from the other reaches the owner | O1, S1 | `tests/durable.rs` `two_yards_never_drive_one_branch_and_a_cancel_reaches_the_other` |
| A write after losing the lease is refused | O3 | `tests/durable.rs` `an_event_of_a_turn_that_lost_its_lease_is_refused` |
| A lease's expiry is the store's clock plus its time to live, on grant and on renewal | O6 | `conformance.rs` `clock`, both backends |
| A held turn reads live from another owner; a dead owner's turn reads lost before recovery | O4 | `tests/durable.rs` (the two tests above); `liveness.rs` unit tests |
| A timer claimed by one holder is not claimed by another until its lease lapses; a finish naming a stale revision deletes nothing | S3 | `conformance.rs` `timers`, both backends |
| A lookup that cannot answer is asked again only after its backoff, and never again once settled | E3, S3 | `tests/effects.rs` `a_lookup_that_cannot_answer_is_asked_again_only_after_its_backoff` |
| After crashes, cancels, merges, rewinds, forks, parks and wakes, every record agrees with its log | P1 | `common::assert_no_drift` in 13 flow tests |
| What a branch owes appears, and disappears when its source settles | B1, B2 | `tests/effects.rs` `obligations_list_what_a_branch_owes_until_its_source_settles` |
