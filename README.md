<img src="assets/brand/icon.svg" width="56" height="56" alt="Branchyard">

# Branchyard

Branchyard runs multiple AI agents in parallel, giving each task its own Git branch and working copy. You can follow their work from the terminal or browser, review the diffs or documents they produce, and merge only the changes that pass your checks.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/screenshots/console-branches-dark.png">
  <img src="assets/screenshots/console-branches.png" alt="The Branchyard console in a desktop browser. The sidebar sorts a Rust library's branches into Needs you, Running, Ready and Done. The open branch, parser-fix, shows the request, the agent's reply with each tool call as a note in the margin beside it, a receipt closing the turn, a Merge button at the top, its two changed files and the first file's diff.">
</picture>

<sub>A scripted agent produced the demo data in these screenshots, and no model ran. [How to regenerate them](assets/screenshots/README.md).</sub>

The **Home** view shows the state of your repository at a glance. Summary tiles at the top count active, waiting, ready, and merged branches alongside total spend, while the timeline below plots every open branch with its turns, tool calls, pending approvals in amber, and cost.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/screenshots/console-board-dark.png">
  <img src="assets/screenshots/console-board.png" alt="The console's home in a desktop browser. Tiles count the branches in flight, needing you, ready and merged today, and what they spent. Below, a board lists a Rust library's open branches on one time axis, each turn a bar with a tick for each tool call: the branch changelog waiting for you in an amber row with an Answer button, an interrupted branch with Resume, two running branches with Steer and two ready branches with Merge, each with its cost, and three merged branches folded under their merge ticks. Under the board, today's decisions and each branch's spend.">
</picture>

The **Needs you** view queues up pending decisions one card at a time, showing what the agent is asking for, what it was doing when it paused, and the actions you can take.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/screenshots/console-decide-dark.png">
  <img src="assets/screenshots/console-decide.png" alt="The console's Needs you view. One raised card, tinted amber, asks whether the agent on the changelog branch may use Claude Code's Write tool on CHANGELOG.md, with what it was doing beside what it asks, a reason field and the answers Allow, Deny, Open and Later; the next card peeks under it, and beside it a queue of the two ready branches to merge and the interrupted branch. Below, what was decided today.">
</picture>

## Why

Running one coding agent in a single checkout is straightforward. Once you run two or more in the same directory, they overwrite each other's files and make it hard to tell which change came from which prompt. Even after an agent finishes, you still have to inspect the diff, run your test suite, and decide whether to land the result.

Branchyard gives every task its own Git branch and worktree (an isolated working copy of your repository). Multiple agents can work at the same time without stepping on each other's files or your main checkout. When a task finishes, you inspect its diff with `by diff`, and `by merge` runs your test command against the merged result before updating your branch. Nothing lands on your working branch unless the check passes.

You can use the same workflow for documents, spreadsheets, and data analysis. Each session reads the source files you upload, works in an isolated directory to produce charts, spreadsheets, or slide decks, and lets you preview and accept the outputs you want to keep.

## How it works

- **Start a task (`by run`)**: Branchyard creates a Git branch (`by/<name>`) and worktree, then launches your chosen harness (such as Antigravity, Claude Code, Codex, or Gemini CLI) using that tool's existing login and configuration.
- **Monitor and steer (`by watch` or the web console)**: Follow live progress, read the conversation and file diffs, answer permission prompts, or send follow-up instructions.
- **Verify and merge (`by merge`)**: Branchyard merges the branch's candidate commit with your target branch, runs your `--check` command (such as `cargo test` or `pytest`) on the combined tree, and fast-forwards your branch only if the check exits 0.
- **Run remotely (`by serve` or `--remote`)**: Host a server for your team or connect over SSH to run tasks on remote machines while controlling them from your local CLI or browser.

## Install and check setup

