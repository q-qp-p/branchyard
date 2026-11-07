---
name: pr-feedback-loop
description: "Open a GitHub pull request for a ready branch and loop CI and review feedback back into it, stacking large changes, until merge."
keywords: [pr, ci, stacked, gh]
---
# Candidate, CI, review, merge

For a session that runs Branchyard as the person or meta-harness, not a spawned leaf: `by pr` refuses inside a harness because it pushes with your git and GitHub credentials. It needs `gh` installed and logged in (`gh auth login`).

1. **Candidate.** The branch is `ready` with a candidate. Read it: `by show BRANCH`, `by diff BRANCH`. Run its check yourself with `by check BRANCH`.
2. **Review before pushing.** `by review BRANCH` opens the diff in your editor; put `>>` comments under the lines they are about, save, and every comment goes to the branch as one prompt (`--print` to preview, `--detach` to carry on). The console's review view does the same in a browser. Re-run `by check BRANCH`.
3. **Open the pull request.** `by pr BRANCH` runs the check on the candidate alone, pushes `by/BRANCH` and opens or updates the pull request. `--draft` for early feedback, `--title` to rename.
4. **Close the loop.** `by pr BRANCH --watch --yes` polls every 30 seconds: failed CI (with the failing log tail), reviews and unresolved threads go back into the branch, its next turn is pushed, and addressed threads are answered and resolved. Bound it with `--max-rounds 3`; Ctrl-C loses nothing.
5. **Large change?** Split into a stack: `by run "..." --base PARENT`, then `by pr` for each; `by pr CHILD --watch` rebases the child onto main once the parent merges.
6. **Merge.** Merging is a person's decision: `by show BRANCH` reads `ready to merge`, then `gh pr merge`. Then `by rm BRANCH`.

The watch feeds back CI and reviews, but it never merges and never judges whether a change really addressed a thread.

References: `references/pr-watch.md` (push, watch rounds, thread resolution, merge readiness), `references/stacks.md` (when to stack, conflicts).
