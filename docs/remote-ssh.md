# Remote over ssh

Passing `by --remote ssh://[user@]host[:port]/path/to/repo` runs any `by` command against a Git repository on a remote machine over SSH. Nothing needs to be pre-configured on the remote host beyond having `by` on its `PATH`: no TCP port is opened, no TLS certificates are required, and you do not need to copy bearer tokens by hand.

```sh
by --remote ssh://me@build.example/srv/app run "Make the flaky parser test deterministic" --check "cargo test" --yes
by --remote ssh://me@build.example/srv/app ls
export BRANCHYARD_REMOTE=ssh://me@build.example/srv/app   # or [remote] url in branchyard.toml
by watch
by remote ssh status            # inspect the SSH connection and remote server
by remote ssh stop              # stop the remote server and close the SSH connection
```

To point at a repository inside your remote home directory, use `ssh://host/~/src/app`. Every `--remote` command works the same way as it does against an `http(s)://` server ([server](server.md#remote-mode-in-by)) because the CLI speaks the same HTTP API over a forwarded Unix domain socket.

## What happens

When you run the first command against an `ssh://` URL, `by` performs five steps automatically:

1. **Opens a shared SSH connection:** Spawns the system `ssh` client as a background ControlMaster (`ssh -M -N -f -o ControlPersist=600`), using your normal SSH configuration (keys, agent, `~/.ssh/config`, `ProxyJump`, and interactive prompts if needed). Subsequent `by` commands reuse the existing master connection, which closes automatically after ten minutes of inactivity.
2. **Starts or reattaches to the remote server:** Sends a POSIX `sh` bootstrap script over standard input that creates `~/.branchyard/remote/<key>/run/` (mode `0700`, where `<key>` is a hash of the repository path), generates a random bearer token (`token`, mode `0600`, read from `/dev/urandom`) if one does not already exist, and starts `by serve --listen-unix ~/.branchyard/remote/<key>/run/by.sock --token-file …/token` in the target repository under `nohup`, waiting until the socket appears. `--listen-unix` serves HTTP over a mode `0600` Unix socket and refuses to run if the parent directory is accessible to other users. This layout is modeled on emdash's workspace server ([ports](vendoring.md#emdash-and-orca-ports-as-data-and-as-translations)).
3. **Reads the bearer token:** Streams the token back over standard output of that same SSH channel into a local `0600` file, so the token never appears in process command-line arguments on either machine.
4. **Forwards the Unix socket:** Runs `ssh -O forward -L <here>/by.sock:<there>/run/by.sock` over the control master so no TCP port is opened on either host.
5. **Connects the HTTP client:** Talks to the forwarded socket using `branchyard-client` (`Client::new("unix:/path/to/socket", token)`).

Local connection state is stored in a private directory per URL under `$BRANCHYARD_SSH_DIR` (or `$XDG_RUNTIME_DIR/branchyard-ssh`, falling back to `~/.branchyard/ssh`), keyed by a digest of the URL. That directory holds the SSH control socket, the forwarded Unix socket, the bearer token, and the control master log. Because Unix domain socket paths must stay under roughly 100 bytes, set `BRANCHYARD_SSH_DIR` to a short path (such as `/tmp/by-ssh-$UID`) if your home directory path is deep.

| Variable | Default | Purpose |
|---|---|---|
| `BRANCHYARD_SSH` | `ssh` | Path to the local `ssh` executable |
| `BRANCHYARD_SSH_BY` | `by` | Path to `by` on the remote host (useful when it is not on the non-interactive `PATH`) |
| `BRANCHYARD_SSH_SERVE_ARGS` | none | Extra arguments passed to `by serve` when starting the remote server (such as `--allow-delegation`), split on whitespace |
| `BRANCHYARD_SSH_DIR` | see above | Local directory for SSH control and forwarded sockets |

The remote server also loads `[serve] config` from the remote repository's `branchyard.toml` ([setup](setup.md)). Server startup flags take effect when the remote `by serve` process starts; run `by remote ssh stop` before your next command if you want it to restart with new flags.

## Trust and isolation

The remote `by serve` process runs as your SSH user on the remote host with **no extra sandbox isolation** by default, just like a local `by serve` instance ([server](server.md#security)). Only processes running as your user on the remote host can access the Unix socket or read the token file. Passing `--token-file` or `--ca-file` alongside an `ssh://` URL is refused because SSH transport manages its own socket and token.

Running `by remote ssh stop` sends `SIGTERM` to the remote server (which drains active work for up to 90 seconds; see [running it](server.md#running-it)), closes the SSH control master, and removes the local socket and cached token. The remote token file and the repository's `.branchyard/` state remain on the remote machine for the next session.

## Tested

Hermetic on 29 October 2026: a local fork probe confirmed that a child can retain
a Unix listener until exec even when its descriptor is close-on-exec. The server
unit test checks that a duplicated live listener remains protected after the
parent handle closes, then waits for stale-socket replacement. The directory and
regular-file refusals remain checked. Record: [socket teardown evidence](evidence/unix-socket-teardown-2026-10-29.json).
Not done: this fork probe on other operating systems.

`crates/branchyard-cli/tests/remote_ssh.rs` runs the whole path hermetically, with `crates/branchyard-recipe/tests/fixtures/fake-ssh` in place of `ssh`: a Python stand-in that runs each "remote" command here in a session of its own with a separate remote `HOME`, keeps a control master as a background process, and implements `-O check|forward|exit` with a small Unix socket proxy. The test checks the master starts once and is reused, the remote directory, run directory, token and socket modes, that the token reached no argv, `by run` with the fake ACP agent, `ls`, `log` and `diff` through the forward, the server reused by pid, `by remote ssh status` (text and JSON), `stop`, and a restart that keeps the branches; and the refusals (an unknown host, a missing directory, an option-shaped host, `--token-file`). `crates/branchyard-server` tests `--listen-unix`'s private-directory rule and stale-socket replacement; `branchyard-client` parses `unix:` endpoints.

Live on 28 October 2026: the workstation's real SSH client reached OpenSSH 9.6p1 on GCE with strict host-key checking. `by --remote ssh://…` started its control master and remote Unix-socket server, ran Antigravity `agy` 1.4.8 through write, check, diff and merge, and reported the connection up and server running. `by remote ssh stop` then reported the server stopped and connection closed; a separate SSH command confirmed the server socket was gone. Record: `logs/agy-backends/ssh-server` in the [remote validation record](remote-validation-2026-10-28.md). Not done: the ten-minute idle expiry, reconnect after transport loss, or alternative remote login shells in this live run.
