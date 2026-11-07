# Limits, envelopes, stop rules

Flags on `by run`, `by fan`, `by send`:

- `--budget-usd N`: harness's cost estimate, or metered cost on `--model-gateway`. Claude Code is given what remains each turn; others are interrupted at the first usage report past the limit. The call that crosses it is paid.
- `--max-turns N`, `--max-minutes M` (or `90s`, `2h`): work for every harness. Codex, Amp and Antigravity report no priceable cost, so a cost limit alone does not stop them run directly.
- `--stall-after M --stall-action interrupt`: ends silence.
- `--permissions read-only`: reads and searches only. `edit-worktree` also writes the worktree; `--yes` grants everything.
- `--model-gateway claude-sonnet-*`: routes model calls through Branchyard, which meters each call and refuses the next past the limit.

Delegating explorer: `by run "<question>" --delegate 1 --helpers codex,gemini-cli --budget-usd 3`. Children run only on those harnesses, one level deep; each child's `--budget-usd` must fit in what its parent has left, and a live child holds its whole limit until it settles. A parent's limit bounds its whole tree.

Many items: `by map "<prompt {{item}}>" --items F --concurrency 4 --total-usd 5 --budget-usd 0.3`. `--total-usd` stops starting items; `--budget-usd` caps each.

Sizing: start at a third of the answer's worth, read the result, raise on evidence with `by send BRANCH "continue" --budget-usd 2`. A quick sweep costs cents; a multi-file audit on a strong model, dollars.

Stop rules in the prompt: define done (at most five candidates with file and line evidence; yes or no with the deciding code), name what to skip, report unknowns with what would settle them instead of widening the search. Flags: `--goal "<what must be shown>" --goal-rounds 2` sends it back for gaps; `--plan` proposes read-only first, approve with `by plan approve BRANCH`.

Cancel (`by cancel BRANCH`) after two turns with no new finding, a repeated search, or over half spent with under half answered. Narrow with `by send BRANCH "<one question>"` in place of a bigger budget. Stopped at a limit is not failed: `by log BRANCH` holds the findings; start a bounded follow-up with them.
