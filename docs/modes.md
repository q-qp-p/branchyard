# Where harnesses run and where state lives

Branchyard supports five execution and storage tiers, ordered below from simplest to most isolated. Choose the lowest tier that meets your requirements, and use the command in the last column to verify your setup.

Every deployment answers two separate questions:

1. **Where does the harness process run** (and what files, environment variables, and networks can it access)?
2. **Where are branch records stored** (and who can reach them)?

The first three tiers configure how the harness process runs. The last two configure how branch state is stored and shared, and can be combined with any of the first three tiers (for example, a PostgreSQL-backed server can run harnesses locally, with `--isolated`, or inside a sandbox).

| Tier | When to use it | Configuration (CLI flag / `branchyard.toml`) | What it guarantees (and what it does not) | Requirements | How to verify |
|---|---|---|---|---|---|
| [**Local**](#local) | Trying Branchyard locally with minimal setup | Default (`--provider local` / `[defaults] provider = "local"`) | Each harness runs in its own Git worktree and process group. On Linux, `--permissions read-only` or `edit-worktree` restricts filesystem writes to the preset's scope ([file guard](#the-file-guard)). *Not* a full sandbox: the harness runs as your user, in your `HOME`, with your credentials, and can read any file you can read. | None | `by doctor` shows `provider.default` as `local (the default)` and `host.landlock` with the kernel ABI; runs print `by: local mode` (and `by: guard: landlock (PRESET)` when guarded) |
| [**Isolated**](#isolated) | Keeping the harness out of your personal dotfiles and default logins | `--isolated` / `[defaults] isolated = true` (`--isolated=false` overrides per run) | Gives the branch a private `HOME` at `.branchyard/homes/BRANCH` and strips `ANTHROPIC*`, `CLAUDE*`, `OPENAI*`, `CODEX*`, and `BRANCHYARD_*` environment variables. *Not* a sandbox: still runs as your user with access to your filesystem, network, and other environment variables. | You must pass credentials explicitly ([provisioning](provisioning.md)) since your personal `HOME` is hidden | Runs print `by: isolated mode`; `by show BRANCH` shows `local, isolated: private HOME, not a sandbox`; `by config show` shows where `isolated` was set |
| [**Sandbox provider**](#sandbox) | Isolating the harness from your host filesystem and process space | `--provider microsandbox --image REF` / `[defaults] provider = "microsandbox"` + `[microsandbox] image = "REF"`; `--provider recipe:NAME` / `provider = "recipe"` + `recipe = "NAME"`; `--provider substrate` + `--substrate-*` flags | Runs each turn inside a microVM, custom recipe host, or remote actor with only the worktree and explicitly passed variables. All three sandbox providers are currently **unqualified** (tested against mocks, not live runtimes in CI). | Microsandbox: KVM, source build with `--features microsandbox`, Rust 1.94, `libcap-ng`, `msb` runtime. Recipe: custom scripts and SSH target. Substrate: a Substrate cluster | `by doctor` (`provider.default` and `host.kvm`), `by up --plan` (`provider` step), or `by recipe check NAME` |
| [**Shared server**](#server) | Running branches that outlive your terminal session or run on a dedicated host | Host: `by serve` (with `[serve] config = "FILE"` in `branchyard.toml`). Client: `--remote URL --token-file FILE` (`--repo`, `--ca-file`) / `[remote] url`, `token_file`, `ca_file`, `repo` | Central SQLite state store for branches and operations, scoped multi-tenant tokens, and background execution that survives client disconnects. *Not* process isolation by itself: local-provider branches still run as the server's OS user. | A server host, TLS when listening off loopback, and `--allow-provider` if using sandboxes | `by doctor` (`server.reachable` on the client; `serve.config` and `serve.database` on the host) and `by serve --check` |
| [**PostgreSQL**](#postgres) | Running multiple servers and `by worker` processes against a shared queue | `by serve --database postgres://USER@HOST/NAME` / `"database"` in the server config file | Queued operations are claimed and executed once across multiple workers and server restarts. Does not change how harness processes are isolated. | A PostgreSQL database and a binary built with `--features postgres` (included in prebuilt release binaries) | `by doctor` (`serve.database`) and `by serve --check` |

For both the **Local** and **Isolated** tiers on Linux, passing `--permissions read-only` or `--permissions edit-worktree` enables the OS-level [file guard](#the-file-guard), which restricts filesystem writes even if the harness does not ask Branchyard before writing.

## Which build reaches which rung

Available tiers depend on which Cargo features were compiled into your `by` binary:

| Binary source | Local | Isolated | Recipe, Substrate | Microsandbox | Server, SQLite | PostgreSQL |
|---|---|---|---|---|---|---|
| Prebuilt release archive or `install.sh` (`by` and `branchyard-server`) | yes | yes | yes | **no** | yes | yes |
| Default source build (`cargo install --locked --path crates/branchyard-cli`) | yes | yes | yes | **no** | yes | **no** |

If you request a feature that was not compiled into your binary, `by` refuses before creating any resources and prints the exact rebuild command. From a checkout of this repository, you can enable optional features as follows:

```sh
cargo install --locked --path crates/branchyard-cli --features postgres       # PostgreSQL stores
cargo install --locked --path crates/branchyard-cli --features microsandbox   # microVMs (needs libcap-ng-dev, Rust 1.94)
cargo install --locked --path crates/branchyard-cli --features microsandbox,postgres   # both
cargo install --locked --path crates/branchyard-server --features postgres    # only the branchyard-server binary
```

`by init` inspects the running binary's compiled features (`build.microsandbox` and `build.postgres`; see [setup](setup.md)). On a build without Microsandbox support, it omits Microsandbox from interactive choices and notes the rebuild command if KVM is available. On a build without PostgreSQL support, `by init server` marks PostgreSQL as requiring `--features postgres`.

Running a Microsandbox microVM also requires the `msb` runtime at the matching SDK version and read/write access to `/dev/kvm` (see [sandbox providers](providers.md#microsandbox-provider)). A machine without KVM can compile the feature and run unit tests, but cannot boot a microVM.

## Local

```sh
by run "Fix the flaky parser test" --check "cargo test"
```

Local mode runs the engine in your CLI process and spawns each harness as a child process in its own Git worktree under `.branchyard/` and its own process group. Branch metadata is stored in `.branchyard/state.db`.

**Guaranteed:** Each branch gets an isolated worktree and process group so its working tree and child processes stay separate and `by cancel` can terminate the entire process group cleanly. Environment variables that mark the calling Claude Code session are stripped so child harnesses do not think they are nested inside that session.

**Not guaranteed:** Any OS-level isolation beyond your user account. The harness inherits your `HOME`, logins, other environment variables, filesystem access, and network access. Passing `--yes` allows the harness to use all of those without prompting. On Linux, `--permissions read-only` or `edit-worktree` enables the [file guard](#the-file-guard) to restrict where the process can write, provided your repository is not inside a writable `HOME` (use `--isolated` so the branch gets a separate `HOME`).

The first time you run a local branch in a repository, `by` prints a one-time notice (`by: local mode: harnesses run as your user, in your own HOME; no sandbox`) and records `.branchyard/local-mode-noted` once the harness starts. All other modes print their mode line on every run.

## Isolated

```sh
by run "Fix the flaky parser test" --isolated --secret ANTHROPIC_API_KEY
```

To enable isolated mode by default for every run in the repository, set it in `branchyard.toml`:

```toml
[defaults]
isolated = true
```

You can override the config file for a single run with `--isolated=false` (accepted boolean values are `yes|no|true|false|on|off|1|0`, and bare `--isolated` means `true`).

**Guaranteed:** The harness runs with `HOME` set to `.branchyard/homes/BRANCH`, a directory used only by that branch (and its forks), keeping it out of your personal dotfiles and cached logins. Environment variables starting with `ANTHROPIC`, `CLAUDE`, `OPENAI`, `CODEX`, or `BRANCHYARD_` are stripped. Any secrets you provision via `--secret` or `[secrets]` are written with `0600` permissions inside that private home directory and nowhere else.

**Not guaranteed:** Isolated mode provides a private `HOME` and scrubbed environment, not a full OS sandbox. The harness still runs as your OS user, so it can read parent directories, access the network, and read environment variables outside the stripped prefixes (such as `GEMINI_API_KEY`, `GITHUB_TOKEN`, or `AWS_*`). Because your personal `HOME` is hidden, the harness will not see your normal interactive login unless you provision credentials via `by harnesses login ID` or `[secrets]`.

Before starting, `by` prints `by: isolated mode: harnesses run as your user with a private HOME under .branchyard/homes/; not a sandbox, so they can still read your files and reach the network`. The branch record stores this mode as well: `by show BRANCH` prints `provider  local, isolated: private HOME, not a sandbox`, and `--json` output includes `"provider": "local"` and `"isolated": true` alongside the turn's [`guard`](#the-file-guard) object.

## The file guard

Permission presets control which tool requests Branchyard approves when a harness asks. However, several harnesses (including Pi, Antigravity, DeepSeek Harness, Hermes, and OpenCode in its default configuration) execute file writes or shell commands without routing a permission request through Branchyard first.

To enforce write scopes at the OS level, Branchyard on Linux uses Landlock to restrict local harness processes whenever a turn runs under `--permissions read-only` or `--permissions edit-worktree`. The harness and every child process it spawns can only write to the paths allowed by the preset, regardless of which tool it invokes:

| Preset | Allowed write paths | Blocked write paths |
|---|---|---|
| `edit-worktree` | The branch worktree; the branch Git directory plus `.git/objects`, `.git/refs`, and `.git/logs` (so commits succeed); the branch's `HOME`; its temporary directory (`.branchyard/tmp/BRANCH`, set as `TMPDIR`); `/dev`; and configured [scratch areas](storage.md) | The rest of the repository (main checkout, other branches' worktrees, `.branchyard/`) and the rest of the filesystem |
| `read-only` | The branch's `HOME`, its temporary directory, and `/dev` | The branch worktree, the Git directory (no commits can be created), and the rest of the filesystem |
| `full`, `--yes`, `--ask`, or custom rules | Anywhere your OS user can write (no Landlock guard) | — |

Note that `HOME` in the table above is `.branchyard/homes/BRANCH` when `--isolated` is enabled, and **your personal `HOME` directory otherwise**. Because Landlock grants write access recursively under every allowed path and cannot carve out subdirectories, if your repository lives inside your personal `HOME` (for example, `/home/you/project`) and you run without `--isolated`, allowing writes to `HOME` would also allow writes to the repository. In that situation, the run is **not guarded**, and `by` prints a warning before starting and records the reason on the branch:

```text
by: note: no file guard for this run: your HOME (/home/you) holds this repository, and a guard that lets the harness write in HOME would let it write the repository too; --isolated gives it a private HOME the guard can hold; --permissions edit-worktree decides only the tool calls the harness asks Branchyard about
```

Pass `--isolated` so the branch uses a private `HOME` under `.branchyard/homes/` and Landlock can protect the repository checkout. Planning turns (`--plan`) automatically run under `read-only` and its Landlock guard, and delegated child branches receive their own guard scoped to their own worktree.

When a guarded process attempts an out-of-scope write, the kernel returns `EACCES` (`Permission denied`), which the harness sees as a normal filesystem error (for example, Pi 1.4.0 on 27 October 2026 reported `EACCES: permission denied, open '…/.branchyard/worktrees/outside.txt'`).

**Reported and recorded.** Before starting a guarded local turn, `by` prints the active Landlock scope:

```text
by: guard: landlock (edit-worktree): the harness can write only in its worktree, its git directory, your own HOME, its temporary directory and /dev
```

The branch record stores the guard status of its latest turn: `by show` and `by inspect` include a `guard` line under `provider` (`landlock (edit-worktree, ABI 7): writes only in …` or `none: <why>`), and `--json` outputs include `"guard": {"kind": "landlock", "preset": "edit-worktree", "abi": 7, "writable": [...]}` or `{"kind": "none", "reason": "..."}`. `by doctor` reports the host's Landlock ABI under `host.landlock`.

**When Landlock is unavailable.** On Linux kernels without Landlock enabled (prior to Linux 5.13 or missing from the `lsm=` boot parameter), on macOS, or when `BRANCHYARD_GUARD=off` is set in the environment, the turn runs without the OS guard and prints a notice:

```text
by: note: no file guard on this host (BRANCHYARD_GUARD=off turns it off): --permissions read-only decides only the tool calls the harness asks Branchyard about
```

The branch record then stores `guard: none: this host cannot hold it: …`. For sandbox providers, the branch records `none: the harness runs in a sandbox (…), which is its boundary`; Microsandbox, Substrate, and recipe providers reject an `ExecSpec::writable` request rather than silently ignoring it.

**What the file guard does not restrict:**

- **Reads:** A guarded process can still read any file your OS user can read, including `~/.ssh`, other worktrees, and the main checkout.
- **Network access:** Network restrictions are controlled separately by the [egress policy](egress.md).
- **Per-command filtering:** Within its writable paths, the harness can modify any file. A shell command that a preset would deny if asked can still execute in a harness that does not ask for approval first.
- **Dotfiles and Git refs in non-isolated `edit-worktree` mode:** Without `--isolated`, your personal `HOME` remains writable. Also, because `.git/refs` is writable under `edit-worktree` so the harness can commit, a harness running raw `git` commands could update refs belonging to other branches.
- **External daemons:** Processes already running outside the branch's process tree (such as a background language server or Docker daemon) are not inside the Landlock ruleset.
- **Nested user-namespace sandboxes:** Because Landlock requires `PR_SET_NO_NEW_PRIVS`, blocks writes to `/proc/self/uid_map`, and forbids `mount(2)` inside restricted processes, inner sandboxes that rely on user namespaces and mounts (such as `bubblewrap` or `unshare -rm`) will fail inside a guarded turn.

**How it is built.** [`crates/branchyard-runtime/src/landlock.rs`](../crates/branchyard-runtime/src/landlock.rs): the ruleset is built before the fork (each allowed path opened `O_PATH` and added beneath with every write-side right the kernel's ABI knows: writing, truncating, removing, making files, directories, links, sockets, FIFOs and devices, and `REFER` for renames between directories); between `fork` and `exec` the child sets `PR_SET_NO_NEW_PRIVS` and calls `landlock_restrict_self`, after the network namespace's own setup when egress is confined. Reads, execution, `ioctl` and the network are not handled. Support is probed once per process by restricting `/bin/sh` and seeing a write to `/dev/null` refused. `PR_SET_NO_NEW_PRIVS`, which Landlock needs, also means a set-user-ID program (`sudo`) runs without its privileges inside a guarded harness. The engine derives the scope from the turn's preset in [`crates/branchyard/src/guard.rs`](../crates/branchyard/src/guard.rs).

**Delegation under the guard.** A delegating harness needs nothing more: the `by` its shell runs (and `by mcp`) only connects to the engine's broker over a Unix socket, which Landlock's file rights do not cover, and the child's turn runs on a thread of the engine, outside the parent's restriction, under its own guard.

Status, 27 October 2026: built; hermetic, and run live once. Hermetic: `/bin/sh` and the fake ACP agent under the guard (`branchyard-runtime` `tests/guard.rs`, `tests/guard_off.rs`, the `write_scope` provider check), the engine (`tests/guard.rs`: a commit the harness makes under `edit-worktree` lands) and the built `by` (`branchyard-cli` `tests/guard.rs`: the lines, the record, `--yes`, `BRANCHYARD_GUARD=off`, `by doctor`, and a harness that runs `by spawn --wait` from inside the guard while its child is guarded in its own worktree). Live: Pi 1.4.0 on `zai/glm-5.3-flash` (Z.ai), `--isolated --allow-unapproved-tools`, Linux 6.18 with Landlock ABI 7, from this repository's `by`. Under `read-only`, asked to create `hello.txt`: `The write failed: the current directory (…/.branchyard/worktrees/ro) is not writable — permission denied (EACCES)`, and the branch ended `no changes`. Under `edit-worktree`, asked to write `hello.txt` and `../outside.txt`: `Successfully wrote to hello.txt`, then `EACCES: permission denied, open '…/.branchyard/worktrees/outside.txt'`, and the branch ended `ready` with one file. The same `read-only` prompt with `BRANCHYARD_GUARD=off` created `hello.txt` and ended `ready`: what every Pi run under `read-only` did before. The three runs took 15, 19 and 15 seconds and cost $0.0005, $0.0004 and $0.0001 as Pi reported; the quoted lines, copied from their output, are the record. Not done: the other four harnesses that write without asking (Antigravity, DeepSeek Harness, Hermes, OpenCode) and the ones that ask (Claude Code, Codex, Gemini CLI) were not run under the guard; Codex's own Linux sandbox under it was not tried; no kernel older than ABI 7 was tried (ABI 1 and 2 hosts get fewer handled rights and refuse renames between directories); macOS has no guard.

## Sandbox

A sandbox provider runs each turn's harness somewhere that is not your process space. Three exist; all three are **unqualified**: they run against fakes in this repository's tests (the Microsandbox mapping, a fake Substrate cluster, a recipe's scripts), and none has run a harness on a real runtime in them. See [validation](validation.md) before you rely on one.

| Provider | Flag | File | Prerequisites |
|---|---|---|---|
| Microsandbox | `--provider microsandbox --image REF [--cpus N] [--memory MIB] [--pass-env NAME,...]` | `[defaults] provider = "microsandbox"` and `[microsandbox] image`, `cpus`, `memory_mib` (or `memory`), `pass_env`, `keep` (or `keep_sandbox`), `snapshots` (or `sandbox_snapshots`) | KVM; a build with `--features microsandbox`; `msb`. An image that has the harness installed |
| Recipe | `--provider recipe:NAME` | `[defaults] provider = "recipe"` and `recipe = "NAME"`, with `[recipes.NAME]` ([recipes](recipes.md)) | Your create, exec, suspend, resume and destroy commands, trusted with `by recipe trust NAME` |
| Agent Substrate | `--provider substrate` with `--substrate-*` flags ([substrate](substrate.md)) | none: the file takes no `substrate` | A cluster and a template that runs the bridge |

```sh
by run "Fix the flaky parser test" --provider microsandbox --image ghcr.io/me/claude:1 --pass-env ANTHROPIC_API_KEY
```

**Guaranteed, when the provider works:** the harness sees the worktree (mounted or copied), a home of its own and the variables you named, and not your filesystem or your login. A kept sandbox, snapshots and live branching are options of the lifecycle ([sandbox snapshots](sandbox-snapshots.md)).

**Not guaranteed:** that the provider is sound. They are unqualified. Network egress policy is enforced only for local branches on Linux with user namespaces; sandboxes declare none ([egress](egress.md)). A sandbox that cannot reach the host runs a delegating turn without the delegation tools, and the turn says so.

Before it starts, `by` says: `by: sandbox mode: harnesses run in Microsandbox microVMs from IMAGE; the worktree is mounted at /workspace`. On a build without the SDK the same command stops before creating anything:

```text
by: unsupported: this build has no Microsandbox support; rebuild from a checkout with `cargo install --locked --path crates/branchyard-cli --features microsandbox` (it needs Rust 1.94 or newer and libcap-ng-dev to build, and KVM to run a microVM); see docs/modes.md#sandbox
```

To check a configured default before you run anything: `by doctor` (`provider.default`: "this build can run it", or the command above; `host.kvm`) and `by up --plan` (the `provider` step, ready or blocked, changing nothing).

## Server

A server keeps branches, runs the work and outlives your terminal. It does not isolate anything: with the local provider it runs harnesses as the user it runs as.

On the host, in the repository:

```sh
by init server                       # an interview that writes .branchyard/server.json and token files
by serve --config .branchyard/server.json --check   # load and validate it; serves nothing
by serve --config .branchyard/server.json
```

or just `by serve`, which serves the current repository on loopback and makes a token at `.branchyard/server/token` the first time (and says where, each time it starts). `[serve] config = ".branchyard/server.json"` in `branchyard.toml` makes a bare `by serve` read that file.

Where you work:

```sh
by --remote http://127.0.0.1:8421 --token-file .branchyard/server/token ls
```

```toml
[remote]
url = "http://127.0.0.1:8421"
token_file = ".branchyard/server/token"   # or .branchyard/tokens/admin.token, from `by init server`
repo = "app"                              # when the server serves several
# ca_file = "ca.pem"                      # for a private certificate authority
```

`BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO` and `BRANCHYARD_CA_FILE` are the same four, above the file. `--remote ssh://HOST/PATH` starts a server on the host for you and reaches it through a forwarded socket ([remote over ssh](remote-ssh.md)).

**Guaranteed:** operations are admitted once and survive a restart; tokens carry scopes (`read`, `run`, `merge`, `admin`), tenants and quotas; plain HTTP is refused off loopback. The operator-only `BRANCHYARD_MANAGER_TOKEN` is removed from native harness environments, including inherited and explicitly configured environments. Sandbox pass-through refuses that variable. The separate [workspace manager](workspace-management.md) refuses managed turns.

**Not guaranteed:** isolation. A `run` token on a repository can make a harness do anything the server's user can; scopes bound what a credential reaches, not what a harness does. `--allow-provider microsandbox,substrate` lets a request name a sandbox, the server's operator chooses it, and `local` stays allowed ([server](server.md#security)).

When the server does not answer, `by` ends with the way out:

```text
by: cannot reach http://127.0.0.1:8421: Connection refused (os error 111)
  next: start a server with `by serve` on that host, or fix the URL, token or CA file ([remote] url, token_file, ca_file)
```

`by doctor` asks a configured server once (`server.reachable`), and reads the file `[serve] config` names without starting anything: `serve.config` is `by serve --check` on it, and `serve.database` says which store it names.

## Postgres

SQLite keeps one server's branches and operations. PostgreSQL lets several servers and `by worker` processes share them: an operation admitted before a crash runs once, on whichever worker claims it.

```sh
by serve --config .branchyard/server.json --database postgres://branchyard@db.internal/branchyard
by worker --config .branchyard/server.json --database postgres://branchyard@db.internal/branchyard --label gpu
```

or `"database": "postgres://…"` in the server's file. The URL is stored in that file, so it holds no password: use a `.pgpass` file. [Server](server.md) has the schema, the scheduling and the labels.

**Needs the `postgres` feature.** The released binaries have it. A `cargo install` from the README does not, and then `by serve` would refuse to start. `by serve --check` says so first, without connecting to anything:

```text
$ by serve --config .branchyard/server.json --check
by serve: this build has no PostgreSQL support; use a prebuilt release, which has it, or rebuild from a checkout with `cargo install --locked --path crates/branchyard-cli --features postgres` (`--path crates/branchyard-server` for branchyard-server); see docs/modes.md#postgres
```

`--check` and `serve.database` read the file and the build; they do not open the database. A wrong host, user or password shows when `by serve` connects.

## Moving between rungs

| From | To | Change |
|---|---|---|
| Local | Isolated | add `--isolated`, or `isolated = true` under `[defaults]`; then `--secret NAME` for the harness's login |
| Isolated | Local, for one run | `--isolated=false` |
| Either | Sandbox | build with the feature if you need it; add `--provider ...` or `[defaults] provider`; run `by doctor` first |
| Any | Server | `by serve` on the host; `[remote]` or `--remote` here. The harnesses then run there, as that host's user |
| Server | PostgreSQL | a database, a build that has the feature, `--database` or `"database"`; `by serve --check` |

The branch records which one a branch ran in, and `by show` says so. Where it ran does not change afterwards: `by send` continues a branch where it started unless you name another provider.

## Not done

- The sandbox rungs have not run a harness on a real runtime from this repository's tests. Microsandbox compiles and passes its unit tests; its KVM tests are `#[ignore]`d. No Substrate cluster has been used.
- No released binary has the Microsandbox SDK. Shipping one needs `libcap-ng` at build time and a story for KVM and `msb` in `by up`; that is a decision, not a fix. Until then, the rung is reachable only from a source build.
- The file guard has no flag of its own: the preset decides it, and `BRANCHYARD_GUARD=off` is the only way to run a preset unguarded on a host that has Landlock. It guards writes only; a read scope (keeping a harness out of `~/.ssh`) and guarding `--ask` and a denying default (which name no preset) are not built.
- The sandbox's address for the host is three keys with three names (`[connectors] sandbox_gateway`, `[models] sandbox_host`, `[connectors] effects_sandbox_host`). They are not merged.
- `--isolated` and `--provider` are two controls for one axis. A single `isolation = "none" | "private-home" | "sandbox"` would be clearer; it is a design change, not made here.
- There is no `by doctor --modes` that prints the highest rung this build and host can reach, with the command for the next. The table above is that, by hand.
- `by serve --check` does not connect to PostgreSQL. A server file that names a database nothing listens on passes it.
- `[microsandbox]` accepts the flags' names (`memory`, `keep_sandbox`, `sandbox_snapshots`) as well as its own; the flags do not accept the file's (`--memory-mib`). Substrate has no file section at all.
