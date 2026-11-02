# Usage meters and adopting sessions

Branchyard provides two commands that inspect the local session stores of your installed harnesses without reading any credential files:

- **`by usage`** checks how much of your Claude Code and Codex 5-hour and weekly usage windows have been consumed, allowing `by run`, `by fan`, and the fleet router to warn or skip a login before it hits a rate limit.
- **`by adopt`** lists existing interactive Claude Code and Codex sessions for the current repository and turns a chosen session into a Branchyard branch so you can continue it in an isolated worktree.

Both features are ported from Orca (MIT license, pinned at `2807332`; see [third-party sources](../THIRD_PARTY.md)).

## `by usage`

```text
$ by usage
LOGIN        WINDOW  USED  RESETS                              TOKENS   COST
claude-code  5-hour  50%   in 2h 13m (2026-10-01 16:00 UTC)      2.0k  $0.02
             weekly  -     rolling 7 days                        7.0k  $0.04
codex        5-hour  93%   in 1h 0m (2026-10-01 14:47 UTC)       1.1k  $0.01
             weekly  40%   in 2d 23h (2026-10-04 13:47 UTC)      1.1k  $0.01
```

Running `by usage --json` prints `{"now_ms", "near_percent", "logins": [...]}`. Each entry in `logins` contains `harness`, `account`, `dir`, `found`, `last_seen_ms`, `notes`, and the `five_hour` and `weekly` window objects (`{window, minutes, used_percent, resets_at_ms, tokens, cost_usd, limit_reached, source}`). The `source` field indicates how the percentage was calculated: `rate_limits` (reported directly by the harness), `budget` (computed against your configured `[usage]` token limits), `limit_message` (inferred from a rate-limit error in a transcript), or `none`.

| Login | Session store location | How usage is calculated |
|---|---|---|
| Codex | `$CODEX_HOME` or `~/.codex` (`sessions/**/*.jsonl` and `archived_sessions/**/*.jsonl`) | Codex writes its rate-limit status into `token_count` events (`primary` and `secondary` windows with `used_percent`, `window_minutes`, and `resets_at`). Branchyard classifies the most recent report using Orca's rules: a 300-minute window (±1 minute) is the 5-hour window and a 10,080-minute window is the weekly window (falling back to `primary` as 5-hour and `secondary` as weekly). Windows whose reset timestamp has already passed are shown as `0%`. Token counts and cost are summed from `last_token_usage` (or deltas in `total_token_usage`) within each window. |
| Claude Code | `$CLAUDE_CONFIG_DIR` or `~/.claude` (`projects/**/*.jsonl` and `transcripts/**/*.jsonl`) | Claude Code logs token counts rather than rate-limit percentages. Branchyard deduplicates streaming assistant records by message and request ID and reconstructs the 5-hour window starting from the hour of the first message after the previous window expired. The weekly window covers a rolling 7-day period. To display percentages for Claude Code, configure `[usage] claude_five_hour_tokens` and `claude_weekly_tokens` in `branchyard.toml`. If a transcript records a "usage limit reached" message, the window is marked full until the reset time named in that message. |

Token totals include input, output, and cache-write tokens. Cache-read tokens (billed at one-tenth the base rate) are excluded from the token count column but included in the estimated USD cost. Costs are estimated using Orca's pricing table in [`catalog/pricing.toml`](../catalog/pricing.toml) (generated from the vendored TypeScript via `BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog` and verified on every test run). Transcript files untouched for more than nine days are skipped.

### The guard and the router

```toml
[usage]
guard = "warn"                 # or "refuse", or "off"
near_percent = 90              # a window this full counts as near its limit
skip_over = 95                 # the router passes over a candidate this full
claude_five_hour_tokens = 4000000
claude_weekly_tokens = 40000000
max_running = { codex = 4, "claude-code" = 2 }   # branches at once, per harness

[usage.accounts.work]          # another login to meter, by its configuration directory
harness = "claude-code"
dir = "~/.claude-work"
```

