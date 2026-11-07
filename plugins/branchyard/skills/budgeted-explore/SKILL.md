---
name: budgeted-explore
description: "Run cost-bounded investigation with spending, turn and time limits, read-only access and stop rules."
keywords: [explore, budget, dollars, spike]
---
# Budgeted exploration

For a session that runs Branchyard as the person or meta-harness, not a spawned leaf. Use it for investigation, research, audits and spikes where the answer matters and the bill must stay bounded.

1. **Set the envelope first.** Every exploring branch gets `--budget-usd`, `--max-minutes` and `--max-turns`, and a read-only policy: `by run "<question>" --permissions read-only --budget-usd 1 --max-minutes 20 --max-turns 15`. A cost limit alone does not stop a harness that reports no cost (Codex, Amp, Antigravity run directly); the turn and time limits, or `--model-gateway`, do.
2. **Ask for a bounded answer.** Say in the prompt what counts as done and when to stop: three candidates, the top five files, a yes or no with evidence. For a machine-readable result add `--result-schema-file F --result-required --result-deliverable`.
3. **Go cheap, then deep.** `--model small` for a first sweep; `by map "<prompt with {{item}}>" --items FILE --concurrency 4 --total-usd 5` runs one branch per item and stops starting items at the total. Escalate only the items that need it.
4. **Watch the spend.** `by watch --once` shows status and cost; `by show BRANCH` prints cost and budget left; `by log BRANCH` has `turn cost` per turn; `by stats` totals; `by fleet stats --kind research` gives the mean cost to plan from.
5. **Stop on a rule, not a feeling.** Cancel with `by cancel BRANCH` when the last two turns added no new finding; send `by send BRANCH "<narrower question>"` instead of widening the budget; stop when the answer is stated with evidence.
6. **Spend more deliberately.** Raise a limit only after reading what was found: `by send BRANCH "continue" --budget-usd 2`.

A cost figure is the harness's estimate, not a bill, and runs low while output streams. Keep a quarter of margin. A branch cut at a limit leaves its notes: read them before restarting.

References: `references/limits.md` (flags, envelopes, stop rules, sizing), `references/reading-cost.md` (cost, usage and telemetry commands).
