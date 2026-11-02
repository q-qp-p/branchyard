# Troubleshooting

Look up the status or error message you see below for what it means and the command to fix it. If you are not sure where to start, run `by doctor` (which checks your machine and repository and lists what to fix) and `by ls`. The status labels below match `by`'s output; see [concepts](concepts.md) for the complete table of branch states.

## A branch is in a bad state

### `interrupted`

The turn stopped before it finished, either because you cancelled it with `by cancel NAME` or because the engine process running it exited (due to a closed terminal, a killed process, or a reboot). Both the harness session and the worktree are preserved.

- `by log NAME` shows which happened. A cancellation logs ``warning: cancelled with `by cancel` `` (or `cancelled by alice` when cancelled by another user), whereas an unexpected engine exit logs `recovered: its engine (pid N) is no longer running; the prompt had been submitted and the turn's outcome is unknown`.
- `by diff NAME` shows any changes the turn wrote before stopping.
- `by send NAME --retry` resubmits the interrupted prompt, `by send NAME "…"` sends a new follow-up prompt into the same session, and `by rewind NAME --to N` rolls back to checkpoint `N` first (`0` is the base commit).

### `failed`

The turn ended in an error. The specific reason appears after the colon in `by ls` and `by show NAME`: for example, the harness exited unexpectedly or lost its connection (`the turn's outcome is unknown: the harness connection closed before the turn ended`), a proposed plan was rejected, or the harness process could not start.

- `by log NAME` shows the most recent events and the error reported by the harness.
- `by harnesses` checks whether the harness is installed, on `PATH`, and logged in, and `by harnesses login ID` starts its sign-in flow.
- `by send NAME "…"` retries in the same session, or `by rm NAME` removes the branch.
- For Gemini CLI (`could not resume session …: open failed: Authentication required`): Gemini CLI's `session/load` method (used by `by send`) refuses to resume a session until an authentication type is selected in `~/.gemini/settings.json`, even though `session/new` accepts `GEMINI_API_KEY` directly. Branchyard now authenticates once using the `gemini-api-key` method and retries, which writes `security.auth.selectedType: "gemini-api-key"` into `~/.gemini/settings.json` just as Gemini's interactive `/auth` command does. If resuming still fails, the key is missing from the harness environment (`by harnesses` shows `key set (GEMINI_API_KEY)` when present), and the error message reports `authenticate with gemini-api-key failed: …`.
- For Antigravity (`open failed: authentication failed or timed out` within a second): `GEMINI_API_KEY` is set in the environment, but `~/.gemini/antigravity-cli/settings.json` does not select the Gemini provider, causing `agy` to wait for a Google sign-in. `by harnesses` reports `key set, unused`; run `by harnesses login antigravity` to preview the one-line setting change and pass `--yes` to write it (`by run` also catches this before creating a branch).

### `blocked`

A delegated child branch is marked `blocked` when it cannot start because one of its prerequisite branches failed, was interrupted, hit a budget limit, or was removed. `by ls` prints the specific cause (such as `its prerequisite lint failed`), and `by graph show PARENT` displays the dependency graph. To unblock the child, fix the prerequisite branch (using `by send` as shown above) or update the dependency with a graph proposal. Run `by rm NAME` if you want to discard the blocked branch instead.

### `awaiting plan approval`

A planning turn has proposed a plan and is waiting for your decision before making changes. Run `by plan show NAME` to read the plan, `by plan approve NAME` to execute it (`--edit` opens it in your editor first), `by plan reject NAME --reason "…"` to reject it and fail the branch, or `--replan` to ask for a revised plan.

### `over budget`

The branch stopped because it reached a configured budget limit. `by ls` names the limit that was reached: `max_usd (--budget-usd)`, `max_turns (--max-turns)`, or `max_duration (--max-minutes)`. Note that Branchyard relies on the cost reported by the harness, so a harness that does not report cost cannot trigger `max_usd`. Inspect the current changes with `by diff NAME`, and pass a higher ceiling to continue (for example, `by send NAME "…" --max-turns 5`). Running `by send` without raising the limit that was reached will stop immediately again.

### `running`, and nothing seems to happen

- Run `by watch` to monitor live activity across all branches, or `by log NAME -f` to follow a single branch. If a turn produces no harness output for longer than its stall window, `by` marks it `(stalled)` (stall detection is disabled unless you pass `--stall-after`).
- Run `by send NAME --steer "…"` to inject guidance into the running turn without stopping it, or `by cancel NAME` to stop the turn.
- If the `by` engine process died unexpectedly, the branch remains listed as `running` until the next `by` command opens the repository; running `by ls` automatically recovers it to `interrupted` and logs the recovery notice described above, after which you can run `by send NAME --retry`.
- If the run was started with `--detach`, its output is written to `.branchyard/detached/NAME-run-*.log`.

