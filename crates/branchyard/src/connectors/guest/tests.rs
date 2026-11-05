//! Starting the credential proxy in a sandbox: its command line and
//! environment, waiting for it to be ready, its reason when it cannot start,
//! and its end with the turn. A shell script stands in for `by`, run by the
//! fake sandbox provider on this host.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_sandbox::{Mount, SandboxSpec};
use branchyard_testkit::{wait, Scratch};

use super::super::sandbox_proxy::IdentityKind;
use super::*;

fn config(identity: IdentityKind) -> ProxyConfig {
    ProxyConfig {
        listen: "127.0.0.1:18900".into(),
        server: "https://by.example".into(),
        upstream: "http://192.168.127.1:8931/mcp".into(),
        manifest: "blake3:abc".into(),
        branch: "app/feat".into(),
        turn: 4,
        identity,
        secret: None,
    }
}

fn script(dir: &Path, body: &str) -> String {
    let path = dir.join("by");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn sandbox(scratch: &Scratch) -> FakeProvider {
    let provider = FakeProvider::plain(Box::new(LocalProvider::new()), scratch.join("fake"));
    // The scratch directory is mounted at its own path, so the stand-in
    // `by` and what it writes are where the guest and the test both see.
    provider
        .ensure(&SandboxSpec::new("box").mount(Mount::writable(scratch.path(), scratch.path())))
        .unwrap();
    provider
}

#[test]
fn the_command_line_names_the_turn_and_never_holds_a_secret() {
    let guest = argv("by", &config(IdentityKind::Secret));
    assert_eq!(
        guest,
        [
            "by",
            "credential-proxy",
            "--listen",
            "127.0.0.1:18900",
            "--server",
            "https://by.example",
            "--upstream",
            "http://192.168.127.1:8931/mcp",
            "--manifest",
            "blake3:abc",
            "--branch",
            "app/feat",
            "--turn",
            "4",
            "--identity",
            "secret"
        ]
    );
    let workload = argv(
        "/opt/by",
        &config(IdentityKind::Workload {
            audience: "https://by.example".into(),
        }),
    );
    assert_eq!(workload[0], "/opt/by");
    assert_eq!(
        workload[workload.len() - 4..],
        ["--identity", "workload", "--audience", "https://by.example"]
    );
}

#[test]
fn a_ready_proxy_is_given_the_secret_in_its_environment_and_dies_with_the_turn() {
    let scratch = Scratch::new("guest-proxy");
    let command = script(
        scratch.path(),
        r#"out="$(dirname "$0")"
echo "$$" > "$out/pid"
echo "$@" > "$out/argv"
echo "$BRANCHYARD_CREDENTIAL_SECRET" > "$out/secret"
echo "listening 127.0.0.1:18900"
exec cat"#,
    );
    let provider = sandbox(&scratch);
    let proxy = start(
        &provider,
        "box",
        scratch.path().to_str().unwrap(),
        &command,
        &config(IdentityKind::Secret),
        Some("s3cr3t-for-this-turn"),
    )
    .unwrap();
    // The secret came in the environment; no argument carries it.
    assert_eq!(
        fs::read_to_string(scratch.join("secret")).unwrap().trim(),
        "s3cr3t-for-this-turn"
    );
    let arguments = fs::read_to_string(scratch.join("argv")).unwrap();
    assert!(!arguments.contains("s3cr3t"), "{arguments}");
    assert!(arguments.contains("--manifest blake3:abc"), "{arguments}");
    let pid: i32 = fs::read_to_string(scratch.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // A zombie is dead: only a running process counts.
    let alive = |pid: i32| {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit(')')
                .next()
                .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
        })
    };
    assert!(alive(pid));
    // Dropping it, as the end of the turn does, kills the process.
    drop(proxy);
    wait::until("the proxy to be gone", || !alive(pid));
}

#[test]
fn a_proxy_that_cannot_start_fails_the_turn_with_its_own_reason() {
    let scratch = Scratch::new("guest-proxy-fails");
    let command = script(
        scratch.path(),
        r#"echo "the server refused a token (409): turn_not_running: not running" >&2
exit 1"#,
    );
    let provider = sandbox(&scratch);
    let failed = start(
        &provider,
        "box",
        scratch.path().to_str().unwrap(),
        &command,
        &config(IdentityKind::Secret),
        Some("x"),
    )
    .err()
    .expect("a proxy that exits does not start");
    assert!(failed.contains("turn_not_running"), "{failed}");
    // A command that is not there is a failure too, and names the command.
    let missing = start(
        &provider,
        "box",
        scratch.path().to_str().unwrap(),
        "/no/such/by",
        &config(IdentityKind::Secret),
        Some("x"),
    )
    .err()
    .expect("no such program");
    assert!(
        missing.contains("/no/such/by") || missing.contains("did not start"),
        "{missing}"
    );
}
