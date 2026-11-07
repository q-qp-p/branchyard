# Stacked pull requests

Stack when a change is too big for one diff and parts build on each other: parser, CLI use, docs. Each branch small, with its own check.

```sh
by run "add the parser" --name parser --check "cargo test" --yes
by pr parser
by run "use it in the CLI" --name cli --base parser --check "cargo test" --yes
by pr cli
by show cli
by pr cli --watch --yes
```

`--base BRANCH` (also on `by fan`) starts on the parent's candidate and records the stack. `by pr cli` opens against the parent's PR head, so the diff holds only the child's commits, and lists the stack in the body. The parent needs its PR first; otherwise `by pr` is refused with the next step.

When the parent merges (seen on each `by pr` and each watch poll) the child is rebased onto the branch the parent merged into, rechecked, pushed and retargeted. Squash or rebase merges of the parent do not matter. Merge bottom first.

A conflict is reported, not forced: the rebase aborts, `restack_blocked` names files, `by pr` fails with `conflict` and the watch stops. Fix: `by send cli "resolve the conflict with main"`, or rebase in `by open cli`, then `by pr cli --base main`. A running turn or uncommitted work blocks the rebase.

Limits: GitHub through gh only; a parent merged locally is not followed; there is no stack subcommand, only the `--base` flag of `by run`; a branch started from a PR (`by run --pr N`) can be a parent but cannot be stacked.