## Messages and other errors

### `the check for <NAME> failed`

`by merge` printed `by: the check for NAME failed (exit N); nothing was merged.`, then `check:` with the command, `output:` with the last lines it printed, and `next: by check NAME (runs the check without merging)`. Nothing moved. The check ran on the exact merge result, so it can fail where the branch's own tests passed. A check that overran shows `check timed out after …`.

- `by compare NAME --check` runs the check on the candidate in a private worktree and shows `failed` or `passed`.
- `by try NAME` applies the branch's changes to your checkout (it must be clean) so you can run the check yourself; `by try --off` takes them back out.
- `by send NAME "The check fails with: … Fix it."` gives the harness the output, then `by merge NAME` again.

### `has scripts you have not trusted`

The full message:

```text
by: /path/to/repo's [workspace] in /path/to/repo/branchyard.toml has scripts you have not trusted, and there is no terminal to ask on; nothing was created. Review it with `by workspace show`, then run `by workspace trust` in the repository
```

A repository's `[workspace]` scripts run as you, so they never run before you say so; on a terminal `by run` asks instead. `by workspace show` prints them; `by workspace trust` records their digest. A change to the scripts asks again (`changed since you trusted it`). `by doctor` shows the same state as `config.workspace_trust`.

### `login not verified`

`by harnesses`, `by doctor` and `by up` show `login not verified` in the LOGIN column and a line such as `ID: login not verified: no ~/.claude/.credentials.json and no ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN or CLAUDE_CODE_OAUTH_TOKEN; run `by harnesses login ID``. Not verified means Branchyard looked for credential files and key variables by name and found none; a login kept somewhere else, such as a keychain, a key helper or a cloud provider other than Vertex AI, looks the same, and `by run --harness ID` still starts the harness. Vertex AI is read for Claude Code (`CLAUDE_CODE_USE_VERTEX`), Gemini CLI (`GOOGLE_GENAI_USE_VERTEXAI=true` or `selectedType` in its settings) OpenCode (`GOOGLE_CLOUD_PROJECT`), Antigravity (`AGY_ADC_AUTH=true`) and Hermes (`VERTEX_PROJECT_ID`), whose details say they sign in with Application Default Credentials, which Branchyard does not check. `logged out` with no qualifier means the harness's own status command said so (Claude Code is asked `claude auth status`), and `by run --harness ID` then refuses before it creates a branch. The other words are `logged in`, `key set (VAR)`, `key stored (VAR)` and `credentials found`.

