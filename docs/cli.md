# Command line

Run `by --help` to list all commands by group, or `by help <command>` (or `by <command> --help`; use `-h` for a compact summary) to view a command's options organized under sections like *Checks and limits*, *Permissions*, *Launch*, *Provisioning*, and *Global options*. Nested subcommands have their own help pages as well, such as `by help graph apply` or `by artifact publish --help`. If you mistype a command or flag, `by` suggests the closest match (for example, `by mrege` suggests `merge`) and exits with status `2`. All other failures exit with the status code matching their error class ([exit status](#exit-status)).

Global options control where commands run and can be placed either before or after the subcommand. Each global option falls back to an environment variable if the flag is not passed, and then to `branchyard.toml` in the repository and `~/.config/branchyard/config.toml` ([setup](setup.md#configuration)). The precedence order is: command-line flags, environment variables, project configuration, and finally user configuration.

| Option | Variable |
|---|---|
| `--remote URL` | `BRANCHYARD_REMOTE` |
| `--token-file FILE` | `BRANCHYARD_TOKEN_FILE` |
| `--repo NAME` | `BRANCHYARD_REPO` |
| `--ca-file FILE` | `BRANCHYARD_CA_FILE` |

Common short flags include `-n` (`--name`), `-b` (`--base`), `-y` (`--yes`), `-f` (`log --follow`), `-o` (`artifact get|export --out`), and `-V` (`--version`). Duration flags accept explicit units such as `--max-minutes 90s`, `--stall-after 2h`, or `watch --interval 250ms` (a bare number uses the flag's default unit), and `--budget-usd` also accepts a leading dollar sign (`$2.50`). Both `--check` and `--command` are tokenized using POSIX shell quoting rules, including `#` comments.

Shell completion scripts and a `man` page are generated from the same CLI definitions, including subcommand topics for `by init` (`project`, `server`, `rig`, `deploy`, `plugin`) and `by config`:

```sh
by completions bash > ~/.local/share/bash-completion/completions/by
by completions zsh > "${fpath[1]}/_by"
by completions fish > ~/.config/fish/completions/by.fish
by completions powershell >> $PROFILE      # or elvish
by man | man -l -
```

`by serve` and `by worker` delegate their remaining arguments to the server's CLI parser, so `by serve --help` prints the server's options and `--repo NAME=PATH` registers a server repository rather than setting the global client flag. If `[serve] config` is set in `branchyard.toml`, `by serve` and `by worker` use it as their default `--config` (`-c FILE`), and `by serve --config FILE --check` validates a server configuration file without starting the listener. The standalone binaries (`branchyard-server`, `branchyard-bridge`, `branchyard-herdr`, `branchyard-mcp`, and `branchyard-qualify`) all provide `--help` flags built with `clap`.

## Exit status

When a command fails, `by` prints what went wrong and what to run next (`by: …` followed by `  next: …` on `stderr`, or a structured JSON envelope `{"error": {"kind", "message", "next", "detail", "exit"}}` on `stdout` when `--json` is passed) and exits with the code for that error class:

- `2`: Invalid command-line arguments or unknown target name.
- `3`: Environment or setup prerequisite missing (for example, not in a repository, harness not installed or logged out, invalid config file, or unreachable server).
- `4`: Blocked by a policy, budget, quota, or capability check before execution started.

The `exit` field in the JSON error envelope always matches the process exit code. The table below is generated from `crates/branchyard-cli/src/errors.rs` (`Exit`, `EXITS`, `KINDS`), where unit tests verify that every engine error (both locally and over HTTP) maps to a single exit code:

<!-- fact:cli.exit_codes -->

| Status | Meaning | `--json` kinds |
|---|---|---|
| 0 | the command did what it was asked | — |
| 1 | the operation ran and failed: a branch failed, a check failed, a merge conflicted, the branch was busy; also a failure not yet given a kind (`unclassified`), and any kind not listed here | `already_merged`, `check_failed`, `check_not_started`, `check_timed_out`, `command_failed`, `conflict`, `dirty_target`, `fenced`, `git`, `harness`, `incomplete`, `internal`, `invalid_answers`, `invalid_candidate`, `invalid_plan`, `io`, `no_candidate`, `no_plan`, `not_running`, `running`, `stale_revision`, `state`, `steer_refused`, `target_moved`, `unclassified`, `would_overwrite` |
| 2 | the command line asks for what cannot be: a bad flag or value (clap's own usage errors too), an unknown branch, harness or repository, a name already taken | `branch_exists`, `invalid_memory`, `invalid_name`, `invalid_request`, `invalid_rig`, `not_found`, `unknown_branch`, `unknown_harness`, `unknown_knowledge`, `unknown_memory`, `unknown_message`, `unknown_operation`, `unknown_repo`, `unknown_trigger`, `usage` |
| 3 | something around the command must change first: not a repository, a harness (or gh, ssh) not installed, a configuration file that does not load, a server that does not answer or refuses the token, a login only you can do | `config`, `harness_unavailable`, `manual`, `not_a_repository`, `remote_protocol`, `remote_unreachable`, `shutting_down`, `tool_unavailable`, `unauthorized` |
| 4 | refused before anything ran: a policy, a delegation envelope, a budget or quota, or a capability the harness lacks | `command_not_allowed`, `delegation_not_allowed`, `denied`, `provider_not_allowed`, `quota_exceeded`, `rate_limited`, `repo_not_allowed`, `scope_required`, `secret_not_allowed`, `unapproved_tools_not_allowed`, `unsupported` |
| 130 | interrupted with Ctrl-C: the shell reports 128 + SIGINT. `by serve` and `by worker` catch it and shut down; `by substrate check --smoke` catches SIGINT and SIGTERM to remove its actor, then reports the cancelled smoke with exit 1 | — |

<!-- /fact -->

`by run`, `by fan`, and `by send` exit with status `1` when a branch (or every branch in a `fan` run) ends in `failed`, after printing what happened. A branch that finishes as `ready`, `no changes`, `budget exceeded`, or `interrupted` does not cause the CLI command itself to fail. If a pre-flight check stops before the harness starts (for example, because the harness binary is missing or logged out), the command exits with status `3` (`harness_unavailable`). Steps that require interactive human action, such as running `by harnesses login` without an attached terminal, also exit with status `3` (`manual`) and return the standard JSON error envelope (along with command-specific fields like `status` and `run`) when `--json` is passed. (Prior to 27 October 2026, all failures other than `clap` usage errors and `manual` exited with status `1`; scripts that check exit codes should check for any non-zero status or match the specific code they expect.) Any bare error message that has not yet been assigned a specific kind is reported as `unclassified` (exit code `1`), and `tools/failure_message_ratchet.json` tracks the remaining sites so the count only goes down.

When called outside a harness session, both local and remote `by send --json` wait for the turn to complete and print a single `Sent` JSON object (`name` and `status`), even if a harness or snapshot error causes the command to exit with status `1` (no second error object is appended). When called from inside a running harness without `--wait`, `by send` acknowledges the queued turn immediately and exits with status `0`.

## Tool permissions at the terminal

When a harness supports permission routing, every tool permission request is sent to Branchyard. Because the Antigravity, Pi, and Amp profiles cannot route tool approval requests through Branchyard, `by` refuses to run them unless you explicitly pass `--allow-unapproved-tools` (see [harness integration](harness-integration.md#implemented-drivers)). For harnesses that do route requests, `--ask` prompts you interactively in the terminal, `--yes` approves every request automatically, and running without either flag (or without a terminal) denies unapproved requests. Every permission decision is recorded in `by log`. These flows are tested end to end against a fake ACP agent, and a full `by run` → `by diff` → `by merge` workflow has been verified live against Claude Code 2.1.283 ([validation](validation.md); see the [live testing checklist](testing-live.md) for remaining coverage). Note that resuming or forking a session in a new worktree may fail on harnesses that scope sessions to a specific working directory (such as Claude Code); when that happens, the branch reports the error explicitly rather than silently starting a fresh session.

## Per-command options

You can view options for any command with `by help <command>`, or read the complete generated [command reference](cli-reference.md) (`by man --markdown`). Individual feature guides also document their relevant flags: [pull requests](pull-requests.md), [triggers](triggers.md), [workspace](workspace.md), [fleet](fleet.md), [plans and goals](plans-and-goals.md), [knowledge](knowledge.md), [map](map.md), [observability](observability.md), [connectors](connectors.md), [effects](effects.md), [model gateway](model-gateway.md), [egress](egress.md), and [setup](setup.md). See [Surfaces](surfaces.md) for a matrix of all operations across every interface and which options are restricted in each context.