From inside your Git repository, install the prebuilt binaries and launch the guided setup:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --setup
```

The setup wizard detects which agent CLIs you have installed, writes `branchyard.toml` with sensible defaults for your repository, and installs Branchyard hooks and skills into your agents. If any agent still needs you to sign in or trust a hook, setup prints the exact command to run. You can also run `by up --plan` at any time to check readiness without modifying files or starting services. See [installation options](docs/distribution.md#install-by) for version pinning, source builds, and headless setups.

## Run a task and review it

Once an agent CLI (such as Antigravity, Claude Code, or Codex) is installed and signed in, run a task from your repository:

```sh
by run "Make the flaky parser test deterministic. Add a regression test." \
  --name parser-fix --harness antigravity \
  --check "cargo test" --yes --allow-unapproved-tools
by diff parser-fix
by merge parser-fix
```

Replace `cargo test` with your project's test command. The agent works on the branch `by/parser-fix` in its own worktree. When it finishes, `by diff` shows the changes it made, and `by merge` runs your check command on the merged result before landing it on your current branch.

In the example above, `--allow-unapproved-tools` lets Antigravity use its own tool permission rules, while `--yes` automatically approves permission requests that route through Branchyard. By default, a local agent runs as your OS user and can access your files; see [execution modes](docs/modes.md) to run agents with a private `HOME` directory (`--isolated`), a Linux Landlock file guard (`--permissions edit-worktree`), or a sandbox provider.

While tasks are running, use `by watch` for an interactive terminal dashboard. To coordinate multiple agents across implementation, research, and review, see [cooperative teams](docs/cooperative-teams.md).

## Follow work in the browser

Run `by serve --app` in a repository to start the web console, then run `by serve token new --link` to print a sign-in link and QR code. The console groups branches by what they need from you, displays each branch's conversation and file diffs, and highlights the next action (such as **Merge** or **Resume**) at the top of the page. It works on both desktop and mobile browsers. See the [web companion guide](docs/companion.md) for details on pairing, token scopes, and push notifications.

<p align="center"><img src="assets/screenshots/console-phone.png" width="260" alt="The parser-fix branch in the console on a phone, in one column, with Merge at the top and the agent's reply below."></p>

## Work with documents and data

You can also use the console for document and data projects outside of a Git repository:

```sh
by serve --app --knowledge
```

Open the printed URL, create a knowledge project, and upload your source files. From there, you can start one or more named sessions, follow their progress, preview the spreadsheets, documents, or presentations they generate, and accept the final outputs you want to keep. Each session works in its own isolated copy of the workspace. The [web companion guide](docs/companion.md#projects-and-workspaces) covers credentials, roles, and preview setup.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/screenshots/console-workspace-dark.png">
  <img src="assets/screenshots/console-workspace.png" alt="A knowledge workspace in the console. Tabs name its two sessions, the open session answers questions about customer churn from two uploaded exports, and the Outputs tab previews the CSV table it published.">
</picture>

Branchyard's [built-in skills](plugins/branchyard/skills/INDEX.md) help agents inspect source data, clean tables, verify calculations, and format documents, charts, and slide decks. In the [parallel deck example](examples/rigs/parallel-deck.toml), multiple workers draft individual sections in parallel before a synthesizer assembles the final presentation.

To generate and preview Office files locally, install the required [document packages](docs/artifact-toolkit.md#installed-skills-and-runtime) along with LibreOffice and Poppler. Google Workspace tasks require a connected account with access to the target files. For more details, see [knowledge and design skills](docs/knowledge-skills.md) and [artifact capture](docs/artifact-capture.md).

## Use a remote worker

If `by` and your agent CLI are installed on a remote machine reachable over SSH, you can run any command against a remote repository without setting up a persistent server first:

```sh
by --remote ssh://you@worker.example/srv/project ls
```

See the [SSH remote guide](docs/remote-ssh.md), [GCE worker setup](docs/compute-gce.md), and [GKE Substrate setup](docs/substrate-gke.md) for remote execution options. Branchyard is in early development; the [status page](docs/status.md) tracks what has been tested live and what remains in progress.

## Documentation and license

[Getting started](docs/getting-started.md) · [All guides](docs/README.md) · [SDKs](docs/sdk.md) · [Contributing](CONTRIBUTING.md)

Branchyard's own code is [Apache-2.0](LICENSE). Bundled dependencies keep their own licenses; see [third-party notices](THIRD_PARTY.md).
