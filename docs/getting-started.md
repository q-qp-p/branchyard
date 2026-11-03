# Getting started

This guide walks through installing `by`, running a task on a branch, inspecting the resulting diff, and merging it back into your main branch.

You can start right away by installing a prebuilt release binary. Steps 5 through 8 run against a built-in fake agent so you can test the entire workflow locally with no model and no API cost. Those steps are executed automatically by `tests/test_getting_started.py` on every change, so the output shown below matches what the CLI prints.

## What you need

- **git**, with `user.name` and `user.email` configured, and a repository with at least one commit.
- **An installed and logged-in harness CLI**: Claude Code, Codex, or Gemini CLI are the most common choices. Running `by harnesses` (Step 2) shows which ones are installed on your machine. If you do not have one set up yet, you can still run [Step 5](#5-try-it-without-a-model) using the fake agent.
- **curl and a POSIX shell** to install the prebuilt release binaries on Linux or macOS. You only need a Rust toolchain if you are building from source or running the fake-agent walkthrough in Step 5.

## 1. Install

Install the latest release binaries with a single command:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --no-setup
by --version
```

By default, the installer places verified prebuilt binaries in `~/.local/bin`. If your shell cannot find `by`, open a new terminal or add `~/.local/bin` to your `PATH`. You can pass `--setup` instead of `--no-setup` to launch the interactive repository setup immediately, or run `by setup` separately in Step 3. See [distribution](distribution.md#install-by) for supported platforms, version pinning, and `--runtime` builds.

If you prefer to build from source, install [rustup](https://rustup.rs) first. The repository pins toolchain 1.94.0, which `rustup` will download automatically on the first build:

```sh
git clone https://github.com/vamsiramakrishnan/branchyard
cd branchyard
cargo install --locked --path crates/branchyard-cli
by --version
```

Running `by --version` prints the installed version. Source builds install into `~/.cargo/bin`, so make sure that directory is on your `PATH`.

When you run tasks later on, you can pass a check command (any shell command that exits `0` when the changes are good, such as `cargo test`, `npm test`, `pytest`, or `make test`).

## 2. See what you have

Check which coding agents are installed and ready to use on your machine:

```sh
by harnesses
```

```text
HARNESS      VERSION  LOGIN                    LIVE RUN     QUOTA        PATH
claude-code  2.1.310  logged in                live 9/9     5h - · wk -  /opt/node22/bin/claude
codex        0.164.0  key set (CODEX_API_KEY)  live 9/9     -            ~/.npm-global/bin/codex
qwen-code    0.26.0   unknown                  no live run  -            ~/.npm-global/bin/qwen
  claude-code: claude auth status: logged in (oauth_token)
  codex: CODEX_API_KEY is set (an API key; not checked)
  codex: installed 0.164.0 · live-tested 0.157.1
  qwen-code: Branchyard cannot read qwen's login: the catalog records no status command, credential file or key variable for it; sign in inside qwen itself

3 of the 45 harnesses with a known executable are installed on this machine. `by harnesses --all` lists the rest with their install commands; `by harnesses install ID` installs one.
```

`by harnesses` queries every installed harness in parallel (with a ten-second timeout) and prints one row per harness:

- **LOGIN** reports only what Branchyard can verify locally:
  - `logged in` or `logged out` when the harness provides its own auth status command.
  - `key set (VAR)` when the expected API key environment variable is present (printing the variable name, never the secret value).
  - `credentials found` when a known credential file exists on disk.
  - `login not verified` when neither is found and the status check could not complete (see [troubleshooting](troubleshooting.md#login-not-verified)).
  - `unknown` when the catalog has no way to inspect login state for that harness.
- **LIVE RUN** shows the result of the most recent recorded qualification run against a live model, with the tested version listed below the table.

To sign in to a harness or store an API key for it, run `by harnesses login ID`. If anything else about your environment looks wrong, run `by doctor` to inspect your machine and repository configuration.

## 3. Set up the repository

From inside your Git repository, run `by setup`:

```sh
cd path/to/your/repo
by setup
```

`by setup` configures a repository for Branchyard in one step:

1. It detects which harnesses are installed, whether they are logged in, which API-key environment variables are set (recording only variable names, never secret values), and which test command matches the files in your repository.
2. If no `branchyard.toml` exists yet, it writes one using those detected defaults. In an interactive terminal, it only prompts when a choice is ambiguous.
3. It installs Branchyard hooks, slash commands, and skills into each detected harness so your interactive coding sessions are notified when background branches finish, and it verifies that the installed hook responds.
4. It never launches an interactive login flow on its own. If a harness is logged out, `by setup` prints the exact login command for you to run.

When `by setup` finishes, it prints a summary of what it detected and wrote, any manual follow-up steps under `Only you can do:` (such as trusting hooks in Codex with `/hooks` and `t`), and a `Next:` command to try. Re-running `by setup` when everything is already configured makes no changes.

If you run `by setup` without a terminal and without `--yes`, it prints the plan without writing anything, just like `--dry-run`. Other useful flags include `--json`, `--harness ID,ID`, `--no-hooks`, and `--no-config`. After setup, running bare `by` shows the current status of the repository and suggests what to run next.

If you prefer to run each setup step individually, you can use `by init project`, `by init plugin --all-detected`, and `by harnesses login ID`. See [setup](setup.md) for all configuration options.

To authenticate through a Z.ai or Gemini gateway key instead of a harness's native login, run `printf %s "$KEY" | by harnesses login claude-code --provider zai` (or `--provider gemini` for Gemini CLI, Antigravity, or OpenCode). That stores the key for isolated runs and prints the `export` line for local shells (see [gateway providers](harness-lifecycle.md#through-a-gateway-provider)).

Here is what `by setup --dry-run` followed by bare `by` prints in a fresh repository with no harnesses on `PATH`:

```sh test
cd "$(mktemp -d)" && git init -q -b main plan-by && cd plan-by && git commit -q --allow-empty -m "first commit"
by setup --dry-run
by
```

```text
Detected
  repository  … (default branch main)
  harnesses   …
Summary
  config      would write branchyard.toml: harness … · no check · local, no isolation
Nothing was written (--dry-run).
Next: by setup…
Branchyard
  repository  …
  set up      not yet: branchyard.toml no · .branchyard/ no
  branches    none yet
Next: by setup
All commands: by --help
```

## 4. Run a task

Once a harness is installed and logged in, run a task in your repository:

```sh
by run "Make the flaky parser test deterministic" --check "cargo test" --yes
```

`by run` creates a new Git branch (`by/NAME`) and checks it out in a dedicated worktree under `.branchyard/` (which Git ignores), starts the harness there, and streams its progress to your terminal.

- `--check` specifies the command that must pass on the merged result before `by merge` will update your target branch.
- `--yes` approves every tool permission request from the harness automatically. Use `--ask` to be prompted in the terminal instead, or `--permissions read-only|edit-worktree|full` to apply a permission preset.
- `--harness ID` selects a specific harness, and `--name` sets the branch name.
- In another terminal, you can run `by watch` to monitor all active branches in a live TUI.

Keep two practical points in mind before starting a real run:

1. **Isolation mode**: By default, `by` runs in local mode, meaning the harness runs as your user account without a sandbox (`by` prints a reminder the first time it runs in a repository). See [`docs/modes.md`](modes.md) to enable environment scrubbing or VM/container sandboxes.
2. **Usage limits**: Real runs use your harness login's quota or API key. You can cap a run with `--budget-usd`, `--max-turns`, or `--max-minutes`.

Every run finishes with a summary card showing the branch name, its status, the candidate diff stats, the cost, and `next` commands you can run. Step 5 shows this output in full.

## 5. Try it without a model

Branchyard includes a fake ACP agent (`fake-acp-agent`) used in its own test suite. It speaks the same protocol as Gemini CLI, follows simple instructions in the prompt (for example, `WRITE path=content` writes a file), and requires no model or API key.

From a source checkout of Branchyard, build the fake agent binary:

```sh
cargo build -p branchyard-runtime --bin fake-acp-agent
FAKE="$PWD/target/debug/fake-acp-agent"
```

Now create a temporary Git repository and run a task in it using the fake agent:

```sh test
cd "$(mktemp -d)" && git init -q -b main try-by && cd try-by
echo "# try-by" > README.md
git add README.md && git commit -q -m "first commit"
by run "WRITE hello.txt=hello" --name hello --harness gemini-cli --command "$FAKE" --check "test -f hello.txt" --yes
```

```text
by: local mode: harnesses run as your user, in your own HOME; no sandbox
wrote hello.txt
turn 1 completed
candidate … (1 file +1 -0)

branch     hello (by/hello)
status     ready
candidate  1 file +1 -0 at …
cost       not reported, 1 turn
next       by diff hello
           by merge hello
```

Here, `--command "$FAKE"` launches the fake agent binary instead of the real `gemini` executable, while `--harness gemini-cli` selects the ACP driver profile. Everything else (the worktree creation, checkpoint commit, event log, and status card) works identically to a real harness run.

## 6. Read it

Use the branch name from the summary card (`hello` in this example) to inspect what the branch did:

```sh test
by ls
by show hello
by diff hello
```

```text
BRANCH  HARNESS     STATUS  CANDIDATE     TURNS  COST  AGE
hello   gemini-cli  ready   1 file +1 -0      1     -  …
branch      hello
git branch  by/hello
harness     gemini-cli (gemini-cli-acp)
status      ready
candidate   1 file +1 -0 at …
checkpoints
+++ b/hello.txt
+hello
```

- `by ls` lists all branches in the repository in a compact table.
- `by show hello` displays full metadata for the branch, including its prompt, base commit, candidate commit, cost, session ID, worktree path, and per-turn checkpoints.
- `by diff hello` shows the diff between the base commit and the branch's latest checkpoint.
- `by log hello` prints the timestamped event log for the branch.

## 7. Merge it

When you are happy with the candidate diff, merge the branch into your current branch:

```sh test
by merge hello
by ls
cat hello.txt
```

```text
merged hello into main (…); `by rm hello` frees the name
hello   gemini-cli  merged into main  1 file +1 -0      1     -  …
hello
```

`by merge` runs your `--check` command against the exact merged tree before updating your current branch. If the check fails, `by merge` leaves your branch untouched and prints the check output (see [troubleshooting](troubleshooting.md#the-check-for--failed)).

After merging, the branch record and worktree remain on disk so you can still inspect the log until you remove the branch with `by rm`:

```sh test
by log hello
by rm hello
by ls
```

```text
…Z  status: running
…Z  prompt: WRITE hello.txt=hello
…Z  turn 1 completed
…Z  status: ready
…Z  status: merged into main
removed hello
no branches; start one with: by run "<prompt>"
```

## 8. Try several harnesses

`by fan` runs the same prompt across multiple harnesses in parallel, creating one branch per harness so you can compare their changes side by side. Here, the fake agent stands in for both harnesses:

```sh test
by fan "WRITE fan.txt=hi" --name pair --harness gemini-cli,qwen-code --command "$FAKE" --yes
by compare --fan pair
```

```text
BRANCH           STATUS  CANDIDATE     TURNS  COST
pair-gemini-cli  ready   1 file +1 -0      1     -
pair-qwen-code   ready   1 file +1 -0      1     -
BRANCH           STATUS  TURNS  COST  TOKENS   TIME  CHECK  FILES    +/-  UNIQUE FILES
pair-gemini-cli  ready       1     -       -  …  none       1  +1 -0  -
pair-qwen-code   ready       1     -       -  …  none       1  +1 -0  -
```

When running with real harnesses (for example, `--harness claude-code,codex`), each harness works in its own isolated worktree. Once they finish, `by compare --fan pair --pick pair-qwen-code` merges the candidate you choose.

When you are finished with the walkthrough, you can delete the temporary repository with `cd .. && rm -rf try-by`.

## What the `next` lines mean

After `by run`, `by send`, or `by fork`, the summary card prints `next` lines with the commands that make sense for the branch's current state:

| Status | `next` says | Meaning |
|---|---|---|
| `ready` | `by diff NAME`, `by merge NAME` | The branch produced a candidate commit. Review the diff, then merge it. |
| `no changes` | `by send NAME "<prompt>"`, `by rm NAME` | The turn completed without modifying any files. Send a follow-up prompt or remove the branch. |
| `running` | `by log NAME` | A turn is currently running. Use `by watch` to monitor all active branches live. |
| `interrupted`, `over budget`, `failed` | `by log NAME`, `by diff NAME`, `by rm NAME` | The turn stopped early. Check the event log and diff to decide whether to continue or discard the branch. |
| `merged` | `by rm NAME` | The candidate has been merged. Remove the branch when you no longer need its worktree. |

See [Concepts](concepts.md) for the complete list of branch states, and [troubleshooting](troubleshooting.md) for what to do when a branch does not finish `ready`.

## Where to go next

- [Concepts](concepts.md): core terms (task, branch, turn, candidate, run, fan, map, spawn) and every branch state.
- [`docs/modes.md`](modes.md): local, isolated, sandbox, server, and PostgreSQL modes, with the configuration setting and `by doctor` check for each.
- [Setup](setup.md): `branchyard.toml` settings, `by config show`, and the interactive `by init` commands.
- [Harness hooks](harness-hooks.md): how interactive Claude Code or Codex sessions get notified when background branches finish.
- [The Python SDK](sdk.md): running and inspecting branches from Python scripts.
- [`docs/adding-a-harness.md`](adding-a-harness.md): how to add a driver for a CLI agent that Branchyard does not yet support.
- [Troubleshooting](troubleshooting.md) and [upgrading](upgrade.md).

Not done: step 3's `by setup --dry-run` and the bare `by` run in CI in a repository with no harness on `PATH`; `by setup` writing a configuration and installing hooks into a real harness was not run here. The real-harness command in step 4 and the `by harnesses` rows were not run against a model here. The source install command was run in debug mode, offline, into a scratch directory. Everything was run on Linux; macOS and Windows were not.
