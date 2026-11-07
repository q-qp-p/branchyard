# PR watch: push, poll, resolve, merge readiness

`by pr BRANCH`: the branch must be ready (`--allow-not-ready` pushes a failed or limited one); its check runs on the candidate in a temporary worktree (`--no-check` skips; `--allow-failing-check` pushes and says so in the body); the candidate is pushed to `--git-remote` (default origin) as `by/NAME` or `--head`; gh opens the pull request or edits the open one. The body is regenerated each run: task, `Closes #n`, turns, cost, check, diffstat. `--gh-repo OWNER/REPO` when gh cannot tell the repository; `--force` for a remote branch the candidate does not descend from.

`by pr BRANCH --watch --yes`, each poll:

1. pushes a new ready candidate (a failing check becomes feedback, not a push);
2. reads PR state, checks and review threads;
3. collects new items: failed check with the last 80 lines of its Actions log, changes-requested reviews, comments, unresolved thread comments;
4. delivers them as one message, steered into a running turn or starting a new one.

Each item is delivered once (keyed in the log), polls back off to ten times `--interval`. Knobs: `--max-rounds 3` against a branch failing the same check; `--interval 60` for slow CI; `--no-resolve` to leave threads; `--ask` or `--permissions edit-worktree` in place of `--yes`.

After a push the watch replies "Addressed in <commit>" and resolves a thread only when the push changed the thread's file and every comment was delivered. It does not judge whether the change fixed it, and never merges.

It stops on merged or closed, `--max-rounds`, a failed turn or a rebase conflict. A flaky runner: rerun it in GitHub. The same finding twice: `by send BRANCH "<what was missed>"` and read the diff.

Merge readiness: `by show BRANCH --refresh` prints `ready to merge`, `not ready: <blockers>`, `merged`, `closed` or `unknown`. Blockers: stale or failed local check (`by check BRANCH`), failing CI names, unresolved threads, behind or blocked, review pending, draft (`gh pr ready`). Then `gh pr merge` and `by rm BRANCH`.
