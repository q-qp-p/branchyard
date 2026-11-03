# Watching branches

`by watch` displays every branch in the repository as a tree, with child branches and forks nested under their parents. For each branch it shows the status, harness, current activity (such as the tool currently running, a pending permission request, or the latest message line), turn count, cost, and age.

Pass `--once` to print a snapshot table and exit:

```text
by watch · /src/app

BRANCH        HARNESS      STATUS      TURNS   COST  AGE  ACTIVITY
parser        claude-code  running         2  $0.41   3m  ▸ Bash · Running the parser tests
└ parser-alt  codex        ready           1  $0.12   1m  Rewrote the tokenizer loop
docs          gemini-cli   no changes      1      -   9m  The docs already cover this

3 branches, 1 running, $0.53 reported
```

When run in an interactive terminal, `by watch` opens a live TUI dashboard (built with `ratatui`):

- The left or top pane shows the branch tree with color-coded statuses (for example, an interrupted branch is highlighted in black on yellow because it stopped mid-turn).
- The detail pane beside or below it shows the selected branch's recent events, cost and token usage, unread inbox messages, child branches, original prompt, and suggested `next` commands.
- Use `j`/`k` or the arrow keys to move the selection, `Enter` to focus the selected branch's subtree, `/` to filter by branch name, harness, status, or prompt text, `?` to view the full keybinding help sheet, and `q`, `Esc`, or `Ctrl-C` to exit cleanly and restore the terminal.

When stdout is piped rather than attached to a terminal, `by watch` prints one line per state change. It reads local event logs incrementally and works identically with `--remote`, streaming live events from the server.

You can also act on the selected branch directly from `by watch`. Each shortcut key runs the corresponding `by` command (forwarding `--remote` and other global flags), and the footer bar lists the shortcuts that apply to the currently selected branch:

| Key | Action | Command run |
|---|---|---|
| `s` | Open an input box and send a follow-up prompt | `by send` (in the background) |
| `S` | Steer a currently running turn | `by send --steer` |
| `R` | Resume an interrupted branch in its existing session | `by send` with a "continue where you left off" prompt |
| `x` | Cancel the running turn (and any running descendants) after confirmation | `by cancel` |
| `m` | Merge the branch after confirmation, displaying check output in a pane | `by merge` |
| `f` | Fork the branch with a new prompt | `by fork` (in the background) |
| `d` / `l` | View the candidate diff (`d`) or live event log (`l`) in a scrollable pane | `by diff`, `by log` |
| `y` / `Y` | Copy the branch name (`y`) or worktree path (`Y`) to the clipboard via OSC 52 | |
| `p` / `P` | Run the check, push, and open or update a pull request after confirmation; `P` continues watching the PR and feeds CI failures and review comments back as new turns | `by pr` (output shown in a pane), `by pr --watch` (in the background) |
| `o` | Open the branch worktree in `$VISUAL` or `$EDITOR`, returning to the dashboard when the editor exits | `by open` |
| `r` | Rewind the branch to an earlier checkpoint number after confirmation | `by rewind --to N --yes` |
| `c` | Compare the branch against its siblings (from the same `by fan` or parent branch) | `by compare` |
| `t` | Apply the branch's candidate changes to your current checkout after confirmation; pressing `t` again restores the checkout | `by try`, `by try --off` |
| `v` | Review the candidate diff in `$VISUAL` or `$EDITOR`; comments added on lines starting with `>>` are sent as a single follow-up prompt when the editor closes | `by review --detach` |
| `b` / `K` | Open a port the branch is listening on in your browser (`b`), or stop its listening processes after confirmation (`K`) | `by workspace browse`, `by workspace kill --yes` |

Actions that start a new turn run detached in the background (writing logs to `.branchyard/watch/` in the repository) so they continue running even if you close `by watch`. Completion notices or refusals (such as pressing `R` on a branch that is not `interrupted`) appear in the dashboard status line. Because background turns have no interactive terminal attached, they use the permission rules configured in `branchyard.toml` and deny unapproved requests by default.

When connected with `--remote`, actions that require access to a local checkout or worktree (`Y`, `p`, `P`, `o`, `r`, and `t`) are refused with an explanation. The detail pane also shows the branch's current checkpoint position (`at 2 of 3`), its pull-request merge readiness, and whether `by try` is currently active. All keybindings are defined in a single table in `crates/branchyard-cli/src/watch/actions.rs`, which also generates the `?` help overlay.

## Notifications and other interfaces

Both `by watch` and foreground commands that wait on a branch (`by run`, `by fan`, `by send`, and `by fork`) emit a notification whenever a branch needs attention or finishes (specifically when a tool asks for permission, a branch asks or escalates a question, a turn stalls, or a branch finishes, fails, or is interrupted):

- Each new event triggers a terminal bell and a desktop-notification escape sequence: OSC 9 (supported by iTerm2, WezTerm, kitty, Ghostty, and Windows Terminal) or OSC 777 (supported by foot, urxvt, and VTE terminals like GNOME Terminal), forwarded through `tmux` automatically without needing D-Bus. Historical events that already occurred before `by watch` started do not trigger notifications.
- Setting `[notify] desktop = true` in `branchyard.toml` also invokes `notify-send` on Linux or `osascript` on macOS.
- Setting `[notify] terminal` selects the escape sequence format (`auto`, `osc9`, `osc777`, `bell`, or `none`).
- Passing `--no-notify` or setting `[notify] enabled = false` disables notifications completely. Terminal escape sequences are only written when attached to a TTY.

If you prefer a plain streaming log for a single branch, run `by log --follow <branch>`.

For [Herdr](https://github.com/herdrdev/herdr) users, the [Branchyard plugin](../plugins/herdr/README.md) subscribes to a server's event stream, opens a tab running `by log --follow` for each branch, mirrors the branch state in Herdr's agent sidebar (`working`, `blocked` on a pending permission request, or `idle`), and provides actions to merge, cancel, or send a prompt to the focused branch. It is currently tested against a fake `herdr` binary rather than a live Herdr release.

To monitor and control branches from a phone, start the server with `by serve --app` to serve the [web companion](companion.md) at `/app/`. It streams the live branch list, events, checkpoints, diffs, and merge readiness, and lets you send prompts, steer, cancel, merge, fork, answer questions and escalations, and toggle triggers using scoped API tokens. Run `by serve token new --link --scopes read,run` to print a one-time pairing URL and QR code; opening the link on your phone exchanges it for an expiring token that you can revoke at any time with `by serve token revoke`. When Web Push is enabled, the phone receives the same alerts as `by watch`.