- If the harness works when you start it yourself, ignore the row. `--auto` routing leaves such a harness out (`ID is likely logged out`); name it with `--harness` instead.
- Otherwise `by harnesses login ID` runs the harness's own sign-in, or stores a key (`printf %s "$KEY" | by harnesses login ID --api-key`), or another provider's (`--provider zai`, [gateway providers](harness-lifecycle.md#through-a-gateway-provider)). `by up --plan` lists what only you can do.
- Codex with a model provider of its own (a `model_provider` and a `[model_providers.NAME]` with an `env_key` in `~/.codex/config.toml`) and no OpenAI login: `codex login status` says `Not logged in`, but Codex runs on the provider. Export the variable `env_key` names and the row reads `key set (VAR)`; if it is exported and the row still says `logged out`, the line under the table says which provider and variable the configuration selects, and `by run --harness codex --no-preflight` starts it anyway. `by harnesses login codex --api-key` would store an OpenAI key that this provider never reads. Detection reads `config.toml` once a minute (`by harnesses --refresh` asks again).

### `the CLI soft-denied a …`

An Antigravity turn on the `antigravity-stream-json` profile (`--harness antigravity-stream-json`; the default `antigravity-acp` asks Branchyard instead and never soft-denies) ended `no changes` with `warning: the CLI soft-denied a write_file (WriteFile): no permissions.allow rule in its settings lets it run`. `agy` decides its tools by its own rules and cannot ask in headless mode, and agy 1.4.7 does not allow file writes by default there (its documentation says it does). With `--isolated --secret GEMINI_API_KEY`, Branchyard writes the rules from `--yes` or `--permissions` into the branch's private home (`by log NAME` shows `permission rules: … (allow: …, write_file(…))`). In your own home it writes nothing: add `"permissions": {"allow": ["write_file(/path/to/repository/.branchyard/worktrees)"]}` to `~/.gemini/antigravity-cli/settings.json`, or use `--isolated`.

### `slow to start`

`by harnesses` shows `slow to start` as the version and `ID: version: slow to start (took >10 s); `by harnesses --refresh` asks again, and BRANCHYARD_HARNESS_TIMEOUT (seconds) sets how long to wait`. Every installed harness is asked for its version at once, with ten seconds for all of them, and a Node-based CLI can take longer than that on its first start after an install or a reboot. A harness that answered before keeps that version, with the note. Run the CLI once yourself, then `by harnesses --refresh`. Without a version the qualification note cannot be matched, but a slow start alone does not stop `by run`.

### `never heard`

`by setup` printed `hooks: by hook answers · never heard: start a new Codex session` under a harness, bare `by` a row `hooks       never heard: Codex (start a new session)`, or `by doctor` a `todo` row `hooks.codex  install current · never heard`. The hooks are installed and `by hook` answers when Branchyard calls it, but no session of the harness has called it since: `by hook` notes each call a harness makes ([heard](harness-hooks.md#heard-did-the-harness-run-it)), and there is none. `last heard 2d ago (session-start), before by setup rewrote the hooks` is the same after a reinstall: the call proved the old files, not these.

- Start a new session of the harness (in any repository; a session already open read its hooks when it started) and send a prompt, then run `by doctor` again: the row reads `heard just now (…)` once the harness ran the hook.
- Still never heard: the harness is not running it. The causes are listed under [hooks are silent](#hooks-are-silent-in-my-harness-session) (a trust prompt, a restart, `by` off the harness's `PATH`), and `by init plugin --status ID` lists the one-time steps its install left to you. `by doctor` says so first when `by` is not on `PATH`.
- A test by hand with JSON on stdin is noted like the harness's own call unless `BRANCHYARD_HOOK_CHECK=1` is set, as in the command there; a note from your own test makes `by doctor` read `heard`.
- Pi, Oh My Pi, OpenCode and Amp share one `generic` note: a call from one of them is heard for all four.

### `cannot reach`

```text
by: cannot reach http://host:8421: Connection refused (os error 111)
  next: start a server with `by serve` on that host, or fix the URL, token or CA file ([remote] url, token_file, ca_file)
by: remote mode needs a token
  next: pass --token-file FILE, or set BRANCHYARD_TOKEN_FILE, or set token_file under [remote] in branchyard.toml (`by serve` writes one at .branchyard/server/token on the server's host)
```

The first comes from `--remote` (or `BRANCHYARD_REMOTE`, or `[remote] url`) when nothing answers; the second when no token file is given. `by doctor` shows the first as `server.reachable` with the same next step, and with `--json` the failures are `{"error": {"kind": "remote_unreachable", …}}` and `{"error": {"kind": "usage", …}}`, each with its `next` list. Check the scheme and port, start `by serve` on the host, and pass `--ca-file` for an https server with a private CA. Without `--remote`, commands run here and need no server.

A `by run`, `fan` or `send` whose server stops while it waits does not end with this. It says `by: cannot reach http://host:8421: Connection refused (os error 111); the server keeps operation op_…, and this waits for it to answer again (Ctrl-C stops watching, not the work)` once, and `by: http://host:8421 answers again` when the server is back, then ends as it would have. Interrupting it leaves the work on the server: `by ls` and `by show NAME` find it. A turn that was running when its server died ends `interrupted`, and `by log NAME` says whether the prompt had reached the harness and whether `by send NAME --retry` runs it ([server](server.md#remote-mode-in-by)).

### `this build has no PostgreSQL support` and `this build has no Microsandbox support`

Both are cargo features that are off by default. `by serve --database` needs a build with `postgres`, and `--provider microsandbox` one with `microsandbox` (it also needs Rust 1.94 or newer and `libcap-ng-dev`). Rebuild from the clone: `cargo install --locked --force --path crates/branchyard-cli --features postgres`, or `--features microsandbox`. [Server](server.md#postgresql) and [providers](providers.md) cover each.

### `is not inside a git repository`, beside a `.git`

`by` takes a directory for a repository the way git does: its `.git` is a git directory (one holding a `HEAD` file) or a file naming one (`gitdir: …`, a linked worktree or a submodule). An empty `.git` directory does not count: a harness run without `HOME` once left one in `/tmp`, and before 27 October 2026 `by` then took `/tmp` for a repository where git did not. `git rev-parse --show-toplevel` shows what git sees; remove a stray empty `.git`, or run `git init` if the directory should be one.

### The shell cannot find `by`

`cargo install` puts `by` in `~/.cargo/bin`. Add that directory to `PATH`, then open a new shell. A harness started from an editor or a desktop launcher may have a shorter `PATH` than your shell; see the hooks entry below.

### Claude Code says it has no Bash or Write tool

Behind `ANTHROPIC_BASE_URL` on another provider's model (GLM-4.6 through Z.ai, Claude Code 2.1.310, 25 October 2026), the model can go looking for its tools instead of using them. `by log BRANCH` shows several `▸ ToolSearch` lines and then a reply such as `No Write or Bash tool is available in this session`, though `Bash`, `Edit`, `Read` and `Write` were in the request (the session transcript under `~/.claude/projects/` lists them): Claude Code's system prompt tells the model that some tools are deferred and must be found with `ToolSearch`, and this model took the empty search result for the whole tool set. The branch still ends `ready` when it finds another way (the first run wrote the file through the `Monitor` tool), but it costs turns, and `branchyard-qualify` fails `permission_denied`, `permission_allowed`, `interrupt_during_permission` and `interrupt_during_tool` (5 of 9; no permission request ever reaches the driver). A [provider login](harness-lifecycle.md#through-a-gateway-provider) (`by harnesses login claude-code --provider zai`) sets it for isolated and sandboxed runs and puts it in the `export` line it prints. Otherwise set `ENABLE_TOOL_SEARCH=false` where Claude Code starts (the environment of `by`, or `env` in `~/.claude/settings.json`; for an `--isolated` run, in the private home's `.claude/settings.json`, where the isolated run that worked had it beside the base URL): the same model then called `Write` first, and qualification passed 9 of 9. One run each way, one model; an Anthropic model was not tried.

## Hooks are silent in my harness session

A harness hook says nothing when `by` has nothing new to say, and nothing when it cannot run. First ask whether the harness ran it at all: `by doctor` has a `hooks.ID` row per harness with hooks installed, `heard 3m ago (session-start)` when a session of it called `by hook`, `never heard` when none did ([`never heard`](#never-heard)). Then test the command by hand, from the repository, with a branch ready or failed (`BRANCHYARD_HOOK_CHECK=1` keeps your own call from being noted as the harness's):

```sh
echo '{"session_id":"s1","cwd":"'"$PWD"'"}' | BRANCHYARD_HOOK_CHECK=1 by hook --harness codex session-start
```

Printing `{"hookSpecificOutput":…"Branchyard, this repository. Tell the person in one line about any question or failure below; act on nothing unless they ask:\n- NAME: ready to merge, 1 file changed: by diff NAME, then by merge NAME"}` means `by` works and the harness is not running the hook. Printing nothing with no branches in the repository is correct. The cause is then one of:

- **`by` is not on the harness's `PATH`.** Every hook is `command -v by >/dev/null 2>&1 && by hook … || true`, so a missing `by` is silent. Check from the harness's own environment, then link `by` into a directory it has (`ln -s ~/.cargo/bin/by ~/.local/bin/by`).
- **The hook is not installed.** `by init plugin ID --dry-run` shows what would be written; `by setup` installs for every harness it finds and changes nothing the second time. Start a new session afterwards; one already open may not have read them.
- **Claude Code has not restarted.** It reads `~/.claude/settings.json` when a session starts; `/hooks` lists the two Branchyard hooks (`2 hooks on 2 events`). A plugin and `by setup` together run them twice: keep one. `claude -p` needs no trust step; the interactive `claude` asks about the folder first.
- **Codex has not trusted it.** It runs no hook until you review it: choose "Trust all and continue" at the start, or run `/hooks` and press `t`. `codex exec` never asks and skips an untrusted hook; `codex exec --dangerously-bypass-hook-trust` runs it for that one run.
- **Gemini CLI is in an untrusted folder.** Its hooks run only in a trusted folder, and `gemini -p` refuses to start in an untrusted one; trust the folder when asked, or set `GEMINI_CLI_TRUST_WORKSPACE=true` for automation.
- **Hermes has not approved it.** It asks once per hook and skips an unapproved one when there is no terminal. Approve it from a terminal first, or run once with `HERMES_ACCEPT_HOOKS=1`; `hermes hooks list` shows `✓ allowed` once it has.
- **DeepSeek Harness's profile has no bridge.** The hook row starts without it (`dsh: warning: 1 entry did not activate`) and says nothing to the session. `by setup` prints the line that adds it to the `acp` and `headless` profiles; `ls ~/.dsh/profiles/*/node_modules/@deepseek-ai` shows which profiles have `dsh-hooks-claude-code`. A prompt that asks the model for the note proves nothing in a resumed session, which still has the first note in its history; read what `by hook` printed instead.

[Harness hooks](harness-hooks.md) has a section per harness with what each one needs.

Run live on 25 October 2026 (Gemini CLI 0.65.0 with a Gemini API key; Antigravity CLI 1.4.7): the `interrupted` messages for `by cancel` and for a `kill -9` of `by` during a tool call, `over budget: max_turns` after `--max-turns 1` and a `by send`, the `failed` text of a Gemini CLI resume and of an Antigravity run without its provider, and Antigravity's soft-denial warning. Not done: the recovery, cancel, over-budget, failed and trust messages of every other harness were reproduced here with the fake agent. `blocked`, `awaiting plan approval` and the hook trust steps of Gemini CLI and Hermes come from the code and from [harness hooks](harness-hooks.md), not from a run on this machine. The `--remote` text is the refused-connection case only.