- **Direct runs (`by run` and `by fan`):** Before creating a branch, Branchyard checks the target harness's usage window. When usage is at or above `near_percent` (default `90%`) or a limit message is active, `guard = "warn"` (the default) prints a warning to `stderr` and continues, `guard = "refuse"` aborts before creating the branch (`the codex login has used 93% of its 5-hour window; it resets in 1h 0m; [usage] guard = "refuse" stops new branches on it`), and `guard = "off"` disables the check.
- **Fleet routing (`--auto` or `[fleet]` without `--harness`; see [fleet](fleet.md)):** Any candidate harness whose usage exceeds `skip_over` is skipped automatically, with the reason logged (`by:   not codex: the codex login has used 99% of its 5-hour window; …, over [usage] skip_over = 95`). When `guard = "refuse"`, candidates above `near_percent` are also skipped; when `guard = "warn"`, they remain eligible with a warning.
- **Account mapping:** Harnesses map to usage meters by harness ID (`claude-code` and its profiles use the Claude Code meter; `codex` and its profiles use the Codex meter). The guard and router inspect the default configuration directories; extra directories configured under `[usage.accounts]` appear in `by usage` output.
- **Concurrency limits (`max_running`):** Caps how many branches may run concurrently per harness (or profile) in the repository. Delegated child branches that exceed the cap wait in queue until a slot opens ([placement](delegation.md#placement)).
- **Live dashboard (`by watch`):** The `by watch` header displays a usage summary refreshed once per minute (`usage claude-code 5h 50% wk 7.0k · codex 5h 93% wk 40%`), highlighted in red when a window is full.

**Named account profiles** correspond to separate configuration directories (`CLAUDE_CONFIG_DIR` or `CODEX_HOME`). Because Branchyard does not yet record which custom account directory a branch used, the guard and router check the default login directory.

**Not done.** Orca's other sources of the same numbers need credentials or a running harness (Claude's OAuth usage endpoint, Codex's app-server `account/rateLimits/read`, terminal probes of `/status`) and are not ported; nor are Gemini, Cursor, Kimi and the other providers Orca meters. Claude's weekly reset time, and its limits, are unknown on disk. Remote mode: the meters are this machine's, and `by --remote` uses none of it.

## `by adopt`

```sh
by adopt                                   # this repository's Claude Code and Codex sessions, newest first
by adopt 0b5e7a1c --name faster            # make one a branch
by send faster "now add a regression test" # resumes that session in the branch's worktree
```

```text
SESSION   HARNESS      AGE  TURNS  WHERE  TASK
0b5e7a1c  claude-code   9m      1  .      make the parser faster
019a0000  codex         1h      1  .      write the docs
```

**Finding sessions.** `by adopt` scans the default login stores in read-only mode and lists sessions whose working directory is inside the current repository (excluding sessions already inside `.branchyard/` worktrees):

- **Claude Code:** Scans `projects/` directories whose encoded path matches the repository root or one of its subdirectories (using Orca's `claude-project-dir-encoding.ts` rule, which replaces non-alphanumeric ASCII characters with `-`). Each top-level transcript is listed as a session using its summary or first prompt as the title.
- **Codex:** Scans rollouts under `sessions/` and `archived_sessions/`, skipping internal subagent, review, and compaction threads (`session-scanner-codex-non-user-origin.ts`), using the thread name or first message as the title.

Passing `--json` outputs `id`, `harness`, `file`, `cwd`, `git_branch`, `commit`, `title`, `model`, `started_ms`, `updated_ms`, and `turns`.

**Creating a branch from a session.** Running `by adopt SESSION [--name NAME] [--harness PROFILE] [--no-diff] [--json]` (where `SESSION` can be any unique ID prefix) creates a Branchyard branch and worktree, associates the session ID with the branch in the `no_changes` state, and allows the next turn (`by send` or `R`/`s` in `by watch`) to resume the conversation natively. Branchyard picks the base commit conservatively and reports which rule it used:

1. **`session_commit` (recorded session commit):** If the session recorded a commit hash (Codex writes `git.commit_hash` in its rollout) and that commit exists in the repository, the branch starts at that commit.
2. **`head` (current `HEAD` of the session directory):** Because Claude Code does not record a commit hash in its transcript, Branchyard uses the current `HEAD` of the directory where the session ran and notes that `HEAD` may have moved since the session started.
3. **`head_with_diff` (uncommitted tracked changes applied):** If the session directory currently has uncommitted modifications to tracked files relative to the chosen base commit, Branchyard applies them (`git diff --binary HEAD` followed by `git apply`) as uncommitted changes in the new worktree and lists the affected files in the notes. Untracked files are never copied (though their count is noted), and `--no-diff` skips applying uncommitted diffs altogether. If a diff fails to apply cleanly, `by adopt` removes the new branch and exits with an error.

For Claude Code sessions, `by adopt` also **copies** the transcript file into the project directory matching the new worktree path (`~/.claude/projects/<encoded worktree>/<id>.jsonl`), because Claude Code only resumes sessions that belong to its current working directory's project folder; the original transcript is left untouched. Codex can resume a rollout ID from any directory. You can pass `--harness` to select a specific profile for future turns on that harness (for example, `claude-code-acp` instead of `claude-code-stream-json`). If a session was modified within the last two minutes, `by adopt` prints a warning that the original session may still be open and that continuing both will cause them to diverge.

Adopting a session records an `adopted` event as the branch's first log entry (`by log` shows `adopted claude-code session 0b5e… at 1a2b3c4d5e (head_with_diff), 1 changed file applied`, and `by log --json` outputs `{"activity": "adopted", "adopted": {harness, session, source, cwd, base, how, diff_files, notes}}`). In the Rust SDK, this corresponds to `Yard::adopt(AdoptSpec)` and `Activity::Adopted`.

**Not done.** Only the default logins' stores are searched (not `[usage.accounts]` directories, and not WSL or remote hosts Orca also scans); other harnesses' sessions (Orca reads OpenCode, Gemini, Cursor and more); a branch for an isolated or sandboxed home (the adopted branch uses your own home, where the session is); and recovering a session's own changes when they were committed elsewhere or mixed with others in the directory.

## What is tested

Hermetic, against fixture session stores, local mock servers and real local processes (`crates/branchyard-cli/tests/devex.rs`, run as root and as an unprivileged user; unit tests beside each module):

- **Meters**: Claude records deduplicated and windowed, a budget's percent, a limit message filling the window with its reset; Codex windows classified as Orca classifies them (by duration, legacy fallback, a minute's tolerance), a passed reset, tokens per window; prices for model names old and new from the generated table, the long-context tiers; the guard warning, refusing (nothing created) and off; the router passing over a candidate over `skip_over` and saying why.
- **Adopt**: a Claude and a Codex session of this repository listed, another repository's and a Codex subagent's not; a Claude session adopted with the checkout's uncommitted change applied and its untracked file left out, the transcript copied where Claude Code looks, the `adopted` event, and its next turn resuming the same session ID (the fake ACP agent standing in for `claude-agent-acp`); a Codex session at its recorded commit with `--no-diff`; refusals.
- **Not tested**: against real Claude Code or Codex files written by current releases (the fixtures follow the formats Orca parses), a real Claude Code `--resume` in the new worktree, macOS (paths normalized to NFC are not tried).
