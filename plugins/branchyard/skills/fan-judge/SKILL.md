---
name: fan-judge
description: "Fan one task across several harnesses, then compare, judge and pick the best attempt within per-attempt budgets."
keywords: [fan, attempts, judge, winner]
---
# Fan out, judge, pick

For a session that runs Branchyard as the person or meta-harness (`BRANCHYARD_BRANCH` unset), not for a spawned leaf. Fan out when you have two or more plausible approaches, or no data on which harness suits the task. Do small or tightly coupled work yourself, and split work that owns different files with `by delegate`, not a fan.

1. **Choose harnesses with data.** `by fleet advise "<task>" --json` shows each candidate's availability, recorded outcomes and cost; `by harnesses` lists what is installed. Take two or three on different vendors, not five.
2. **Fix the contract before the fan.** `by fan` takes the contract and the caps. Code: a `--check` that fails without the change. Answers: `--result-schema-file`, `--result-required`, `--result-deliverable`. Cap every attempt with `--budget-usd` and `--max-minutes`; the fan costs about attempts times the cap.
3. **Run it.** `by fan "<prompt>" --harness claude-code,codex --name NAME --check "cargo test" --budget-usd 2 --max-minutes 30 --yes --json`. Each attempt is a branch named `NAME-HARNESS`.
4. **Compare.** `by compare --fan NAME --check` puts status, check, diff size, cost and time side by side; `by compare --diff A B` shows how two differ. Read the diffs of the top two.
5. **Judge.** `by judge NAME` scores deterministically; add `--harness ID` for a model judge and `--require-judge` when a missing verdict must leave no pick. A verdict is advice: a failed check is never picked.
6. **Pick and merge.** `by judge NAME --pick --into main --discard-others --yes`, or `by compare --pick BRANCH`. For review on GitHub, `by pr BRANCH` instead (see pr-feedback-loop).

Never `git merge` an attempt. A result is a claim: a schema proves shape, not truth.

References: `references/fan-setup.md` (when to fan, harness mix, result contract, budgets), `references/judge-pick.md` (judge flags, scoring, picking, merging).
