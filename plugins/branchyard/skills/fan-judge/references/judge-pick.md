# Judge and pick

`by judge NAME` (a fan's name, or branches) runs each attempt's check on its exact candidate in a private worktree, then scores 0 to 100: check 60 (none: 40), diff size 20, cost 10, time 10, relative to the other eligible attempts. Failed check, failed turn or no candidate scores 0.

Flags:

- `--deterministic`: no model judge.
- `--harness ID`: a read-only judge harness ranks with reasons on a scratch branch. `--rubric "<text>"` adds criteria (minimal diff, house style, security).
- `--require-judge`: no pick unless a valid model verdict exists; use it when no person reads the diffs.
- `by fan ... --judge` judges as attempts finish, prints the pick, merges nothing.

`by compare --fan NAME --check` is the side-by-side table: status, check, diff stats, cost, time, files only one attempt changed. `by compare --diff A B` shows how two differ. Read the top two diffs yourself; a judge ranking a failed check first is overridden.

Pick:

- `by judge NAME --pick --into main --discard-others --yes` merges the pick after its check passes on the exact merge result. `by compare --pick BRANCH` chooses by hand.
- To review on GitHub, `by pr BRANCH` pushes the winner instead (pr-feedback-loop).
- Good in different ways: pick one, `by send BRANCH "<idea from the other>"`, `by check BRANCH`.
- All failed: `by log BRANCH` on the cheapest, tighten the prompt or check, fan again narrower.

Keep losers until the pick merges, then `by rm BRANCH`. Never `git merge` an attempt.
